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
    /// Tag for grouping (repeatable)
    #[arg(long = "tag")]
    pub tags: Vec<String>,
    /// Add the server in disabled state
    #[arg(long)]
    pub disabled: bool,
    /// Overwrite an existing server without asking
    #[arg(long)]
    pub force: bool,
    /// Command and arguments for a stdio server, verbatim after `--`
    #[arg(last = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

pub fn run(args: &AddArgs) -> anyhow::Result<()> {
    let server = Server {
        enabled: !args.disabled,
        tags: args.tags.clone(),
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
    Ok(())
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
                headers: parse_pairs(&args.headers, "--header")?,
            })
        }
        (None, Some((command, rest))) => {
            if !args.headers.is_empty() {
                bail!("--header is for http servers; use --env for stdio");
            }
            Ok(Transport::Stdio {
                command: command.clone(),
                args: rest.to_vec(),
                env: parse_pairs(&args.env, "--env")?,
            })
        }
    }
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
