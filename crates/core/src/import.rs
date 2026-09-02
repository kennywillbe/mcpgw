//! Planning for `mcpgw import`: turn lenient client reads into canonical
//! candidates — slugified names, cross-client dedup, canonical-collision
//! classification. Pure logic; the CLI owns prompting and writing.

use std::collections::BTreeMap;

use crate::clients::ClientRead;
use crate::config::{Server, Transport, validate_name};

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
    /// The stdio command resolved neither as an absolute path nor on PATH,
    /// so `server.enabled` was forced off. Importing such an entry enabled
    /// publishes an endpoint that can never answer, and the next sync spreads
    /// it to every client — one broken entry becomes one failure per client.
    pub command_missing: bool,
    /// An http candidate that addresses something already known under a
    /// different definition. `None` for everything else.
    pub same_address: Option<SameAddress>,
    /// For a conflict, the free name this entry can be adopted under while
    /// the canonical entry keeps its own — the "keep both" outcome. `None`
    /// for anything that is not a conflict, because nothing else needs a
    /// second name.
    ///
    /// Chosen here rather than by the caller so that two conflicts in one run
    /// cannot be offered the same name: only the planner sees every candidate
    /// and every canonical entry at once.
    pub adopt_as: Option<String>,
}

/// The entry an http candidate shares an address with, when the only thing
/// separating them is the value of a header they both carry.
///
/// Names the counterpart and nothing else: the values that differ are
/// credentials, and the whole point of the flag is that the caller can talk
/// about the difference without printing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SameAddress {
    /// The entry the candidate matched.
    pub name: String,
    /// True when that entry is already in the canonical config, false when
    /// it is another candidate in this same plan.
    pub canonical: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportPlan {
    /// Not present in the canonical config: import.
    pub new: Vec<ImportCandidate>,
    /// Identical entry already canonical: nothing to write, but sources
    /// are still adopted to avoid the perpetual sync conflict.
    pub already: Vec<ImportCandidate>,
    /// Same canonical name, different definition: needs a user decision.
    /// Each carries an [`ImportCandidate::adopt_as`] name, so the decision is
    /// three-way — keep canonical, overwrite it, or keep both.
    pub conflicts: Vec<ImportCandidate>,
}

/// Builds an import plan. `sources` order is significant: on cross-client
/// name clashes with different definitions, the first client keeps the
/// name and later ones are suffixed.
///
/// `command_exists` is the same PATH lookup `doctor` is given, and it is a
/// parameter for the same reason: planning stays testable without a real
/// machine underneath it. Passing it here rather than checking afterwards in
/// each caller is deliberate — the wizard's import step and `mcpgw import`
/// both plan through this function, so a check that lives here cannot be the
/// one a caller forgot.
#[must_use]
pub fn plan_import(
    sources: &[(String, ClientRead)],
    canonical: &BTreeMap<String, Server>,
    command_exists: &dyn Fn(&str) -> bool,
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
                .find(|c| same_transport(&c.server.transport, &server.transport))
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
                let name = next_free(&base, |name| {
                    claimed(name) || (canonical.contains_key(name) && name != orig_name)
                });
                (name, true)
            };

            let same_address = same_address(server, canonical, &candidates);
            let command_missing = match &server.transport {
                Transport::Stdio { command, .. } => !command_exists(command),
                Transport::Http { .. } => false,
            };
            let mut server = server.clone();
            // The entry is kept — it is the user's data and the command may
            // come back — but off, where it reaches neither the gateway nor
            // any client.
            server.enabled &= !command_missing;

            candidates.push(ImportCandidate {
                name,
                server,
                origins: vec![(client_id.clone(), orig_name.clone())],
                renamed,
                notes,
                command_missing,
                same_address,
                adopt_as: None,
            });
        }
    }

    // Every name the run must not hand out twice: what the canonical config
    // already holds, what this plan is about to write, and — added as they
    // are chosen — the second names offered to conflicts.
    let mut taken: std::collections::BTreeSet<String> = canonical.keys().cloned().collect();
    taken.extend(candidates.iter().map(|c| c.name.clone()));

    let mut plan = ImportPlan::default();
    for candidate in candidates {
        match canonical.get(&candidate.name) {
            None => plan.new.push(candidate),
            Some(existing) if same_transport(&existing.transport, &candidate.server.transport) => {
                plan.already.push(candidate);
            }
            Some(_) => {
                let adopt_as = next_free(&candidate.name, |name| taken.contains(name));
                taken.insert(adopt_as.clone());
                plan.conflicts.push(ImportCandidate {
                    adopt_as: Some(adopt_as),
                    ..candidate
                });
            }
        }
    }
    plan
}

