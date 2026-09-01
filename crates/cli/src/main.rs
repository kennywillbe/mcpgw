use std::io::IsTerminal as _;

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
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::List { json } => commands::list::run(json, std::io::stdout().is_terminal()),
        Command::Add(args) => commands::add::run(&args),
        Command::Remove(args) => commands::remove::run(&args),
        Command::Enable { name } => commands::toggle::run(&name, true),
        Command::Disable { name } => commands::toggle::run(&name, false),
    }
}
