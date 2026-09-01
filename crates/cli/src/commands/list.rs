use anyhow::Context as _;
use mcpgw_core::{Config, Error, paths};

pub fn run(json: bool, color: bool) -> anyhow::Result<()> {
    let path = paths::config_path()
        .context("cannot determine a home directory to resolve the config path")?;
    let config = match Config::load(&path) {
        Ok(config) => config,
        // A missing file is the normal first-run state, not an error.
        Err(Error::NotFound { .. }) => Config::empty(),
        Err(err) => return Err(err).with_context(|| format!("cannot load {}", path.display())),
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&config)?);
    } else if config.servers.is_empty() {
        println!("no servers configured (config: {})", path.display());
    } else {
        print!("{}", crate::render::server_table(&config, color));
    }
    Ok(())
}
