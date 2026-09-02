pub mod add;
pub mod connect;
pub mod daemon;
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
pub mod wizard;

use std::path::PathBuf;

use anyhow::Context as _;

// `confirm` moved to `crate::ui` when the wizard needed more shapes of
// question than one; re-exported so the three commands that ask a plain y/N
// keep saying `super::confirm`.
pub use crate::ui::confirm;

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
