use std::sync::Arc;

use anyhow::{Context as _, bail};
use mcpgw_core::Config;
use mcpgw_core::capture::{Bodies, CapturePolicy, CaptureWriter, RedactionRules};
use mcpgw_core::endpoints::{EndpointTable, Endpoints, endpoint_path};
use mcpgw_core::gateway::{GatewayAuth, serve_http_with};
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

/// The `--capture-bodies` values, spelled here rather than derived on
/// `mcpgw_core::capture::Bodies` so the core crate stays free of clap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum CaptureBodies {
    Off,
    Redacted,
    Full,
}

impl From<CaptureBodies> for Bodies {
    fn from(bodies: CaptureBodies) -> Self {
        match bodies {
            CaptureBodies::Off => Bodies::Off,
            CaptureBodies::Redacted => Bodies::Redacted,
            CaptureBodies::Full => Bodies::Full,
        }
    }
}

#[derive(clap::Args)]
pub struct ServeArgs {
    /// Port to listen on
    #[arg(long, default_value_t = 8137)]
    pub port: u16,
    /// Address to bind (loopback by default; anything else is unauthenticated!)
    #[arg(long, default_value = "127.0.0.1")]
    pub bind: String,
    /// Serve only these servers (repeatable; default: every enabled
    /// server).
    #[arg(long, value_name = "NAME")]
    pub server: Vec<String>,
    /// Accepted and ignored: per-server endpoints are always served. Kept
    /// for one release so existing scripts keep running.
    #[arg(long, hide = true)]
    pub per_server: bool,
    /// Do not write the JSONL traffic log
    #[arg(long)]
    pub no_capture: bool,
    /// How much of each request to keep in the traffic log: metadata only,
    /// bodies with credentials replaced, or bodies verbatim
    #[arg(long, value_enum, default_value = "redacted", value_name = "MODE")]
    pub capture_bodies: CaptureBodies,
    /// Set by `mcpgw daemon install` in the service definition, never by
    /// hand: it makes the gateway end itself when its own binary is
    /// upgraded, which is only safe under something that will start it
    /// again. See `mcpgw_core::upgrade`.
    #[arg(long, hide = true)]
    pub supervised: bool,
}

pub fn run(args: &ServeArgs) -> anyhow::Result<()> {
    let config_path = super::canonical_config_path()?;
    let (config, unknown) = Config::load_reporting(&config_path)?;
    // Before anything else the gateway prints: a key it does not recognize
    // is usually a restriction that is not being applied, and the operator
    // has to see that above the banner, not buried under it.
    for key in &unknown {
        eprintln!("warning: {}", key.message());
    }

    let selected = select(args, &config, &config_path)?;

    // Before the listener, because a token that cannot be written is a
    // gateway nobody can be synced to, and finding that out after the banner
    // would be finding it out in a log.
    let auth = gateway_auth(&config)?;
    warn_if_reachable(&args.bind, &auth);
    let Built {
        manager,
        endpoints,
        reloader,
        capture_note,
        capture,
    } = build(
        config_path,
        &args.server,
        &selected,
        capture_policy(args.no_capture, args.capture_bodies.into(), &config)?,
        config.capture.retain_days,
    )?;

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        // Before the first request can reach it: from here on an append is a
        // queue push, and nothing on the request path waits on the disk the
        // state directory lives on.
        if let Some(writer) = &capture {
            writer.offload();
        }
        let serving = reloader.apply(config).await.serving;
        let listener = tokio::net::TcpListener::bind((args.bind.as_str(), args.port))
            .await
            .with_context(|| format!("cannot bind {}:{}", args.bind, args.port))?;
        let addr = listener.local_addr()?;
        // Published only once the listener is bound, so `--port 0` records
        // the port the kernel actually handed out rather than the zero it
        // asked for.
        let (state_dir, record) = publish_record(&args.bind, addr.port(), args.supervised).unzip();
        print_banner(addr, &serving, &capture_note, &auth.note);

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
        let mut served = std::pin::pin!(serve_http_with(endpoints, auth.auth, listener, shutdown));
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
        let standing_aside = restart.is_some();
        if let Some(restart) = restart {
            stand_aside(restart, state_dir.as_deref(), record, &manager).await;
        }

        // Withdrawn before the error is propagated: this gateway is gone
        // either way, and a record left behind by a bind that died under us
        // is exactly the stale one readers should not have to reason about.
        //
        // Never on the stand-aside, which has just written the record the
        // gateway replacing this one reads to find out which binary it has
        // already restarted for.
        if !standing_aside && let Some(dir) = &state_dir {
            mcpgw_core::runtime::remove_record(dir, addr.port());
        }
        // Before anything that can return early — or exit — because the
        // queue is the one place a record of traffic that already happened
        // is still only in memory.
        if let Some(writer) = &capture {
            writer.flush().await;
        }
        // The upgrade restart is a status the supervisor reads, not a return
        // value, so it is the one exit this function does not walk out of.
        // Taken here rather than in `stand_aside` so it comes after the
        // flush above, like every other way out of this gateway.
        if standing_aside {
            std::process::exit(i32::from(upgrade::UPGRADE_EXIT));
        }
        if let Some(served) = served {
            served?;
        }
        // The shutdown signal fell through the graceful drain: kill the
        // children too.
        manager.shutdown().await;
        Ok(())
    })
}

