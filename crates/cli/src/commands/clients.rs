//! `mcpgw clients`: which servers and tools each client is given, and the
//! edits to `[clients.KIND]` that change that.
//!
//! Nothing here dials anything. What a client would actually see once the
//! servers have answered is `mcpgw doctor --probe`, which prices it; this
//! command is the scope itself — the list a user writes and reads back.

use std::fmt::Write as _;

use anyhow::Context as _;
use mcpgw_core::config::ClientScope;
use mcpgw_core::{ClientKind, Config, ConfigStore, Detection, Error};
use owo_colors::OwoColorize as _;

#[derive(clap::Args)]
pub struct ClientsArgs {
    /// Client id (omit to list every client)
    #[arg(value_name = "KIND", long_help = super::client_ids_help("Client id"))]
    pub kind: Option<String>,
    #[command(subcommand)]
    pub command: Option<ClientsCommand>,
}

#[derive(clap::Subcommand)]
pub enum ClientsCommand {
    /// Give this client only these servers, or `all` for every one
    Servers {
        /// Canonical server names, or the single word `all`
        #[arg(required = true, value_name = "NAME")]
        names: Vec<String>,
    },
    /// Narrow which tools this client sees, on top of each server's own lists
    Tools {
        #[command(subcommand)]
        command: ToolsCommand,
    },
}

#[derive(clap::Subcommand)]
pub enum ToolsCommand {
    /// Add names (or `prefix*`) to this client's allow list
    Allow {
        #[arg(required = true, value_name = "TOOL")]
        tools: Vec<String>,
    },
    /// Add names (or `prefix*`) to this client's deny list
    Deny {
        #[arg(required = true, value_name = "TOOL")]
        tools: Vec<String>,
    },
    /// Remove this client's tool lists, leaving only the servers' own
    Clear,
}

/// The word `servers` takes to mean "no restriction at all".
const ALL: &str = "all";

pub fn run(args: &ClientsArgs, color: bool) -> anyhow::Result<()> {
    let Some(id) = &args.kind else {
        return list(color);
    };
    let kind = resolve(id)?;
    match &args.command {
        Some(ClientsCommand::Servers { names }) => set_servers(kind, names),
        Some(ClientsCommand::Tools { command }) => tools(kind, command),
        None => show(kind, color),
    }
}

fn resolve(id: &str) -> anyhow::Result<ClientKind> {
    ClientKind::from_id(id).with_context(|| {
        let valid: Vec<&str> = ClientKind::ALL.iter().map(|k| k.id()).collect();
        format!("unknown client {id:?} (valid: {})", valid.join(", "))
    })
}

fn load() -> anyhow::Result<Config> {
    let path = super::canonical_config_path()?;
    match Config::load(&path) {
        Ok(config) => Ok(config),
        // The first-run state: no config, so no client is scoped and every
        // one of them would be given everything.
        Err(Error::NotFound { .. }) => Ok(Config::empty()),
        Err(err) => Err(err).with_context(|| format!("cannot load {}", path.display())),
    }
}

/// Rewrites one client's scope through the store, then saves.
fn edit(kind: ClientKind, change: impl FnOnce(&mut ClientScope)) -> anyhow::Result<ClientScope> {
    let path = super::canonical_config_path()?;
    let mut store = ConfigStore::edit(&path)?;
    let mut scope = store
        .config()
        .clients
        .get(kind.id())
        .cloned()
        .unwrap_or_default();
    change(&mut scope);
    store.set_client_scope(kind.id(), &scope)?;
    store.save()?;
    Ok(scope)
}

fn set_servers(kind: ClientKind, names: &[String]) -> anyhow::Result<()> {
    let all = names.len() == 1 && names[0] == ALL;
    if !all {
        // Checked here rather than at parse: a name that goes stale later is
        // a doctor warning, but one that was never right when it was typed
        // is a typo, and the moment to say so is now.
        let config = load()?;
        for name in names {
            if !config.servers.contains_key(name) {
                return Err(Error::UnknownServer {
                    name: name.clone(),
                    available: config.servers.keys().cloned().collect(),
                }
                .into());
            }
        }
    }
    let scope = edit(kind, |scope| {
        scope.servers = if all { Vec::new() } else { names.to_vec() };
    })?;
    if scope.servers.is_empty() {
        println!(
            "{}: every server again — run `mcpgw sync --client {}` to write it",
            kind.id(),
            kind.id()
        );
    } else {
        println!(
            "{}: {} — run `mcpgw sync --client {}` to write it",
            kind.id(),
            scope.servers.join(", "),
            kind.id()
        );
    }
    Ok(())
}

