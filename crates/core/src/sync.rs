//! Planning and applying canonical→client synchronization.
//!
//! Everything here is pure value manipulation: the CLI owns all I/O
//! (detection, backups, atomic writes), so the risky logic — what to touch
//! and what to leave alone — is fully unit-testable. Client-shaped values
//! come from the client's [`codec`](crate::clients::codec); a plan is the
//! same computation whatever format they end up written in.

use std::collections::{BTreeMap, BTreeSet};

use crate::clients::ClientKind;
use crate::clients::codec::{self, ClientDocument};
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
    /// The full managed target set (enabled canonical servers minus
    /// conflicts) in the client's entry shape.
    desired: BTreeMap<String, serde_json::Value>,
}

impl SyncPlan {
    #[must_use]
    pub fn has_changes(&self) -> bool {
        !(self.adds.is_empty() && self.updates.is_empty() && self.removes.is_empty())
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
        let emitted = client_entry(kind, server);
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

/// Applies a plan to a parsed client document of any format, touching only
/// the entries the plan owns. Everything else — other root keys, foreign
/// entries, and whatever the format preserves around them — is left alone.
pub fn apply_plan_to(kind: ClientKind, doc: &mut ClientDocument, plan: &SyncPlan) {
    doc.edit(kind.codec().root, &plan.removes, &plan.upserts());
}

/// [`apply_plan_to`] for a client document already in hand as JSON.
pub fn apply_plan(kind: ClientKind, root: &mut serde_json::Value, plan: &SyncPlan) {
    codec::edit_json(kind.codec().root, root, &plan.removes, &plan.upserts());
}

/// The entry name mcpgw owns in every client when syncing in gateway mode.
pub const GATEWAY_NAME: &str = "mcpgw";

/// The synthetic server that points one client at a running gateway.
///
/// It is a plain [`Server`] so gateway mode reaches the client file through
/// [`client_entry`] like every other entry — the two shapes cannot drift.
#[must_use]
pub fn gateway_server(kind: ClientKind, url: &str, bridge_command: &str) -> Server {
    let transport = if kind.supports_http_entries() {
        Transport::Http {
            url: url.to_owned(),
            headers: BTreeMap::new(),
        }
    } else {
        Transport::Stdio {
            command: bridge_command.to_owned(),
            // The URL is spelled out even when it is the default, so a reader
            // of the client file can see where the bridge points.
            args: vec!["connect".to_owned(), "--url".to_owned(), url.to_owned()],
            env: BTreeMap::new(),
        }
    };
    Server {
        enabled: true,
        tags: Vec::new(),
        transport,
    }
}

/// The client-shaped value for one canonical server, as its codec spells it.
#[must_use]
pub fn client_entry(kind: ClientKind, server: &Server) -> serde_json::Value {
    kind.codec().entries.emit(server)
}
