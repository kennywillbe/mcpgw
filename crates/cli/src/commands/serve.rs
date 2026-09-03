use std::sync::Arc;

use anyhow::{Context as _, bail};
use mcpgw_core::Config;
use mcpgw_core::capture::CaptureWriter;
use mcpgw_core::endpoints::{EndpointTable, Endpoints, endpoint_path};
use mcpgw_core::gateway::{Gateway, ServerList, serve_http_with};
use mcpgw_core::reload::{POLL_INTERVAL, Reloader};
use mcpgw_core::runtime::GatewayRecord;
use mcpgw_core::upgrade::{self, UpgradeRestart};
use mcpgw_core::upstream::UpstreamManager;

/// How long the gateway keeps draining before it stands aside for a new
/// binary. The point of the restart is to be over quickly, and an SSE stream
/// a client is parked on would otherwise hold the old build up for as long
/// as that client cares to stay.
const UPGRADE_DRAIN: std::time::Duration = std::time::Duration::from_secs(5);

/// How long a supervised gateway waits before its first release check, and
/// how often it asks again afterwards whether the day has turned.
///
/// Not at boot: a machine that has just logged in — or has just restarted
/// this service onto a new binary — is busy with things somebody is waiting
/// for, and nobody is waiting for this. The hourly poll only decides how
/// soon a gateway that has been up for weeks notices midnight; how often it
/// actually reaches the network is the stamp's business, and the stamp says
/// once a day for the CLI and the daemon together.
// Spelled in seconds for the reason `update::notice::INTERVAL` gives: the
// larger-unit constructors clippy asks for here are too new to name in a
// workspace that pins no rust-version.
#[allow(clippy::duration_suboptimal_units)]
const FIRST_UPDATE_CHECK: std::time::Duration = std::time::Duration::from_secs(5 * 60);
#[allow(clippy::duration_suboptimal_units)]
const UPDATE_CHECK_POLL: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Brings the first check forward, in milliseconds. A test seam, and
/// debug-only like `MCPGW_UPDATE_BASE_URL`: the suite has to watch a gateway
/// do in a moment what a real one does after five minutes, and nothing
/// outside the suite has any business rescheduling it.
#[cfg(debug_assertions)]
const FIRST_UPDATE_CHECK_ENV: &str = "MCPGW_UPDATE_FIRST_CHECK_MS";

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
    /// Set by `mcpgw daemon install` in the service definition, never by
    /// hand: it makes the gateway end itself when its own binary is
    /// upgraded, which is only safe under something that will start it
    /// again. See `mcpgw_core::upgrade`.
    #[arg(long, hide = true)]
    pub supervised: bool,
}

