//! `mcpgw daemon`: run the gateway as a supervised background service.
//!
//! The per-OS installers land one at a time (see the contract in
//! [`mcpgw_core::daemon`]), so today `install`/`start`/`stop` report what
//! their platform still owes the user while `status` and `logs` are already
//! useful — the state a user is actually in right now is "a gateway I
//! started in a terminal", and that is precisely what `status` has to name.

use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use mcpgw_core::daemon::{
    DaemonError, DaemonSpec, GatewayReach, LogPaths, PROBE_TIMEOUT, PortPolicy,
    ServiceManager as _, ServiceStatus,
};

/// How often `logs --follow` looks for appended bytes. The same interval
/// `mcpgw watch` uses, for the same reason: live enough to read along with,
/// cheap enough to leave running.
const FOLLOW_POLL: Duration = Duration::from_millis(500);

/// Lines of history `logs` prints before it starts following.
const DEFAULT_LINES: usize = 50;

#[derive(clap::Args)]
pub struct DaemonArgs {
    #[command(subcommand)]
    pub command: DaemonCommand,
}

#[derive(clap::Subcommand)]
pub enum DaemonCommand {
    /// Install the gateway as a service that starts at login
    Install(AddressArgs),
    /// Remove the service (the config and captured traffic stay)
    Uninstall,
    /// Start the installed service
    Start(AddressArgs),
    /// Stop the running service, leaving it installed
    Stop,
    /// Report what is running, what is installed and where the logs are
    Status {
        /// Gateway URL to probe
        #[arg(
            long,
            value_name = "URL",
            default_value = None,
            long_help = "Gateway URL to probe.\n\nDefaults to the address the installed service \
                         was installed with, and to the standard gateway URL when nothing is \
                         installed."
        )]
        url: Option<String>,
    },
    /// Print the service's captured stdout and stderr
    Logs {
        /// Keep printing as the daemon writes
        #[arg(long, short)]
        follow: bool,
        /// Lines of history to print first
        #[arg(long, short = 'n', default_value_t = DEFAULT_LINES, value_name = "N")]
        lines: usize,
    },
    /// The elevated half of `install`. Not for typing: it is what the UAC
    /// prompt raised by `mcpgw daemon install` approves.
    #[cfg(windows)]
    #[command(name = mcpgw_core::daemon::windows::INSTALL_ELEVATED_COMMAND, hide = true)]
    InstallElevated(SpecArgs),
    /// The service itself. Not for typing: the Windows service manager runs
    /// this, and outside it there is no service controller to connect to.
    #[cfg(windows)]
    #[command(name = mcpgw_core::daemon::windows::RUN_SERVICE_COMMAND, hide = true)]
    RunService(SpecArgs),
}

/// A whole [`DaemonSpec`] on the command line.
///
/// The two hidden Windows entry points run in processes that share neither
/// a console nor a user profile with the one that computed the spec — the
/// service manager starts one as `LocalSystem`, and an over-the-shoulder
/// elevation starts the other as a different user entirely. Passing the
/// spec rather than letting them re-derive one is what keeps all three
/// pointed at the same config and the same log files.
#[cfg(windows)]
#[derive(clap::Args)]
pub struct SpecArgs {
    #[arg(long)]
    pub bind: String,
    #[arg(long)]
    pub port: u16,
    #[arg(long, value_name = "PATH")]
    pub config: PathBuf,
    #[arg(long, value_name = "DIR")]
    pub state_dir: PathBuf,
    #[arg(long, value_name = "PATH")]
    pub stdout: PathBuf,
    #[arg(long, value_name = "PATH")]
    pub stderr: PathBuf,
}

#[cfg(windows)]
impl SpecArgs {
    fn spec(&self) -> anyhow::Result<DaemonSpec> {
        Ok(DaemonSpec {
            exe: std::env::current_exe().context("cannot locate the running mcpgw binary")?,
            config_path: self.config.clone(),
            state_dir: self.state_dir.clone(),
            bind: self.bind.clone(),
            port: self.port,
            logs: LogPaths {
                stdout: self.stdout.clone(),
                stderr: self.stderr.clone(),
            },
        })
    }
}

