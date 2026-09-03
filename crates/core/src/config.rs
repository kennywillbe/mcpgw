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
    // Flattened last so plain values serialize before the env/headers tables;
    // TOML requires values ahead of tables within one section.
    #[serde(flatten)]
    pub transport: Transport,
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
    },
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
        for name in config.servers.keys() {
            validate_name(name)?;
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

/// Validates a server name against `[a-z0-9-_]+`, minus `__`.
///
/// Names end up in the gateway's `server__tool` namespace, so anything
/// outside this set would break tool-name parsing there, and `__` inside a
/// name would make the server/tool split ambiguous.
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
        return invalid("'__' is reserved as the gateway's server__tool separator");
    }
    Ok(())
}
