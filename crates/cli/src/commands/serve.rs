use std::sync::Arc;

use anyhow::{Context as _, bail};
use mcpgw_core::gateway::{Gateway, serve_http};
use mcpgw_core::upstream::UpstreamManager;
use mcpgw_core::{Config, Transport};

#[derive(clap::Args)]
pub struct ServeArgs {
    /// Port to listen on
    #[arg(long, default_value_t = 8137)]
    pub port: u16,
    /// Address to bind (loopback by default; anything else is unauthenticated!)
    #[arg(long, default_value = "127.0.0.1")]
    pub bind: String,
    /// Which canonical server to pipe (optional when exactly one is enabled)
    #[arg(long, value_name = "NAME")]
    pub server: Option<String>,
}

pub fn run(args: &ServeArgs) -> anyhow::Result<()> {
    let config_path = super::canonical_config_path()?;
    let config = Config::load(&config_path)?;

    let enabled: Vec<&String> = config
        .servers
        .iter()
        .filter(|(_, s)| s.enabled && matches!(s.transport, Transport::Stdio { .. }))
        .map(|(name, _)| name)
        .collect();
    let upstream = match (&args.server, enabled.as_slice()) {
        (Some(name), _) => {
            let Some(server) = config.servers.get(name) else {
                bail!("no server named {name:?} in {}", config_path.display());
            };
            if !server.enabled {
                bail!("server {name:?} is disabled");
            }
            if !matches!(server.transport, Transport::Stdio { .. }) {
                bail!("server {name:?} is http; http upstreams arrive in a later milestone");
            }
            name.clone()
        }
        (None, [single]) => (*single).clone(),
        (None, []) => bail!("no enabled stdio servers in {}", config_path.display()),
        (None, many) => bail!(
            "multiple enabled servers ({}); pick one with --server until aggregation lands",
            many.iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };

    if !is_loopback(&args.bind) {
        eprintln!(
            "warning: binding to {} without any authentication — anyone who can reach \
             this address can call your MCP servers; keep it behind a trusted network \
             or reverse proxy until the auth milestone lands",
            args.bind
        );
    }

    let manager = Arc::new(UpstreamManager::new(config.servers));
    let gateway = Gateway::new(Arc::clone(&manager), upstream.clone());

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind((args.bind.as_str(), args.port))
            .await
            .with_context(|| format!("cannot bind {}:{}", args.bind, args.port))?;
        let addr = listener.local_addr()?;
        println!("mcpgw gateway listening on http://{addr}/mcp — piping {upstream:?}");

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