/// Shared by the two commands that need to know where the gateway listens.
///
/// Both halves are optional rather than defaulted by clap so that `start`
/// can tell "the user asked for 8137" apart from "the user asked for
/// nothing" — a service installed on another port has to come back up on
/// that port, not on the default.
#[derive(clap::Args)]
pub struct AddressArgs {
    /// Port the service should listen on [default: 8137; `start` falls back
    /// to the port the service was installed with]
    #[arg(long)]
    pub port: Option<u16>,
    /// Address the service should bind. Loopback only, unless `[gateway]
    /// require_token = true` and this install has a token [default:
    /// 127.0.0.1]
    #[arg(long)]
    pub bind: Option<String>,
}

/// Where a service listens when nobody says otherwise.
const DEFAULT_PORT: u16 = 8137;
const DEFAULT_BIND: &str = "127.0.0.1";

pub fn run(args: &DaemonArgs) -> anyhow::Result<u8> {
    match &args.command {
        DaemonCommand::Install(address) => install(address).map(|()| 0),
        DaemonCommand::Uninstall => uninstall().map(|()| 0),
        DaemonCommand::Start(address) => start(address).map(|()| 0),
        DaemonCommand::Stop => stop().map(|()| 0),
        DaemonCommand::Status { url } => status(url.as_deref()),
        DaemonCommand::Logs { follow, lines } => logs(*follow, *lines).map(|()| 0),
        #[cfg(windows)]
        DaemonCommand::InstallElevated(spec) => install_elevated(spec).map(|()| 0),
        #[cfg(windows)]
        DaemonCommand::RunService(spec) => run_service(spec),
    }
}

fn install(address: &AddressArgs) -> anyhow::Result<()> {
    let spec = spec(address, None)?;
    let service = mcpgw_core::daemon::platform_service();
    // Asked before the port check so that reinstalling over our own service
    // — the whole of what changing how mcpgw is installed amounts to — is
    // not refused as if a stranger held the port. A supervisor that cannot
    // be queried at all decides nothing, and the refusal stands.
    let policy = port_policy(service.query().ok().as_ref(), &spec)?;
    // Minted here if this install has never served: the token has to exist
    // before the bind is judged against it, and before `sync` can write it
    // into a client entry that will dial the service being installed.
    let (token, minted) = super::token::ensure(&spec.state_dir)?;
    preflight(&spec, policy)?;
    if policy == PortPolicy::OwnServiceReinstall {
        println!("{}", reinstall_notice(&spec.state_dir));
    }
    warn_about_protected_paths(&spec);
    let installed = service.install(&spec)?;
    // Recorded only once the supervisor has accepted the job, and never
    // fatally: the service exists either way, and the only thing a missing
    // record costs is `status` falling back to the default address.
    if let Err(err) = mcpgw_core::daemon::save_spec(&spec) {
        eprintln!(
            "warning: could not record the installed address at {}: {err}\n         \
             `mcpgw daemon status` will probe the default port until you reinstall",
            mcpgw_core::daemon::spec_path(&spec.state_dir).display()
        );
    }
    println!(
        "installed the mcpgw gateway service at {}",
        installed.unit_path.display()
    );
    for note in &installed.notes {
        println!("  {note}");
    }
    println!("it will answer on {}", spec.url());
    if minted {
        println!(
            "issued this install's gateway token ({}) — `mcpgw sync` writes it into your clients",
            token.masked()
        );
    }
    Ok(())
}

/// Performs the registration this process was elevated in order to perform.
///
/// The preflight and the log files are the unelevated half's work and have
/// already happened — see the ordering contract in [`mcpgw_core::daemon`] —
/// and repeating them here would be repeating them as the wrong user.
#[cfg(windows)]
fn install_elevated(args: &SpecArgs) -> anyhow::Result<()> {
    let installed = mcpgw_core::daemon::windows::install_here(&args.spec()?)?;
    // This console closes the moment the process does, so the output is for
    // the operator who ran the elevated command by hand. The unelevated
    // parent prints the copy anyone else will read.
    println!(
        "installed the mcpgw gateway service at {}",
        installed.unit_path.display()
    );
    Ok(())
}

