//! Planning and applying canonical→client synchronization.
//!
//! Everything here is pure value manipulation: the CLI owns all I/O
//! (detection, backups, atomic writes), so the risky logic — what to touch
//! and what to leave alone — is fully unit-testable. Client-shaped values
//! come from the client's [`codec`](crate::clients::codec); a plan is the
//! same computation whatever format they end up written in.

use std::collections::{BTreeMap, BTreeSet};

use crate::clients::codec::{self, ClientDocument};
use crate::clients::{self, ClientKind};
use crate::config::{Server, Transport};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncPlan {
    pub adds: Vec<String>,
    pub updates: Vec<String>,
    pub removes: Vec<String>,
    /// Canonical names that already exist in the client but were never
    /// written by mcpgw — never overwritten, only reported.
    pub conflicts: Vec<String>,
    /// Client-only entries mcpgw does not manage; left untouched.
    pub foreign: Vec<String>,
    /// Names to drop from the client's own exclusion list — see
    /// [`ClientKind::exclusion_list`]. Only ever names mcpgw manages: the
    /// list is the user's, and a foreign name in it is their decision.
    ///
    /// Filled by [`plan_client_context`], which needs the whole document;
    /// empty for every client that has no such list.
    pub unexclude: Vec<String>,
    /// The full managed target set (enabled canonical servers minus
    /// conflicts) in the client's entry shape.
    desired: BTreeMap<String, serde_json::Value>,
}

impl SyncPlan {
    #[must_use]
    pub fn has_changes(&self) -> bool {
        // `unexclude` counts: without it a managed server that is enabled,
        // written and named in `mcp.excluded` is a stable wrong state — the
        // entry already matches, so nothing else in the plan ever fires.
        !(self.adds.is_empty()
            && self.updates.is_empty()
            && self.removes.is_empty()
            && self.unexclude.is_empty())
    }

    /// The names mcpgw manages in this client once the plan is applied.
    #[must_use]
    pub fn managed_after(&self) -> BTreeSet<String> {
        self.desired.keys().cloned().collect()
    }

    /// The entries the plan actually writes. Entries whose emitted value
    /// already matches the file are left out, so their formatting — and in
    /// JSONC/TOML their comments — is never touched.
    fn upserts(&self) -> Vec<(&str, &serde_json::Value)> {
        self.adds
            .iter()
            .chain(&self.updates)
            .map(|name| (name.as_str(), &self.desired[name]))
            .collect()
    }
}

/// Computes what `sync` would change for one client.
///
/// `current` is the entry map read out of the client file, `managed` the
/// names mcpgw previously wrote there. An entry counts as changed only when
/// the value mcpgw would emit differs from what is on disk, so a run that
/// changes nothing writes nothing.
///
/// The comparison is on parsed values, not raw text. For plain JSON the two
/// are the same thing, but TOML and JSONC can spell one entry several ways
/// (section vs inline table, single vs double quotes, trailing commas, key
/// order, a comment in the middle) — comparing text there would report an
/// update every run and rewrite the user's file into mcpgw's spelling.
///
/// What the plan would write is the emitted entry *plus* whatever the entry
/// already on disk holds in the client's own fields (see
/// [`codec::EntrySchema::preserved_fields`]), and the comparison is against
/// that —
/// so preserving a field cannot make the entry differ from itself and
/// re-diff forever.
#[must_use]
pub fn plan_sync(
    kind: ClientKind,
    current: &serde_json::Map<String, serde_json::Value>,
    canonical: &BTreeMap<String, Server>,
    managed: &BTreeSet<String>,
) -> SyncPlan {
    let mut plan = SyncPlan {
        adds: Vec::new(),
        updates: Vec::new(),
        removes: Vec::new(),
        conflicts: Vec::new(),
        foreign: Vec::new(),
        unexclude: Vec::new(),
        desired: BTreeMap::new(),
    };

    for (name, server) in canonical {
        // Disabled canonical servers are not mirrored; if previously
        // managed they fall through to the remove pass below.
        if !server.enabled {
            continue;
        }
        let exists = current.contains_key(name);
        if exists && !managed.contains(name) {
            plan.conflicts.push(name.clone());
            continue;
        }
        let mut emitted = client_entry(kind, server);
        carry_over_client_fields(kind, current.get(name), &mut emitted);
        if !exists {
            plan.adds.push(name.clone());
        } else if current.get(name) != Some(&emitted) {
            plan.updates.push(name.clone());
        }
        plan.desired.insert(name.clone(), emitted);
    }

    for name in current.keys() {
        if plan.desired.contains_key(name) || plan.conflicts.contains(name) {
            continue;
        }
        if managed.contains(name) {
            plan.removes.push(name.clone());
        } else {
            plan.foreign.push(name.clone());
        }
    }
    plan
}

/// Copies the client's own fields off the entry already on disk onto the one
/// mcpgw is about to write.
///
/// `emit` builds an entry from the canonical server, which knows nothing
/// about a switch the user flipped inside the client. Writing that value
/// straight over the old entry reset every such field on every sync — the
/// user's "off" became "on" again and the entry re-diffed forever.
fn carry_over_client_fields(
    kind: ClientKind,
    existing: Option<&serde_json::Value>,
    emitted: &mut serde_json::Value,
) {
    let fields = kind.codec().entries.preserved_fields();
    if fields.is_empty() {
        return;
    }
    let (Some(existing), Some(target)) = (
        existing.and_then(serde_json::Value::as_object),
        emitted.as_object_mut(),
    ) else {
        return;
    };
    for field in fields {
        // Only what is missing: a field mcpgw does emit is one it owns.
        if let Some(value) = existing.get(*field)
            && !target.contains_key(*field)
        {
            target.insert((*field).to_owned(), value.clone());
        }
    }
}

