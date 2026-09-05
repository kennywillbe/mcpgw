use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::Error;

pub const SUPPORTED_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub version: u32,
    // Ahead of `servers` because TOML wants every table of one section
    // written before the next section starts, and both of these are tables.
    #[serde(default, skip_serializing_if = "Capture::is_default")]
    pub capture: Capture,
    #[serde(default, skip_serializing_if = "GatewaySettings::is_default")]
    pub gateway: GatewaySettings,
    /// The `[clients.KIND]` tables: which servers and tools each client is
    /// given. Ahead of `servers` for the same reason `capture` is — one
    /// section's tables have to be written before the next section starts.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub clients: BTreeMap<String, ClientScope>,
    #[serde(default)]
    pub servers: BTreeMap<String, Server>,
}

/// The `[gateway]` table: how the gateway treats the clients that dial it.
///
/// Absent from a config that never mentions it, and skipped on the way out,
/// for the same reason `[capture]` is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewaySettings {
    /// Ends the one-release grace period early.
    ///
    /// The gateway ships holding the install token against every request but
    /// letting a *loopback* request without one through, so an install whose
    /// clients have not been re-synced yet keeps working and says so in the
    /// log. Turning this on makes the token mandatory everywhere, which is
    /// also what allows a supervised gateway to bind past loopback — see
    /// [`crate::gateway_token::BindPolicy`].
    #[serde(default)]
    pub require_token: bool,
}

impl GatewaySettings {
    #[must_use]
    pub fn is_default(&self) -> bool {
        !self.require_token
    }
}

/// What one client is given, from `[clients.KIND]`.
///
/// ```toml
/// [clients.cursor]
/// servers = ["github", "linear"]
/// max_tools = 60
///
/// [clients.cursor.tools]
/// deny = ["delete_*"]
/// ```
///
/// Every key is opt-in and a client with no table of its own is given
/// everything, which is what every client had before this existed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientScope {
    /// The servers this client is offered at all. Empty means every one:
    /// an absent list is "no opinion", not "nothing".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub servers: Vec<String>,
    /// What `doctor` warns above, in tools. `None` leaves the client with
    /// whatever ceiling its own software has, which for most of them is
    /// none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tools: Option<usize>,
    /// Tool rules applied on top of each server's own, in the same
    /// glob-lite language. Written last: it is a sub-table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolRules>,
}

impl ClientScope {
    /// Whether this client is offered `server` at all.
    #[must_use]
    pub fn has_server(&self, server: &str) -> bool {
        self.servers.is_empty() || self.servers.iter().any(|name| name == server)
    }

    /// Whether `tool` survives this client's own rules. The server's rules
    /// are asked separately, and first — see [`Server::allows_tool`].
    #[must_use]
    pub fn allows_tool(&self, tool: &str) -> bool {
        self.tools.as_ref().is_none_or(|rules| rules.allows(tool))
    }

    /// Whether this scope narrows anything a client can reach.
    ///
    /// What decides whether `sync` writes this client a tagged endpoint: a
    /// table holding only `max_tools` is a reporting threshold and nothing
    /// else, and tagging for it would rewrite a client file to say something
    /// the gateway would not act on.
    #[must_use]
    pub fn restricts(&self) -> bool {
        !self.servers.is_empty() || self.tools.as_ref().is_some_and(|rules| !rules.is_empty())
    }

    /// Whether the table says nothing at all, and so may as well not be
    /// there — what an edit that empties it reduces to.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.restricts() && self.max_tools.is_none()
    }
}

/// The live `[clients]` table a running gateway reads on every request.
///
/// Behind an [`ArcSwap`](arc_swap::ArcSwap) for the same reason the endpoint
/// table is: a reload publishes a whole new set at once, and a request that
/// is already running keeps the one it read. Cloning shares the cell, so
/// every pipe built from it sees a swap immediately.
#[derive(Debug, Clone, Default)]
pub struct ClientScopes(Arc<arc_swap::ArcSwap<BTreeMap<String, ClientScope>>>);

