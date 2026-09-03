use std::io::IsTerminal as _;
use std::process::ExitCode;

use clap::{CommandFactory as _, Parser, Subcommand};

mod commands;
mod render;
mod ui;
mod update;

#[derive(Parser)]
#[command(
    name = "mcpgw",
    version,
    about = "Manage MCP servers across every client from one place"
)]
struct Cli {
    // Optional so a bare `mcpgw` on a terminal can open the wizard. Off a
    // terminal it is still effectively required — see `bare`.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List servers from the canonical config
    List {
        /// Machine-readable output
        #[arg(long)]
        json: bool,
        /// Print env and header values instead of masking them
        #[arg(long)]
        show_secrets: bool,
    },
    /// Add a server to the canonical config
    Add(commands::add::AddArgs),
    /// Remove a server from the canonical config
    Remove(commands::remove::RemoveArgs),
    /// Re-enable a disabled server
    Enable {
        /// Server name
        name: String,
    },
    /// Disable a server without deleting its entry
    Disable {
        /// Server name
        name: String,
    },
    /// Pull servers from client configs into the canonical list
    Import(commands::import::ImportArgs),
    /// Push the canonical server list into client configs
    Sync(commands::sync::SyncArgs),
    /// Put every client back the way it was before mcpgw
    Eject(commands::eject::EjectArgs),
    /// Run the gateway: serve canonical servers over one MCP endpoint
    Serve(commands::serve::ServeArgs),
    /// Bridge a stdio-only client to a running gateway over HTTP
    Connect(commands::connect::ConnectArgs),
    /// Run the gateway as a background service, and report on it
    #[command(subcommand_help_heading = "Daemon commands")]
    Daemon(commands::daemon::DaemonArgs),
    /// Show what one server offers: identity, tools and resources
    Inspect(commands::inspect::InspectArgs),
    /// Follow the gateway's captured traffic live
    Watch(commands::watch::WatchArgs),
    /// Diagnose the canonical config and every detected client
    Doctor {
        /// Machine-readable output
        #[arg(long)]
        json: bool,
        /// Also reach every server directly, and every gateway endpoint the
        /// managed client entries point at, with a live MCP handshake
        #[arg(long)]
        probe: bool,
        /// Per-server probe timeout in seconds
        #[arg(long, default_value_t = 10, requires = "probe", value_name = "SECS")]
        timeout: u64,
        /// Gateway the managed client entries are expected to reach
        #[arg(long, default_value = commands::connect::DEFAULT_URL, value_name = "URL")]
        gateway_url: String,
    },
    /// Replace this binary with the latest release
    SelfUpdate(commands::self_update::SelfUpdateArgs),
    /// Set mcpgw up from scratch, one confirmed step at a time
    Init(commands::wizard::InitArgs),
}

fn main() -> anyhow::Result<ExitCode> {
    let cli = Cli::parse();
    let color = std::io::stdout().is_terminal();
    let Some(command) = cli.command else {
        return bare(color);
    };
    // `serve` and `connect` run until they are killed and own the terminal
    // (connect owns stdio outright), and `self-update` reports on releases
    // itself — none of them wants a version notice appended.
    let notice = !matches!(
        command,
        Command::Serve(_) | Command::Connect(_) | Command::SelfUpdate(_)
    );
    // The two commands a user runs *because* something is wrong. Suppressing
    // the notice on their failing exit is how the person whose gateway will
    // not answer — often because it is an old gateway — is the one person
    // never told a newer mcpgw exists. They get the cached line only: a
    // failed command is the wrong moment to spend a network round trip.
    let notice_when_failed = matches!(
        command,
        Command::Doctor { .. }
            | Command::Daemon(commands::daemon::DaemonArgs {
                command: commands::daemon::DaemonCommand::Status { .. },
            })
    );
    let code = dispatch(command, color)?;
    // Only after a command that worked, and only once its own output is
    // out: a notice is a footnote, never the last word on a failure.
    if notice {
        if code == 0 {
            update::notice::print_if_due(env!("CARGO_PKG_VERSION"));
        } else if notice_when_failed {
            update::notice::print_cached(env!("CARGO_PKG_VERSION"));
        }
    }
    Ok(ExitCode::from(code))
}

/// What `mcpgw` with no arguments does.
///
/// On a terminal it opens the wizard — the whole point of the tool is that a
/// new user does not have to know which subcommand to type first. Off one it
/// prints the same help clap printed when a subcommand was mandatory, and
/// exits 2: a wizard that cannot ask is not a wizard, and a script that
/// piped us somewhere expects an error, not four steps of prose.
fn bare(color: bool) -> anyhow::Result<ExitCode> {
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        let args = commands::wizard::InitArgs {
            yes: false,
            gateway_url: mcpgw_core::endpoints::DEFAULT_URL.to_owned(),
        };
        return Ok(ExitCode::from(commands::wizard::run(&args, color)?));
    }
    // stderr, byte for byte where clap put it: this is still the
    // missing-subcommand failure it always was.
    eprint!("{}", Cli::command().render_help());
    Ok(ExitCode::from(2))
}

/// Runs one command, returning its process exit code.
fn dispatch(command: Command, color: bool) -> anyhow::Result<u8> {
    match command {
        Command::List { json, show_secrets } => {
            commands::list::run(json, show_secrets, color).map(|()| 0)
        }
        Command::Add(args) => commands::add::run(&args).map(|()| 0),
        Command::Remove(args) => commands::remove::run(&args).map(|()| 0),
        Command::Enable { name } => commands::toggle::run(&name, true).map(|()| 0),
        Command::Disable { name } => commands::toggle::run(&name, false).map(|()| 0),
        Command::Import(args) => commands::import::run(&args).map(|()| 0),
        Command::Sync(args) => commands::sync::run(&args, color).map(|()| 0),
        Command::Eject(args) => commands::eject::run(&args, color),
        Command::Serve(args) => commands::serve::run(&args).map(|()| 0),
        Command::Connect(args) => commands::connect::run(&args).map(|()| 0),
        Command::Daemon(args) => commands::daemon::run(&args),
        Command::Inspect(args) => commands::inspect::run(&args, color).map(|()| 0),
        Command::Watch(args) => commands::watch::run(&args, color).map(|()| 0),
        Command::Doctor {
            json,
            probe,
            timeout,
            gateway_url,
        } => commands::doctor::run(
            json,
            color,
            probe.then(|| std::time::Duration::from_secs(timeout)),
            &gateway_url,
        ),
        Command::SelfUpdate(args) => commands::self_update::run(&args),
        Command::Init(args) => commands::wizard::run(&args, color),
    }
}
