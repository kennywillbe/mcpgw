//! Pure finding-generation for `mcpgw doctor`. Everything environmental
//! (PATH lookups, filesystem, detection) is injected or done by the caller,
//! so these rules are unit-testable without a real machine state.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::Serialize;

use crate::auth::TokenState;
use crate::clients::{ClientKind, ClientRead};
use crate::config::{ClientScope, Server, Transport};
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
    /// A stable tag for the findings a `--json` consumer is expected to act
    /// on differently rather than print. Most findings have none: their
    /// message is the whole of what they are, and inventing a code per
    /// sentence would be an API surface nobody asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<&'static str>,
}

/// The code on the finding for a server that answered 401.
pub const NEEDS_OAUTH: &str = "needs_oauth";

/// The two words every report uses for a header set a command produces, so
/// `doctor` and the probe rows cannot describe the same thing differently.
pub const HEADERS_FROM_COMMAND: &str = "from command";

/// The finding for a server whose own OAuth the gateway cannot stand in for.
///
/// A warning rather than an error: nothing on this machine is misconfigured
/// and nothing is down — the server is working exactly as an OAuth-protected
/// server should. What is missing is a login, and it has to happen here,
/// because relaying the server's `WWW-Authenticate` to the client would have
/// the client send that server's token through the gateway.
///
/// `tokens` is what `<state>/auth/<name>.json` says, or `None` when there is
/// no such file. It only changes the middle clause — the diagnosis is the
/// same and so is the command — but the middle clause is the whole difference
/// between "you have never logged in here" and "you have, and it stopped
/// working", which are the two things a reader is trying to tell apart.
#[must_use]
pub fn needs_oauth(client: Option<&str>, name: &str, tokens: Option<TokenState>) -> Finding {
    let because = match tokens {
        None => "the gateway cannot complete a client-side login",
        // Past its expiry with nothing to renew it: the ordinary end of a
        // login that has been sitting for a while.
        Some(TokenState::Expired) => "the stored login expired",
        // Inside its lifetime, or renewable, and still refused — revoked at
        // the provider, or granted scopes the server has since stopped
        // accepting. Either way the fix is the same one.
        Some(TokenState::Valid | TokenState::Renewable) => "the stored login was refused",
    };
    Finding {
        client: client.map(str::to_owned),
        server: Some(name.to_owned()),
        severity: Severity::Warning,
        message: format!("{name} needs OAuth — {because}; run mcpgw auth login {name}"),
        code: Some(NEEDS_OAUTH),
    }
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
        code: None,
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
        Transport::Http {
            url,
            headers_command,
            ..
        } => {
            let mut findings = match url::Url::parse(url) {
                Err(err) => vec![finding(
                    Severity::Error,
                    format!("invalid url {url:?}: {err}"),
                )],
                Ok(parsed) if !matches!(parsed.scheme(), "http" | "https") => vec![finding(
                    Severity::Warning,
                    format!("unusual url scheme {:?}", parsed.scheme()),
                )],
                Ok(_) => Vec::new(),
            };
            // Resolved exactly like a stdio `command`, and with the same
            // lookup, because it is one: a program mcpgw spawns. The advice
            // is longer because this one is spawned by whatever is running
            // the gateway, and a service manager hands it a PATH that has
            // almost nothing on it.
            if let Some(program) = crate::headers::program(headers_command)
                && !command_exists(program)
            {
                findings.push(finding(
                    Severity::Error,
                    format!(
                        "headers {HEADERS_FROM_COMMAND} {} — {program:?} not found in PATH \
                         (an installed service runs with a PATH of its own, so give an \
                         absolute path)",
                        crate::headers::display(headers_command)
                    ),
                ));
            }
            findings
        }
    }
}

/// The findings for `[servers.NAME.tools]` entries that match none of
/// `tools` — the list the server answered `tools/list` with.
///
/// A warning, not an error: nothing is broken, and the gateway is doing
/// exactly what the file says. What it costs is silent, which is why it is
/// worth a line — an `allow` entry with a typo in it takes a tool away, and
/// the only symptom is a client that never sees it.
#[must_use]
pub fn unmatched_tool_rules(name: &str, server: &Server, tools: &[String]) -> Vec<Finding> {
    let Some(rules) = &server.tools else {
        return Vec::new();
    };
    rules
        .unmatched(tools)
        .into_iter()
        .map(|(list, rule)| Finding {
            client: None,
            server: Some(name.to_owned()),
            severity: Severity::Warning,
            message: format!(
                "[servers.{name}.tools] {list} entry {rule:?} matches no tool {name} offers"
            ),
            code: None,
        })
        .collect()
}