impl ClientScopes {
    #[must_use]
    pub fn new(scopes: BTreeMap<String, ClientScope>) -> Self {
        Self(Arc::new(arc_swap::ArcSwap::from_pointee(scopes)))
    }

    /// Publishes `scopes` in place of the current set.
    pub fn store(&self, scopes: BTreeMap<String, ClientScope>) {
        self.0.store(Arc::new(scopes));
    }

    /// The scope for a client id, as of right now.
    #[must_use]
    pub fn get(&self, client: &str) -> Option<ClientScope> {
        self.0.load().get(client).cloned()
    }
}

/// The `[capture]` table: what the gateway's traffic log is allowed to keep.
///
/// Absent from a config that never mentions it, and skipped on the way out
/// again, so adding the table here does not rewrite everybody's file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capture {
    /// Extra regexes whose matches are replaced in captured bodies, on top of
    /// the built-in credential rules — the site-specific shapes only the
    /// person running the gateway knows about (an internal ticket id, a
    /// customer number).
    ///
    /// Validated at parse time: an unusable pattern is a config error, not a
    /// rule that quietly matches nothing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redact: Vec<String>,
    /// How many days of the daily traffic log to keep, counting today.
    ///
    /// Finite by default ([`crate::capture::DEFAULT_RETAIN_DAYS`]): the log
    /// deliberately survives `daemon uninstall` and `eject`, so a gateway
    /// that never pruned would be the only thing on the machine that grows
    /// forever. `0` opts out and keeps every day.
    #[serde(
        default = "default_retain_days",
        skip_serializing_if = "is_default_retain_days"
    )]
    pub retain_days: u32,
}

impl Default for Capture {
    fn default() -> Self {
        Self {
            redact: Vec::new(),
            retain_days: default_retain_days(),
        }
    }
}

impl Capture {
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.redact.is_empty() && is_default_retain_days(&self.retain_days)
    }
}

fn default_retain_days() -> u32 {
    crate::capture::DEFAULT_RETAIN_DAYS
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "the reference is serde's skip_serializing_if signature, not a choice"
)]
fn is_default_retain_days(days: &u32) -> bool {
    *days == crate::capture::DEFAULT_RETAIN_DAYS
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Server {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// The `calls_per_minute` ceiling this server's `tools/call` traffic is
    /// metered against, or 0 for an entry that has none — which is every
    /// entry until someone opts in, and the reason an upgrade changes
    /// nothing about how fast a client may call.
    #[serde(
        default,
        skip_serializing_if = "unlimited",
        deserialize_with = "calls_per_minute"
    )]
    pub calls_per_minute: u32,
    // Flattened before `tools` so plain values serialize before any table;
    // TOML requires values ahead of tables within one section, and both the
    // env/headers tables and `[tools]` are tables.
    #[serde(flatten)]
    pub transport: Transport,
    /// The `[servers.NAME.tools]` table, or `None` for an entry that has
    /// none — which is every entry until someone opts in, and the reason an
    /// upgrade changes nothing about what a client sees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolRules>,
}

/// Which of a server's tools the gateway lets through, from
/// `[servers.NAME.tools]`.
///
/// ```toml
/// [servers.github.tools]
/// allow = ["search_repositories", "get_file_contents"]
/// deny  = ["delete_*"]
/// ```
///
/// Deny-by-default starts the moment `allow` has an entry, and not before:
/// the table is opt-in, so a config that never mentions it keeps every tool
/// visible and callable exactly as it was.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRules {
    /// The only tools that survive, once it is non-empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    /// Tools removed from whatever `allow` left, applied second so a broad
    /// `allow` can be trimmed without listing every name that should stay.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
    /// What the gateway does when this server's tool definitions stop
    /// matching the ones it pinned.
    #[serde(default, skip_serializing_if = "Drift::is_default")]
    pub drift: Drift,
}