/// Folds the client's out-of-entry state into a finished plan.
///
/// [`plan_sync`] sees the server map alone, which is all any client but
/// Gemini keeps a server's state in. Gemini's `mcp.excluded` is the
/// exception, and it needs the whole document — so it is a second pass here
/// rather than a wider signature every other client would ignore. Symmetric
/// with the read side's own document-context pass.
pub fn plan_client_context(kind: ClientKind, document: &serde_json::Value, plan: &mut SyncPlan) {
    let Some(path) = kind.exclusion_list() else {
        return;
    };
    let Some(excluded) = clients::value_at(document, path).and_then(serde_json::Value::as_array)
    else {
        return;
    };
    // Every name mcpgw is responsible for after this run: the managed target
    // set, which must actually start, plus the entries being removed, whose
    // name would otherwise linger in the list and silently disable a server
    // the user later re-adds by hand. Conflicts and foreign entries are
    // deliberately absent — those exclusions are the user's.
    let mine: BTreeSet<&str> = plan
        .desired
        .keys()
        .chain(&plan.removes)
        .map(String::as_str)
        .collect();
    plan.unexclude = excluded
        .iter()
        .filter_map(serde_json::Value::as_str)
        .filter(|name| mine.contains(name))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
}

/// Applies a plan to a parsed client document of any format, touching only
/// the entries the plan owns. Everything else — other root keys, foreign
/// entries, and whatever the format preserves around them — is left alone.
///
/// # Errors
///
/// Returns [`codec::NotAnObject`] when a key on the way to the server map
/// holds something that is not one; the document is then left untouched.
pub fn apply_plan_to(
    kind: ClientKind,
    doc: &mut ClientDocument,
    plan: &SyncPlan,
) -> Result<(), codec::NotAnObject> {
    doc.edit(kind.codec().root, &plan.removes, &plan.upserts())?;
    if let Some(path) = kind.exclusion_list() {
        doc.remove_from_string_array(path, &plan.unexclude.iter().cloned().collect());
    }
    Ok(())
}

/// [`apply_plan_to`] for a client document already in hand as JSON.
///
/// # Errors
///
/// Same failure as [`apply_plan_to`].
pub fn apply_plan(
    kind: ClientKind,
    root: &mut serde_json::Value,
    plan: &SyncPlan,
) -> Result<(), codec::NotAnObject> {
    codec::edit_json(kind.codec().root, root, &plan.removes, &plan.upserts())
}

/// The synthetic server that points one client at *one* server's own gateway
/// endpoint (`<base>/s/<name>`).
///
/// The entry is named as the server it stands for, which is the whole point:
/// a client synced directly and then synced in gateway mode sees the same
/// names change transport, so the flip is a set of updates rather than a set
/// of conflicts against entries mcpgw already manages. It also means the
/// client's own fields on those entries — [`preserved_fields`], Cline's
/// `disabled` and `autoApprove` and their kin — carry over the flip: the user
/// switched *that server* off *in that client*, and reaching it through the
/// gateway does not undo that decision. Only the transport changes.
///
/// [`preserved_fields`]: codec::EntrySchema::preserved_fields
///
/// `enabled` and `tags` are carried over rather than forced on: this is the
/// same canonical server, reached differently, and [`plan_sync`] stays the one
/// place that decides a disabled server is not mirrored.
///
/// # Errors
///
/// Returns the parse error when `base_url` is not an absolute URL.
pub fn per_server_gateway_server(
    kind: ClientKind,
    name: &str,
    server: &Server,
    base_url: &str,
    bridge_command: &str,
) -> Result<Server, url::ParseError> {
    let transport = if kind.supports_http_entries() {
        Transport::Http {
            url: crate::endpoints::per_server_url(base_url, name)?,
            headers: BTreeMap::new(),
        }
    } else {
        Transport::Stdio {
            command: bridge_command.to_owned(),
            // The gateway's base URL plus the server's name, not the endpoint
            // path spelled out: the bridge derives the path, so a client file
            // written today keeps working if the path shape ever moves.
            args: vec![
                "connect".to_owned(),
                "--server".to_owned(),
                name.to_owned(),
                "--url".to_owned(),
                base_url.to_owned(),
            ],
            env: BTreeMap::new(),
        }
    };
    Ok(Server {
        enabled: server.enabled,
        tags: server.tags.clone(),
        transport,
    })
}

/// The whole desired set for one client in per-server gateway mode: every
/// canonical server, by its own name, pointing at its own endpoint.
///
/// Disabled servers are kept in the map — as disabled — so [`plan_sync`]
/// applies exactly the rule it applies in direct mode: not mirrored, and
/// removed if it managed them before.
///
/// # Errors
///
/// Returns the parse error when `base_url` is not an absolute URL.
pub fn per_server_gateway_servers(
    kind: ClientKind,
    canonical: &BTreeMap<String, Server>,
    base_url: &str,
    bridge_command: &str,
) -> Result<BTreeMap<String, Server>, url::ParseError> {
    canonical
        .iter()
        .map(|(name, server)| {
            let entry = per_server_gateway_server(kind, name, server, base_url, bridge_command)?;
            Ok((name.clone(), entry))
        })
        .collect()
}

/// The client-shaped value for one canonical server, as its codec spells it.
#[must_use]
pub fn client_entry(kind: ClientKind, server: &Server) -> serde_json::Value {
    kind.codec().entries.emit(server)
}