/// `base`, or the first of `base-2`, `base-3` … that `taken` says is free.
///
/// The one place a suffix is invented, so `context7-2` means the same thing
/// wherever it turns up: a name clash between two clients, and a conflict
/// kept alongside the canonical entry it disagrees with.
fn next_free(base: &str, taken: impl Fn(&str) -> bool) -> String {
    let mut name = base.to_owned();
    let mut i = 1;
    while taken(&name) {
        i += 1;
        name = format!("{base}-{i}");
    }
    name
}

/// Whether this candidate is one already-known server wearing a second set of
/// credentials: same URL, the same header *keys*, different values.
///
/// That shape is what a user gets by pasting a remote server into two clients
/// with two tokens, and treating it as two servers — which is what transport
/// equality has to do — imports the same thing twice under invented names.
/// It cannot be merged automatically either: the two definitions may be two
/// accounts, and picking one silently would change what runs. So it is only
/// detected, and the caller asks.
///
/// A *different* set of header keys is left alone on purpose: one side
/// sending an extra header is a genuinely different definition, not the same
/// definition with a different secret in it.
fn same_address(
    server: &Server,
    canonical: &BTreeMap<String, Server>,
    candidates: &[ImportCandidate],
) -> Option<SameAddress> {
    let Transport::Http { url, headers } = &server.transport else {
        return None;
    };
    if headers.is_empty() {
        return None;
    }
    let matches = |other: &Server| match &other.transport {
        Transport::Http {
            url: other_url,
            headers: other_headers,
        } => {
            canonical_url(url) == canonical_url(other_url)
                && other_headers.keys().eq(headers.keys())
                && other_headers != headers
        }
        Transport::Stdio { .. } => false,
    };
    // The canonical config first: "the one you already have" is a more useful
    // thing to be told about than another entry arriving in the same run.
    canonical
        .iter()
        .find(|(_, existing)| matches(existing))
        .map(|(name, _)| SameAddress {
            name: name.clone(),
            canonical: true,
        })
        .or_else(|| {
            candidates
                .iter()
                .find(|c| matches(&c.server))
                .map(|c| SameAddress {
                    name: c.name.clone(),
                    canonical: false,
                })
        })
}

/// Whether two transports address the same server.
///
/// Byte equality everywhere except an HTTP URL, which is compared as a URL:
/// scheme and host are case-insensitive per RFC 3986, and one trailing slash
/// on the path names the same endpoint. That much is safe to merge, and not
/// merging it means the same remote server imported twice under two names.
///
/// Nothing beyond that is normalized, deliberately. Argument order, path
/// case, query order and a default port spelled out are all things two
/// clients can differ on *and mean differently*, and a dedupe is a silent
/// behaviour change: the surviving copy's definition is the one that runs.
fn same_transport(a: &Transport, b: &Transport) -> bool {
    match (a, b) {
        (
            Transport::Http {
                url: a_url,
                headers: a_headers,
            },
            Transport::Http {
                url: b_url,
                headers: b_headers,
            },
        ) => a_headers == b_headers && canonical_url(a_url) == canonical_url(b_url),
        _ => a == b,
    }
}

/// Lowercases scheme and host and drops a single trailing slash from the
/// path. Anything that is not `scheme://…` is left exactly as it came in —
/// this is a comparison key, not a validator, so an unparseable URL should
/// only ever equal itself.
fn canonical_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let (authority, tail) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
    // Userinfo keeps its case: a password is not a hostname.
    let authority = match authority.split_once('@') {
        Some((userinfo, host)) => format!("{userinfo}@{}", host.to_ascii_lowercase()),
        None => authority.to_ascii_lowercase(),
    };
    let (path, query) = tail.split_at(tail.find(['?', '#']).unwrap_or(tail.len()));
    let path = path.strip_suffix('/').unwrap_or(path);
    format!("{}://{authority}{path}{query}", scheme.to_ascii_lowercase())
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