/// What a server's drifted tool definitions cost it.
///
/// There is no `"deny"`: a gateway that refuses calls on a description
/// change is a gateway people turn off, and servers do legitimately version
/// their tools. See [`crate::pins`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Drift {
    /// Pin on first sight, report every later disagreement, keep serving.
    #[default]
    Warn,
    /// Do not pin and do not compare. Nothing is written and nothing is
    /// reported for this server.
    Off,
}

impl Drift {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Drift::Warn => "warn",
            Drift::Off => "off",
        }
    }

    #[must_use]
    pub fn is_watched(self) -> bool {
        matches!(self, Drift::Warn)
    }

    // By reference because that is the shape `skip_serializing_if` calls
    // with; `as_str`/`is_watched` are the by-value ones callers reach for.
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Drift::default()
    }
}

impl std::fmt::Display for Drift {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ToolRules {
    /// Whether the table says nothing at all — no table, and a table whose
    /// lists are empty and whose `drift` is the default, mean the same thing:
    /// "unchanged".
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.allow.is_empty() && self.deny.is_empty() && self.drift.is_default()
    }

    /// Whether `tool` survives: `allow` first (deny-by-default once it has
    /// an entry), then `deny` over what is left.
    #[must_use]
    pub fn allows(&self, tool: &str) -> bool {
        if !self.allow.is_empty() && !self.allow.iter().any(|rule| matches(rule, tool)) {
            return false;
        }
        !self.deny.iter().any(|rule| matches(rule, tool))
    }

    /// Every entry that matches none of `tools`, as `(list, entry)` pairs.
    ///
    /// What `doctor --probe` reports: an entry matching nothing is either a
    /// typo or a tool the server has since renamed, and in the `allow` case
    /// it silently costs the user a tool they meant to keep.
    #[must_use]
    pub fn unmatched<'r>(&'r self, tools: &[String]) -> Vec<(&'static str, &'r str)> {
        let mut dead = Vec::new();
        for (list, rules) in [("allow", &self.allow), ("deny", &self.deny)] {
            for rule in rules {
                if !tools.iter().any(|tool| matches(rule, tool)) {
                    dead.push((list, rule.as_str()));
                }
            }
        }
        dead
    }
}

/// Glob-lite: a literal tool name, or a prefix with a trailing `*`.
///
/// Deliberately not a glob crate and not a regex. The names on both sides of
/// this are MCP tool names, where the only shape anyone writes is a family
/// prefix (`delete_*`), and a rule language that can express more than the
/// user can predict is the wrong thing to put in front of "which tools can
/// this agent call".
fn matches(rule: &str, tool: &str) -> bool {
    match rule.strip_suffix('*') {
        Some(prefix) => tool.starts_with(prefix),
        None => rule == tool,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Transport {
    Stdio {
        command: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        /// A command whose stdout is a JSON object of header names and
        /// values, merged over [`headers`](Self::Http::headers) every time
        /// the upstream connects. The answer to a credential that expires:
        /// an SSO or STS token belongs in a command, not in a literal string
        /// that stops working an hour after it was pasted.
        ///
        /// Stored as argv rather than as the single string Claude Code and
        /// Codex spell theirs with, and run with no shell, for the same
        /// reason `command`/`args` are: a string has to be split by
        /// somebody, and every splitter is either a shell — which turns a
        /// path with a space, a `$` or a `;` into something else entirely —
        /// or a whitespace split that quietly disagrees with one. A config
        /// copied from either client still parses: a bare string is read as
        /// whitespace-separated argv, which is what those two do for
        /// everything that is not already quoted.
        ///
        /// Written ahead of `headers` because TOML wants an array before a
        /// table within one section.
        #[serde(
            default,
            skip_serializing_if = "Vec::is_empty",
            deserialize_with = "argv"
        )]
        headers_command: Vec<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
        /// The `[servers.<name>.auth]` table: what identity `mcpgw auth
        /// login` presents to this server's authorization server.
        ///
        /// Absent from every entry that never needed one, which is the
        /// common case — with no table at all the broker picks its own
        /// identity (a Client ID Metadata Document, or Dynamic Client
        /// Registration where the server still offers it). The table exists
        /// for the hosts that accept neither and issue client ids by hand.
        ///
        /// Last of the http fields because it is a table and TOML wants
        /// every value of a section written before its tables.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auth: Option<ServerAuth>,
    },
}

