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
    DaemonError, DaemonSpec, GatewayReach, LogPaths, PROBE_TIMEOUT, ServiceManager as _,
    ServiceStatus,
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
        #[arg(long, default_value = mcpgw_core::endpoints::DEFAULT_URL, value_name = "URL")]
        url: String,
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
#[derive(clap::Args)]
pub struct AddressArgs {
    /// Port the service should listen on
    #[arg(long, default_value_t = 8137)]
    pub port: u16,
    /// Address the service should bind. Loopback only — see `mcpgw daemon
    /// install --help` output on refusal
    #[arg(long, default_value = "127.0.0.1")]
    pub bind: String,
}

pub fn run(args: &DaemonArgs) -> anyhow::Result<u8> {
    match &args.command {
        DaemonCommand::Install(address) => install(address).map(|()| 0),
        DaemonCommand::Uninstall => uninstall().map(|()| 0),
        DaemonCommand::Start(address) => start(address).map(|()| 0),
        DaemonCommand::Stop => stop().map(|()| 0),
        DaemonCommand::Status { url } => status(url),
        DaemonCommand::Logs { follow, lines } => logs(*follow, *lines).map(|()| 0),
        #[cfg(windows)]
        DaemonCommand::InstallElevated(spec) => install_elevated(spec).map(|()| 0),
        #[cfg(windows)]
        DaemonCommand::RunService(spec) => run_service(spec),
    }
}

fn install(address: &AddressArgs) -> anyhow::Result<()> {
    let spec = spec(address)?;
    mcpgw_core::daemon::preflight(&spec)?;
    let service = mcpgw_core::daemon::platform_service();
    let installed = service.install(&spec)?;
    println!(
        "installed the mcpgw gateway service at {}",
        installed.unit_path.display()
    );
    for note in &installed.notes {
        println!("  {note}");
    }
    println!("it will answer on {}", spec.url());
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

fn uninstall() -> anyhow::Result<()> {
    let service = mcpgw_core::daemon::platform_service();
    service.uninstall()?;
    println!("removed the mcpgw gateway service (your config and traffic log are untouched)");
    Ok(())
}

fn start(address: &AddressArgs) -> anyhow::Result<()> {
    let spec = spec(address)?;
    mcpgw_core::daemon::preflight(&spec)?;
    let service = mcpgw_core::daemon::platform_service();
    service.start(&spec)?;
    println!("started the mcpgw gateway service on {}", spec.url());
    Ok(())
}

fn stop() -> anyhow::Result<()> {
    let service = mcpgw_core::daemon::platform_service();
    service.stop()?;
    println!("stopped the mcpgw gateway service");
    Ok(())
}

/// Builds the spec, creating the log files as it goes so a platform never
/// has to (see the ordering contract in [`mcpgw_core::daemon`]).
fn spec(address: &AddressArgs) -> anyhow::Result<DaemonSpec> {
    let state_dir = state_dir()?;
    let logs = mcpgw_core::daemon::prepare_logs(&state_dir)?;
    Ok(DaemonSpec {
        // The service has to name a binary that will still be there after
        // the shell that installed it is gone, so it is resolved now.
        exe: std::env::current_exe().context("cannot locate the running mcpgw binary")?,
        config_path: super::canonical_config_path()?,
        state_dir,
        bind: address.bind.clone(),
        port: address.port,
        logs,
    })
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
fn status(url: &str) -> anyhow::Result<u8> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let reach = runtime.block_on(mcpgw_core::daemon::probe_gateway(url, PROBE_TIMEOUT));

    let service = mcpgw_core::daemon::platform_service();
    let queried = service.query();

    println!("gateway   {}", describe_reach(reach, url));
    println!("service   {}", describe_service(service.name(), &queried));

    let logs = LogPaths::under_state_dir(&state_dir()?);
    println!("logs      {}", describe_log(&logs.stdout));
    println!("          {}", describe_log(&logs.stderr));

    // The state nearly every user is in during this release wave, and the
    // one a bare "not installed" would leave them puzzling over.
    let installed = matches!(&queried, Ok(status) if status.installed);
    if reach.is_up() && !installed {
        println!(
            "\nno service is installed, but a gateway is already answering at {url} — \
             that is a foreground `mcpgw serve`, and it stops when its terminal does"
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

    #[test]
    fn a_running_gateway_without_a_service_is_named_as_a_foreground_serve() {
        assert!(describe_reach(GatewayReach::Answering(405), "http://x/mcp").contains("running"));
        assert!(describe_reach(GatewayReach::Down, "http://x/mcp").contains("not running"));
        assert!(
            describe_service("launchd", &Ok(ServiceStatus::default())).contains("not installed")
        );
    }
}
