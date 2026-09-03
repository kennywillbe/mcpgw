pub mod add;
pub mod connect;
pub mod daemon;
pub mod doctor;
pub mod eject;
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

/// Whether a stdio command can actually be started on this machine: an
/// absolute path that exists and is executable, or a bare name PATH resolves.
///
/// Shared by `doctor` and by import planning so the two cannot drift — an
/// entry import refuses to switch on is exactly the one doctor would flag.
pub fn command_exists(command: &str) -> bool {
    which::which(command).is_ok()
}

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

/// The repo-local MCP configs around the working directory, as bullets
/// naming each file, its client and how much it holds.
///
/// Shared by the wizard's import and sync steps: both offer the same set of
/// files, and a user who was shown one wording before the read and another
/// before the write would reasonably wonder whether they were the same files.
pub fn project_bullets(found: &[mcpgw_core::projects::ProjectConfig]) -> Vec<String> {
    found
        .iter()
        .map(|config| {
            let count = config.read.servers.len();
            let plural = if count == 1 { "server" } else { "servers" };
            format!(
                "{}  {} — {count} {plural}",
                config
                    .path
                    .strip_prefix(&config.dir)
                    .unwrap_or(&config.path)
                    .display(),
                config.kind.display_name(),
            )
        })
        .collect()
}
