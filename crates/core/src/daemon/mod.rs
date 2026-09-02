//! Running the gateway as a supervised background service.
//!
//! The gateway is only useful if it is up before the first MCP client asks
//! for a tool, and no harness restarts it. That is a job for the platform's
//! own supervisor — launchd, systemd `--user`, the Windows service manager —
//! so this module is the platform-agnostic half: what a service needs to
//! know ([`DaemonSpec`]), what may never be installed ([`preflight`]), where
//! its output goes ([`prepare_logs`]) and how to tell a user what is
//! actually running ([`probe_gateway`]).
//!
//! # Contract for the per-platform implementations
//!
//! **This file is final.** [`launchd`], [`systemd`] and [`windows`] each ship
//! as a stub whose operations report [`DaemonError::NotSupportedYet`], and
//! each is filled in by exactly one later change touching exactly one file.
//! An implementor writes the body of [`ServiceManager`] for their platform
//! and nothing else: the selector below, [`DaemonError`], [`DaemonSpec`],
//! [`ServiceStatus`], the loopback and port preflight and the log helpers are
//! shared, already tested, and must not be forked per platform. If a platform
//! genuinely needs something new here, it belongs in this file for all three
//! — a helper that exists in two spellings is how the three services drift
//! into three different products.
//!
//! Ordering the CLI relies on and platforms may assume: [`preflight`] has
//! already run (and [`prepare_logs`] has already created the log files)
//! before `install` or `start` is called, so an implementation can redirect
//! straight into [`LogPaths`] and never has to re-check the bind address or
//! the port.

use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(target_os = "macos")]
pub mod launchd;
#[cfg(not(any(target_os = "macos", windows)))]
pub mod systemd;
#[cfg(windows)]
pub mod windows;

// Exactly one arm matches on any target — macOS, Windows, and "everything
// else", which in practice means the systemd distributions. A target that
// has neither service manager still gets `systemd`, whose stub says so.
#[cfg(target_os = "macos")]
pub use launchd::Launchd as PlatformService;
#[cfg(not(any(target_os = "macos", windows)))]
pub use systemd::Systemd as PlatformService;
#[cfg(windows)]
pub use windows::WindowsService as PlatformService;

/// Subdirectory of the state dir holding the daemon's captured output.
pub const LOGS_DIR: &str = "logs";

/// Filename the daemon's stdout is redirected to.
pub const STDOUT_LOG: &str = "daemon.out.log";

/// Filename the daemon's stderr is redirected to.
pub const STDERR_LOG: &str = "daemon.err.log";

/// How long a status probe waits for the gateway before calling it down.
/// Loopback either answers immediately or is not there.
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// The service manager for the host platform.
#[must_use]
pub fn platform_service() -> PlatformService {
    PlatformService::new()
}

/// Everything a platform needs to write a service definition.
///
/// Built once by the CLI and handed to the platform unchanged, so the three
/// implementations cannot disagree about which binary, which config or which
/// address the installed service will use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonSpec {
    /// Absolute path to the `mcpgw` binary the service should run.
    pub exe: PathBuf,
    /// Canonical config the served servers are read from.
    pub config_path: PathBuf,
    /// mcpgw's state directory, which also holds [`LogPaths`].
    pub state_dir: PathBuf,
    /// Address to bind. Guaranteed loopback by [`preflight`].
    pub bind: String,
    /// Port to listen on.
    pub port: u16,
    /// Where the service's stdout and stderr are redirected.
    pub logs: LogPaths,
}