/// Everything a gateway needs before it can be handed a listener.
///
/// Named and returned as a whole so `connect` can raise the same gateway
/// in-process as `serve` does: a bridge that starts its own must serve what
/// the daemon would have served, down to the reload behaviour, or the two
/// ways of reaching mcpgw quietly become two products.
pub(crate) struct Built {
    pub(crate) manager: Arc<UpstreamManager>,
    pub(crate) endpoints: Endpoints,
    pub(crate) reloader: Reloader,
    pub(crate) capture_note: String,
    /// The traffic log, handed back as well as wired in: this is built
    /// outside any runtime, and the writer's own thread can only be started
    /// inside one — see [`CaptureWriter::offload`].
    pub(crate) capture: Option<Arc<CaptureWriter>>,
}

/// Builds that gateway. `selection` is what the user asked for by name (empty
/// means "every enabled server") and `selected` is what [`select`] resolved
/// it to.
pub(crate) fn build(
    config_path: std::path::PathBuf,
    selection: &[String],
    selected: &[String],
    capture: Option<CapturePolicy>,
    retain_days: u32,
) -> anyhow::Result<Built> {
    let (capture, capture_note) = capture_writer(capture, retain_days)?;

    // Started empty and filled by the first `apply` in the caller, so the
    // servers present at boot arrive through exactly the code path a reload
    // uses. One construction site means an added server cannot end up served
    // differently from one that was in the config all along.
    let mut manager = UpstreamManager::new(std::collections::BTreeMap::new());
    // Where `mcpgw auth login` left its tokens. Absent only on a machine with
    // no home directory to resolve, where there is nothing to have logged in
    // with either — an upstream then dials bare and answers `401`, which is
    // the state that names the login command.
    if let Some(state_dir) = mcpgw_core::paths::state_dir() {
        manager = manager.with_state_dir(state_dir);
    }
    let manager = Arc::new(manager);
    let endpoints = Endpoints::new(EndpointTable::new(Vec::new()));
    let mut reloader = Reloader::new(config_path, Arc::clone(&manager), endpoints.clone());
    if !selection.is_empty() {
        reloader = reloader.with_selection(selected.to_vec());
    }
    if let Some(writer) = &capture {
        reloader = reloader.with_capture(Arc::clone(writer));
    }
    // Independent of capture: the pin file is what `doctor` and
    // `mcpgw tools NAME pin --show` read, and a gateway started with
    // `--no-capture` still has to notice a server rewriting its tools.
    if let Some(dir) = mcpgw_core::paths::state_dir() {
        reloader = reloader.with_pins(Arc::new(mcpgw_core::pins::PinStore::under_state_dir(&dir)));
    }

    Ok(Built {
        manager,
        endpoints,
        reloader,
        capture_note,
        capture,
    })
}

