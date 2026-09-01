use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::Error;

pub const SUPPORTED_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub version: u32,
    #[serde(default)]
    pub servers: BTreeMap<String, Server>,
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
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
    },
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
    /// and [`Error::InvalidName`] for server names outside `[a-z0-9-_]`.
    pub fn parse(text: &str, path: &Path) -> Result<Self, Error> {
        let parse_err = |source| Error::Parse {
            path: path.to_owned(),
            source,
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
        toml::to_string_pretty(self).map_err(|source| Error::Serialize { source })
    }
}

/// Validates a server name against `[a-z0-9-_]+`.
///
/// Names end up in the gateway's `server__tool` namespace later, so anything
/// outside this set would break tool-name parsing there.
///
/// # Errors
///
/// Returns [`Error::InvalidName`] when the name is empty or contains other
/// characters.
pub fn validate_name(name: &str) -> Result<(), Error> {
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if valid {
        Ok(())
    } else {
        Err(Error::InvalidName {
            name: name.to_owned(),
        })
    }
}
