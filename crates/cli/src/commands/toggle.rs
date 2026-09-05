use mcpgw_core::ConfigStore;

/// A disabled server is not mirrored into any client, so flipping the switch
/// is an edit to the client files too: enabling one adds its entry back,
/// disabling one takes it away.
pub fn run(name: &str, enabled: bool, no_sync: bool, color: bool) -> anyhow::Result<()> {
    let path = super::canonical_config_path()?;
    let mut store = ConfigStore::edit(&path)?;
    store.set_enabled(name, enabled)?;
    store.save()?;
    println!("{} {name:?}", if enabled { "enabled" } else { "disabled" });
    super::sync::after_edit(no_sync, color)
}