/// Runs this process as the Windows service, returning when it is stopped.
#[cfg(windows)]
fn run_service(args: &SpecArgs) -> anyhow::Result<u8> {
    mcpgw_core::daemon::windows::run_service(&args.spec()?)?;
    // Exited rather than returned: the service manager has already been told
    // the service stopped, and returning through `main` would run the daily
    // update check — a network call, as LocalSystem, with no one to read it.
    std::process::exit(0);
}

/// The shared preflight, with the one thing the refusal itself cannot say.
///
/// [`mcpgw_core::daemon::preflight`] is the single place the rules live and
/// the message is one sentence for all three supervisors; what it has no way
/// to know is whether *this* install could make the bind allowed. That is a
/// question about the token, so it is answered here.
fn preflight(spec: &DaemonSpec, policy: PortPolicy) -> anyhow::Result<()> {
    let result =
        mcpgw_core::daemon::preflight(spec, policy, super::token::bind_policy(&spec.state_dir));
    if matches!(result, Err(DaemonError::NonLoopbackBind { .. })) {
        eprintln!("hint: {BIND_HINT}");
    }
    Ok(result?)
}

/// How to make a bind past loopback allowed rather than refused.
const BIND_HINT: &str = "a gateway whose clients authenticate may bind anywhere — set \
     `[gateway] require_token = true` in your config, run `mcpgw sync` so every client \
     carries this install's token, then install again";

fn uninstall() -> anyhow::Result<()> {
    let service = mcpgw_core::daemon::platform_service();
    service.uninstall()?;
    // The record describes a service that no longer exists; leaving it would
    // have `status` probing an address nothing was ever going to answer on.
    // Reported rather than propagated: the service is gone either way, and
    // "uninstall failed" would be a worse lie than a stale file.
    if let Err(err) = state_dir()
        .map_err(|err| format!("{err:#}"))
        .and_then(|dir| mcpgw_core::daemon::remove_spec(&dir).map_err(|err| format!("{err:#}")))
    {
        eprintln!("warning: the recorded service address could not be removed: {err}");
    }
    println!("removed the mcpgw gateway service (your config and traffic log are untouched)");
    Ok(())
}

fn start(address: &AddressArgs) -> anyhow::Result<()> {
    let installed = mcpgw_core::daemon::load_spec(&state_dir()?);
    // An address the user did not name comes from the record, so a service
    // installed on 18137 comes back on 18137.
    let spec = spec(address, installed.as_ref())?;
    preflight(&spec, PortPolicy::MustBeFree)?;
    let service = mcpgw_core::daemon::platform_service();
    service.start(&spec)?;
    println!("started the mcpgw gateway service on {}", spec.url());
    Ok(())
}

fn stop() -> anyhow::Result<()> {
    let service = mcpgw_core::daemon::platform_service();
    service.stop()?;
    // Named when it is known: "the service stopped" and "this URL is now
    // dead" are the same sentence to whoever has a client pointed at it.
    match state_dir()
        .ok()
        .and_then(|dir| mcpgw_core::daemon::load_spec(&dir))
    {
        Some(spec) => println!(
            "stopped the mcpgw gateway service — nothing answers at {} now",
            spec.url()
        ),
        None => println!("stopped the mcpgw gateway service"),
    }
    Ok(())
}

/// Builds the spec, creating the log files as it goes so a platform never
/// has to (see the ordering contract in [`mcpgw_core::daemon`]).
///
/// `installed` is the address the service was recorded with, if there is
/// one. An address the user did not ask for is taken from there before the
/// default, so `daemon start` after `daemon install --port 18137` brings the
/// service back on 18137 rather than somewhere nothing was ever installed.
fn spec(address: &AddressArgs, installed: Option<&DaemonSpec>) -> anyhow::Result<DaemonSpec> {
    let state_dir = state_dir()?;
    let logs = mcpgw_core::daemon::prepare_logs(&state_dir)?;
    Ok(DaemonSpec {
        // The service has to name a binary that will still be there after
        // the shell that installed it is gone, so it is resolved now.
        exe: std::env::current_exe().context("cannot locate the running mcpgw binary")?,
        config_path: super::canonical_config_path()?,
        state_dir,
        bind: address
            .bind
            .clone()
            .or_else(|| installed.map(|spec| spec.bind.clone()))
            .unwrap_or_else(|| DEFAULT_BIND.to_owned()),
        port: address
            .port
            .or_else(|| installed.map(|spec| spec.port))
            .unwrap_or(DEFAULT_PORT),
        logs,
    })
}