fn tools(kind: ClientKind, command: &ToolsCommand) -> anyhow::Result<()> {
    let (add, into_allow) = match command {
        ToolsCommand::Allow { tools } => (tools.clone(), true),
        ToolsCommand::Deny { tools } => (tools.clone(), false),
        ToolsCommand::Clear => (Vec::new(), false),
    };
    let cleared = matches!(command, ToolsCommand::Clear);
    edit(kind, |scope| {
        if cleared {
            scope.tools = None;
            return;
        }
        let mut rules = scope.tools.clone().unwrap_or_default();
        let (into, out_of) = if into_allow {
            (&mut rules.allow, &mut rules.deny)
        } else {
            (&mut rules.deny, &mut rules.allow)
        };
        // Both halves, for the reason `mcpgw tools` does it: the lists are
        // read allow-then-deny, so allowing a name the deny list still holds
        // would print a confirmation and change nothing.
        for tool in &add {
            out_of.retain(|existing| existing != tool);
            if !into.contains(tool) {
                into.push(tool.clone());
            }
        }
        scope.tools = (!rules.is_empty()).then_some(rules);
    })?;
    match command {
        ToolsCommand::Clear => println!(
            "{}: tool lists cleared — it sees whatever its servers allow",
            kind.id()
        ),
        ToolsCommand::Allow { tools } => {
            println!("{}: allowed {}", kind.id(), tools.join(", "));
        }
        ToolsCommand::Deny { tools } => println!("{}: denied {}", kind.id(), tools.join(", ")),
    }
    Ok(())
}

fn show(kind: ClientKind, color: bool) -> anyhow::Result<()> {
    let config = load()?;
    let scope = config.clients.get(kind.id());
    print!(
        "{}",
        render_one(kind, scope, &detected(kind), config.servers.len(), color)
    );
    Ok(())
}

fn list(color: bool) -> anyhow::Result<()> {
    let config = load()?;
    let total = config.servers.len();
    let rows: Vec<String> = ClientKind::ALL
        .iter()
        .map(|kind| {
            render_one(
                *kind,
                config.clients.get(kind.id()),
                &detected(*kind),
                total,
                color,
            )
        })
        .collect();
    print!("{}", rows.join(""));
    println!();
    println!(
        "{}",
        crate::ui::dim(
            "a client with no scope is given every server; \
             `mcpgw doctor --probe` prices what each one sees",
            color,
        )
    );
    Ok(())
}

/// The one-word state of a client on this machine, for the row's tail.
fn detected(kind: ClientKind) -> String {
    match kind.detect() {
        Detection::NotInstalled => "not installed".to_owned(),
        Detection::Installed => "installed, no config".to_owned(),
        Detection::Configured(path) => path.display().to_string(),
    }
}

/// One client's block: what it is given, and where its file is.
fn render_one(
    kind: ClientKind,
    scope: Option<&ClientScope>,
    where_: &str,
    total_servers: usize,
    color: bool,
) -> String {
    let mut out = String::new();
    let servers = match scope.filter(|scope| !scope.servers.is_empty()) {
        Some(scope) => format!("{} of {total_servers} servers", scope.servers.len()),
        None => format!("all {total_servers} servers"),
    };
    let heading = format!("{} — {servers}", kind.id());
    let _ = writeln!(
        out,
        "{}",
        if color {
            heading.bold().to_string()
        } else {
            heading
        }
    );
    let _ = writeln!(out, "  {}", crate::ui::dim(where_, color));
    let Some(scope) = scope else {
        return out;
    };
    if !scope.servers.is_empty() {
        let _ = writeln!(out, "  servers = {:?}", scope.servers);
    }
    if let Some(rules) = scope.tools.as_ref().filter(|rules| !rules.is_empty()) {
        if !rules.allow.is_empty() {
            let _ = writeln!(out, "  tools allow = {:?}", rules.allow);
        }
        if !rules.deny.is_empty() {
            let _ = writeln!(out, "  tools deny  = {:?}", rules.deny);
        }
    }
    if let Some(max) = scope.max_tools {
        let _ = writeln!(out, "  max_tools = {max}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcpgw_core::ToolRules;

    fn scope() -> ClientScope {
        ClientScope {
            servers: vec!["github".to_owned(), "linear".to_owned()],
            max_tools: Some(40),
            tools: Some(ToolRules {
                allow: Vec::new(),
                deny: vec!["delete_*".to_owned()],
                ..ToolRules::default()
            }),
        }
    }

    #[test]
    fn a_scoped_client_shows_its_whole_table() {
        insta::assert_snapshot!(render_one(
            ClientKind::Cursor,
            Some(&scope()),
            "/home/x/.cursor/mcp.json",
            5,
            false
        ));
    }

    #[test]
    fn an_unscoped_client_says_it_gets_everything() {
        let rendered = render_one(ClientKind::Zed, None, "not installed", 5, false);
        assert!(rendered.contains("all 5 servers"), "{rendered}");
    }
}
