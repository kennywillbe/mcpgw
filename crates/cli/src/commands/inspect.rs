//! `mcpgw inspect <server>`: what one server actually offers, read from the
//! server itself. It connects directly (the same spawn/dial path
//! `doctor --probe` uses), so no gateway has to be running.

use std::time::Duration;

use anyhow::Context as _;
use mcpgw_core::probe::{Inspection, inspect_server};
use mcpgw_core::{Config, Error};
use owo_colors::OwoColorize as _;

#[derive(clap::Args)]
pub struct InspectArgs {
    /// Server name from the canonical config
    pub name: String,
    /// Machine-readable output
    #[arg(long)]
    pub json: bool,
    /// Connection timeout in seconds
    #[arg(long, default_value_t = 10, value_name = "SECS")]
    pub timeout: u64,
}

pub fn run(args: &InspectArgs, color: bool) -> anyhow::Result<()> {
    let path = super::canonical_config_path()?;
    let config = Config::load(&path).with_context(|| format!("cannot load {}", path.display()))?;
    let server = config
        .servers
        .get(&args.name)
        .ok_or_else(|| Error::UnknownServer {
            name: args.name.clone(),
            available: config.servers.keys().cloned().collect(),
        })?;

    let state_dir = mcpgw_core::paths::state_dir();
    let runtime = tokio::runtime::Runtime::new()?;
    let inspection = runtime
        .block_on(inspect_server(
            &args.name,
            server,
            // Same reason `--probe` reaches for it: `inspect` answers what
            // this server offers *the way mcpgw reaches it*, and for an
            // OAuth server that includes the login the gateway would present.
            state_dir.as_deref(),
            Duration::from_secs(args.timeout),
        ))
        .with_context(|| format!("cannot inspect server {:?}", args.name))?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&inspection)?);
    } else {
        print!("{}", render(&args.name, &inspection, color));
    }
    Ok(())
}

fn render(name: &str, inspection: &Inspection, color: bool) -> String {
    let mut out = String::new();
    let heading = |out: &mut String, text: String| {
        if color {
            out.push_str(&text.bold().to_string());
        } else {
            out.push_str(&text);
        }
        out.push('\n');
    };

    heading(
        &mut out,
        format!(
            "{name} — {} {}",
            inspection.server_name, inspection.server_version
        ),
    );

    out.push('\n');
    heading(&mut out, format!("tools ({})", inspection.tools.len()));
    let rows: Vec<(String, String)> = inspection
        .tools
        .iter()
        .map(|tool| (tool.name.clone(), first_line(tool.description.as_deref())))
        .collect();
    out.push_str(&table(&rows));

    out.push('\n');
    if inspection.supports_resources {
        heading(
            &mut out,
            format!("resources ({})", inspection.resources.len()),
        );
        let rows: Vec<(String, String)> = inspection
            .resources
            .iter()
            .map(|resource| {
                let detail = match &resource.mime_type {
                    Some(mime) => format!("{} ({mime})", resource.uri),
                    None => resource.uri.clone(),
                };
                (resource.name.clone(), detail)
            })
            .collect();
        out.push_str(&table(&rows));
    } else {
        out.push_str("resources: not supported by this server\n");
    }
    out
}

/// Two aligned columns, or an explicit empty marker. Padded by hand for the
/// same reason the server table is: ANSI escapes skew format! widths.
fn table(rows: &[(String, String)]) -> String {
    if rows.is_empty() {
        return "  (none)\n".to_owned();
    }
    let width = rows
        .iter()
        .map(|(left, _)| left.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for (left, right) in rows {
        let pad = " ".repeat(width - left.chars().count());
        out.push_str(format!("  {left}{pad}  {right}").trim_end());
        out.push('\n');
    }
    out
}

/// Descriptions are often several paragraphs; a table row shows the first
/// line and nothing else.
fn first_line(description: Option<&str>) -> String {
    description
        .and_then(|text| text.lines().find(|line| !line.trim().is_empty()))
        .unwrap_or_default()
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use mcpgw_core::probe::{ResourceInfo, ToolInfo};

    use super::*;

    fn inspection() -> Inspection {
        Inspection {
            server_name: "mcpgw-test-server".to_owned(),
            server_version: "9.9.9".to_owned(),
            tools: vec![
                ToolInfo {
                    name: "echo".to_owned(),
                    description: Some("echoes input\n\nand keeps going".to_owned()),
                },
                ToolInfo {
                    name: "reverse".to_owned(),
                    description: None,
                },
            ],
            resources: Vec::new(),
            supports_resources: false,
        }
    }

    #[test]
    fn renders_tools_and_the_missing_resources_capability() {
        insta::assert_snapshot!(render("fx", &inspection(), false));
    }

    #[test]
    fn renders_resources_when_the_server_has_them() {
        let mut inspection = inspection();
        inspection.supports_resources = true;
        inspection.resources = vec![ResourceInfo {
            uri: "file:///notes.md".to_owned(),
            name: "notes".to_owned(),
            description: None,
            mime_type: Some("text/markdown".to_owned()),
        }];
        insta::assert_snapshot!(render("fx", &inspection, false));
    }

    #[test]
    fn an_empty_capable_server_says_none() {
        let mut inspection = inspection();
        inspection.supports_resources = true;
        assert!(render("fx", &inspection, false).contains("(none)"));
    }
}
