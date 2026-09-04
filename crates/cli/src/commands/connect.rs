use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mcpgw_core::capture::Bodies;
use mcpgw_core::daemon::{GatewayReach, PROBE_TIMEOUT, is_loopback, probe_gateway};
use mcpgw_core::daemon_check::{url_host, url_port};
use mcpgw_core::gateway::{Gateway, serve_stdio};
use mcpgw_core::reload::POLL_INTERVAL;
use mcpgw_core::upstream::UpstreamManager;
use mcpgw_core::{Config, Server, Transport};

/// Where `mcpgw serve` listens with its own defaults.
pub use mcpgw_core::endpoints::DEFAULT_URL;

/// Name of the single synthetic upstream. It never reaches tool names — the
/// bridge is a pure pipe — but it does appear in upstream error messages.
const UPSTREAM: &str = "gateway";

/// How long the loser of a bind race waits for the winner to answer, and how
/// often it looks. Two clients launching their bridges together is the whole
/// reason this exists, and the winner is already listening by the time the
/// loser's bind fails — so this is slack for its first accept, not a wait for
/// anything slow.
const RACE_DEADLINE: Duration = Duration::from_secs(10);
const RACE_POLL: Duration = Duration::from_millis(25);

/// How long the embedded gateway is given to drain when the bridge's client
/// hangs up. The client is already gone and its own supervisor is waiting for
/// this process to leave, so a connection that will not close cannot be
/// allowed to hold the exit.
const DRAIN: Duration = Duration::from_secs(3);

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
    /// Bridge to one server's own endpoint instead of the gateway's `/mcp`,
    /// so its tools arrive unprefixed
    #[arg(long, value_name = "NAME")]
    pub server: Option<String>,
    /// Never serve a gateway of this bridge's own: fail the way the bridge
    /// failed before it could. Hidden because the answer to "I did not want
    /// that gateway" is `mcpgw daemon install`, not a flag; it exists for
    /// scripts that mean to assert a gateway is already up.
    #[arg(long, hide = true)]
    pub no_auto_start: bool,
}

/// The gateway URL to bridge to. `--url` alone is taken verbatim: it is the
/// escape hatch for a gateway on another port or path. Alongside `--server` it
/// says where the *gateway* is and the server's endpoint is resolved on it —
/// which is the shape `sync` writes into stdio-only clients, so the name in
/// the file stays the server's rather than a hard-coded path.
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
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        // stdout is the MCP transport from here on; diagnostics go to stderr.
        let startup = startup(&url, !args.no_auto_start).await;
        let gateway = bridge(&url, &startup);
        let manager = Arc::clone(gateway.manager());

        eprintln!("mcpgw connect: bridging stdio to {url}");
        // Held rather than propagated, so a handshake that failed still takes
        // the gateway this process raised down with it — and withdraws its
        // record, which is the one readers must not be left to reason about.
        let served = serve_stdio(gateway).await;
        if let Ok(reason) = &served {
            eprintln!("mcpgw connect: closed ({reason:?})");
        }
        // The bridge's own connection to the gateway goes first: the embedded
        // one below shuts down gracefully, and gracefully means "once the
        // clients have gone".
        manager.shutdown().await;
        if let Startup::Serving(embedded) = startup {
            embedded.shutdown().await;
        }
        served?;
        Ok(())
    })
}

/// What this bridge found at its target, and what it did about it.
enum Startup {
    /// Something answers there — or nothing does and nothing this process may
    /// do would help. Bridge, and let the upstream report what it finds.
    Bridge,
    /// Nothing answered, so this process is the gateway for as long as the
    /// bridge lives.
    Serving(Embedded),
    /// Nothing answered and a service is installed on that port. Starting a
    /// rival there would be a gateway the supervisor does not know about,
    /// serving until the client closes and then vanishing — so the bridge
    /// says who should be started instead.
    ServiceDown,
}