/// [`mcpgw_core::daemon::port_policy`] on a runtime built for it and torn
/// down again — everything in `mcpgw daemon` outside `status` is
/// synchronous, and one loopback probe does not justify colouring it.
///
/// Shared with the wizard's daemon step so both installers get the same
/// answer to "is that our own service holding the port".
///
/// # Errors
///
/// Fails only when no runtime can be built to run the probe on.
pub(crate) fn port_policy(
    queried: Option<&ServiceStatus>,
    spec: &DaemonSpec,
) -> anyhow::Result<PortPolicy> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    Ok(runtime.block_on(mcpgw_core::daemon::port_policy(queried, spec)))
}

/// The line printed before a reinstall replaces the service that is running.
///
/// No confirmation goes with it: the user asked for an install, and the only
/// thing being taken away is a service of ours that this command is about to
/// put back. The old binary is named because switching between a `cargo
/// install` and a Homebrew mcpgw is what brings people here, and seeing the
/// path they are leaving is how they know the reinstall was the point.
pub(crate) fn reinstall_notice(state_dir: &Path) -> String {
    match mcpgw_core::daemon::load_spec(state_dir) {
        Some(recorded) => format!(
            "stopping the running service to reinstall it (was: {})",
            recorded.exe.display()
        ),
        // Nothing recorded: an install from before 0.3.1, which knew what it
        // ran but never wrote it down.
        None => "stopping the running service to reinstall it".to_owned(),
    }
}

