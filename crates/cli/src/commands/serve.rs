use std::sync::Arc;

use anyhow::{Context as _, bail};
use mcpgw_core::Config;
use mcpgw_core::capture::CaptureWriter;
use mcpgw_core::gateway::{Gateway, serve_http};
use mcpgw_core::upstream::UpstreamManager;

#[derive(clap::Args)]
pub struct ServeArgs {
    /// Port to listen on
    #[arg(long, default_value_t = 8137)]
    pub port: u16,
    /// Address to bind (loopback by default; anything else is unauthenticated!)
    #[arg(long, default_value = "127.0.0.1")]
    pub bind: String,
    /// Serve only these servers (repeatable; default: every enabled
    /// server). Exactly one turns the gateway into an unprefixed pipe.
    #[arg(long, value_name = "NAME")]
    pub server: Vec<String>,
    /// Do not write the JSONL traffic log
    #[arg(long)]
    pub no_capture: bool,
}

pub fn run(args: &ServeArgs) -> anyhow::Result<()> {
    let config_path = super::canonical_config_path()?;
    let config = Config::load(&config_path)?;

    let enabled: Vec<&String> = config
        .servers
        .iter()
        .filter(|(_, s)| s.enabled)
        .map(|(name, _)| name)
        .collect();
    let selected: Vec<String> = if args.server.is_empty() {
        if enabled.is_empty() {
            bail!("no enabled servers in {}", config_path.display());
        }
        enabled.iter().map(|name| (*name).clone()).collect()
    } else {
        for name in &args.server {
            let Some(server) = config.servers.get(name) else {
                bail!("no server named {name:?} in {}", config_path.display());
            };
            if !server.enabled {
                bail!("server {name:?} is disabled");
            }
        }
        args.server.clone()
    };

    if !is_loopback(&args.bind) {
        eprintln!(
            "warning: binding to {} without any authentication — anyone who can reach \
             this address can call your MCP servers; keep it behind a trusted network \
             or reverse proxy until the auth milestone lands",
            args.bind
        );
    }

    let capture = if args.no_capture {
        None
    } else {
        let state_dir = mcpgw_core::paths::state_dir()
            .context("cannot determine a home directory to resolve the state directory")?;
        Some(Arc::new(CaptureWriter::under_state_dir(&state_dir)))
    };

    let manager = Arc::new(UpstreamManager::new(config.servers));
    // One explicit --server keeps the M9 shape: a pure pipe with untouched
    // tool names. Everything else aggregates under `server__tool`.
    let (gateway, serving) = match selected.as_slice() {
        [single] if !args.server.is_empty() => (
            Gateway::new(Arc::clone(&manager), single.clone()),
            format!("piping {single:?}"),
        ),
        many => (
            Gateway::aggregate(Arc::clone(&manager), selected.clone()),
            format!("aggregating {} server(s): {}", many.len(), many.join(", ")),
        ),
    };

    let capture_note = match &capture {
        Some(writer) => format!("capturing traffic to {}", writer.dir().display()),
        None => "traffic capture disabled (--no-capture)".to_owned(),
    };
    let gateway = match &capture {
        Some(writer) => gateway.with_capture(Arc::clone(writer)),
        None => gateway,
    };

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind((args.bind.as_str(), args.port))
            .await
            .with_context(|| format!("cannot bind {}:{}", args.bind, args.port))?;
        let addr = listener.local_addr()?;
        println!("mcpgw gateway listening on http://{addr}/mcp — {serving}");
        println!("{capture_note}");

        let shutdown = async {
            let _ = tokio::signal::ctrl_c().await;
            println!("\nshutting down");
        };
        serve_http(gateway, listener, shutdown).await?;
        // Ctrl-C fell through the graceful shutdown: kill the children too.
        manager.shutdown().await;
        Ok(())
    })
}

fn is_loopback(bind: &str) -> bool {
    bind == "localhost"
        || bind
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}