/// The bridge itself: a pipe with one synthetic upstream, the gateway at
/// `url`.
fn bridge(url: &str, startup: &Startup) -> Gateway {
    let server = Server {
        enabled: true,
        tags: Vec::new(),
        tools: None,
        transport: Transport::Http {
            url: url.to_owned(),
            headers_command: Vec::new(),
            headers: BTreeMap::new(),
        },
    };
    let manager = Arc::new(UpstreamManager::new(BTreeMap::from([(
        UPSTREAM.to_owned(),
        server,
    )])));
    // The one failure mode worth naming here is "the daemon isn't up", and
    // the client only ever shows the MCP error text — so the fix goes in it.
    let mut hint = format!(
        "gateway is not running at {url} — start it with `mcpgw daemon start` \
         (or `mcpgw serve` in a terminal)"
    );
    if matches!(startup, Startup::ServiceDown) {
        hint.push_str(SERVICE_DOWN);
    }
    Gateway::new(manager, UPSTREAM.to_owned()).with_unavailable_hint(hint)
}

/// The half-sentence added when a service is installed on the port and is not
/// answering. The bridge could have served that port itself; naming the
/// service is what stops the user fixing it twice.
const SERVICE_DOWN: &str = "; the installed service is not running";

/// Probes the target and, when nothing is there, decides whether this bridge
/// may serve one itself.
async fn startup(url: &str, auto_start: bool) -> Startup {
    // NotHttp is somebody else's port and Answering is a gateway: neither is
    // ours to take, and the first fails with the message it already had.
    if probe_gateway(url, PROBE_TIMEOUT).await != GatewayReach::Down {
        return Startup::Bridge;
    }
    let (Some(host), Some(port)) = (url_host(url), url_port(url)) else {
        return Startup::Bridge;
    };
    // A gateway on another machine is not this process's to start, and a
    // listener bound here would not be the one the URL names anyway.
    if !auto_start || !is_loopback(&host) {
        return Startup::Bridge;
    }
    let state_dir = mcpgw_core::paths::state_dir();
    let installed = state_dir
        .as_deref()
        .and_then(mcpgw_core::daemon::load_spec)
        .is_some_and(|spec| spec.port == port);
    if installed {
        eprintln!("mcpgw connect: no gateway at {url}{SERVICE_DOWN} — `mcpgw daemon start`");
        return Startup::ServiceDown;
    }

    match embed(&host, port).await {
        Ok(Some(embedded)) => {
            eprintln!(
                "mcpgw connect: no gateway at {url}; serving one for this session \
                 (install a service with `mcpgw daemon install` to keep it running)"
            );
            Startup::Serving(embedded)
        }
        // Somebody bound the port between the probe and here — the other
        // bridge a client started at the same moment. It is about to answer
        // for both of us.
        Ok(None) => {
            wait_for_the_winner(url).await;
            Startup::Bridge
        }
        // A missing config, no servers, a state directory that cannot be
        // written: all of them leave the bridge exactly as usable as it was
        // before, so none is worth failing the client's startup over.
        Err(err) => {
            eprintln!("mcpgw connect: cannot serve a gateway for this session: {err:#}");
            Startup::Bridge
        }
    }
}

