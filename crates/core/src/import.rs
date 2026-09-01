//! Planning for `mcpgw import`: turn lenient client reads into canonical
//! candidates — slugified names, cross-client dedup, canonical-collision
//! classification. Pure logic; the CLI owns prompting and writing.

use std::collections::BTreeMap;

use crate::clients::ClientRead;
use crate::config::{Server, validate_name};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportCandidate {
    /// Target name in the canonical config (slugified / suffixed).
    pub name: String,
    pub server: Server,
    /// `(client id, original entry name)` for every source carrying an
    /// identical definition — adoption marks these names managed so the
    /// next sync owns (and, when renamed, renames) the client entries.
    pub origins: Vec<(String, String)>,
    pub renamed: bool,
    /// Lossy-read notes (e.g. sse→http) surfaced next to the entry.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportPlan {
    /// Not present in the canonical config: import.
    pub new: Vec<ImportCandidate>,
    /// Identical entry already canonical: nothing to write, but sources
    /// are still adopted to avoid the perpetual sync conflict.
    pub already: Vec<ImportCandidate>,
    /// Same canonical name, different definition: needs a user decision.
    pub conflicts: Vec<ImportCandidate>,
}

/// Builds an import plan. `sources` order is significant: on cross-client
/// name clashes with different definitions, the first client keeps the
/// name and later ones are suffixed.
#[must_use]
pub fn plan_import(
    sources: &[(String, ClientRead)],
    canonical: &BTreeMap<String, Server>,
) -> ImportPlan {
    let mut candidates: Vec<ImportCandidate> = Vec::new();

    for (client_id, read) in sources {
        for (orig_name, server) in &read.servers {
            let notes: Vec<String> = read
                .problems
                .iter()
                .filter(|p| p.server.as_deref() == Some(orig_name))
                .map(|p| p.message.clone())
                .collect();

            // Identical definition seen before (any name): one canonical
            // entry, several adopted origins.
            if let Some(existing) = candidates
                .iter_mut()
                .find(|c| c.server.transport == server.transport)
            {
                existing
                    .origins
                    .push((client_id.clone(), orig_name.clone()));
                // Dedup keys on the transport alone, so everything *around*
                // the transport has to be merged rather than dropped with
                // the later source: a lossy-read note that only one client
                // produced still matters, and so does a tag only one client
                // carries. `enabled` merges conservatively — disabled in any
                // client imports disabled, because re-enabling is one
                // command while an unnoticed re-enable is a surprise.
                existing.server.enabled &= server.enabled;
                for tag in &server.tags {
                    if !existing.server.tags.contains(tag) {
                        existing.server.tags.push(tag.clone());
                    }
                }
                for note in notes {
                    if !existing.notes.contains(&note) {
                        existing.notes.push(note);
                    }
                }
                continue;
            }

            let valid = validate_name(orig_name).is_ok();
            let claimed = |name: &str| candidates.iter().any(|c| c.name == name);
            let (name, renamed) = if valid && !claimed(orig_name) {
                (orig_name.clone(), false)
            } else {
                // Slugified or clashing names never conflict — they take
                // the first free suffix against canonical and the plan.
                let base = if valid {
                    orig_name.clone()
                } else {
                    slugify(orig_name)
                };
                let mut name = base.clone();
                let mut i = 1;
                while claimed(&name) || (canonical.contains_key(&name) && name != *orig_name) {
                    i += 1;
                    name = format!("{base}-{i}");
                }
                (name, true)
            };

            candidates.push(ImportCandidate {
                name,
                server: server.clone(),
                origins: vec![(client_id.clone(), orig_name.clone())],
                renamed,
                notes,
            });
        }
    }

    let mut plan = ImportPlan::default();
    for candidate in candidates {
        match canonical.get(&candidate.name) {
            None => plan.new.push(candidate),
            Some(existing) if existing.transport == candidate.server.transport => {
                plan.already.push(candidate);
            }
            Some(_) => plan.conflicts.push(candidate),
        }
    }
    plan
}

/// Maps an arbitrary client name into `[a-z0-9-_]`: lowercases, replaces
/// invalid runs with a single `-`, collapses consecutive dashes.
#[must_use]
pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for c in name.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(c);
        } else {
            // Covers literal dashes too, collapsing runs of them.
            pending_dash = true;
        }
    }
    // `__` is reserved for the gateway's server__tool split, so a client
    // name carrying it must not slugify into a still-invalid name.
    while out.contains("__") {
        out = out.replace("__", "_");
    }
    if out.is_empty() {
        "imported-server".to_owned()
    } else {
        out
    }
}