/// Warns when anything the service will execute sits in a folder macOS keeps
/// behind a privacy grant.
///
/// Shared by `mcpgw daemon install` and the wizard's daemon step, because the
/// failure it heads off is the same on both paths and it is a silent one: a
/// launch agent cannot read through `~/Desktop`, `~/Documents` or
/// `~/Downloads` and has no way to ask, so the process hangs in dyld and the
/// user is left with a service that reports itself running, writes empty
/// logs, and never listens.
///
/// A warning rather than a refusal: Full Disk Access may already have been
/// granted, and nothing in the API says whether it was.
pub(crate) fn warn_about_protected_paths(spec: &DaemonSpec) {
    #[cfg(target_os = "macos")]
    {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return;
        };
        for line in protected_path_warnings(spec, &home) {
            println!("{line}");
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = spec;
}

/// The warning above, as lines and against an injected home, so the whole of
/// it can be asserted on without a terminal and without a real `~/Desktop`.
#[cfg(target_os = "macos")]
fn protected_path_warnings(spec: &DaemonSpec, home: &Path) -> Vec<String> {
    let mut hits = Vec::new();
    if let Some(dir) = mcpgw_core::daemon::tcc_protected_dir(&spec.exe, home) {
        hits.push(format!(
            "  the mcpgw binary itself, {} (~/{dir})",
            spec.exe.display()
        ));
    }
    // A config that will not parse is somebody else's error to report; this
    // check has nothing to say about it.
    if let Ok(config) = mcpgw_core::config::Config::load(&spec.config_path) {
        for (name, server) in &config.servers {
            let mcpgw_core::config::Transport::Stdio { command, .. } = &server.transport else {
                continue;
            };
            if !server.enabled {
                continue;
            }
            // Resolved rather than string-matched: `~/Desktop/bin/x` and a
            // bare `x` found on a PATH entry under Desktop hang identically.
            if let Ok(resolved) = which::which(command)
                && let Some(dir) = mcpgw_core::daemon::tcc_protected_dir(&resolved, home)
            {
                hits.push(format!(
                    "  {name}, which runs {} (~/{dir})",
                    resolved.display()
                ));
            }
        }
    }
    if hits.is_empty() {
        return Vec::new();
    }

    let mut lines = vec![
        "warning: this service will run something out of a folder macOS keeps behind a \
              privacy grant:"
            .to_owned(),
    ];
    lines.extend(hits);
    lines.push(
        "launchd has no grant to read through ~/Desktop, ~/Documents or ~/Downloads and no way \
         to ask for one, so a process started from there hangs before it runs — the service \
         looks installed and running, with empty logs and nothing listening."
            .to_owned(),
    );
    lines.push(
        "Move it somewhere unprotected (a Homebrew or `cargo install` path is fine), or grant \
         Full Disk Access in System Settings › Privacy & Security. Installing anyway."
            .to_owned(),
    );
    lines
}

fn state_dir() -> anyhow::Result<PathBuf> {
    mcpgw_core::paths::state_dir()
        .context("cannot determine a home directory to resolve the state directory")
}

/// Three questions in one screen: is a gateway answering, is a service
/// installed, and where would its output be.
///
/// Exits 0 only when a gateway is actually answering — that is the one bit a
/// script wants out of this command.
fn status(url: Option<&str>) -> anyhow::Result<u8> {
    let state_dir = state_dir()?;
    // What the service was installed with, when a version that records it
    // did the installing. Probing the default instead is how a healthy
    // service on `--port 18137` got reported as a gateway that is not there.
    let recorded = mcpgw_core::daemon::load_spec(&state_dir);
    let url = url.map_or_else(
        || {
            recorded.as_ref().map_or_else(
                || mcpgw_core::endpoints::DEFAULT_URL.to_owned(),
                DaemonSpec::url,
            )
        },
        str::to_owned,
    );
    let url = url.as_str();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let reach = runtime.block_on(mcpgw_core::daemon::probe_gateway(url, PROBE_TIMEOUT));

    let service = mcpgw_core::daemon::platform_service();
    let queried = service.query();

    println!("gateway   {}", describe_reach(reach, url));
    println!("service   {}", describe_service(service.name(), &queried));
    // A service still aimed at a binary that moved keeps working, which is
    // exactly why nobody notices: the supervisor reports a healthy job while
    // every upgrade lands on the copy it is not running.
    if let Some(advice) = recorded
        .as_ref()
        .and_then(mcpgw_core::daemon_check::service_exe)
        .as_ref()
        .and_then(mcpgw_core::daemon_check::ServiceExe::advice)
    {
        println!("service   {advice}");
    }
    // A different fact from the line above, and printed beside it rather
    // than instead of it: a service can be aimed at the right binary and
    // still be answering on the build it was started with, which is what
    // every in-place upgrade leaves behind.
    if let Some(advice) = mcpgw_core::daemon_check::url_port(url)
        .map(|port| mcpgw_core::daemon_check::service_version(&state_dir, port, reach))
        .as_ref()
        .and_then(mcpgw_core::daemon_check::ServiceVersion::advice)
    {
        println!("service   {advice}");
    }

    let logs = LogPaths::under_state_dir(&state_dir);
    println!("logs      {}", describe_log(&logs.stdout));
    println!("          {}", describe_log(&logs.stderr));

    // The state nearly every user is in during this release wave, and the
    // one a bare "not installed" would leave them puzzling over.
    let installed = matches!(&queried, Ok(status) if status.installed);
    let running = matches!(&queried, Ok(status) if status.running);
    if reach.is_up() && !installed {
        println!(
            "\nno service is installed, but a gateway is already answering at {url} — \
             that is a foreground `mcpgw serve`, and it stops when its terminal does"
        );
    }
    // A supervisor holding a healthy job while the probe finds nothing is
    // almost always this: a pre-0.3.1 install, whose address was never
    // written down and cannot be read back out of the plist or the unit.
    if !reach.is_up() && running && recorded.is_none() {
        println!(
            "\nthe service is running, but it was installed before 0.3.1 — mcpgw did not record \
             the address it was installed with, so this probed the default. Pass \
             `--url` to check another, or reinstall (`mcpgw daemon uninstall` then \
             `mcpgw daemon install --port <port>`) so status knows where to look"
        );
    }
    Ok(u8::from(!reach.is_up()))
}

fn describe_reach(reach: GatewayReach, url: &str) -> String {
    match reach {
        GatewayReach::Answering(status) => format!("running — {url} answers (HTTP {status})"),
        GatewayReach::NotHttp => {
            format!("not running — something holds the port at {url} but does not speak HTTP")
        }
        GatewayReach::Down => format!("not running — nothing is listening at {url}"),
    }
}

fn describe_service(manager: &str, queried: &Result<ServiceStatus, DaemonError>) -> String {
    match queried {
        Ok(status) if status.installed => {
            let state = if status.running { "running" } else { "stopped" };
            let unit = status
                .unit_path
                .as_ref()
                .map(|path| format!(" ({})", path.display()))
                .unwrap_or_default();
            let detail = status
                .detail
                .as_ref()
                .map(|detail| format!(" — {detail}"))
                .unwrap_or_default();
            format!("installed under {manager}, {state}{unit}{detail}")
        }
        Ok(_) => format!("not installed under {manager}"),
        // A platform that has not shipped its installer yet is a known
        // state, not a failed query, and its sentence is already written for
        // a user rather than for a log.
        Err(DaemonError::NotSupportedYet(message)) => format!("not installed — {message}"),
        Err(err) => format!("cannot be queried — {err}"),
    }
}

fn describe_log(path: &Path) -> String {
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() > 0 => format!("{} ({} bytes)", path.display(), meta.len()),
        Ok(_) => format!("{} (empty)", path.display()),
        Err(_) => format!("{} (not written yet)", path.display()),
    }
}