impl Server {
    /// Whether `tool` is visible and callable through this server's
    /// endpoint. Everything is, while there is no `[tools]` table.
    #[must_use]
    pub fn allows_tool(&self, tool: &str) -> bool {
        self.tools.as_ref().is_none_or(|rules| rules.allows(tool))
    }

    /// What this server's tool definitions are watched for. Warn, until an
    /// entry says otherwise.
    #[must_use]
    pub fn drift(&self) -> Drift {
        self.tools
            .as_ref()
            .map_or(Drift::default(), |rules| rules.drift)
    }
}

/// The identity half of one server's OAuth, as the config spells it.
///
/// Only the parts a user has to *choose* live here. Everything the flow
/// discovers — the authorization server, its endpoints, the scopes it
/// grants — is read off the server at login time and kept with the tokens,
/// not written back into the config: a config that pinned a discovered
/// endpoint would go stale the first time the provider moved one.
///
/// No secret is stored here either. `client_secret_env` names an
/// environment variable; the secret itself never reaches the file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerAuth {
    /// A client id issued out of band, for the providers that register
    /// clients by hand (Atlassian, GitHub) and accept nothing else.
    /// Persisted by `mcpgw auth login --client-id`, so the next login and
    /// every refresh present the same identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// The environment variable holding the secret paired with
    /// [`client_id`](Self::client_id), for the rare confidential client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret_env: Option<String>,
    /// Scopes to ask for, when the ones the server advertises are not the
    /// ones wanted. Empty means "let the server's own metadata decide",
    /// which is what almost every provider expects.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
}

/// Reads `calls_per_minute`, rejecting the values that cannot mean anything.
///
/// # Errors
///
/// A budget of zero is a config error rather than a silent "no budget": the
/// two readings of `calls_per_minute = 0` — refuse everything, or meter
/// nothing — are opposites, and a file that has to be guessed at is the
/// wrong thing to put in front of a circuit breaker. Negative and
/// fractional values fail here too, out of `u32` itself; the way to say "no
/// budget" is to have no key, which is what `mcpgw tools <server> budget
/// off` writes.
fn calls_per_minute<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let calls = u32::deserialize(deserializer)?;
    if calls == 0 {
        return Err(serde::de::Error::custom(
            "calls_per_minute must be at least 1; drop the key for no budget",
        ));
    }
    Ok(calls)
}

// Taken by reference because that is the shape `skip_serializing_if` calls
// with.
#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde calls `skip_serializing_if` with a reference to the field"
)]
fn unlimited(calls: &u32) -> bool {
    *calls == 0
}

/// Reads a `headers_command` as argv, from either spelling.
///
/// # Errors
///
/// An empty command, or one carrying an empty argument, is a config error
/// rather than a value the gateway discovers it cannot run at connect time.
fn argv<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Spelling {
        Line(String),
        Argv(Vec<String>),
    }

    let argv = match Spelling::deserialize(deserializer)? {
        Spelling::Line(line) => line.split_whitespace().map(str::to_owned).collect(),
        Spelling::Argv(argv) => argv,
    };
    if argv.is_empty() {
        return Err(serde::de::Error::custom("headers_command is empty"));
    }
    if argv.iter().any(String::is_empty) {
        return Err(serde::de::Error::custom(
            "headers_command has an empty argument",
        ));
    }
    Ok(argv)
}

fn default_true() -> bool {
    true
}