pub fn run(args: &ServeArgs) -> anyhow::Result<()> {
    let config_path = super::canonical_config_path()?;
    let config = Config::load(&config_path)?;

    let selected = select(args, &config, &config_path)?;

    warn_if_reachable(&args.bind);
    let (capture, capture_note) = capture_writer(args.no_capture)?;

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
        // Published only once the listener is bound, so `--port 0` records
        // the port the kernel actually handed out rather than the zero it
        // asked for.
        let (state_dir, record) = publish_record(&args.bind, addr.port(), args.supervised).unzip();
        print_banner(addr, pipe, &serving, &capture_note);

        // `notify_one` rather than `notify_waiters`: it leaves a permit
        // behind, so the watcher is stopped even if Ctrl-C lands while it is
        // between two `select!` registrations. One notify per watcher, since
        // a permit is handed to a single waiter.
        let stop = Arc::new(tokio::sync::Notify::new());
        let stop_upgrades = Arc::new(tokio::sync::Notify::new());
        let watcher = tokio::spawn({
            let stop = Arc::clone(&stop);
            async move {
                reloader
                    .watch(POLL_INTERVAL, async move { stop.notified().await })
                    .await;
            }
        });

        let (decision, decided) = tokio::sync::watch::channel(None);
        let exe_watcher = args.supervised.then(|| {
            watch_for_upgrades(
                state_dir.as_deref(),
                record.as_ref().and_then(|r| r.last_upgrade_restart.clone()),
                Arc::clone(&stop_upgrades),
                decision,
            )
        });

        // Next to the upgrade watcher, and gated on the same flag for the
        // same reason: only a service nobody is looking at needs to find
        // out about a release on its own. A foreground `serve` is a command
        // in a terminal, and the notice after that command covers it.
        watch_for_releases(args.supervised, state_dir.as_deref());

        let shutdown = shutdown_signal(
            Arc::clone(&stop),
            Arc::clone(&stop_upgrades),
            decided.clone(),
        );
        let mut served = std::pin::pin!(serve_http_with(
            gateway,
            Some(endpoints),
            listener,
            shutdown
        ));
        // Only the upgrade path is ever cut short: the drain cannot resolve
        // before the watcher has decided, and a Ctrl-C shutdown is allowed
        // to take as long as its clients need.
        let served = tokio::select! {
            result = &mut served => Some(result),
            () = drain_for_an_upgrade(decided.clone()) => None,
        };
        // Covers the paths Ctrl-C did not take (a bind that dies under us):
        // neither watcher may outlive the gateway it watches for.
        stop.notify_one();
        stop_upgrades.notify_one();
        let _ = watcher.await;
        if let Some(exe_watcher) = exe_watcher {
            let _ = exe_watcher.await;
        }

        let restart = decided.borrow().clone();
        if let Some(restart) = restart {
            stand_aside(restart, state_dir.as_deref(), record, &manager).await;
        }

        // Withdrawn before the error is propagated: this gateway is gone
        // either way, and a record left behind by a bind that died under us
        // is exactly the stale one readers should not have to reason about.
        if let Some(dir) = &state_dir {
            mcpgw_core::runtime::remove_record(dir, addr.port());
        }
        if let Some(served) = served {
            served?;
        }
        // Ctrl-C fell through the graceful shutdown: kill the children too.
        manager.shutdown().await;
        Ok(())
    })
}

/// Says once, loudly, that this gateway is reachable by other people.
///
/// The same classification `mcpgw daemon` refuses to install past, so a bind
/// that only warns here can never quietly become one that passes there.
fn warn_if_reachable(bind: &str) {
    if !mcpgw_core::daemon::is_loopback(bind) {
        eprintln!(
            "warning: binding to {bind} without any authentication — anyone who can reach \
             this address can call your MCP servers; keep it behind a trusted network \
             or reverse proxy until the auth milestone lands"
        );
    }
}

/// The traffic log this gateway writes, with the banner line that says where
/// it goes — or that there is none.
fn capture_writer(disabled: bool) -> anyhow::Result<(Option<Arc<CaptureWriter>>, String)> {
    if disabled {
        return Ok((None, "traffic capture disabled (--no-capture)".to_owned()));
    }
    let state_dir = mcpgw_core::paths::state_dir()
        .context("cannot determine a home directory to resolve the state directory")?;
    let writer = CaptureWriter::under_state_dir(&state_dir);
    let note = format!("capturing traffic to {}", writer.dir().display());
    Ok((Some(Arc::new(writer)), note))
}

/// The three lines a gateway prints once it is listening.
fn print_banner(addr: std::net::SocketAddr, pipe: bool, serving: &[String], capture_note: &str) {
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
}

