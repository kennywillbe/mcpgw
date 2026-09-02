use std::sync::Arc;

use anyhow::{Context as _, bail};
use mcpgw_core::Config;
use mcpgw_core::capture::CaptureWriter;
use mcpgw_core::endpoints::{EndpointTable, Endpoints, endpoint_path};
use mcpgw_core::gateway::{Gateway, ServerList, serve_http_with};
use mcpgw_core::reload::{POLL_INTERVAL, Reloader};
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
    /// Accepted and ignored: per-server endpoints are always served. Kept
    /// for one release so existing scripts keep running.
    #[arg(long, hide = true)]
    pub per_server: bool,
    /// Do not write the JSONL traffic log
    #[arg(long)]
    pub no_capture: bool,
}

pub fn run(args: &ServeArgs) -> anyhow::Result<()> {
    let config_path = super::canonical_config_path()?;
    let config = Config::load(&config_path)?;

    let selected = select(args, &config, &config_path)?;

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
    let capture_note = match &capture {
        Some(writer) => format!("capturing traffic to {}", writer.dir().display()),
        None => "traffic capture disabled (--no-capture)".to_owned(),
    };

    // Started empty and filled by the first `apply` below, so the servers
    // present at boot arrive through exactly the code path a reload uses.
    // One construction site means an added server cannot end up served
    // differently from one that was in the config all along.
    let manager = Arc::new(UpstreamManager::new(std::collections::BTreeMap::new()));
    let endpoints = Endpoints::new(EndpointTable::new(Vec::new()));
    let mut reloader = Reloader::new(config_path, Arc::clone(&manager), endpoints.clone());
    if !args.server.is_empty() {
        reloader = reloader.with_selection(selected.clone());
    }
    if let Some(writer) = &capture {
        reloader = reloader.with_capture(Arc::clone(writer));
    }

    // One explicit --server keeps the M9 shape: a pure pipe with untouched
    // tool names. Everything else aggregates under `server__tool`.
    let (gateway, pipe) = match selected.as_slice() {
        [single] if !args.server.is_empty() => {
            (Gateway::new(Arc::clone(&manager), single.clone()), true)
        }
        _ => {
            // The aggregate's list is shared with the reloader rather than
            // fixed here: a server added later has to appear on `/mcp` too,
            // and the `/mcp` service is built once, for the whole process.
            let servers = ServerList::new(Vec::new());
            reloader = reloader.with_aggregate(servers.clone());
            (
                Gateway::aggregate_shared(Arc::clone(&manager), servers),
                false,
            )
        }
    };
    let gateway = match &capture {
        Some(writer) => gateway.with_capture(Arc::clone(writer)),
        None => gateway,
    };

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let serving = reloader.apply(config).await.serving;
        let listener = tokio::net::TcpListener::bind((args.bind.as_str(), args.port))
            .await
            .with_context(|| format!("cannot bind {}:{}", args.bind, args.port))?;
        let addr = listener.local_addr()?;
        let shape = if pipe {
            format!("piping {:?}", serving.join(", "))
        } else {
            format!(
                "aggregating {} server(s): {}",
                serving.len(),
                serving.join(", ")
            )
        };
        println!("mcpgw gateway listening on http://{addr}/mcp — {shape}");
        let urls: Vec<String> = serving
            .iter()
            .map(|name| format!("http://{addr}{}", endpoint_path(name)))
            .collect();
        println!("per-server endpoints: {}", urls.join(", "));
        println!("{capture_note}");

        // `notify_one` rather than `notify_waiters`: it leaves a permit
        // behind, so the watcher is stopped even if Ctrl-C lands while it is
        // between two `select!` registrations.
        let stop = Arc::new(tokio::sync::Notify::new());
        let watcher = tokio::spawn({
            let stop = Arc::clone(&stop);
            async move {
                reloader
                    .watch(POLL_INTERVAL, async move { stop.notified().await })
                    .await;
            }
        });

        let shutdown = {
            let stop = Arc::clone(&stop);
            async move {
                let _ = tokio::signal::ctrl_c().await;
                println!("\nshutting down");
                stop.notify_one();
            }
        };
        let served = serve_http_with(gateway, Some(endpoints), listener, shutdown).await;
        // Covers the paths Ctrl-C did not take (a bind that dies under us):
        // the watcher must not outlive the gateway it reloads.
        stop.notify_one();
        let _ = watcher.await;
        served?;
        // Ctrl-C fell through the graceful shutdown: kill the children too.
        manager.shutdown().await;
        Ok(())
    })
}

/// The servers to serve at startup, refusing the mistakes worth refusing.
///
/// Strict where a reload is lenient, and deliberately so: a typo in
/// `--server` at boot is a command nobody wants to run, while the same server
/// vanishing from the config an hour later is just an edit, and taking the
/// whole gateway down over it would be worse than serving what is left.
fn select(
    args: &ServeArgs,
    config: &Config,
    path: &std::path::Path,
) -> anyhow::Result<Vec<String>> {
    if args.server.is_empty() {
        let enabled: Vec<String> = config
            .servers
            .iter()
            .filter(|(_, server)| server.enabled)
            .map(|(name, _)| name.clone())
            .collect();
        if enabled.is_empty() {
            bail!("no enabled servers in {}", path.display());
        }
        return Ok(enabled);
    }
    for name in &args.server {
        let Some(server) = config.servers.get(name) else {
            bail!("no server named {name:?} in {}", path.display());
        };
        if !server.enabled {
            bail!("server {name:?} is disabled");
        }
    }
    Ok(args.server.clone())
}

fn is_loopback(bind: &str) -> bool {
    bind == "localhost"
        || bind
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}
