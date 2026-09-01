use anyhow::Context as _;
use mcpgw_core::{Config, Error, Transport, paths};

/// What a masked `env` / `headers` value renders as. The key survives —
/// knowing that `GITHUB_TOKEN` is set is useful; its value in a pipe or in
/// terminal scrollback is not.
const MASK: &str = "***";

pub fn run(json: bool, show_secrets: bool, color: bool) -> anyhow::Result<()> {
    let path = paths::config_path()
        .context("cannot determine a home directory to resolve the config path")?;
    let config = match Config::load(&path) {
        Ok(config) => config,
        // A missing file is the normal first-run state, not an error.
        Err(Error::NotFound { .. }) => Config::empty(),
        Err(err) => return Err(err).with_context(|| format!("cannot load {}", path.display())),
    };

    if json {
        let config = if show_secrets { config } else { masked(config) };
        println!("{}", serde_json::to_string_pretty(&config)?);
    } else if config.servers.is_empty() {
        println!("no servers configured (config: {})", path.display());
    } else {
        // The table renders names, transports and tags only, so there is
        // nothing to mask on this path.
        print!("{}", crate::render::server_table(&config, color));
    }
    Ok(())
}

/// Replaces every stdio `env` value and every HTTP header value with
/// [`MASK`]. Both are where API keys and `Authorization: Bearer …` live, and
/// `--json` output is routinely piped somewhere it outlives the terminal.
fn masked(mut config: Config) -> Config {
    for server in config.servers.values_mut() {
        let values = match &mut server.transport {
            Transport::Stdio { env, .. } => env,
            Transport::Http { headers, .. } => headers,
        };
        for value in values.values_mut() {
            MASK.clone_into(value);
        }
    }
    config
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn sample() -> Config {
        Config::parse(
            r#"
version = 1

[servers.github]
type = "stdio"
command = "npx"
env = { GITHUB_TOKEN = "ghp_realsecret", LOG = "debug" }

[servers.linear]
type = "http"
url = "https://mcp.linear.app/mcp"
headers = { Authorization = "Bearer t0ken" }
"#,
            Path::new("sample.toml"),
        )
        .unwrap()
    }

    #[test]
    fn masking_keeps_the_keys_and_drops_every_value() {
        let json = serde_json::to_string(&masked(sample())).unwrap();
        assert!(json.contains("GITHUB_TOKEN"), "{json}");
        assert!(json.contains("Authorization"), "{json}");
        assert!(!json.contains("ghp_realsecret"), "{json}");
        assert!(!json.contains("t0ken"), "{json}");
        // Non-secret env values are masked too: there is no reliable way to
        // tell which of a server's variables carry credentials.
        assert!(!json.contains("debug"), "{json}");
        // Everything a user reads the output for is untouched.
        assert!(json.contains("https://mcp.linear.app/mcp"), "{json}");
        assert!(json.contains("npx"), "{json}");
    }
}
