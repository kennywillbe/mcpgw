use std::collections::BTreeMap;
use std::io::IsTerminal as _;

use anyhow::{Context as _, bail};
use mcpgw_core::{ConfigStore, Error, Server, Transport};

#[derive(clap::Args)]
pub struct AddArgs {
    /// Server name ([a-z0-9-_])
    pub name: String,
    /// URL of an HTTP server (instead of a command)
    #[arg(long)]
    pub url: Option<String>,
    /// Environment variable for a stdio server (repeatable)
    #[arg(long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,
    /// HTTP header for an http server (repeatable)
    #[arg(long = "header", value_name = "KEY=VALUE")]
    pub headers: Vec<String>,
    /// Command printing a JSON object of headers, re-run on every connect
    #[arg(long = "headers-command", value_name = "COMMAND")]
    pub headers_command: Option<String>,
    /// Tag for grouping (repeatable)
    #[arg(long = "tag")]
    pub tags: Vec<String>,
    /// Add the server in disabled state
    #[arg(long)]
    pub disabled: bool,
    /// Overwrite an existing server without asking
    #[arg(long)]
    pub force: bool,
    /// Add without syncing the clients. They do not carry the new server
    /// until `mcpgw sync` runs
    #[arg(long)]
    pub no_sync: bool,
    /// Command and arguments for a stdio server, verbatim after `--`
    #[arg(last = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

pub fn run(args: &AddArgs, color: bool) -> anyhow::Result<()> {
    let server = Server {
        enabled: !args.disabled,
        tags: args.tags.clone(),
        calls_per_minute: 0,
        tools: None,
        transport: build_transport(args)?,
    };
    let path = super::canonical_config_path()?;
    let mut store = ConfigStore::edit_or_create(&path)?;

    let replaced = match store.upsert_server(&args.name, &server, args.force) {
        Ok(replaced) => replaced,
        Err(Error::DuplicateName { .. }) => {
            if !std::io::stdin().is_terminal() {
                bail!(
                    "server {:?} already exists (pass --force to overwrite)",
                    args.name
                );
            }
            if !super::confirm(&format!(
                "server {:?} already exists — overwrite?",
                args.name
            ))? {
                bail!("aborted");
            }
            store.upsert_server(&args.name, &server, true)?
        }
        Err(err) => return Err(err.into()),
    };

    store
        .save()
        .with_context(|| format!("cannot write {}", path.display()))?;
    let kind = match server.transport {
        Transport::Stdio { .. } => "stdio",
        Transport::Http { .. } => "http",
    };
    println!(
        "{} {:?} ({kind})",
        if replaced { "updated" } else { "added" },
        args.name
    );
    warn_unreachable_by_daemon(&server.transport);
    super::sync::after_edit(args.no_sync, color)
}

/// Says so when the command just stored resolves for the caller but not for
/// the installed gateway service.
///
/// After the entry is written rather than instead of writing it: the entry is
/// exactly what was asked for and is right for a foreground `mcpgw serve`, so
/// this is a warning about where it will *not* work, not a refusal. On stderr
/// so the success line above stays the only thing a script reads.
///
/// A server carrying its own `PATH` is left alone: `--env` reaches the child
/// whatever the daemon's own environment is, which is the whole reason it is
/// one of the fixes offered here.
fn warn_unreachable_by_daemon(transport: &Transport) {
    let Transport::Stdio { command, env, .. } = transport else {
        return;
    };
    if env.contains_key("PATH") {
        return;
    }
    let service_path = mcpgw_core::daemon_check::service_path();
    let reach = mcpgw_core::daemon_check::stdio_command_reach(command, service_path.as_deref());
    if let Some(advice) = reach.advice() {
        eprintln!("warning: {advice}");
        eprintln!("         (or re-add it with --env PATH=... to give this server its own PATH)");
    }
}

fn build_transport(args: &AddArgs) -> anyhow::Result<Transport> {
    match (&args.url, args.command.split_first()) {
        (Some(_), Some(_)) => bail!("give either --url or a command after `--`, not both"),
        (None, None) => bail!("missing server target: add a command after `--` or pass --url"),
        (Some(url), None) => {
            if !args.env.is_empty() {
                bail!("--env is for stdio servers; use --header for http");
            }
            Ok(Transport::Http {
                url: url.clone(),
                headers_command: headers_command(args.headers_command.as_deref())?,
                headers: parse_pairs(&args.headers, "--header")?,
                // `mcpgw auth login` writes this table when it is told a
                // client id; `add` has no business guessing one, and a server
                // that needs no OAuth must not grow an empty table for it.
                auth: None,
            })
        }
        (None, Some((command, rest))) => {
            if !args.headers.is_empty() {
                bail!("--header is for http servers; use --env for stdio");
            }
            if args.headers_command.is_some() {
                bail!(
                    "--headers-command is for http servers; a stdio server's credentials go in --env"
                );
            }
            Ok(Transport::Stdio {
                command: command.clone(),
                args: rest.to_vec(),
                env: parse_pairs(&args.env, "--env")?,
            })
        }
    }
}

/// Splits the flag's one string into argv on whitespace.
///
/// A flag is a line somebody types, and a line is what both Claude Code and
/// Codex spell their helper with, so this is the shape a user arrives with.
/// mcpgw stores argv and runs it with no shell, so a command whose arguments
/// carry spaces has no spelling here — it is written as an array in the
/// config file, where argv is spelled directly.
fn headers_command(flag: Option<&str>) -> anyhow::Result<Vec<String>> {
    let Some(line) = flag else {
        return Ok(Vec::new());
    };
    let argv: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
    if argv.is_empty() {
        bail!("--headers-command needs a command to run");
    }
    Ok(argv)
}

fn parse_pairs(pairs: &[String], flag: &str) -> anyhow::Result<BTreeMap<String, String>> {
    pairs
        .iter()
        .map(|pair| {
            pair.split_once('=')
                .map(|(key, val)| (key.to_owned(), val.to_owned()))
                .with_context(|| format!("{flag} expects KEY=VALUE, got {pair:?}"))
        })
        .collect()
}
