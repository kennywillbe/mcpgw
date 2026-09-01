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
}

pub fn run(args: &RemoveArgs) -> anyhow::Result<()> {
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
    Ok(())
}
