use std::collections::BTreeMap;
use std::sync::Arc;

use mcpgw_core::gateway::{Gateway, serve_stdio};
use mcpgw_core::upstream::UpstreamManager;
use mcpgw_core::{Server, Transport};

/// Where `mcpgw serve` listens with its own defaults.
pub use mcpgw_core::endpoints::DEFAULT_URL;

/// Name of the single synthetic upstream. It never reaches tool names — the
/// bridge is a pure pipe — but it does appear in upstream error messages.
const UPSTREAM: &str = "gateway";

#[derive(clap::Args)]
pub struct ConnectArgs {
    /// URL of the running gateway. Optional rather than defaulted so
    /// `--server` can tell "the user picked this URL" from "nobody did".
    // The default is spelled out in the help text by hand for the same reason
    // — clap only prints one for an argument that actually has one.
    #[arg(
        long,
        value_name = "URL",
        help = "URL of the running gateway [default: http://127.0.0.1:8137/mcp]"
    )]
    pub url: Option<String>,
    /// Bridge to one server's own endpoint instead of the aggregate, so its
    /// tools arrive unprefixed
    #[arg(long, value_name = "NAME")]
    pub server: Option<String>,
}

/// The gateway URL to bridge to. `--url` alone is taken verbatim: it is the
/// escape hatch for a gateway on another port or path. Alongside `--server` it
/// says where the *gateway* is and the server's endpoint is resolved on it —
/// which is the shape `sync --gateway` writes into stdio-only clients, so the
/// name in the file stays the server's rather than a hard-coded path.
fn target_url(args: &ConnectArgs) -> anyhow::Result<String> {
    match (&args.url, &args.server) {
        (Some(url), Some(name)) => Ok(mcpgw_core::endpoints::per_server_url(url, name)?),
        (Some(url), None) => Ok(url.clone()),
        (None, Some(name)) => Ok(mcpgw_core::endpoints::per_server_url(DEFAULT_URL, name)?),
        (None, None) => Ok(DEFAULT_URL.to_owned()),
    }
}

pub fn run(args: &ConnectArgs) -> anyhow::Result<()> {
    let url = target_url(args)?;
    let server = Server {
        enabled: true,
        tags: Vec::new(),
        transport: Transport::Http {
            url: url.clone(),
            headers: BTreeMap::new(),
        },
    };
    let manager = Arc::new(UpstreamManager::new(BTreeMap::from([(
        UPSTREAM.to_owned(),
        server,
    )])));
    // The one failure mode worth naming here is "the daemon isn't up", and
    // the client only ever shows the MCP error text — so the fix goes in it.
    let gateway = Gateway::new(Arc::clone(&manager), UPSTREAM.to_owned()).with_unavailable_hint(
        format!("gateway is not running at {url} — start it with `mcpgw serve`"),
    );

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        // stdout is the MCP transport from here on; diagnostics go to stderr.
        eprintln!("mcpgw connect: bridging stdio to {url}");
        let reason = serve_stdio(gateway).await?;
        eprintln!("mcpgw connect: closed ({reason:?})");
        manager.shutdown().await;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::{ConnectArgs, DEFAULT_URL, target_url};

    fn args(url: Option<&str>, server: Option<&str>) -> ConnectArgs {
        ConnectArgs {
            url: url.map(ToOwned::to_owned),
            server: server.map(ToOwned::to_owned),
        }
    }

    #[test]
    fn server_names_the_per_server_endpoint_and_url_still_wins() {
        assert_eq!(target_url(&args(None, None)).unwrap(), DEFAULT_URL);
        assert_eq!(
            target_url(&args(None, Some("github"))).unwrap(),
            "http://127.0.0.1:8137/s/github"
        );
        // An explicit URL alone is never rewritten; with --server it is the
        // gateway the endpoint is resolved on.
        let explicit = "http://127.0.0.1:9000/mcp";
        assert_eq!(target_url(&args(Some(explicit), None)).unwrap(), explicit);
        assert_eq!(
            target_url(&args(Some(explicit), Some("github"))).unwrap(),
            "http://127.0.0.1:9000/s/github"
        );
    }
}
