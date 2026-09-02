//! Pure finding-generation for `mcpgw doctor`. Everything environmental
//! (PATH lookups, filesystem, detection) is injected or done by the caller,
//! so these rules are unit-testable without a real machine state.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::clients::ClientRead;
use crate::config::{Server, Transport};
use crate::endpoints;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    /// `None` means the finding is about the canonical config.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    pub severity: Severity,
    pub message: String,
}

/// Static health checks for one server entry: command resolution for stdio,
/// URL syntax for http. `command_exists` abstracts the PATH lookup.
///
/// Disabled servers are skipped entirely — they cannot break anything while
/// off, and a red doctor over an intentionally parked entry helps no one.
#[must_use]
pub fn check_server(
    client: Option<&str>,
    name: &str,
    server: &Server,
    command_exists: &dyn Fn(&str) -> bool,
) -> Vec<Finding> {
    if !server.enabled {
        return Vec::new();
    }
    let finding = |severity, message| Finding {
        client: client.map(str::to_owned),
        server: Some(name.to_owned()),
        severity,
        message,
    };
    match &server.transport {
        Transport::Stdio { command, .. } => {
            if command_exists(command) {
                Vec::new()
            } else {
                vec![finding(
                    Severity::Error,
                    format!("command {command:?} not found in PATH"),
                )]
            }
        }
        Transport::Http { url, .. } => match url::Url::parse(url) {
            Err(err) => vec![finding(
                Severity::Error,
                format!("invalid url {url:?}: {err}"),
            )],
            Ok(parsed) if !matches!(parsed.scheme(), "http" | "https") => vec![finding(
                Severity::Warning,
                format!("unusual url scheme {:?}", parsed.scheme()),
            )],
            Ok(_) => Vec::new(),
        },
    }
}

/// Turns a lenient client read's problems into findings.
///
/// Severity rule: if the named server still exists in the parsed map, the
/// problem was a lossy-but-successful note (warning); if the entry was
/// dropped, something is actually broken (error). File-level problems
/// (no server name) are always errors.
#[must_use]
pub fn classify_problems(client: &str, read: &ClientRead) -> Vec<Finding> {
    read.problems
        .iter()
        .map(|problem| {
            let survived = problem
                .server
                .as_ref()
                .is_some_and(|name| read.servers.contains_key(name));
            Finding {
                client: Some(client.to_owned()),
                server: problem.server.clone(),
                severity: if survived {
                    Severity::Warning
                } else {
                    Severity::Error
                },
                message: problem.message.clone(),
            }
        })
        .collect()
}

/// One managed client entry that dials the gateway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GatewayEntry {
    /// Display name of the client the entry lives in.
    pub client: String,
    /// The entry's name inside that client's config.
    pub entry: String,
}

/// One endpoint on the gateway, plus every managed entry that dials it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GatewayTarget {
    pub url: String,
    /// The server whose own endpoint this is (`/s/<name>`), or `None` for the
    /// aggregate face.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    pub entries: Vec<GatewayEntry>,
}

