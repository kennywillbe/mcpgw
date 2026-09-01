use std::io::IsTerminal as _;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod commands;
mod render;

#[derive(Parser)]
#[command(
    name = "mcpgw",
    version,
    about = "Manage MCP servers across every client from one place"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
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
    /// Run the gateway: serve canonical servers over one MCP endpoint
    Serve(commands::serve::ServeArgs),
    /// Bridge a stdio-only client to a running gateway over HTTP
    Connect(commands::connect::ConnectArgs),
    /// Show what one server offers: identity, tools and resources
    Inspect(commands::inspect::InspectArgs),
    /// Follow the gateway's captured traffic live
    Watch(commands::watch::WatchArgs),
    /// Diagnose the canonical config and every detected client
    Doctor {
        /// Machine-readable output
        #[arg(long)]
        json: bool,
        /// Also reach every server and run a live MCP handshake
        #[arg(long)]
        probe: bool,
        /// Per-server probe timeout in seconds
        #[arg(long, default_value_t = 10, requires = "probe", value_name = "SECS")]
        timeout: u64,
    },
}

fn main() -> anyhow::Result<ExitCode> {
    let cli = Cli::parse();
    let color = std::io::stdout().is_terminal();
    match cli.command {
        Command::List { json, show_secrets } => {
            commands::list::run(json, show_secrets, color).map(|()| ExitCode::SUCCESS)
        }
        Command::Add(args) => commands::add::run(&args).map(|()| ExitCode::SUCCESS),
        Command::Remove(args) => commands::remove::run(&args).map(|()| ExitCode::SUCCESS),
        Command::Enable { name } => commands::toggle::run(&name, true).map(|()| ExitCode::SUCCESS),
        Command::Disable { name } => {
            commands::toggle::run(&name, false).map(|()| ExitCode::SUCCESS)
        }
        Command::Import(args) => commands::import::run(&args).map(|()| ExitCode::SUCCESS),
        Command::Sync(args) => commands::sync::run(&args, color).map(|()| ExitCode::SUCCESS),
        Command::Serve(args) => commands::serve::run(&args).map(|()| ExitCode::SUCCESS),
        Command::Connect(args) => commands::connect::run(&args).map(|()| ExitCode::SUCCESS),
        Command::Inspect(args) => commands::inspect::run(&args, color).map(|()| ExitCode::SUCCESS),
        Command::Watch(args) => commands::watch::run(&args, color).map(|()| ExitCode::SUCCESS),
        Command::Doctor {
            json,
            probe,
            timeout,
        } => commands::doctor::run(
            json,
            color,
            probe.then(|| std::time::Duration::from_secs(timeout)),
        ),
    }
}