// Deserialized ahead of the full model so a future schema fails with a clear
// "unsupported version" instead of a confusing field-level parse error.
#[derive(Deserialize)]
struct VersionProbe {
    version: u32,
}

impl Config {
    /// An in-memory config with no servers, at the current schema version.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            version: SUPPORTED_VERSION,
            capture: Capture::default(),
            gateway: GatewaySettings::default(),
            clients: BTreeMap::new(),
            servers: BTreeMap::new(),
        }
    }

    /// Parses and validates config text. `path` is only used in error
    /// messages, so callers may pass a logical path.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Parse`] for malformed TOML or schema violations,
    /// [`Error::UnsupportedVersion`] for a version this build does not know,
    /// [`Error::InvalidName`] for server names outside `[a-z0-9-_]` or
    /// containing the reserved `__` separator, and
    /// [`Error::InvalidRedaction`] for an unusable `[capture] redact`
    /// pattern.
    pub fn parse(text: &str, path: &Path) -> Result<Self, Error> {
        let parse_err = |source| Error::Parse {
            path: path.to_owned(),
            source: Box::new(source),
        };
        let probe: VersionProbe = toml::from_str(text).map_err(parse_err)?;
        if probe.version != SUPPORTED_VERSION {
            return Err(Error::UnsupportedVersion {
                found: probe.version,
            });
        }
        let config: Self = toml::from_str(text).map_err(parse_err)?;
        for (name, server) in &config.servers {
            validate_name(name)?;
            validate_auth(name, server)?;
        }
        // The client id is validated and the server names inside a scope are
        // not: a misspelled id is a table the gateway would silently never
        // consult, while a name that no longer exists is the ordinary state
        // between `mcpgw remove` and the next edit — `doctor` reports it,
        // and refusing to parse would leave the file unrepairable by the
        // commands that edit it.
        for id in config.clients.keys() {
            if crate::clients::ClientKind::from_id(id).is_none() {
                return Err(Error::UnknownClient {
                    id: id.clone(),
                    available: crate::clients::ClientKind::ALL
                        .iter()
                        .map(|kind| (*kind).id().to_owned())
                        .collect(),
                });
            }
        }
        // Compiled and thrown away: the gateway builds its own rules later,
        // and the point here is that `mcpgw serve` never starts believing it
        // is redacting with a pattern the engine rejected.
        crate::capture::RedactionRules::compile(&config.capture.redact)?;
        Ok(config)
    }

    /// Loads and validates the config file at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotFound`] when the file does not exist (the normal
    /// first-run state), [`Error::Io`] for other read failures, plus
    /// everything [`Config::parse`] returns.
    pub fn load(path: &Path) -> Result<Self, Error> {
        Ok(Self::load_reporting(path)?.0)
    }

    /// Loads it, and reports the keys this build does not recognize
    /// alongside it.
    ///
    /// What every caller that has somewhere to print a warning uses —
    /// `doctor`, and the gateway on start and on reload. The keys are
    /// diagnostics only: the config that comes back is exactly the one
    /// [`Config::load`] returns, unknown keys and all. See [`unknown_keys`].
    ///
    /// # Errors
    ///
    /// The same set [`Config::load`] returns.
    pub fn load_reporting(path: &Path) -> Result<(Self, Vec<UnknownKey>), Error> {
        let text = std::fs::read_to_string(path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                Error::NotFound {
                    path: path.to_owned(),
                }
            } else {
                Error::Io {
                    path: path.to_owned(),
                    source,
                }
            }
        })?;
        let config = Self::parse(&text, path)?;
        Ok((config, unknown_keys(&text)))
    }

    /// Serializes the config back to TOML.
    ///
    /// This is the plain serde form used for round-trips and tests; CLI writes
    /// that must preserve user comments go through `toml_edit` instead (M2).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Serialize`] if the model cannot be represented as TOML.
    pub fn to_toml_string(&self) -> Result<String, Error> {
        toml::to_string_pretty(self).map_err(|source| Error::Serialize {
            source: Box::new(source),
        })
    }
}

