pub mod add;
pub mod list;
pub mod remove;
pub mod toggle;

use std::io::Write as _;
use std::path::PathBuf;

use anyhow::Context as _;

pub fn canonical_config_path() -> anyhow::Result<PathBuf> {
    mcpgw_core::paths::config_path()
        .context("cannot determine a home directory to resolve the config path")
}

/// Asks a y/N question on the terminal. Callers must first check that stdin
/// is a TTY; piped invocations should require an explicit flag instead.
pub fn confirm(question: &str) -> anyhow::Result<bool> {
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "YES"))
}