impl DaemonSpec {
    /// The aggregate endpoint the installed service will answer on.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://{}/mcp", self.authority())
    }

    /// `host:port`, as it appears in messages about the listening socket.
    #[must_use]
    pub fn authority(&self) -> String {
        // An IPv6 literal needs its brackets back before it can be a URL
        // authority; a hostname or IPv4 address is already one.
        if self.bind.contains(':') {
            format!("[{}]:{}", self.bind, self.port)
        } else {
            format!("{}:{}", self.bind, self.port)
        }
    }

    /// The argument vector the service should run: `serve` with this spec's
    /// address, spelled out rather than left to defaults so a service keeps
    /// pointing where it was installed even if a default later moves.
    #[must_use]
    pub fn serve_args(&self) -> Vec<String> {
        vec![
            "serve".to_owned(),
            "--bind".to_owned(),
            self.bind.clone(),
            "--port".to_owned(),
            self.port.to_string(),
        ]
    }
}

/// The two files a supervised gateway writes its output to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogPaths {
    pub stdout: PathBuf,
    pub stderr: PathBuf,
}

impl LogPaths {
    /// Where the logs live under `state_dir`, without touching the disk.
    #[must_use]
    pub fn under_state_dir(state_dir: &Path) -> Self {
        let dir = state_dir.join(LOGS_DIR);
        Self {
            stdout: dir.join(STDOUT_LOG),
            stderr: dir.join(STDERR_LOG),
        }
    }
}

/// What the platform reports about the service it manages.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServiceStatus {
    /// Whether a service definition for mcpgw exists.
    pub installed: bool,
    /// Whether the supervisor currently has it running.
    pub running: bool,
    /// The plist / unit / registry entry backing it, when there is one.
    pub unit_path: Option<PathBuf>,
    /// One line of platform detail worth showing: a last exit status, or
    /// "user lingering is off, so this stops at logout".
    pub detail: Option<String>,
}

/// What an `install` produced, so the CLI can say where it went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    /// The file the service definition was written to.
    pub unit_path: PathBuf,
    /// Anything the user has to do or know — the macOS Background Items
    /// prompt, a `loginctl enable-linger` suggestion.
    pub notes: Vec<String>,
}

/// The six operations `mcpgw daemon` needs from a platform's supervisor.
///
/// See the module docs: this trait is the whole seam, and the file
/// implementing it for one platform is the only file that platform's
/// milestone touches.
pub trait ServiceManager {
    /// Name of the supervisor, for messages ("launchd", "systemd --user").
    fn name(&self) -> &'static str;

    /// Writes the service definition and registers it to start at login.
    ///
    /// # Errors
    ///
    /// [`DaemonError`] when the definition cannot be written or the
    /// supervisor refuses it.
    fn install(&self, spec: &DaemonSpec) -> Result<Installed, DaemonError>;

    /// Stops the service if it runs and removes its definition. Removing a
    /// service that is not installed succeeds: the end state is what was
    /// asked for.
    ///
    /// # Errors
    ///
    /// [`DaemonError`] when the supervisor refuses.
    fn uninstall(&self) -> Result<(), DaemonError>;

    /// Starts the installed service. `spec` is passed so a platform whose
    /// supervisor needs no persistent definition can start from it directly.
    ///
    /// # Errors
    ///
    /// [`DaemonError`] when nothing is installed or the supervisor refuses.
    fn start(&self, spec: &DaemonSpec) -> Result<(), DaemonError>;

    /// Stops the running service, leaving it installed.
    ///
    /// # Errors
    ///
    /// [`DaemonError`] when the supervisor refuses.
    fn stop(&self) -> Result<(), DaemonError>;

    /// What the supervisor knows about the service right now.
    ///
    /// # Errors
    ///
    /// [`DaemonError`] when the supervisor cannot be asked at all. "Not
    /// installed" is a [`ServiceStatus`], not an error.
    fn query(&self) -> Result<ServiceStatus, DaemonError>;
}

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    /// The platform's service support has not shipped yet. Carries its own
    /// full sentence: the three platforms land in three different releases
    /// and each wants to name its own supervisor and its own fallback.
    #[error("{0}")]
    NotSupportedYet(&'static str),

    #[error(
        "refusing to run an unattended gateway on {bind}: it has no authentication, so anyone \
         who can reach that address could call your MCP servers — and unlike `mcpgw serve`, a \
         service prints its warning into a logfile nobody reads. Use a loopback address \
         (127.0.0.1, ::1 or localhost), and put a reverse proxy in front if it has to be \
         reachable from elsewhere"
    )]
    NonLoopbackBind { bind: String },

    #[error(
        "something already listens on {authority} — run `mcpgw daemon status` to see whether \
         that is an mcpgw gateway you already started"
    )]
    PortInUse { authority: String },

    #[error("failed to {action} {path}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The supervisor was reached and said no.
    #[error("{manager}: {message}")]
    Service {
        manager: &'static str,
        message: String,
    },
}