/// Prints the tail of both log files, then optionally follows them.
///
/// Both streams rather than a choice between them: a gateway that failed to
/// start says why on stderr and nothing at all on stdout, and asking a user
/// to guess which file to open is asking them to look in the empty one first.
fn logs(follow: bool, lines: usize) -> anyhow::Result<()> {
    let state_dir = state_dir()?;
    // Created here too, so `--follow` has something to follow from the
    // moment it starts and the permissions are ours rather than the
    // supervisor's.
    let paths = mcpgw_core::daemon::prepare_logs(&state_dir)?;

    let mut tails = Vec::new();
    for path in [&paths.stdout, &paths.stderr] {
        let history = tail_lines(path, lines)?;
        println!("--- {} ---", path.display());
        for line in &history {
            println!("{line}");
        }
        tails.push(Tail::at_end(path.clone()));
    }
    if !follow {
        return Ok(());
    }

    println!("--- following (Ctrl-C to stop) ---");
    let mut last: Option<PathBuf> = None;
    loop {
        for tail in &mut tails {
            let appended = match tail.poll() {
                Ok(appended) => appended,
                // A log left running all day must survive one bad stat: the
                // next poll is 500ms away and rereads everything anyway.
                Err(err) => {
                    eprintln!("daemon logs: {err:#} — retrying");
                    Vec::new()
                }
            };
            if appended.is_empty() {
                continue;
            }
            // Only when the source changes, so a busy stream is not
            // interrupted by a header every round.
            if last.as_ref() != Some(&tail.path) {
                println!("--- {} ---", tail.path.display());
                last = Some(tail.path.clone());
            }
            for line in appended {
                println!("{line}");
            }
        }
        std::thread::sleep(FOLLOW_POLL);
    }
}

/// The last `count` complete lines of `path`.
fn tail_lines(path: &Path, count: usize) -> anyhow::Result<Vec<String>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let text = match std::fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("cannot read {}", path.display())),
    };
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(count);
    Ok(all[start..].iter().map(|line| (*line).to_owned()).collect())
}

/// Follows one file by byte offset.
struct Tail {
    path: PathBuf,
    offset: u64,
}

impl Tail {
    /// Starts at the current end, so the history already printed is not
    /// printed a second time by the first poll. An unreadable file starts at
    /// zero: the first poll reports the failure with the path in it.
    fn at_end(path: PathBuf) -> Self {
        let offset = std::fs::metadata(&path).map_or(0, |meta| meta.len());
        Self { path, offset }
    }

    fn poll(&mut self) -> anyhow::Result<Vec<String>> {
        let mut file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(err).with_context(|| format!("cannot read {}", self.path.display()));
            }
        };
        let len = file
            .metadata()
            .with_context(|| format!("cannot stat {}", self.path.display()))?
            .len();
        // Shrunk under us: the file was rotated or truncated, so start over
        // rather than seek past its new end.
        if len < self.offset {
            self.offset = 0;
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)?;
        let (lines, consumed) = complete_lines(&buffer);
        self.offset += consumed;
        Ok(lines)
    }
}

