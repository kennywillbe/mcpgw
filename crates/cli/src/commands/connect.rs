use std::collections::BTreeMap;
use std::sync::Arc;

use mcpgw_core::gateway::{Gateway, serve_stdio};
use mcpgw_core::upstream::UpstreamManager;
use mcpgw_core::{Server, Transport};

/// Where `mcpgw serve` listens with its own defaults.
pub const DEFAULT_URL: &str = "http://127.0.0.1:8137/mcp";

/// Name of the single synthetic upstream. It never reaches tool names — the
/// bridge is a pure pipe — but it does appear in upstream error messages.
const UPSTREAM: &str = "gateway";

#[derive(clap::Args)]
pub struct ConnectArgs {
    /// URL of the running gateway
    #[arg(long, default_value = DEFAULT_URL, value_name = "URL")]
    pub url: String,
}

pub fn run(args: &ConnectArgs) -> anyhow::Result<()> {
    let server = Server {
        enabled: true,
        tags: Vec::new(),
        transport: Transport::Http {
            url: args.url.clone(),
            headers: BTreeMap::new(),
        },
    };
    let manager = Arc::new(UpstreamManager::new(BTreeMap::from([(
        UPSTREAM.to_owned(),
        server,
    )])));
    // The one failure mode worth naming here is "the daemon isn't up", and
    // the client only ever shows the MCP error text — so the fix goes in it.
    let gateway =
        Gateway::new(Arc::clone(&manager), UPSTREAM.to_owned()).with_unavailable_hint(format!(
            "gateway is not running at {} — start it with `mcpgw serve`",
            args.url
        ));

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        // stdout is the MCP transport from here on; diagnostics go to stderr.
        eprintln!("mcpgw connect: bridging stdio to {}", args.url);
        let reason = serve_stdio(gateway).await?;
        eprintln!("mcpgw connect: closed ({reason:?})");
        manager.shutdown().await;
        Ok(())
    })
}
