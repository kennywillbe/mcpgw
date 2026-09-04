use anyhow::Context as _;
use mcpgw_core::capture::{RedactionRules, redact_text};
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
    // The site's own `[capture] redact` patterns count here too: they name
    // the credential shapes only this user knows about, and a config already
    // rejected them at parse time if any of them were unusable.
    let rules = if show_secrets {
        None
    } else {
        Some(
            RedactionRules::compile(&config.capture.redact).with_context(|| {
                format!("cannot compile the redaction rules in {}", path.display())
            })?,
        )
    };

    if json {
        let config = match &rules {
            Some(rules) => masked(config, rules),
            None => config,
        };
        println!("{}", serde_json::to_string_pretty(&config)?);
    } else if config.servers.is_empty() {
        println!("no servers configured (config: {})", path.display());
    } else {
        // TARGET can carry a credential in an argument or in a URL's query
        // string, so it goes through `redact_text` like every other string
        // this crate prints.
        print!(
            "{}",
            crate::render::server_table(&config, color, rules.as_ref())
        );
    }
    Ok(())
}

/// Masks everything in `config` that a reader does not need: every stdio
/// `env` value and every HTTP header value becomes [`MASK`], and the strings
/// that are still worth reading — `url`, `args`, `headers_command` — keep
/// their shape but lose any credential inside them.
///
/// `env` and `headers` are where API keys and `Authorization: Bearer …`
/// live, and `--json` output is routinely piped somewhere it outlives the
/// terminal. The rest is a `?token=` in a URL or an `--api-key=` in an
/// argument, which [`redact_text`] takes out while leaving the command line
/// legible.
fn masked(mut config: Config, rules: &RedactionRules) -> Config {
    for server in config.servers.values_mut() {
        match &mut server.transport {
            Transport::Stdio { args, env, .. } => {
                for arg in args.iter_mut() {
                    *arg = redact_text(arg, rules);
                }
                for value in env.values_mut() {
                    MASK.clone_into(value);
                }
            }
            Transport::Http {
                url,
                headers_command,
                headers,
                ..
            } => {
                *url = redact_text(url, rules);
                for arg in headers_command.iter_mut() {
                    *arg = redact_text(arg, rules);
                }
                for value in headers.values_mut() {
                    MASK.clone_into(value);
                }
            }
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
        let json = serde_json::to_string(&masked(sample(), &RedactionRules::builtin())).unwrap();
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