/// Polls until the gateway that won the bind race answers, or the deadline
/// passes — in which case the bridge carries on and the upstream reports what
/// it finds, rather than this waiting any longer on a client's startup path.
async fn wait_for_the_winner(url: &str) {
    let deadline = tokio::time::Instant::now() + RACE_DEADLINE;
    loop {
        if probe_gateway(url, PROBE_TIMEOUT).await.is_up() {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(RACE_POLL).await;
    }
}

/// A gateway served by this process for the life of the bridge in front of it.
struct Embedded {
    stop_serving: Arc<tokio::sync::Notify>,
    stop_watching: Arc<tokio::sync::Notify>,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
    watcher: tokio::task::JoinHandle<()>,
    manager: Arc<UpstreamManager>,
    /// Where this gateway published its runtime record, and under which port,
    /// so the shutdown withdraws exactly the one it wrote.
    record: Option<(PathBuf, u16)>,
}

impl Embedded {
    /// Ends the gateway the way `serve` ends on Ctrl-C: drain, stop the
    /// config watcher, withdraw the record, then kill the stdio children.
    async fn shutdown(self) {
        // One permit per waiter, as in `serve`: `notify_waiters` would be
        // lost on a task that is between two registrations.
        self.stop_serving.notify_one();
        self.stop_watching.notify_one();
        let _ = tokio::time::timeout(DRAIN, self.server).await;
        let _ = tokio::time::timeout(DRAIN, self.watcher).await;
        if let Some((dir, port)) = &self.record {
            mcpgw_core::runtime::remove_record(dir, *port);
        }
        self.manager.shutdown().await;
    }
}

/// Raises a gateway on `host:port` through the construction path `serve`
/// uses.
///
/// [`None`] means the port was taken between the probe and the bind, which is
/// the racing-bridges case and not a failure.
///
/// Every configured server is served, `--server` notwithstanding: the second
/// client's bridge resolves its own endpoint on this gateway, and a gateway
/// serving one server would leave it with nothing.
///
/// # Errors
///
/// Anything that makes a gateway impossible here — no config, no enabled
/// servers, a bind that failed for its own reasons.
async fn embed(host: &str, port: u16) -> anyhow::Result<Option<Embedded>> {
    // Bound first, so the window two bridges can both pass through is a bind
    // and not a config load.
    let listener = match tokio::net::TcpListener::bind((host, port)).await {
        Ok(listener) => listener,
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let config_path = super::canonical_config_path()?;
    let config = Config::load(&config_path)?;
    let selected = super::serve::enabled_servers(&config, &config_path)?;
    // No selection: a face per server under `/s/<name>`, exactly as a plain
    // `mcpgw serve` would. Capture is on for the same reason it is there — a
    // session nobody logged is a session `mcpgw watch` cannot explain.
    //
    // Redacted, with no flag to say otherwise. A `--capture-bodies` on the
    // bridge would do nothing at all in the usual case, where a gateway is
    // already running and this branch is never reached; somebody who wants
    // the bodies verbatim wants them from the gateway that serves every
    // session, which is `mcpgw serve --capture-bodies full`.
    let capture = super::serve::capture_policy(false, Bodies::Redacted, &config)?;
    let built = super::serve::build(config_path, &[], &selected, capture)?;
    built.reloader.apply(config).await;

    // Published like `serve`'s, so `status`, `doctor` and a second bridge
    // read one kind of record whoever wrote it — and withdrawn again on the
    // way out, because this one really does end with the client.
    let record = super::serve::publish_record(host, port, false).map(|(dir, _)| (dir, port));

    let stop_serving = Arc::new(tokio::sync::Notify::new());
    let stop_watching = Arc::new(tokio::sync::Notify::new());
    let server = tokio::spawn({
        let stop = Arc::clone(&stop_serving);
        mcpgw_core::gateway::serve_http_with(built.endpoints, listener, async move {
            stop.notified().await;
        })
    });
    let watcher = tokio::spawn({
        let stop = Arc::clone(&stop_watching);
        let reloader = built.reloader;
        async move {
            reloader
                .watch(POLL_INTERVAL, async move { stop.notified().await })
                .await;
        }
    });

    Ok(Some(Embedded {
        stop_serving,
        stop_watching,
        server,
        watcher,
        manager: built.manager,
        record,
    }))
}

#[cfg(test)]
mod tests {
    use super::{ConnectArgs, DEFAULT_URL, target_url};

    fn args(url: Option<&str>, server: Option<&str>) -> ConnectArgs {
        ConnectArgs {
            url: url.map(ToOwned::to_owned),
            server: server.map(ToOwned::to_owned),
            no_auto_start: false,
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