/// Refuses an entry that carries both an `[auth]` table and a
/// `headers_command`.
///
/// The two are two answers to one question — what goes in the
/// `Authorization` header — and an entry with both would have the command's
/// output silently win over a token a user just logged in for, or the other
/// way round depending on which layer ran last. Neither is a behaviour worth
/// documenting, and a config that asks for both is a mistake worth naming at
/// parse time rather than at the next connect.
///
/// # Errors
///
/// Returns [`Error::AuthConflict`] when both are set.
fn validate_auth(name: &str, server: &Server) -> Result<(), Error> {
    let Transport::Http {
        headers_command,
        auth: Some(_),
        ..
    } = &server.transport
    else {
        return Ok(());
    };
    if headers_command.is_empty() {
        return Ok(());
    }
    Err(Error::AuthConflict {
        name: name.to_owned(),
    })
}

/// Validates a server name against `[a-z0-9-_]+`, minus `__`.
///
/// A name is a URL path segment (`/s/<name>`) and a column in `mcpgw watch`,
/// which joins a captured call as `server__tool`: outside this set a name
/// would have to be escaped in the first, and `__` inside one makes the
/// second ambiguous to read.
///
/// # Errors
///
/// Returns [`Error::InvalidName`] when the name is empty, contains other
/// characters, or contains the reserved `__` separator.
pub fn validate_name(name: &str) -> Result<(), Error> {
    let invalid = |reason| {
        Err(Error::InvalidName {
            name: name.to_owned(),
            reason,
        })
    };
    let charset_ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if !charset_ok {
        return invalid("only lowercase letters, digits, '-' and '_' are allowed");
    }
    if name.contains(crate::gateway::SEPARATOR) {
        return invalid("'__' is reserved and cannot appear in a server name");
    }
    Ok(())
}

/// One key in `config.toml` that this build does not recognize.
///
/// Reported, never enforced. The gateway keeps loading the file and keeps
/// ignoring the key, exactly as it did before this existed — see
/// [`unknown_keys`] for why that is the policy and not an oversight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownKey {
    /// Where the key sits, as the file spells it:
    /// `servers.context7.calls_per_minutes`, `clients.cursor.server`.
    pub path: String,
    /// The recognized key at the same level that it is one or two edits
    /// away from, when there is one. This is what turns "something here is
    /// wrong" into a fix, and a typo in a restriction is the case the whole
    /// check exists for.
    pub did_you_mean: Option<&'static str>,
}

impl UnknownKey {
    /// The one-line diagnostic, identical wherever it is printed.
    #[must_use]
    pub fn message(&self) -> String {
        match self.did_you_mean {
            Some(known) => format!(
                "unknown key {} in config.toml — did you mean {known:?}? \
                 It is ignored as written",
                self.path
            ),
            None => format!(
                "unknown key {} in config.toml — it is ignored; check for a typo, or for a \
                 key added by a newer mcpgw",
                self.path
            ),
        }
    }
}