/// The gateway's own credential, plus the banner line describing it.
struct Auth {
    auth: GatewayAuth,
    note: String,
}

/// Reads this install's token, minting one on the first `serve`, and decides
/// whether the grace period still applies.
///
/// A state directory that cannot be written costs the gateway its token
/// rather than its life: an mcpgw with nowhere to put one has nowhere to put
/// the OAuth credentials the token exists to protect either, so there is
/// nothing here worth refusing to serve over. It is said out loud.
fn gateway_auth(config: &Config) -> anyhow::Result<Auth> {
    let Some(state_dir) = mcpgw_core::paths::state_dir() else {
        return Ok(Auth {
            auth: GatewayAuth::open(),
            note: "no state directory: serving without a token".to_owned(),
        });
    };
    let (token, minted) = super::token::ensure(&state_dir)?;
    let require = config.gateway.require_token;
    let note = match (minted, require) {
        (true, _) => format!(
            "issued this install's gateway token ({}) — `mcpgw sync` writes it into your clients",
            token.masked()
        ),
        (false, true) => {
            "requiring the gateway token on every request ([gateway] require_token)".to_owned()
        }
        (false, false) => format!(
            "gateway token {} required; loopback clients without one still pass this release",
            token.masked()
        ),
    };
    Ok(Auth {
        auth: GatewayAuth::new(token, require),
        note,
    })
}

/// Says once, loudly, that this gateway is reachable by other people and
/// nothing stops them.
///
/// Still only a warning, and still only for a foreground `serve`: a person is
/// reading this terminal and can decide. What `mcpgw daemon` refuses to
/// install past is the same address with the same reasoning — see
/// [`BindPolicy`](mcpgw_core::gateway_token::BindPolicy) — so a bind that
/// warns here can never quietly become one that passes there.
fn warn_if_reachable(bind: &str, auth: &Auth) {
    if mcpgw_core::daemon::is_loopback(bind) {
        return;
    }
    if auth.auth.requires_token() {
        eprintln!(
            "warning: binding to {bind} — anyone who can reach this address and holds \
             this install's gateway token can call your MCP servers"
        );
        return;
    }
    eprintln!(
        "warning: binding to {bind} without any authentication — anyone who can reach \
         this address can call your MCP servers; set `[gateway] require_token = true` \
         and `mcpgw sync` your clients, or keep it behind a trusted network"
    );
}

/// What a gateway captures under, or [`None`] for no traffic log at all.
///
/// One construction site for both callers: `serve` reads the mode off its
/// flag and `connect`'s embedded gateway takes the default, so neither can
/// end up redacting by a different set of rules than the other.
///
/// The rules are read once, here. A `[capture] redact` edit therefore needs a
/// restart, unlike a server added to the config: the reload path hands the
/// running gateway a new upstream table, not a new writer, and swapping a
/// writer underneath a request would be a far bigger change than the one
/// thing it buys.
pub(crate) fn capture_policy(
    no_capture: bool,
    bodies: Bodies,
    config: &Config,
) -> anyhow::Result<Option<CapturePolicy>> {
    if no_capture {
        return Ok(None);
    }
    let rules = RedactionRules::compile(&config.capture.redact)?;
    Ok(Some(CapturePolicy::new(bodies, rules)))
}

/// The traffic log this gateway writes, with the banner line that says where
/// it goes and how much of each request it keeps — or that there is none.
fn capture_writer(
    policy: Option<CapturePolicy>,
    retain_days: u32,
) -> anyhow::Result<(Option<Arc<CaptureWriter>>, String)> {
    let Some(policy) = policy else {
        return Ok((None, "traffic capture disabled (--no-capture)".to_owned()));
    };
    let state_dir = mcpgw_core::paths::state_dir()
        .context("cannot determine a home directory to resolve the state directory")?;
    let bodies = policy.bodies();
    let writer = CaptureWriter::under_state_dir(&state_dir)
        .with_policy(policy)
        .with_retain_days(retain_days);
    // Once here, and again on every rotation from `append`. A gateway that
    // starts and then sits idle for a week still has to drop the days that
    // fell out of the window.
    writer.prune_if_due(mcpgw_core::capture::now_millis());
    let retention = if retain_days == 0 {
        "kept forever".to_owned()
    } else {
        format!("kept {retain_days} days")
    };
    let note = format!(
        "capturing traffic to {} (bodies: {bodies}, {retention})",
        writer.dir().display()
    );
    Ok((Some(Arc::new(writer)), note))
}