/// Starts the watcher that ends this gateway when the binary underneath it
/// is replaced, publishing its verdict through `decision`.
fn watch_for_upgrades(
    state_dir: Option<&std::path::Path>,
    guard: Option<UpgradeRestart>,
    stop: Arc<tokio::sync::Notify>,
    decision: tokio::sync::watch::Sender<Option<UpgradeRestart>>,
) -> tokio::task::JoinHandle<()> {
    let exe = upgrade::watched_exe(state_dir).unwrap_or_default();
    let watcher = upgrade::Watcher::new(exe, upgrade::stamp).with_guard(guard);
    // On stderr, which is where the daemon's log is: it is the one line that
    // says which of two plausible paths — the installed service's or this
    // process's own image — an upgrade has to land on to be noticed.
    eprintln!(
        "watching {} for an upgrade; a new binary there restarts this gateway",
        watcher.path().display()
    );
    tokio::spawn(async move {
        let ended = watcher
            .watch(upgrade::POLL_INTERVAL, async move { stop.notified().await })
            .await;
        if let Some(restart) = ended {
            let _ = decision.send(Some(restart));
        }
    })
}

/// Starts the daily release check a supervised gateway does on its own
/// behalf, so a machine whose owner never types `mcpgw` still learns that a
/// newer one exists.
///
/// It only writes the stamp the CLI already reads, which is what makes
/// `daemon status`, `doctor` and the wizard able to say "0.6.0 is
/// available" without a request of their own. Nothing is downloaded and
/// nothing restarts: the daemon learns, the human decides.
///
/// Detached rather than stopped like the two watchers above, because it
/// holds nothing the shutdown path has to reclaim — no listener, no child,
/// no record — and it is dropped with the runtime a moment later. The only
/// thing it can delay is a shutdown that lands inside a check, which the
/// lookup's own two-second deadline bounds.
fn watch_for_releases(supervised: bool, state_dir: Option<&std::path::Path>) {
    let Some(state_dir) = state_dir
        .filter(|_| supervised)
        .map(std::path::Path::to_path_buf)
    else {
        return;
    };
    tokio::spawn(async move {
        tokio::time::sleep(first_update_check()).await;
        let mut announced: Option<String> = None;
        loop {
            let dir = state_dir.clone();
            // The lookup is a blocking HTTP call; the runtime's threads are
            // for serving MCP traffic, not for waiting on github.com.
            let latest =
                tokio::task::spawn_blocking(move || crate::update::notice::check_and_stamp(&dir))
                    .await
                    .ok()
                    .flatten();
            let current = env!("CARGO_PKG_VERSION");
            if let Some(latest) = latest
                && announced.as_deref() != Some(latest.as_str())
                && crate::update::is_newer(&latest, current)
            {
                // The one thing this task ever says, and it says it once per
                // release: a daemon log nobody reads daily is no place for a
                // line that repeats.
                eprintln!(
                    "mcpgw {latest} is available (this gateway is running {current}) — \
                     run `mcpgw self-update`"
                );
                announced = Some(latest);
            }
            tokio::time::sleep(UPDATE_CHECK_POLL).await;
        }
    });
}