/// The one finding for a server whose tool definitions have moved since they
/// were pinned, or `None` for one whose have not.
///
/// A warning, not an error, and one line for the whole server rather than
/// one per tool: the gateway is still serving, the tools still work, and
/// there is a single decision to make about all of them — accept the new
/// definitions or go and look at the server. The message names the command
/// that makes it.
///
/// Tool names and lengths, never descriptions: the text is what a poisoned
/// tool carries, and `doctor` is read by people and pasted into issues.
#[must_use]
pub fn tool_drift(name: &str, events: &[crate::pins::DriftEvent]) -> Option<Finding> {
    if events.is_empty() {
        return None;
    }
    let moved: Vec<String> = events
        .iter()
        .map(crate::pins::DriftEvent::summary)
        .collect();
    Some(Finding {
        client: None,
        server: Some(name.to_owned()),
        severity: Severity::Warning,
        message: format!(
            "{name} changed its tool definitions since they were pinned: {} — \
             review them, then run mcpgw tools {name} pin to accept",
            moved.join(", ")
        ),
        code: Some(TOOL_DRIFT),
    })
}

/// The code on the finding for a server whose tool definitions drifted.
pub const TOOL_DRIFT: &str = "tool_drift";

/// Windsurf's own ceiling. It is not configurable there and not ours to
/// change; a client over it does not get a truncated list, it gets a broken
/// one, so `doctor` says so without being asked to.
pub const WINDSURF_TOOL_CAP: usize = 100;

/// What one client is actually offered, priced.
///
/// The whole point of the scoping milestone read back: tool definitions are
/// the largest fixed cost in an agent's context, and "70 tools, 49k tokens
/// before you type anything" is the number nobody could see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClientBudget {
    /// The client id, as `[clients.ID]` spells it.
    pub client: String,
    pub servers: usize,
    pub tools: usize,
    pub tokens: usize,
    /// Servers this client is offered whose tools nothing priced, so the
    /// total is a floor rather than an answer. Under `--probe` these are the
    /// servers that did not answer.
    pub unpriced: Vec<String>,
}

impl ClientBudget {
    /// The report's one line about this client, in the words the milestone
    /// was asked for: `cursor sees 23 tools across 4 servers (~18k tokens)`.
    #[must_use]
    pub fn line(&self) -> String {
        let servers = if self.servers == 1 {
            "server"
        } else {
            "servers"
        };
        let tools = if self.tools == 1 { "tool" } else { "tools" };
        let mut line = format!(
            "{} sees {} {tools} across {} {servers} (~{} tokens)",
            self.client,
            self.tools,
            self.servers,
            round_tokens(self.tokens),
        );
        if !self.unpriced.is_empty() {
            let _ = write!(
                line,
                " — at least: {} did not answer",
                self.unpriced.join(", ")
            );
        }
        line
    }
}

/// Tokens as a reader wants them: `18k` past a thousand, the number itself
/// below it. A budget printed to the digit would claim a precision the
/// estimate does not have.
fn round_tokens(tokens: usize) -> String {
    if tokens < 1000 {
        return tokens.to_string();
    }
    format!("{}k", (tokens + 500) / 1000)
}

/// What each client sees, given what the servers offer.
///
/// `listings` is server name → tool name → the tool's estimated token cost,
/// which is what a probe of that server produced. A server missing from it
/// is one nothing could price; it still counts as a server the client is
/// offered, and its name is reported so the total is not read as complete.
#[must_use]
pub fn client_budget(
    client: ClientKind,
    scope: Option<&ClientScope>,
    canonical: &BTreeMap<String, Server>,
    listings: &BTreeMap<String, BTreeMap<String, usize>>,
) -> ClientBudget {
    let mut budget = ClientBudget {
        client: client.id().to_owned(),
        servers: 0,
        tools: 0,
        tokens: 0,
        unpriced: Vec::new(),
    };
    for (name, server) in canonical {
        // A disabled server is not synced anywhere and costs nobody
        // anything, exactly as it does in `sync`.
        if !server.enabled || !scope.is_none_or(|scope| scope.has_server(name)) {
            continue;
        }
        budget.servers += 1;
        let Some(tools) = listings.get(name) else {
            budget.unpriced.push(name.clone());
            continue;
        };
        for (tool, tokens) in tools {
            // The same two tables the gateway applies, in the same order.
            if server.allows_tool(tool) && scope.is_none_or(|scope| scope.allows_tool(tool)) {
                budget.tools += 1;
                budget.tokens += tokens;
            }
        }
    }
    budget
}

