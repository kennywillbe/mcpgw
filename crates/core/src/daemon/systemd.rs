//! The gateway as a systemd `--user` unit, driven through `systemctl --user`.
//!
//! See the contract in [`super`]: this file is the whole of the systemd
//! milestone. [`preflight`](super::preflight) and
//! [`prepare_logs`](super::prepare_logs) have already run by the time
//! anything here is called, so the unit can redirect straight into
//! [`LogPaths`](super::LogPaths) and never re-checks the address.
//!
//! Everything that talks to the outside world does so through two seams —
//! [`Exec`] for the `systemctl`/`loginctl` calls and a `get` closure for the
//! environment, the same shape [`crate::paths::config_path_with`] uses — so
//! the unit text, the linger reporting and the "this machine has no systemd"
//! path are all testable on a machine that has no systemd.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::{DaemonError, DaemonSpec, Installed, ServiceManager, ServiceStatus};

/// Unit name, fixed here so uninstall can find what an older mcpgw wrote.
pub const UNIT: &str = "mcpgw.service";

/// Name of this supervisor in messages.
pub const MANAGER: &str = "systemd --user";

const XDG_CONFIG_ENV: &str = "XDG_CONFIG_HOME";
const HOME_ENV: &str = "HOME";

/// One finished supervisor command, reduced to what the callers read.
///
/// `stdout` and `stderr` arrive trimmed: every consumer here compares a
/// one-word answer (`active`, `enabled`) or quotes the error back at the
/// user, and neither wants the trailing newline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ran {
    /// Whether the command exited zero.
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

impl Ran {
    /// A successful run with `stdout` and nothing on stderr.
    #[must_use]
    pub fn ok(stdout: &str) -> Self {
        Self {
            ok: true,
            stdout: stdout.to_owned(),
            stderr: String::new(),
        }
    }

    /// A run that exited non-zero with `stderr`.
    #[must_use]
    pub fn failed(stderr: &str) -> Self {
        Self {
            ok: false,
            stdout: String::new(),
            stderr: stderr.to_owned(),
        }
    }
}

/// How a `systemctl` or `loginctl` call is made.
///
/// An `Err` means the command could not be run at all — almost always a
/// distribution without systemd, which is a different thing from systemd
/// answering "no" and gets a different message.
pub type Exec<'a> = dyn Fn(&str, &[&str]) -> std::io::Result<Ran> + 'a;

/// Whether the account's user manager keeps running without a login session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Linger {
    On,
    Off,
    /// `loginctl` is absent, refused to answer, or there is no user name to
    /// ask about. Reported as unknown rather than guessed at: "your gateway
    /// survives logout" is exactly the claim that must not be made up.
    Unknown,
}

/// systemd in user mode, through `systemctl --user`.
#[derive(Debug, Default, Clone, Copy)]
pub struct Systemd {
    _private: (),
}

impl Systemd {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl ServiceManager for Systemd {
    fn name(&self) -> &'static str {
        MANAGER
    }

    fn install(&self, spec: &DaemonSpec) -> Result<Installed, DaemonError> {
        install_with(spec, &unit_path()?, &spawn, env)
    }

    fn uninstall(&self) -> Result<(), DaemonError> {
        uninstall_with(&unit_path()?, &spawn)
    }

    fn start(&self, _spec: &DaemonSpec) -> Result<(), DaemonError> {
        start_with(&unit_path()?, &spawn)
    }

    fn stop(&self) -> Result<(), DaemonError> {
        stop_with(&spawn)
    }

    fn query(&self) -> Result<ServiceStatus, DaemonError> {
        query_with(&unit_path()?, &spawn, env)
    }
}