/// How long to wait for the first check — see [`FIRST_UPDATE_CHECK`].
fn first_update_check() -> std::time::Duration {
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var(FIRST_UPDATE_CHECK_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
    {
        return std::time::Duration::from_millis(ms);
    }
    FIRST_UPDATE_CHECK
}

/// Ends this process so its supervisor starts the binary that replaced it.
///
/// The runtime record is left in place rather than withdrawn: another gateway
/// is about to come up on this port within seconds, and the restart written
/// into the record here is the only thing that will stop *that* one from
/// standing aside for the same binary all over again.
async fn stand_aside(
    restart: UpgradeRestart,
    state_dir: Option<&std::path::Path>,
    record: Option<GatewayRecord>,
    manager: &UpstreamManager,
) -> ! {
    if let (Some(dir), Some(mut record)) = (state_dir, record) {
        record.last_upgrade_restart = Some(restart);
        if let Err(err) = mcpgw_core::runtime::write_record(dir, &record) {
            eprintln!(
                "warning: could not record this restart, so the gateway that replaces this one \
                 may restart for the same binary again: {:#}",
                anyhow::Error::from(err)
            );
        }
    }
    // Bounded like the drain, and for the same reason: a stdio server that
    // will not die must not keep the old binary running.
    let _ = tokio::time::timeout(UPGRADE_DRAIN, manager.shutdown()).await;
    std::process::exit(i32::from(upgrade::UPGRADE_EXIT));
}

/// How long the gateway keeps serving after the exe watcher has decided it
/// must stand aside: the clients mid-request get their answers, and the ones
/// parked on a stream do not get to hold the old binary up.
async fn drain_for_an_upgrade(mut decided: tokio::sync::watch::Receiver<Option<UpgradeRestart>>) {
    upgrade_decided(&mut decided).await;
    tokio::time::sleep(UPGRADE_DRAIN).await;
}

/// What the HTTP server drains on: Ctrl-C, or the exe watcher deciding this
/// gateway must stand aside for the binary that replaced it. Either way both
/// watchers are stopped on the way out.
async fn shutdown_signal(
    stop: Arc<tokio::sync::Notify>,
    stop_upgrades: Arc<tokio::sync::Notify>,
    mut decided: tokio::sync::watch::Receiver<Option<UpgradeRestart>>,
) {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => println!("\nshutting down"),
        // Announced by the watcher already, which is the only party that
        // can say which binary changed.
        () = upgrade_decided(&mut decided) => {}
    }
    stop.notify_one();
    stop_upgrades.notify_one();
}

/// Resolves once the exe watcher has decided this gateway must stand aside.
///
/// A channel with no senders left — which is every run without
/// `--supervised`, where the watcher was never spawned — never resolves.
/// That is the whole difference the flag makes at this end.
async fn upgrade_decided(upgraded: &mut tokio::sync::watch::Receiver<Option<UpgradeRestart>>) {
    if upgraded.wait_for(Option::is_some).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Publishes what this gateway is, returning the state directory it went
/// into — so the shutdown path can withdraw the record again — and the
/// record itself, which the upgrade path amends rather than rebuilds.
///
/// A state directory that cannot be written costs a later `status` its
/// version comparison; it is not worth costing the user their gateway, so
/// the failure is said once and serving continues.
fn publish_record(
    bind: &str,
    port: u16,
    supervised: bool,
) -> Option<(std::path::PathBuf, GatewayRecord)> {
    let dir = mcpgw_core::paths::state_dir()?;
    let mut record = runtime_record(bind, port);
    // Read before it is overwritten. A supervised gateway is very often the
    // successor of one that stood aside for an upgrade, and the restart that
    // predecessor recorded is the only reason this one will not do the same
    // thing to the same binary. A foreground `serve` on the same port
    // inherits nothing: it never restarts itself, so a guard it dragged
    // forward would only go stale.
    if supervised {
        record.last_upgrade_restart = mcpgw_core::runtime::read_record(&dir, port)
            .ok()
            .flatten()
            .and_then(|previous| previous.last_upgrade_restart);
    }
    if let Err(err) = mcpgw_core::runtime::write_record(&dir, &record) {
        eprintln!(
            "warning: could not record what this gateway is running: {:#}",
            anyhow::Error::from(err)
        );
    }
    Some((dir, record))
}

/// What this process publishes about itself while it is serving.
fn runtime_record(bind: &str, port: u16) -> GatewayRecord {
    // Canonicalized so that "is the binary on disk still the one running?"
    // compares two real paths rather than a symlink against its target. The
    // raw path is kept when it cannot be: on unix the running image may
    // already have been replaced or unlinked, and a path that no longer
    // resolves is still better evidence than none.
    let exe = std::env::current_exe().unwrap_or_default();
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    GatewayRecord {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        pid: std::process::id(),
        exe,
        bind: bind.to_owned(),
        port,
        // A clock behind the epoch is not worth a failure path; the field is
        // only ever used to say how long the gateway has been up.
        started_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs()),
        // Filled in by `publish_record` from what the previous gateway on
        // this port left behind, if anything did.
        last_upgrade_restart: None,
    }
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
