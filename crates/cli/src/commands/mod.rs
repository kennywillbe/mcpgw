pub mod add;
pub mod connect;
pub mod doctor;
pub mod import;
pub mod inspect;
pub mod list;
pub mod remove;
pub mod self_update;
pub mod serve;
pub mod sync;
pub mod toggle;
pub mod watch;

use std::io::Write as _;
use std::path::PathBuf;

use anyhow::Context as _;

pub fn canonical_config_path() -> anyhow::Result<PathBuf> {
    mcpgw_core::paths::config_path()
        .context("cannot determine a home directory to resolve the config path")
}

/// Help text for a client-selecting flag, with the ids taken from
/// [`mcpgw_core::ClientKind::ALL`] rather than written out — a hand-kept list
/// goes stale the first time an adapter lands.
pub fn client_ids_help(lead: &str) -> String {
    let ids: Vec<&str> = mcpgw_core::ClientKind::ALL.iter().map(|k| k.id()).collect();
    format!("{lead} (repeatable; ids: {})", ids.join(", "))
}

pub fn select_clients(ids: &[String]) -> anyhow::Result<Vec<mcpgw_core::ClientKind>> {
    use mcpgw_core::ClientKind;
    if ids.is_empty() {
        return Ok(ClientKind::ALL.to_vec());
    }
    ids.iter()
        .map(|id| {
            ClientKind::from_id(id).with_context(|| {
                let valid: Vec<&str> = ClientKind::ALL.iter().map(|k| k.id()).collect();
                format!("unknown client {id:?} (valid: {})", valid.join(", "))
            })
        })
        .collect()
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