/// Every key in `text` that this build does not know, deepest tables
/// included.
///
/// # Why a warning and not a parse error
///
/// A typo in `deny`, `calls_per_minute` or `servers` is a restriction the
/// user wrote and did not get, and nothing about the resulting config looks
/// wrong: the gateway serves everything, quietly. That is worth saying out
/// loud. It is not worth refusing to start over, for two reasons. Configs
/// with such a typo already exist in the wild and would go from "one silent
/// gap" to "gateway down" on upgrade, with no warning period. And a key this
/// build does not know is not necessarily a mistake — it is also what a
/// config written by a *newer* mcpgw looks like, and one machine's config is
/// routinely read by more than one version of the binary (an older CLI, a
/// gateway that has not been upgraded yet). Refusing those would make
/// downgrades and staged rollouts unusable. So an unrecognized key is always
/// a diagnostic, here, in `doctor`, and in the gateway's load and reload
/// logs; enforcement is a separate decision for a later release.
///
/// Text that is not valid TOML yields nothing: that is a parse error, which
/// [`Config::parse`] reports on its own and in far more detail.
#[must_use]
pub fn unknown_keys(text: &str) -> Vec<UnknownKey> {
    let Ok(table) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    let mut found = Vec::new();
    let mut scan = |prefix: &str, table: &toml::Table, known: &'static [&'static str]| {
        for key in table.keys() {
            if !known.contains(&key.as_str()) {
                found.push(UnknownKey {
                    path: join(prefix, key),
                    did_you_mean: nearest(key, known),
                });
            }
        }
    };
    scan("", &table, CONFIG_KEYS);
    for (section, keys) in [
        ("capture", CAPTURE_KEYS),
        ("gateway", GATEWAY_SETTINGS_KEYS),
    ] {
        if let Some(sub) = sub_table(&table, section) {
            scan(section, sub, keys);
        }
    }
    // Both `[clients]` and `[servers]` are tables of user-named tables, so
    // the names themselves are never "unknown" — only what is written inside
    // one is. An unknown client id is refused at parse time and a scoped
    // server name that does not exist is a `doctor` finding of its own.
    for (client, scope) in named_tables(&table, "clients") {
        scan(&client, scope, CLIENT_SCOPE_KEYS);
        if let Some(tools) = sub_table(scope, "tools") {
            scan(&join(&client, "tools"), tools, TOOL_RULES_KEYS);
        }
    }
    for (server, entry) in named_tables(&table, "servers") {
        scan(&server, entry, SERVER_KEYS);
        if let Some(tools) = sub_table(entry, "tools") {
            scan(&join(&server, "tools"), tools, TOOL_RULES_KEYS);
        }
        if let Some(auth) = sub_table(entry, "auth") {
            scan(&join(&server, "auth"), auth, SERVER_AUTH_KEYS);
        }
        // `env` and `headers` are deliberately not descended into: their
        // keys are variable and header names, chosen by the user, and every
        // one of them would be "unknown".
    }
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found
}

/// The keys of `Config`, as serde spells them.
///
/// Hand-written because serde offers no way to ask a type for its field
/// names, and kept honest by a test that walks a fully populated config
/// through [`unknown_keys`] and expects nothing: a field added without a
/// line here fails that test.
const CONFIG_KEYS: &[&str] = &["version", "capture", "gateway", "clients", "servers"];
const CAPTURE_KEYS: &[&str] = &["redact", "retain_days"];
const GATEWAY_SETTINGS_KEYS: &[&str] = &["require_token"];
const CLIENT_SCOPE_KEYS: &[&str] = &["servers", "max_tools", "tools"];
const TOOL_RULES_KEYS: &[&str] = &["allow", "deny", "drift"];
/// One list for both transports, because `Transport` is flattened into the
/// server table: which of these belong together is `type`'s business, and a
/// `url` on a stdio entry is a different complaint than a misspelled key.
const SERVER_KEYS: &[&str] = &[
    "enabled",
    "tags",
    "calls_per_minute",
    "tools",
    "type",
    "command",
    "args",
    "env",
    "url",
    "headers_command",
    "headers",
    "auth",
];
const SERVER_AUTH_KEYS: &[&str] = &["client_id", "client_secret_env", "scopes"];

fn join(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_owned()
    } else {
        format!("{prefix}.{key}")
    }
}

fn sub_table<'t>(table: &'t toml::Table, key: &str) -> Option<&'t toml::Table> {
    table.get(key)?.as_table()
}

/// The `[section.NAME]` tables under `section`, as `(path, table)` pairs.
/// A section that is not a table, or an entry that is not one, is skipped:
/// that is a type error, which the parse reports.
fn named_tables<'t>(table: &'t toml::Table, section: &str) -> Vec<(String, &'t toml::Table)> {
    sub_table(table, section)
        .into_iter()
        .flat_map(toml::Table::iter)
        .filter_map(|(name, value)| Some((format!("{section}.{name}"), value.as_table()?)))
        .collect()
}

