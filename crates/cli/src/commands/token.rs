//! `mcpgw token`: the install token clients present to the gateway.
//!
//! Also the one place the rest of the CLI reads it from. `serve`, `connect`,
//! `sync`, `daemon` and `doctor` all need the same three answers — is there a
//! token, may this gateway bind past loopback, and what does a client entry
//! carry — and three spellings of "read the file if it is there" would be
//! three different answers on the machine where it is not.

use std::path::Path;

use anyhow::Context as _;
use mcpgw_core::Config;
use mcpgw_core::gateway_token::{BindPolicy, GatewayToken};
use owo_colors::OwoColorize as _;

#[derive(clap::Args)]
pub struct TokenArgs {
    #[command(subcommand)]
    pub command: TokenCommand,
}

#[derive(clap::Subcommand)]
pub enum TokenCommand {
    /// Show this install's gateway token
    Show {
        /// Print the token itself instead of masking it
        #[arg(long)]
        show_secrets: bool,
    },
    /// Issue a new token and re-sync every client that carries one
    Rotate {
        /// Rotate without re-syncing. The clients keep the old token and
        /// stop being able to reach the gateway until `mcpgw sync` runs
        #[arg(long)]
        no_sync: bool,
    },
}

pub fn run(args: &TokenArgs, color: bool) -> anyhow::Result<u8> {
    let state_dir = mcpgw_core::paths::state_dir()
        .context("cannot determine a home directory for the state dir")?;
    match &args.command {
        TokenCommand::Show { show_secrets } => show(&state_dir, *show_secrets, color),
        TokenCommand::Rotate { no_sync } => rotate(&state_dir, *no_sync, color),
    }
}

fn show(state_dir: &Path, show_secrets: bool, color: bool) -> anyhow::Result<u8> {
    let path = GatewayToken::path(state_dir);
    let Some(token) = GatewayToken::load(state_dir)? else {
        // Not an error: an install that has never served has nothing to show,
        // and minting one here would put a token on the disk as a side effect
        // of asking a question.
        println!("no gateway token yet — `mcpgw serve` or `mcpgw daemon install` writes one");
        return Ok(0);
    };
    let value = if show_secrets {
        token.secret().to_owned()
    } else {
        token.masked()
    };
    println!("token   {value}");
    println!("file    {}", path.display());
    if !show_secrets {
        println!(
            "  {}",
            crate::ui::dim("--show-secrets prints it in full", color)
        );
    }
    println!(
        "  {}",
        crate::ui::dim(
            "`mcpgw sync` writes it into every client entry that can carry a header",
            color,
        )
    );
    Ok(0)
}

fn rotate(state_dir: &Path, no_sync: bool, color: bool) -> anyhow::Result<u8> {
    let token = GatewayToken::rotate(state_dir)?;
    let line = format!("issued a new gateway token ({})", token.masked());
    if color {
        println!("{}", line.bold());
    } else {
        println!("{line}");
    }
    // Said before the sync runs and again if it does not: every client entry
    // still holds the old token, and until they are rewritten the gateway
    // answers none of them.
    println!("every client entry still carries the old one until it is re-synced.");
    if no_sync {
        println!("run `mcpgw sync` to write the new token into them.");
        return Ok(0);
    }
    println!();
    super::sync::run(
        &super::sync::SyncArgs {
            clients: Vec::new(),
            project: false,
            dry_run: false,
            rollback: false,
            gateway_url: super::connect::DEFAULT_URL.to_owned(),
        },
        color,
    )?;
    println!();
    println!("a running gateway picks the new token up on its next request — restart it");
    println!("(`mcpgw daemon restart`, or Ctrl-C on a foreground `mcpgw serve`) if it does not.");
    Ok(0)
}

/// This install's token, minting one if it has none.
///
/// What `serve` and `daemon install` call: the token has to exist before
/// anything writes it into a client file or checks a request against it, and
/// the first of those two commands to run is where it comes from.
///
/// # Errors
///
/// Fails when the state directory cannot be resolved or written.
pub fn ensure(state_dir: &Path) -> anyhow::Result<(GatewayToken, bool)> {
    Ok(GatewayToken::load_or_create(state_dir)?)
}

/// This install's token, or [`None`] on a machine that has none — which is
/// every machine until a gateway is first started, and any machine whose
/// state directory cannot be read.
///
/// Never fails: a `sync` that refused to run because a token file was
/// unreadable would be worse than one that writes the entries it always did.
#[must_use]
pub fn current() -> Option<GatewayToken> {
    GatewayToken::load(&mcpgw_core::paths::state_dir()?)
        .ok()
        .flatten()
}

/// Whether `[gateway] require_token` is on.
///
/// A config that cannot be read answers "no", which is the same answer the
/// absent table gives and the safe one: the knob only ever *widens* what a
/// gateway may bind, so failing to read it can only refuse an address, never
/// allow one.
#[must_use]
pub fn require_token() -> bool {
    super::canonical_config_path()
        .ok()
        .and_then(|path| Config::load(&path).ok())
        .is_some_and(|config| config.gateway.require_token)
}

/// The addresses a supervised gateway on this machine may be installed on.
///
/// The shared helper `daemon::preflight` asks, so that the wizard's daemon
/// step, `daemon install` and `daemon start` cannot reach three different
/// verdicts about the same install.
#[must_use]
pub fn bind_policy(state_dir: &Path) -> BindPolicy {
    BindPolicy::new(
        require_token(),
        GatewayToken::load(state_dir).ok().flatten().as_ref(),
    )
}