/// The whole lines in `buffer` and how many bytes they took. A trailing
/// partial line is left for the next poll — an append is not atomic, and
/// half a line printed now would be printed again whole later.
fn complete_lines(buffer: &[u8]) -> (Vec<String>, u64) {
    let Some(last) = buffer.iter().rposition(|byte| *byte == b'\n') else {
        return (Vec::new(), 0);
    };
    let complete = &buffer[..=last];
    let lines = String::from_utf8_lossy(complete)
        .lines()
        .map(str::to_owned)
        .collect();
    (lines, complete.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partial_trailing_line_waits_for_the_rest_of_itself() {
        let (lines, consumed) = complete_lines(b"one\ntwo\nthr");
        assert_eq!(lines, ["one", "two"]);
        assert_eq!(consumed, 8);
        assert_eq!(complete_lines(b"no newline yet"), (Vec::new(), 0));
    }

    #[test]
    fn the_tail_keeps_the_last_lines_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.log");
        std::fs::write(&path, "a\nb\nc\nd\n").unwrap();
        assert_eq!(tail_lines(&path, 2).unwrap(), ["c", "d"]);
        assert_eq!(tail_lines(&path, 99).unwrap(), ["a", "b", "c", "d"]);
        // A log that was never written is empty history, not a failure.
        assert!(
            tail_lines(&dir.path().join("nope.log"), 5)
                .unwrap()
                .is_empty()
        );
    }

    /// The silent-hang guard: a binary under `~/Desktop` has to be named,
    /// with the reason and a way out, and one on a normal install path has
    /// to say nothing at all.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_binary_in_a_protected_folder_is_warned_about_and_one_outside_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let spec = |exe: PathBuf| DaemonSpec {
            exe,
            config_path: home.join("config.toml"),
            state_dir: home.join("state"),
            bind: "127.0.0.1".to_owned(),
            port: 8137,
            logs: LogPaths::under_state_dir(&home.join("state")),
        };

        let exe = home.join("Desktop/mcpgw/target/release/mcpgw");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"").unwrap();
        let lines = protected_path_warnings(&spec(exe.clone()), home).join("\n");
        assert!(lines.contains("privacy grant"), "{lines}");
        assert!(lines.contains(&exe.display().to_string()), "{lines}");
        assert!(lines.contains("(~/Desktop)"), "{lines}");
        assert!(lines.contains("hangs before it runs"), "{lines}");
        assert!(lines.contains("Full Disk Access"), "{lines}");
        // Warned, never refused: the grant may already be there.
        assert!(lines.contains("Installing anyway"), "{lines}");

        // Where a real install puts it, and where nothing has to be said.
        assert!(protected_path_warnings(&spec(home.join(".cargo/bin/mcpgw")), home).is_empty());
    }

    /// Printed instead of a refusal when the port is held by the service
    /// this install replaces. The old binary is the whole point of the line:
    /// it is what tells someone who has just moved from `cargo install` to
    /// Homebrew that the reinstall did what they came for.
    #[test]
    fn the_reinstall_notice_names_the_binary_the_service_was_installed_with() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        // Nothing recorded: pre-0.3.1, and there is no path to name.
        assert_eq!(
            reinstall_notice(&state),
            "stopping the running service to reinstall it"
        );

        let exe = dir.path().join(".cargo/bin/mcpgw");
        mcpgw_core::daemon::save_spec(&DaemonSpec {
            exe: exe.clone(),
            config_path: dir.path().join("config.toml"),
            state_dir: state.clone(),
            bind: "127.0.0.1".to_owned(),
            port: 8137,
            logs: LogPaths::under_state_dir(&state),
        })
        .unwrap();
        assert_eq!(
            reinstall_notice(&state),
            format!(
                "stopping the running service to reinstall it (was: {})",
                exe.display()
            )
        );
    }

    #[test]
    fn a_running_gateway_without_a_service_is_named_as_a_foreground_serve() {
        assert!(describe_reach(GatewayReach::Answering(405), "http://x/mcp").contains("running"));
        assert!(describe_reach(GatewayReach::Down, "http://x/mcp").contains("not running"));
        assert!(
            describe_service("launchd", &Ok(ServiceStatus::default())).contains("not installed")
        );
    }
}