/// Writes the unit, reloads the user manager, enables it and starts it.
///
/// # Errors
///
/// [`DaemonError::Io`] when the unit cannot be written,
/// [`DaemonError::Service`] when `systemctl` is missing or refuses.
pub fn install_with(
    spec: &DaemonSpec,
    unit_path: &Path,
    exec: &Exec<'_>,
    get: impl Fn(&str) -> Option<OsString>,
) -> Result<Installed, DaemonError> {
    if let Some(dir) = unit_path.parent() {
        crate::private::create_dir_all(dir).map_err(|source| DaemonError::Io {
            action: "create",
            path: dir.to_owned(),
            source,
        })?;
    }
    let unit = render_unit(spec, inherited_path(&get).as_deref());
    std::fs::write(unit_path, unit).map_err(|source| DaemonError::Io {
        action: "write",
        path: unit_path.to_owned(),
        source,
    })?;

    // The reload has to happen before `enable`: the user manager works off
    // its own cache of the unit directory, and a unit written this second is
    // not in it yet.
    check(run(exec, "systemctl", &["--user", "daemon-reload"])?)?;
    check(run(exec, "systemctl", &["--user", "enable", UNIT])?)?;
    // `restart` rather than `enable --now`'s `start`, which does nothing to a
    // unit that is already active: reinstalling over a running service is how
    // it is pointed at an mcpgw that has moved, and a unit left running would
    // keep executing the binary named by the definition this one replaced.
    // Starting a stopped unit is what `restart` does anyway.
    check(run(exec, "systemctl", &["--user", "restart", UNIT])?)?;

    Ok(Installed {
        unit_path: unit_path.to_owned(),
        notes: notes(spec, exec, get),
    })
}

/// Stops and deregisters the unit, then removes it.
///
/// # Errors
///
/// [`DaemonError::Io`] when the unit file cannot be removed,
/// [`DaemonError::Service`] when `systemctl` is missing while a unit is
/// still on disk, or when the reload fails.
pub fn uninstall_with(unit_path: &Path, exec: &Exec<'_>) -> Result<(), DaemonError> {
    let installed = unit_path.exists();
    // A non-zero exit from `disable` is the ordinary "not loaded" case and
    // not a failure: uninstall promises an end state, and that is the end
    // state already reached.
    if let Err(err) = run(exec, "systemctl", &["--user", "disable", "--now", UNIT]) {
        // No systemctl means nothing was ever registered with it, so a
        // leftover file would be the only thing left to remove — and there
        // is not one.
        if installed {
            return Err(err);
        }
        return Ok(());
    }
    if installed {
        std::fs::remove_file(unit_path).map_err(|source| DaemonError::Io {
            action: "remove",
            path: unit_path.to_owned(),
            source,
        })?;
    }
    check(run(exec, "systemctl", &["--user", "daemon-reload"])?)
}

/// Starts the installed unit.
///
/// # Errors
///
/// [`DaemonError::Service`] when nothing is installed, `systemctl` is
/// missing, or systemd refuses.
pub fn start_with(unit_path: &Path, exec: &Exec<'_>) -> Result<(), DaemonError> {
    if !unit_path.exists() {
        return Err(service_error(format!(
            "there is no unit at {} — run `mcpgw daemon install` first",
            unit_path.display()
        )));
    }
    check(run(exec, "systemctl", &["--user", "start", UNIT])?)
}

/// Stops the running unit, leaving it installed.
///
/// # Errors
///
/// [`DaemonError::Service`] when `systemctl` is missing or refuses.
pub fn stop_with(exec: &Exec<'_>) -> Result<(), DaemonError> {
    check(run(exec, "systemctl", &["--user", "stop", UNIT])?)
}

/// What systemd currently reports about the unit.
///
/// # Errors
///
/// [`DaemonError::Service`] when `systemctl` cannot be run at all. A unit
/// that is merely absent or inactive is a [`ServiceStatus`], not an error.
pub fn query_with(
    unit_path: &Path,
    exec: &Exec<'_>,
    get: impl Fn(&str) -> Option<OsString>,
) -> Result<ServiceStatus, DaemonError> {
    // Both of these exit non-zero for perfectly ordinary answers
    // ("inactive", "disabled", "not-found"), so only the failure to run them
    // at all is an error; the word on stdout is the answer either way.
    let active = run(exec, "systemctl", &["--user", "is-active", UNIT])?;
    let enabled = run(exec, "systemctl", &["--user", "is-enabled", UNIT])?;

    // A unit can be loaded from a directory this build does not write to, so
    // systemd's answer counts as much as the file does.
    let installed = unit_path.exists() || !matches!(enabled.stdout.as_str(), "" | "not-found");
    if !installed {
        return Ok(ServiceStatus::default());
    }
    Ok(ServiceStatus {
        installed: true,
        running: active.stdout == "active",
        unit_path: Some(unit_path.to_owned()),
        detail: Some(format!(
            "{}; {}",
            describe_enabled(&enabled.stdout),
            linger_note(exec, get)
        )),
    })
}

fn describe_enabled(word: &str) -> String {
    match word {
        "enabled" | "enabled-runtime" => "enabled, so it comes back at login".to_owned(),
        "" | "not-found" => "no unit is loaded".to_owned(),
        other => format!("{other}, so it will not come back at login on its own"),
    }
}