/// The known key `key` is most likely a misspelling of, if any.
///
/// Two edits at most, and never more edits than half the key: without that
/// second bound every three-letter key would be "close" to every other one,
/// and a confident wrong suggestion is worse than none.
fn nearest(key: &str, known: &'static [&'static str]) -> Option<&'static str> {
    known
        .iter()
        .map(|candidate| (distance(key, candidate), *candidate))
        .filter(|(d, _)| *d <= 2 && *d * 2 <= key.chars().count())
        .min_by_key(|(d, candidate)| (*d, *candidate))
        .map(|(_, candidate)| candidate)
}

/// Levenshtein distance, over chars.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    // One row of the matrix, reused: the recurrence only ever looks at the
    // row above, and config keys are short enough that the win is clarity,
    // not speed.
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ac) in a.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, bc) in b.iter().enumerate() {
            let cost = usize::from(ac != *bc);
            let next = (row[j + 1] + 1).min(row[j] + 1).min(diagonal + cost);
            diagonal = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()]
}

#[cfg(test)]
mod tests {
    use super::{Drift, ToolRules};

    fn rules(allow: &[&str], deny: &[&str]) -> ToolRules {
        ToolRules {
            allow: allow.iter().map(|s| (*s).to_owned()).collect(),
            deny: deny.iter().map(|s| (*s).to_owned()).collect(),
            ..ToolRules::default()
        }
    }

    #[test]
    fn drift_defaults_to_warn_and_a_table_that_only_turns_it_off_is_not_empty() {
        assert_eq!(ToolRules::default().drift, Drift::Warn);
        let watched = ToolRules {
            drift: Drift::Off,
            ..ToolRules::default()
        };
        assert!(!watched.is_empty());
        // Still an allow-everything table: turning drift off says nothing
        // about which tools reach a client.
        assert!(watched.allows("anything"));
    }

    #[test]
    fn an_empty_table_allows_everything() {
        let rules = ToolRules::default();
        assert!(rules.is_empty());
        assert!(rules.allows("delete_repository"));
    }

    #[test]
    fn an_allow_list_denies_by_default() {
        let rules = rules(&["echo"], &[]);
        assert!(rules.allows("echo"));
        assert!(!rules.allows("reverse"));
        // Prefix matching is a trailing `*` and nothing else: a name that
        // merely starts with an allowed one is a different tool.
        assert!(!rules.allows("echo_all"));
    }

    #[test]
    fn a_deny_list_removes_only_what_it_names() {
        let rules = rules(&[], &["delete_*"]);
        assert!(rules.allows("search_repositories"));
        assert!(!rules.allows("delete_repository"));
        assert!(!rules.allows("delete_"));
        // The prefix itself is not the pattern; `delete` does not start with
        // `delete_`.
        assert!(rules.allows("delete"));
    }

    #[test]
    fn deny_wins_over_allow() {
        let rules = rules(&["repo_*"], &["repo_delete"]);
        assert!(rules.allows("repo_read"));
        assert!(!rules.allows("repo_delete"));
        assert!(!rules.allows("issue_read"));
    }

    #[test]
    fn a_bare_star_allows_everything_it_is_asked_about() {
        assert!(rules(&["*"], &[]).allows("anything"));
        assert!(!rules(&[], &["*"]).allows("anything"));
    }

    #[test]
    fn unmatched_names_the_list_and_the_entry() {
        let rules = rules(&["echo", "gone"], &["missing_*"]);
        let tools = vec!["echo".to_owned(), "reverse".to_owned()];
        assert_eq!(
            rules.unmatched(&tools),
            [("allow", "gone"), ("deny", "missing_*")]
        );
        assert_eq!(rules.unmatched(&[]).len(), 3);
    }
}