impl GatewayTarget {
    /// The entries dialing this endpoint, as one `Client "entry"` phrase per
    /// entry — the report has to name the files somebody would go and edit.
    #[must_use]
    pub fn label(&self) -> String {
        self.entries
            .iter()
            .map(|e| format!("{} {:?}", e.client, e.entry))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The set of gateway endpoints a machine's managed client entries point at.
///
/// Built from the client files rather than from the canonical config on
/// purpose: what `doctor --probe` proves by dialing a server directly is that
/// the server works, not that the path a client actually takes does. Only the
/// entries clients read can say what that path is.
#[derive(Debug, Clone)]
pub struct GatewayPlan {
    base: String,
    targets: BTreeMap<String, GatewayTarget>,
}

impl GatewayPlan {
    #[must_use]
    pub fn new(base: &str) -> Self {
        Self {
            base: base.to_owned(),
            targets: BTreeMap::new(),
        }
    }

    /// Records `entry` if it dials the gateway this plan is about, and says
    /// whether it did. Entries pointing somewhere else — a direct stdio
    /// server, a hosted remote, a gateway on another host — are not this
    /// check's business.
    ///
    /// Disabled entries are skipped for the same reason [`check_server`]
    /// skips them: a parked entry cannot break anything.
    pub fn collect(&mut self, client: &str, entry: &str, server: &Server) -> bool {
        if !server.enabled {
            return false;
        }
        let Some(url) = gateway_entry_url(server) else {
            return false;
        };
        if !same_origin(&url, &self.base) {
            return false;
        }
        // Keyed on the socket-and-path the URL resolves to, not on its text:
        // two clients spelling the same loopback endpoint differently are
        // pointing at one thing, and probing it twice would report one
        // problem twice.
        self.targets
            .entry(target_key(&url))
            .or_insert_with(|| GatewayTarget {
                server: endpoint_server(&url),
                url,
                entries: Vec::new(),
            })
            .entries
            .push(GatewayEntry {
                client: client.to_owned(),
                entry: entry.to_owned(),
            });
        true
    }

    #[must_use]
    pub fn base(&self) -> &str {
        &self.base
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.targets.len()
    }

    #[must_use]
    pub fn into_targets(self) -> Vec<GatewayTarget> {
        self.targets.into_values().collect()
    }
}

/// The URL a client entry dials, when the entry is one that could be aimed at
/// a gateway at all: an http entry dials its own URL, an `mcpgw connect`
/// bridge dials whatever its flags resolve to. `None` for a plain stdio
/// server — a spawned command is nobody's gateway.
///
/// The bridge half must keep agreeing with how `mcpgw connect` resolves the
/// same flags; the entries it reads are the ones `sync` writes for clients
/// that cannot hold an http entry, and those are the majority.
#[must_use]
pub fn gateway_entry_url(server: &Server) -> Option<String> {
    match &server.transport {
        Transport::Http { url, .. } => Some(url.clone()),
        Transport::Stdio { command, args, .. } => bridge_url(command, args),
    }
}

/// Whether `server` already reaches a gateway listening at `base_url` — same
/// scheme, host and port, whatever endpoint path it asks for.
///
/// The question `sync` asks of an entry it is about to replace: one that does
/// not aim there dials a server directly, and replacing it is the migration
/// the user should hear about once.
#[must_use]
pub fn aims_at_gateway(server: &Server, base_url: &str) -> bool {
    gateway_entry_url(server).is_some_and(|url| same_origin(&url, base_url))
}

fn bridge_url(command: &str, args: &[String]) -> Option<String> {
    let binary = std::path::Path::new(command).file_stem()?.to_str()?;
    if binary != "mcpgw" || args.first().map(String::as_str) != Some("connect") {
        return None;
    }
    let flag = |name: &str| {
        args.iter()
            .position(|arg| arg == name)
            .and_then(|at| args.get(at + 1))
            .cloned()
    };
    let base = flag("--url").unwrap_or_else(|| endpoints::DEFAULT_URL.to_owned());
    match flag("--server") {
        Some(name) => endpoints::per_server_url(&base, &name).ok(),
        None => Some(base),
    }
}

/// One endpoint's identity: the socket it reaches plus the path on it.
fn target_key(url: &str) -> String {
    let Ok(parsed) = url::Url::parse(url) else {
        return url.to_owned();
    };
    format!(
        "{}://{}:{}{}",
        parsed.scheme(),
        host_key(&parsed).unwrap_or_default(),
        parsed.port_or_known_default().unwrap_or_default(),
        parsed.path()
    )
}

/// Whether two URLs reach the same listening socket.
fn same_origin(a: &str, b: &str) -> bool {
    let (Ok(a), Ok(b)) = (url::Url::parse(a), url::Url::parse(b)) else {
        return false;
    };
    a.scheme() == b.scheme()
        && a.port_or_known_default() == b.port_or_known_default()
        && host_key(&a) == host_key(&b)
}

/// A host as identity rather than as text: loopback is spelled `localhost`,
/// `127.0.0.1` and `::1` interchangeably across client config files, and an
/// entry written one way still dials the gateway a base wrote the other.
fn host_key(url: &url::Url) -> Option<String> {
    let host = url.host_str()?;
    let bare = host.trim_matches(['[', ']']);
    if host == "localhost"
        || bare
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    {
        return Some("loopback".to_owned());
    }
    Some(host.to_ascii_lowercase())
}

/// The server name in a `/s/<name>` endpoint URL, or `None` for any other
/// path (the aggregate `/mcp` included).
#[must_use]
pub fn endpoint_server(url: &str) -> Option<String> {
    let path = url::Url::parse(url).ok()?.path().to_owned();
    let name = path.strip_prefix(endpoints::PREFIX)?.strip_prefix('/')?;
    let name = name.split('/').next()?;
    (!name.is_empty()).then(|| name.to_owned())
}

/// The one finding for a gateway nothing answers on.
///
/// Deliberately singular. Every managed entry on the machine points at the
/// same socket, so a per-entry finding would repeat one sentence a dozen
/// times and bury every other problem in the report — and they all have the
/// same fix anyway.
#[must_use]
pub fn gateway_unreachable(base: &str) -> Finding {
    Finding {
        client: None,
        server: None,
        severity: Severity::Error,
        message: format!("gateway not reachable at {base} — start it with `mcpgw serve`"),
    }
}

/// Findings for an endpoint the running gateway does not serve: one per
/// entry, because each one is a separate client file somebody has to fix.
#[must_use]
pub fn unserved_endpoint(target: &GatewayTarget, detail: &str) -> Vec<Finding> {
    target
        .entries
        .iter()
        .map(|entry| Finding {
            client: Some(entry.client.clone()),
            server: Some(entry.entry.clone()),
            severity: Severity::Error,
            message: format!(
                "points at {}, which the running gateway does not serve — {detail}",
                target.url
            ),
        })
        .collect()
}

/// The fallback when the 404 carried no body of ours. `mcpgw serve` always
/// answers under `/s`, so a bare 404 there means the port belongs to
/// something that is not this gateway.
const NO_ENDPOINTS: &str = "nothing on that port answered as an mcpgw \
     endpoint — check what is listening there";

/// Why a probe through the gateway failed, as far as the report cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayFault {
    /// The gateway answered and said it serves nothing at that path. The
    /// detail is its own 404 body, which already names what it does serve.
    Unserved(String),
    /// The address was right and the session still failed — a dead upstream,
    /// a timeout, a protocol error. A different problem with a different fix.
    Failed,
}

/// Sorts a failed gateway probe into "wrong address" and "right address, bad
/// session".
///
/// It reads the transport's error text because that is the only place the
/// status code survives: the MCP client collapses every HTTP failure into one
/// opaque variant. The gateway's own 404 body rides along inside that text
/// and already names the endpoints it does serve, which is why the actionable
/// half of the message needs no second request.
#[must_use]
pub fn classify_gateway_failure(message: &str) -> GatewayFault {
    let Some((_, rest)) = message.split_once("HTTP 404") else {
        return GatewayFault::Failed;
    };
    let line = rest.lines().next().unwrap_or_default().trim_start();
    let detail = line
        .strip_prefix("Not Found")
        .unwrap_or(line)
        .trim_start_matches(':')
        .trim();
    GatewayFault::Unserved(if detail.is_empty() {
        NO_ENDPOINTS.to_owned()
    } else {
        detail.to_owned()
    })
}