/// The sentence about surviving logout that both `install` and `status` show.
///
/// Reporting rather than acting is deliberate: `loginctl enable-linger`
/// changes how the whole account behaves — every user unit outlives every
/// session afterwards — and that is not a side effect to hide inside
/// installing one gateway.
#[must_use]
pub fn linger_note(exec: &Exec<'_>, get: impl Fn(&str) -> Option<OsString>) -> String {
    let user = current_user(&get);
    let subject = user.as_deref().unwrap_or("$USER");
    match linger(exec, user.as_deref()) {
        Linger::On => {
            "user lingering is on, so the gateway keeps running after you log out".to_owned()
        }
        Linger::Off => format!(
            "user lingering is off, so the gateway stops when your last session ends — \
             `loginctl enable-linger {subject}` changes that, and mcpgw does not run it for you \
             because it outlives every user service you have, not just this one"
        ),
        Linger::Unknown => format!(
            "could not ask loginctl whether user lingering is on, so the gateway may stop when \
             your last session ends — `loginctl enable-linger {subject}` makes it survive logout"
        ),
    }
}

/// Asks `loginctl` whether this account lingers.
///
/// Degrades to [`Linger::Unknown`] for every way the question can go
/// unanswered — no `loginctl` on a container image, no logind, no user name
/// in the environment — because none of those is a reason to fail an install
/// that otherwise worked.
#[must_use]
pub fn linger(exec: &Exec<'_>, user: Option<&str>) -> Linger {
    let Some(user) = user else {
        return Linger::Unknown;
    };
    let Ok(ran) = exec("loginctl", &["show-user", user, "--property=Linger"]) else {
        return Linger::Unknown;
    };
    if !ran.ok {
        return Linger::Unknown;
    }
    match ran
        .stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("Linger="))
    {
        Some("yes") => Linger::On,
        Some("no") => Linger::Off,
        _ => Linger::Unknown,
    }
}

fn current_user(get: &impl Fn(&str) -> Option<OsString>) -> Option<String> {
    ["USER", "LOGNAME"]
        .iter()
        .filter_map(|key| get(key))
        .find_map(|value| value.into_string().ok().filter(|name| !name.is_empty()))
}

/// The unit file this build reads and writes, from the process environment.
///
/// # Errors
///
/// [`DaemonError::Service`] when no home directory can be determined, which
/// is the only way the path can fail to resolve.
pub fn unit_path() -> Result<PathBuf, DaemonError> {
    unit_path_with(env).ok_or_else(|| {
        service_error(
            "cannot determine a home directory, so there is nowhere to put the unit — set HOME \
             or XDG_CONFIG_HOME"
                .to_owned(),
        )
    })
}

/// Same as [`unit_path`] with an injectable environment.
///
/// `$XDG_CONFIG_HOME/systemd/user/mcpgw.service`, falling back to
/// `~/.config/...` — the directory `systemctl --user` reads, and the same
/// XDG-then-HOME order [`crate::paths::config_path_with`] uses.
#[must_use]
pub fn unit_path_with(get: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let base = match get(XDG_CONFIG_ENV).filter(|value| !value.is_empty()) {
        Some(xdg) => PathBuf::from(xdg),
        None => PathBuf::from(get(HOME_ENV).filter(|value| !value.is_empty())?).join(".config"),
    };
    Some(base.join("systemd").join("user").join(UNIT))
}

/// The unit text `install` writes, in the exact bytes it writes them.
///
/// Separate from the install so the file that decides what systemd runs can
/// be read, diffed and tested without touching a machine's user manager.
/// `path_env` is the `PATH` the service should run with — see
/// [`inherited_path`].
///
/// `Type=simple` and no sd-notify: readiness here means "the gateway answers
/// HTTP", which is what `mcpgw daemon status` probes for real over loopback.
/// Claiming readiness over a notify socket instead would add a dependency
/// and a wire protocol to assert something already checked better.
#[must_use]
pub fn render_unit(spec: &DaemonSpec, path_env: Option<&str>) -> String {
    let exec_start = std::iter::once(spec.exe.to_string_lossy().into_owned())
        .chain(spec.serve_args())
        .map(|token| quoted(&token))
        .collect::<Vec<_>>()
        .join(" ");

    // The service starts from the user manager's environment, not a login
    // shell's: it has no `MCPGW_CONFIG`, so without these it would serve
    // whatever is at the default paths rather than what the install was
    // pointed at.
    let mut environment = String::new();
    let mut variable = |key: &str, value: &str| {
        environment.push_str("Environment=");
        environment.push_str(&quoted(&format!("{key}={value}")));
        environment.push('\n');
    };
    variable(
        crate::paths::CONFIG_ENV,
        &spec.config_path.to_string_lossy(),
    );
    variable(crate::paths::STATE_ENV, &spec.state_dir.to_string_lossy());
    if let Some(path_env) = path_env {
        variable("PATH", path_env);
    }

    format!(
        "# Generated by `mcpgw daemon install`; edits are overwritten by the next install.\n\
         [Unit]\n\
         Description=mcpgw MCP gateway\n\
         Documentation=https://github.com/kennywillbe/mcpgw\n\
         \n\
         [Service]\n\
         Type=simple\n\
         {environment}\
         ExecStart={exec_start}\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         StandardOutput=append:{stdout}\n\
         StandardError=append:{stderr}\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        stdout = specifier_safe(&spec.logs.stdout.to_string_lossy()),
        stderr = specifier_safe(&spec.logs.stderr.to_string_lossy()),
    )
}