/// Whether `host` names the local machine and only the local machine.
///
/// `is_loopback` already covers all of 127.0.0.0/8 and `::1`; `localhost` is
/// added because it is what people type, and it is reserved to resolve to
/// loopback.
#[must_use]
pub fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// The checks that must pass before any platform is asked to install or
/// start a service.
///
/// Kept out of the platform files deliberately: a refusal that only two of
/// three supervisors enforce is not a security property, and the port
/// message is the same sentence on every OS.
///
/// # Errors
///
/// [`DaemonError::NonLoopbackBind`] for a bind address other people could
/// reach, [`DaemonError::PortInUse`] when the port is already taken.
pub fn preflight(spec: &DaemonSpec) -> Result<(), DaemonError> {
    if !is_loopback(&spec.bind) {
        return Err(DaemonError::NonLoopbackBind {
            bind: spec.bind.clone(),
        });
    }
    if port_in_use(&spec.bind, spec.port) {
        return Err(DaemonError::PortInUse {
            authority: spec.authority(),
        });
    }
    Ok(())
}

/// Whether something already holds `host:port`.
///
/// Asked by binding rather than connecting: a socket in the middle of a
/// listen backlog, or one bound without ever accepting, still blocks the
/// gateway from starting, and only a bind sees that.
#[must_use]
pub fn port_in_use(host: &str, port: u16) -> bool {
    let host = host.trim_matches(['[', ']']);
    match std::net::TcpListener::bind((host, port)) {
        Ok(listener) => {
            drop(listener);
            false
        }
        // A host that does not resolve is not a port conflict; it is a bind
        // that will fail later with a message about the address itself.
        Err(err) => err.kind() != std::io::ErrorKind::AddrNotAvailable,
    }
}

/// Creates the log directory (0700) and both log files (0600) if they are
/// missing, and reports where they are.
///
/// Called before a service is installed or started so the supervisor
/// redirects into files that already have the right mode — a logfile created
/// by launchd or systemd inherits the umask, and the gateway's output can
/// carry the same header values the traffic log does.
///
/// # Errors
///
/// [`DaemonError::Io`] when the directory or a file cannot be created.
pub fn prepare_logs(state_dir: &Path) -> Result<LogPaths, DaemonError> {
    let paths = LogPaths::under_state_dir(state_dir);
    let dir = state_dir.join(LOGS_DIR);
    crate::private::create_dir_all(&dir).map_err(|source| DaemonError::Io {
        action: "create",
        path: dir,
        source,
    })?;
    for path in [&paths.stdout, &paths.stderr] {
        touch_owner_only(path)?;
    }
    Ok(paths)
}

fn touch_owner_only(path: &Path) -> Result<(), DaemonError> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        // Only applies when this call creates the file, which is why the
        // hardening below runs unconditionally.
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let file = options.open(path).map_err(|source| DaemonError::Io {
        action: "create",
        path: path.to_owned(),
        source,
    })?;
    drop(file);
    // A log file left behind by an earlier, looser build (or by a supervisor
    // that created it itself) is narrowed here rather than left as found.
    crate::private::harden_file(path).map_err(|source| DaemonError::Io {
        action: "harden",
        path: path.to_owned(),
        source,
    })
}

