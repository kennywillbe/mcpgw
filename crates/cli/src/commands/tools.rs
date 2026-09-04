//! `mcpgw tools <server>`: which of a server's tools reach a client, and the
//! edits to `[servers.NAME.tools]` that change that.
//!
//! The listing connects to the server directly, the way `inspect` does, so
//! it shows the tools the server offers right now rather than the names
//! somebody wrote in the config months ago — which is the whole question
//! when a rule stops matching.

use std::fmt::Write as _;
use std::time::Duration;

use anyhow::Context as _;
use mcpgw_core::probe::inspect_server;
use mcpgw_core::{Config, ConfigStore, Error, ToolRules};
use owo_colors::OwoColorize as _;

#[derive(clap::Args)]
pub struct ToolsArgs {
    /// Server name from the canonical config
    pub name: String,
    #[command(subcommand)]
    pub command: Option<ToolsCommand>,
    /// Connection timeout in seconds, for the listing
    #[arg(long, default_value_t = 10, value_name = "SECS")]
    pub timeout: u64,
}

#[derive(clap::Subcommand)]
pub enum ToolsCommand {
    /// Add names (or `prefix*`) to the server's allow list
    Allow {
        /// Tool names or `prefix*` patterns
        #[arg(required = true, value_name = "TOOL")]
        tools: Vec<String>,
    },
    /// Add names (or `prefix*`) to the server's deny list
    Deny {
        /// Tool names or `prefix*` patterns
        #[arg(required = true, value_name = "TOOL")]
        tools: Vec<String>,
    },
    /// Remove the server's lists entirely, allowing every tool again
    Clear,
}

pub fn run(args: &ToolsArgs, color: bool) -> anyhow::Result<()> {
    match &args.command {
        Some(ToolsCommand::Allow { tools }) => edit(&args.name, tools, Which::Allow),
        Some(ToolsCommand::Deny { tools }) => edit(&args.name, tools, Which::Deny),
        Some(ToolsCommand::Clear) => clear(&args.name),
        None => list(args, color),
    }
}

/// Which list an edit adds to. The other one is the list the same name is
/// taken out of — see [`edit`].
#[derive(Clone, Copy)]
enum Which {
    Allow,
    Deny,
}

impl Which {
    fn word(self) -> &'static str {
        match self {
            Which::Allow => "allowed",
            Which::Deny => "denied",
        }
    }
}

/// Adds `tools` to one list and removes the same entries from the other.
///
/// Both halves, because the lists are read allow-then-deny: `allow foo` on a
/// server whose `deny` still names `foo` would print a confirmation and
/// change nothing a client can see.
fn edit(name: &str, tools: &[String], which: Which) -> anyhow::Result<()> {
    let path = super::canonical_config_path()?;
    let mut store = ConfigStore::edit(&path)?;
    let mut rules = rules_of(store.config(), name)?;
    let (into, out_of) = match which {
        Which::Allow => (&mut rules.allow, &mut rules.deny),
        Which::Deny => (&mut rules.deny, &mut rules.allow),
    };
    for tool in tools {
        out_of.retain(|existing| existing != tool);
        if !into.contains(tool) {
            into.push(tool.clone());
        }
    }
    store.set_tool_rules(name, &rules)?;
    store.save()?;
    println!("{} on {name:?}: {}", which.word(), tools.join(", "));
    Ok(())
}

fn clear(name: &str) -> anyhow::Result<()> {
    let path = super::canonical_config_path()?;
    let mut store = ConfigStore::edit(&path)?;
    store.set_tool_rules(name, &ToolRules::default())?;
    store.save()?;
    println!("cleared the tool lists on {name:?}: every tool is allowed again");
    Ok(())
}

/// The server's current rules, or empty ones for a server that has no table.
fn rules_of(config: &Config, name: &str) -> Result<ToolRules, Error> {
    let server = config
        .servers
        .get(name)
        .ok_or_else(|| Error::UnknownServer {
            name: name.to_owned(),
            available: config.servers.keys().cloned().collect(),
        })?;
    Ok(server.tools.clone().unwrap_or_default())
}

fn list(args: &ToolsArgs, color: bool) -> anyhow::Result<()> {
    let path = super::canonical_config_path()?;
    let config = Config::load(&path).with_context(|| format!("cannot load {}", path.display()))?;
    let name = &args.name;
    let server = config
        .servers
        .get(name)
        .ok_or_else(|| Error::UnknownServer {
            name: name.clone(),
            available: config.servers.keys().cloned().collect(),
        })?;

    let state_dir = mcpgw_core::paths::state_dir();
    let runtime = tokio::runtime::Runtime::new()?;
    let inspection = runtime
        .block_on(inspect_server(
            name,
            server,
            // The listing has to say what *this* gateway can reach, and for
            // an OAuth server that means connecting with the login it holds.
            state_dir.as_deref(),
            Duration::from_secs(args.timeout),
        ))
        .with_context(|| format!("cannot inspect server {name:?}"))?;

    let rows: Vec<(String, bool)> = inspection
        .tools
        .iter()
        .map(|tool| (tool.name.clone(), server.allows_tool(&tool.name)))
        .collect();
    print!("{}", render(name, server.tools.as_ref(), &rows, color));
    Ok(())
}

/// The listing: every tool the server offers, with what the lists make of
/// it, under a header saying which lists are in force.
fn render(name: &str, rules: Option<&ToolRules>, rows: &[(String, bool)], color: bool) -> String {
    let mut out = String::new();
    let heading = if color {
        format!("{name} — {} tool(s)", rows.len())
            .bold()
            .to_string()
    } else {
        format!("{name} — {} tool(s)", rows.len())
    };
    out.push_str(&heading);
    out.push('\n');
    match rules.filter(|rules| !rules.is_empty()) {
        Some(rules) => {
            if !rules.allow.is_empty() {
                let _ = writeln!(out, "  allow = {:?}", rules.allow);
            }
            if !rules.deny.is_empty() {
                let _ = writeln!(out, "  deny  = {:?}", rules.deny);
            }
        }
        // Said out loud rather than left to an all-`allowed` column: the two
        // states look identical in the table and mean different things the
        // moment a tool is added to the server.
        None => out.push_str("  no lists — every tool is allowed\n"),
    }
    out.push('\n');
    if rows.is_empty() {
        out.push_str("  (none)\n");
        return out;
    }
    let width = rows
        .iter()
        .map(|(tool, _)| tool.chars().count())
        .max()
        .unwrap_or(0);
    for (tool, allowed) in rows {
        let pad = " ".repeat(width - tool.chars().count());
        let state = if *allowed { "allowed" } else { "denied" };
        let state = if color && !*allowed {
            state.red().to_string()
        } else {
            state.to_owned()
        };
        let _ = writeln!(out, "  {tool}{pad}  {state}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Vec<(String, bool)> {
        vec![("echo".to_owned(), true), ("reverse".to_owned(), false)]
    }

    #[test]
    fn renders_the_lists_and_the_column() {
        let rules = ToolRules {
            allow: vec!["echo".to_owned()],
            deny: Vec::new(),
        };
        insta::assert_snapshot!(render("fx", Some(&rules), &rows(), false));
    }

    #[test]
    fn a_server_with_no_lists_says_so() {
        let rendered = render("fx", None, &[("echo".to_owned(), true)], false);
        assert!(rendered.contains("every tool is allowed"), "{rendered}");
    }
}