/// The tool ceiling a client is judged against, and where it comes from.
///
/// An explicit `max_tools` wins: a user who wrote one has said what they
/// want to hear about, including on a client whose own limit is lower.
#[must_use]
pub fn tool_cap(client: ClientKind, scope: Option<&ClientScope>) -> Option<(usize, String)> {
    if let Some(max) = scope.and_then(|scope| scope.max_tools) {
        return Some((max, format!("[clients.{}] max_tools", client.id())));
    }
    (client == ClientKind::Windsurf).then(|| (WINDSURF_TOOL_CAP, "Windsurf's own limit".to_owned()))
}

/// The finding for a client offered more tools than it can hold.
///
/// A warning: nothing is misconfigured and the gateway is doing what the
/// file says. What it costs — a client that silently truncates its tool
/// list, or a context spent before the first prompt — is invisible from
/// inside the client, which is why it is worth a line.
#[must_use]
pub fn over_tool_cap(budget: &ClientBudget, cap: usize, source: &str) -> Option<Finding> {
    (budget.tools > cap).then(|| Finding {
        client: Some(budget.client.clone()),
        server: None,
        severity: Severity::Warning,
        message: format!(
            "{} tools is over {cap} ({source}) — narrow it with \
             `mcpgw clients {} servers ...` or a [clients.{}.tools] deny list",
            budget.tools, budget.client, budget.client,
        ),
        code: None,
    })
}

/// The finding for a `[clients.ID] servers` entry naming a server the
/// canonical config does not have.
///
/// Not a parse error, for the same reason an unmatched tool rule is not: the
/// state is ordinary between `mcpgw remove` and the next edit, and a config
/// that refused to load would take the gateway down over a stale name.
#[must_use]
pub fn unknown_scoped_servers(
    client: &str,
    scope: &ClientScope,
    canonical: &BTreeMap<String, Server>,
) -> Vec<Finding> {
    scope
        .servers
        .iter()
        .filter(|name| !canonical.contains_key(name.as_str()))
        .map(|name| Finding {
            client: Some(client.to_owned()),
            server: Some(name.clone()),
            severity: Severity::Warning,
            message: format!(
                "[clients.{client}] servers names {name:?}, which the canonical config does \
                 not have"
            ),
            code: None,
        })
        .collect()
}

/// One warning per key in `config.toml` this build does not recognize.
///
/// A warning, and never anything stronger, for the reasons
/// [`unknown_keys`](crate::config::unknown_keys) documents: the key may be a
/// typo that cost the user a restriction, or it may be a key a newer mcpgw
/// wrote. Both are worth naming; neither is worth refusing to run over.
#[must_use]
pub fn unknown_config_keys(keys: &[crate::config::UnknownKey]) -> Vec<Finding> {
    keys.iter()
        .map(|key| Finding {
            client: None,
            // The server the key sits under, when it sits under one, so a
            // `--json` consumer can group the warning with that server's
            // other findings rather than re-parse the path.
            server: key
                .path
                .strip_prefix("servers.")
                .and_then(|rest| rest.split('.').next())
                .map(str::to_owned),
            severity: Severity::Warning,
            message: key.message(),
            code: Some(UNKNOWN_CONFIG_KEY),
        })
        .collect()
}

/// The code on the finding for a config key this build does not know. Worth
/// one because the action differs by reader: a person fixes the typo, while
/// a tool that manages configs across mixed versions may well ignore it.
pub const UNKNOWN_CONFIG_KEY: &str = "unknown_config_key";

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
                code: None,
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
    /// gateway's own `/mcp`.
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
    let Some(name) = flag("--server") else {
        return Some(base);
    };
    // The client tag rides along: it is part of which endpoint this entry
    // dials, and probing the untagged one would prove a path the client
    // never takes.
    match flag("--client") {
        Some(client) => endpoints::per_client_url(&base, &name, &client).ok(),
        None => endpoints::per_server_url(&base, &name).ok(),
    }
}

/// One endpoint's identity: the socket it reaches, the path on it, and the
/// client it was tagged for.
fn target_key(url: &str) -> String {
    let Ok(parsed) = url::Url::parse(url) else {
        return url.to_owned();
    };
    format!(
        "{}://{}:{}{}{}",
        parsed.scheme(),
        host_key(&parsed).unwrap_or_default(),
        parsed.port_or_known_default().unwrap_or_default(),
        parsed.path(),
        // Part of the identity: two clients dialing one server's endpoint
        // under different `?client=` tags are asking it different questions
        // and get different answers.
        parsed.query().map(|q| format!("?{q}")).unwrap_or_default(),
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
/// path (`/mcp` included).
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
        code: None,
    }
}