/// The `PATH` an install should bake into the unit: the one it was run with.
///
/// A user unit otherwise inherits the manager's own minimal `PATH`, and
/// almost every stdio MCP server is an `npx`, `uvx` or `bunx` command living
/// somewhere else — under `~/.local/bin`, a version manager's shim directory
/// — so a gateway that works in a terminal would come up under systemd with
/// every stdio server failing to spawn. The cost is that the `PATH` is frozen
/// at install time, which the install notes say out loud.
fn inherited_path(get: &impl Fn(&str) -> Option<OsString>) -> Option<String> {
    get("PATH")
        .and_then(|path| path.into_string().ok())
        .filter(|path| !path.is_empty())
}

/// What `install` tells the user they still have to know.
fn notes(
    spec: &DaemonSpec,
    exec: &Exec<'_>,
    get: impl Fn(&str) -> Option<OsString>,
) -> Vec<String> {
    vec![
        linger_note(exec, get),
        format!(
            "it serves {} and runs with the PATH you installed from, so re-run `mcpgw daemon \
             install` if either moves",
            spec.config_path.display()
        ),
        "its output goes to the daemon logs — `mcpgw daemon logs --follow` reads both streams"
            .to_owned(),
    ]
}

/// A token for a whitespace-split unit value (`ExecStart`, `Environment`).
fn quoted(token: &str) -> String {
    let token = specifier_safe(token);
    if token.contains([' ', '\t', '"', '\'', '\\']) {
        let escaped = token.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        token
    }
}

/// systemd expands `%x` specifiers in unit values, so a literal percent in a
/// path — legal on Linux, and what a `%` in a home directory name is — has
/// to be doubled or the unit points somewhere else entirely.
fn specifier_safe(value: &str) -> String {
    value.replace('%', "%%")
}

fn env(key: &str) -> Option<OsString> {
    std::env::var_os(key)
}

/// The real [`Exec`]: spawn, wait, and trim what came back.
fn spawn(program: &str, args: &[&str]) -> std::io::Result<Ran> {
    let output = std::process::Command::new(program).args(args).output()?;
    Ok(Ran {
        ok: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    })
}

/// Runs a supervisor command, turning "there is no such program" into a
/// sentence about this machine rather than an errno.
fn run(exec: &Exec<'_>, program: &str, args: &[&str]) -> Result<Ran, DaemonError> {
    exec(program, args).map_err(|source| {
        service_error(format!(
            "cannot run `{program}` ({source}) — this build installs the gateway as a systemd \
             user unit, and this machine does not appear to be running systemd. Start it with \
             `mcpgw serve` under whatever supervisor you do have (an OpenRC, runit or s6 \
             service), and `mcpgw daemon status` will still report on it"
        ))
    })
}

/// Turns a non-zero exit into an error carrying whatever systemd said.
fn check(ran: Ran) -> Result<(), DaemonError> {
    if ran.ok {
        return Ok(());
    }
    let said = match (ran.stderr.is_empty(), ran.stdout.is_empty()) {
        (false, _) => ran.stderr,
        (true, false) => ran.stdout,
        (true, true) => "no output".to_owned(),
    };
    Err(service_error(format!(
        "systemctl refused: {said} (`systemctl --user status {UNIT}` has the detail)"
    )))
}

fn service_error(message: String) -> DaemonError {
    DaemonError::Service {
        manager: MANAGER,
        message,
    }
}
