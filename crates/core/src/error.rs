use std::path::PathBuf;

use crate::config::SUPPORTED_VERSION;

// The parser error types are boxed: inline they push this enum past the
// 128-byte `clippy::result_large_err` threshold on Windows, where their
// layouts are wider than on Linux and macOS.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    // Distinct from `Io` so callers can treat a missing config as the normal
    // first-run state instead of a failure.
    #[error("no config file at {path}")]
    NotFound { path: PathBuf },

    #[error("failed to read {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid config in {path}")]
    Parse {
        path: PathBuf,
        #[source]
        source: Box<toml::de::Error>,
    },

    #[error("failed to serialize config")]
    Serialize {
        #[source]
        source: Box<toml::ser::Error>,
    },

    #[error(
        "config version {found} is not supported (this mcpgw supports version {SUPPORTED_VERSION})"
    )]
    UnsupportedVersion { found: u32 },

    // `reason` carries the specific rule broken so the message can name it
    // (character set vs. the reserved gateway separator).
    #[error("invalid server name {name:?}: {reason}")]
    InvalidName { name: String, reason: &'static str },

    #[error("server {name:?} already exists")]
    DuplicateName { name: String },

    // Both fill the same header, so an entry with both has no defined
    // behaviour to document — see `config::validate_auth`.
    #[error(
        "server {name:?} sets both headers_command and [auth]; \
         a server's credential comes from one of them, not both"
    )]
    AuthConflict { name: String },

    #[error("no server named {name:?}{}", known(available))]
    UnknownServer {
        name: String,
        available: Vec<String>,
    },

    // The pattern is named because the table can hold several and only the
    // text the user typed identifies which one the engine refused.
    #[error("invalid regex {pattern:?} in [capture] redact")]
    InvalidRedaction {
        pattern: String,
        #[source]
        source: Box<regex::Error>,
    },

    #[error("invalid config in {path}")]
    Edit {
        path: PathBuf,
        #[source]
        source: Box<toml_edit::TomlError>,
    },

    #[error("invalid {client} config in {path}")]
    ClientParse {
        client: &'static str,
        path: PathBuf,
        #[source]
        source: Box<serde_json::Error>,
    },

    #[error("invalid mcpgw state file {path} (delete it to reset; entries become unmanaged)")]
    StateParse {
        path: PathBuf,
        #[source]
        source: Box<serde_json::Error>,
    },

    // Separate from `StateParse` because the advice differs: this file is
    // rewritten by the next gateway start, so deleting it costs nothing.
    #[error("invalid gateway record {path} (delete it; the gateway rewrites it at startup)")]
    RecordParse {
        path: PathBuf,
        #[source]
        source: Box<serde_json::Error>,
    },
}

fn known(available: &[String]) -> String {
    if available.is_empty() {
        " (the config has no servers)".to_owned()
    } else {
        format!(" (known servers: {})", available.join(", "))
    }
}
