use std::io::IsTerminal as _;

use anyhow::bail;
use mcpgw_core::ConfigStore;

#[derive(clap::Args)]
pub struct RemoveArgs {
    /// Server name
    pub name: String,
    /// Skip the confirmation prompt
    #[arg(long)]
    pub yes: bool,
    /// Remove without syncing the clients. They keep their entry for the
    /// removed server until `mcpgw sync` runs
    #[arg(long)]
    pub no_sync: bool,
}

pub fn run(args: &RemoveArgs, color: bool) -> anyhow::Result<()> {
    let path = super::canonical_config_path()?;
    let mut store = ConfigStore::edit(&path)?;
    // Mutate first (validates the name exists), confirm before the actual
    // commit point — nothing touches disk until `save`.
    store.remove_server(&args.name)?;
    if !args.yes {
        if !std::io::stdin().is_terminal() {
            bail!(
                "refusing to remove {:?} without confirmation (pass --yes)",
                args.name
            );
        }
        if !super::confirm(&format!("remove server {:?}?", args.name))? {
            bail!("aborted");
        }
    }
    store.save()?;
    println!("removed {:?}", args.name);
    super::sync::after_edit(args.no_sync, color)
}
