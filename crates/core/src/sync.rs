//! Planning and applying canonical→client synchronization.
//!
//! Everything here is pure JSON-value manipulation: the CLI owns all I/O
//! (detection, backups, atomic writes), so the risky logic — what to touch
//! and what to leave alone — is fully unit-testable.

use std::collections::{BTreeMap, BTreeSet};

use crate::clients::ClientKind;
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
}

/// Computes what `sync` would change for one client.
///
/// `current` is the raw entry map from the client file (its `mcpServers` /
/// `servers` object), `managed` the names mcpgw previously wrote there.
/// Idempotency is byte-level: an entry counts as changed only when the
/// value mcpgw would emit differs from what is on disk.
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

/// Applies a plan to the parsed client document, touching only the entries
/// the plan owns. Root-level keys other than the server map, and foreign
/// entries inside it, are preserved byte-for-byte (`preserve_order`).
pub fn apply_plan(kind: ClientKind, root: &mut serde_json::Value, plan: &SyncPlan) {
    if !root.is_object() {
        *root = serde_json::Value::Object(serde_json::Map::new());
    }
    // Normalized to an object above; the else-arm is unreachable.
    let Some(map) = root.as_object_mut() else {
        return;
    };
    let entries = map
        .entry(kind.root_key().to_owned())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !entries.is_object() {
        *entries = serde_json::Value::Object(serde_json::Map::new());
    }
    let Some(entries) = entries.as_object_mut() else {
        return;
    };

    for name in &plan.removes {
        entries.remove(name);
    }
    for name in plan.adds.iter().chain(&plan.updates) {
        entries.insert(name.clone(), plan.desired[name].clone());
    }
}

/// The client-shaped JSON for one canonical server.
///
/// VS Code's schema wants an explicit `type` on every entry; the
/// `mcpServers` clients infer stdio from `command`, so `type` is emitted
/// only where it carries information (http).
#[must_use]
pub fn client_entry(kind: ClientKind, server: &Server) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    match &server.transport {
        Transport::Stdio { command, args, env } => {
            if matches!(kind, ClientKind::VsCode) {
                obj.insert("type".to_owned(), "stdio".into());
            }
            obj.insert("command".to_owned(), command.as_str().into());
            if !args.is_empty() {
                obj.insert("args".to_owned(), args.clone().into());
            }
            if !env.is_empty() {
                obj.insert("env".to_owned(), string_map(env));
            }
        }
        Transport::Http { url, headers } => {
            obj.insert("type".to_owned(), "http".into());
            obj.insert("url".to_owned(), url.as_str().into());
            if !headers.is_empty() {
                obj.insert("headers".to_owned(), string_map(headers));
            }
        }
    }
    serde_json::Value::Object(obj)
}

fn string_map(map: &BTreeMap<String, String>) -> serde_json::Value {
    map.iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::from(v.as_str())))
        .collect::<serde_json::Map<_, _>>()
        .into()
}