/// How far a status probe got against a gateway URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayReach {
    /// An HTTP server answered, with this status code. Any status counts:
    /// the question is whether a gateway is up, and its considered "no" to
    /// a bare `GET /mcp` is as good an answer as a "yes".
    Answering(u16),
    /// The port accepted a connection but nothing resembling HTTP came back
    /// — some other program owns the port.
    NotHttp,
    /// Nothing accepted the connection.
    Down,
}

impl GatewayReach {
    #[must_use]
    pub fn is_up(self) -> bool {
        matches!(self, GatewayReach::Answering(_))
    }
}

/// Asks whether a gateway is answering at `url`, in two steps.
///
/// The TCP half reuses the reachability check `doctor` already reports on,
/// so the two commands cannot disagree about whether the gateway is up. The
/// HTTP half exists because "the port is open" is a genuinely different
/// state from "an HTTP server is there" — a leftover process holding 8137
/// makes the first true and the second false, and telling a user their
/// gateway is up when it is not costs them the next hour.
pub async fn probe_gateway(url: &str, timeout: Duration) -> GatewayReach {
    if !crate::probe::gateway_listening(url, timeout).await {
        return GatewayReach::Down;
    }
    match http_status(url, timeout).await {
        Some(status) => GatewayReach::Answering(status),
        None => GatewayReach::NotHttp,
    }
}

/// One `GET` against `url`, returning the status code off the response line.
///
/// Hand-written rather than routed through an HTTP client: core's only
/// client comes with rmcp's transport attached, and dialing loopback for a
/// status line does not justify a session handshake.
async fn http_status(url: &str, timeout: Duration) -> Option<u16> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let port = parsed.port_or_known_default()?;
    let path = parsed.path();
    // `host_str` keeps an IPv6 literal's brackets, which the resolver does
    // not want but the `Host` header does.
    let authority = if parsed.port().is_some() {
        format!("{host}:{port}")
    } else {
        host.to_owned()
    };
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nAccept: text/event-stream\r\n\
         User-Agent: mcpgw-daemon-status\r\nConnection: close\r\n\r\n"
    );

    let work = async {
        let mut stream = tokio::net::TcpStream::connect((host.trim_matches(['[', ']']), port))
            .await
            .ok()?;
        stream.write_all(request.as_bytes()).await.ok()?;
        // Only the response line is wanted, and an SSE endpoint may hold the
        // body open forever — so this reads a bounded prefix and stops.
        let mut buffer = [0u8; 256];
        let read = stream.read(&mut buffer).await.ok()?;
        parse_status(&buffer[..read])
    };
    tokio::time::timeout(timeout, work).await.ok().flatten()
}

/// The status code out of an HTTP response line (`HTTP/1.1 405 ...`).
fn parse_status(bytes: &[u8]) -> Option<u16> {
    let text = std::str::from_utf8(bytes).ok()?;
    let line = text.split("\r\n").next()?;
    let mut parts = line.split(' ');
    if !parts.next()?.starts_with("HTTP/") {
        return None;
    }
    parts.next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_covers_the_whole_127_block_v6_and_the_name() {
        for host in [
            "127.0.0.1",
            "127.0.0.53",
            "::1",
            "[::1]",
            "localhost",
            "LOCALHOST",
        ] {
            assert!(is_loopback(host), "{host}");
        }
        for host in ["0.0.0.0", "192.168.1.10", "::", "example.com", "10.0.0.1"] {
            assert!(!is_loopback(host), "{host}");
        }
    }

    #[test]
    fn a_response_line_yields_its_status_and_anything_else_yields_nothing() {
        assert_eq!(
            parse_status(b"HTTP/1.1 405 Method Not Allowed\r\n"),
            Some(405)
        );
        assert_eq!(parse_status(b"HTTP/1.1 200 OK\r\n\r\ndata: x"), Some(200));
        assert_eq!(parse_status(b"SSH-2.0-OpenSSH_9.0\r\n"), None);
        assert_eq!(parse_status(b""), None);
    }
}