/// The code on the finding for a managed entry with no gateway token in it.
pub const MISSING_TOKEN: &str = "missing_gateway_token";

/// The code on the finding for a gateway listening past loopback with nothing
/// to authenticate its clients.
pub const UNAUTHENTICATED_BIND: &str = "unauthenticated_bind";

/// The warning for a managed gateway entry that carries no install token.
///
/// A warning rather than an error because the gateway still answers it: this
/// release lets an unauthenticated loopback request through and logs once.
/// The next one will not, which is what makes saying it now worth a line.
///
/// [`None`] for a client whose entries cannot carry the token at all — Zed,
/// whose remote entry holds no headers, and Claude Desktop, whose entry is an
/// `mcpgw connect` bridge that reads the token off the disk. Neither is
/// missing anything, and a permanent warning naming a fix that does not exist
/// is how a report stops being read.
#[must_use]
pub fn missing_gateway_token(
    client: &str,
    entry: &str,
    server: &Server,
    carries_token: bool,
) -> Option<Finding> {
    if !carries_token {
        return None;
    }
    let Transport::Http { headers, .. } = &server.transport else {
        return None;
    };
    if headers
        .keys()
        .any(|name| name.eq_ignore_ascii_case("authorization"))
    {
        return None;
    }
    Some(Finding {
        client: Some(client.to_owned()),
        server: Some(entry.to_owned()),
        severity: Severity::Warning,
        message: "reaches the gateway without its install token — run `mcpgw sync` \
                  (the next release stops answering entries without one)"
            .to_owned(),
        code: Some(MISSING_TOKEN),
    })
}

/// The error for a gateway bound past loopback that requires no token.
///
/// The one finding in this file about the *gateway's* configuration rather
/// than a client's, and an error rather than a warning: an address other
/// machines can reach, answering without a credential, hands every server —
/// and the OAuth logins behind them — to anything that can route to it.
///
/// `require_token` is `[gateway] require_token`. A token that exists but is
/// not yet required is not a boundary: the grace period still admits an
/// unauthenticated loopback request, and a remote one is refused only because
/// it is remote, which is a rule this bind has just removed the point of.
#[must_use]
pub fn unauthenticated_bind(bind: &str, require_token: bool) -> Option<Finding> {
    if crate::daemon::is_loopback(bind) || require_token {
        return None;
    }
    Some(Finding {
        client: None,
        server: None,
        severity: Severity::Error,
        message: format!(
            "the gateway is bound to {bind}, which is not loopback, and does not require its \
             install token — set `[gateway] require_token = true` and `mcpgw sync`, or bind \
             127.0.0.1"
        ),
        code: Some(UNAUTHENTICATED_BIND),
    })
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
            code: None,
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
    /// The gateway answered and said the server behind that endpoint is
    /// waiting on a login. Nothing here is broken.
    NeedsOAuth,
}

/// The phrase the gateway's own error carries for an upstream behind OAuth.
///
/// Matched as text for the same reason the 404 below is: a JSON-RPC error
/// reaches a probe as the transport's message and nothing else. Unlike the
/// 404 this is mcpgw's own sentence on both ends, pinned by the tests that
/// assert it.
const NEEDS_OAUTH_PHRASE: &str = "needs OAuth; run mcpgw auth login";

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
    if message.contains(NEEDS_OAUTH_PHRASE) {
        return GatewayFault::NeedsOAuth;
    }
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

/// The one finding for a repo-local config holding entries `sync` will not
/// touch.
///
/// A warning, not an error: the entries work, and two live paths to the same
/// server is a thing a team may well have chosen. What it is not is
/// invisible.
///
/// The message names the two commands that end the state it reports, in the
/// order they have to be run: the entries have to be in the canonical config
/// before anything can point at them through the gateway.
#[must_use]
pub fn project_unmanaged(client: &str, path: &std::path::Path, count: usize) -> Finding {
    let entries = if count == 1 { "entry" } else { "entries" };
    Finding {
        client: Some(client.to_owned()),
        server: None,
        severity: Severity::Warning,
        message: format!(
            "{} holds {count} direct MCP {entries} mcpgw does not manage — \
             they stay live alongside the gateway until `mcpgw import --project` \
             adopts them and `mcpgw sync --project` points them at it",
            path.display()
        ),
        code: None,
    }
}