/// The four lines a gateway prints once it is listening.
fn print_banner(
    addr: std::net::SocketAddr,
    serving: &[String],
    capture_note: &str,
    auth_note: &str,
) {
    println!(
        "mcpgw gateway listening on http://{addr}/mcp — serving {} server(s): {}",
        serving.len(),
        serving.join(", ")
    );
    let urls: Vec<String> = serving
        .iter()
        .map(|name| format!("http://{addr}{}", endpoint_path(name)))
        .collect();
    println!("per-server endpoints: {}", urls.join(", "));
    println!("{capture_note}");
    println!("{auth_note}");
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

/// Readies this process to end so its supervisor starts the binary that
/// replaced it: the restart is recorded and the children are stopped, and
/// the caller does the exiting once it has flushed everything it holds.
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
) {
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
}

/// How long the gateway keeps serving after the exe watcher has decided it
/// must stand aside: the clients mid-request get their answers, and the ones
/// parked on a stream do not get to hold the old binary up.
async fn drain_for_an_upgrade(mut decided: tokio::sync::watch::Receiver<Option<UpgradeRestart>>) {
    upgrade_decided(&mut decided).await;
    tokio::time::sleep(UPGRADE_DRAIN).await;
}

/// What the HTTP server drains on: Ctrl-C, the stop a supervisor sends, or
/// the exe watcher deciding this gateway must stand aside for the binary that
/// replaced it. Either way both watchers are stopped on the way out.
async fn shutdown_signal(
    stop: Arc<tokio::sync::Notify>,
    stop_upgrades: Arc<tokio::sync::Notify>,
    mut decided: tokio::sync::watch::Receiver<Option<UpgradeRestart>>,
) {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => println!("\nshutting down"),
        // The stop every supervisor sends. Its default disposition would end
        // the process where it stands, which is the crash path: no drain, no
        // children killed, and a runtime record left claiming this gateway is
        // still up.
        () = terminated() => println!("shutting down"),
        // Announced by the watcher already, which is the only party that
        // can say which binary changed.
        () = upgrade_decided(&mut decided) => {}
    }
    stop.notify_one();
    stop_upgrades.notify_one();
}

/// Resolves on SIGTERM.
///
/// Never resolves off Unix, where there is no such signal to catch: Windows
/// stops a service by other means, and the arm being unreachable there is
/// what keeps the `select!` above free of a platform split.
async fn terminated() {
    #[cfg(unix)]
    {
        // A handler that cannot be installed is not worth ending the gateway
        // over; the arm simply never fires, which is where it started.
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut term) => {
                term.recv().await;
            }
            Err(err) => {
                eprintln!("warning: this gateway cannot be stopped gracefully: {err:#}");
                std::future::pending::<()>().await;
            }
        }
    }
    #[cfg(not(unix))]
    std::future::pending::<()>().await;
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
pub(crate) fn publish_record(
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

/// Every enabled server in `config`, which is what a gateway serves when
/// nobody named one — `serve` without `--server`, and the gateway `connect`
/// raises for itself.
///
/// # Errors
///
/// Fails when there is nothing to serve: a gateway with no servers answers
/// every client with an empty tool list, which is worse than a refusal that
/// names the file.
pub(crate) fn enabled_servers(
    config: &Config,
    path: &std::path::Path,
) -> anyhow::Result<Vec<String>> {
    let enabled: Vec<String> = config
        .servers
        .iter()
        .filter(|(_, server)| server.enabled)
        .map(|(name, _)| name.clone())
        .collect();
    if enabled.is_empty() {
        bail!("no enabled servers in {}", path.display());
    }
    Ok(enabled)
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
        return enabled_servers(config, path);
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
