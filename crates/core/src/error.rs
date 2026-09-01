use std::path::PathBuf;

use crate::config::SUPPORTED_VERSION;

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
        source: toml::de::Error,
    },

    #[error("failed to serialize config")]
    Serialize {
        #[source]
        source: toml::ser::Error,
    },

    #[error(
        "config version {found} is not supported (this mcpgw supports version {SUPPORTED_VERSION})"
    )]
    UnsupportedVersion { found: u32 },

    #[error(
        "invalid server name {name:?}: only lowercase letters, digits, '-' and '_' are allowed"
    )]
    InvalidName { name: String },
}
