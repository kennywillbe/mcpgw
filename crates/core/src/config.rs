use std::collections::BTreeMap;
use std::path::Path;

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
    #[serde(default)]
    pub servers: BTreeMap<String, Server>,
}

/// The `[capture]` table: what the gateway's traffic log is allowed to keep.
///
/// Absent from a config that never mentions it, and skipped on the way out
/// again, so adding the table here does not rewrite everybody's file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
}

impl Capture {
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.redact.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Server {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
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
}

impl ToolRules {
    /// Whether the table says nothing at all — no table and a table with two
    /// empty lists mean the same thing, and both mean "unchanged".
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.allow.is_empty() && self.deny.is_empty()
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
        Self::parse(&text, path)
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

#[cfg(test)]
mod tests {
    use super::ToolRules;

    fn rules(allow: &[&str], deny: &[&str]) -> ToolRules {
        ToolRules {
            allow: allow.iter().map(|s| (*s).to_owned()).collect(),
            deny: deny.iter().map(|s| (*s).to_owned()).collect(),
        }
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
