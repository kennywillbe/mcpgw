use mcpgw_core::ConfigStore;

pub fn run(name: &str, enabled: bool) -> anyhow::Result<()> {
    let path = super::canonical_config_path()?;
    let mut store = ConfigStore::edit(&path)?;
    store.set_enabled(name, enabled)?;
    store.save()?;
    println!("{} {name:?}", if enabled { "enabled" } else { "disabled" });
    Ok(())
}
