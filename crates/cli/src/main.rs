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
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::List { json } => commands::list::run(json, std::io::stdout().is_terminal()),
    }
}
