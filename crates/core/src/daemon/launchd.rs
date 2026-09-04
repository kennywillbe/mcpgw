//! macOS launch agent: a hand-authored plist in `~/Library/LaunchAgents`,
//! loaded into the user's GUI domain with `launchctl bootstrap`.
//!
//! The plist is written here rather than delegated to a service-manager
//! library because the restart policy this gateway needs is a `KeepAlive`
//! *dictionary* — `SuccessfulExit = false`, "restart it when it dies, leave
//! it alone when it exits cleanly". The libraries that generate launchd
//! plists write `KeepAlive` as a bare boolean, and a bare `true` means
//! launchd fights every deliberate `mcpgw daemon stop`.
//!
//! See the contract in [`super`]: `preflight` and `prepare_logs` have already
//! run by the time anything here is called, so the bind address is known
//! loopback, the port is known free, and both log files already exist with
//! the modes mcpgw wants rather than the ones launchd would have given them.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use super::{DaemonError, DaemonSpec, Installed, ServiceManager, ServiceStatus};

/// The reverse-DNS label a launch agent is addressed by. Fixed here rather
/// than in the implementation because uninstall has to find what a previous
/// version of mcpgw installed.
pub const LABEL: &str = "io.mcpgw.gateway";

/// Where a per-user launch agent has to live for launchd to find it at login.
const LAUNCH_AGENTS: &str = "Library/LaunchAgents";

/// Name used in every [`DaemonError::Service`] this file raises.
const MANAGER: &str = "launchd";

/// launchd, through `launchctl bootstrap gui/<uid>`.
#[derive(Debug, Default, Clone, Copy)]
pub struct Launchd {
    // Private so the struct can gain fields without breaking construction.
    _private: (),
}

impl Launchd {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl ServiceManager for Launchd {
    fn name(&self) -> &'static str {
        MANAGER
    }

    fn install(&self, spec: &DaemonSpec) -> Result<Installed, DaemonError> {
        let path = plist_path()?;
        let dir = path.parent().unwrap_or(Path::new("/"));
        // Apple's directory, not mcpgw's, so it is created with the usual
        // mode instead of the 0700 mcpgw gives the dirs it owns.
        std::fs::create_dir_all(dir).map_err(|source| DaemonError::Io {
            action: "create",
            path: dir.to_owned(),
            source,
        })?;

        let plist = render_plist(spec, inherited_path().as_deref());
        std::fs::write(&path, &plist).map_err(|source| DaemonError::Io {
            action: "write",
            path: path.clone(),
            source,
        })?;
        // Spelled out rather than left to the umask: this file names the
        // program launchd runs as this user, so it must not be writable by
        // anyone else however loose the shell that installed it was.
        set_mode(&path, 0o644)?;

        // Bootstrapping over a job that is already loaded fails, and
        // installing onto an existing one is how a user changes the port.
        bootout()?;
        launchctl_ok(&["bootstrap".into(), domain_target()?, os(&path)], "load")?;

        Ok(Installed {
            unit_path: path,
            notes: notes(spec),
        })
    }

    fn uninstall(&self) -> Result<(), DaemonError> {
        let path = plist_path()?;
        bootout()?;
        // Removing what is not there is the end state that was asked for.
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(DaemonError::Io {
                action: "remove",
                path,
                source,
            }),
        }
    }

    fn start(&self, _spec: &DaemonSpec) -> Result<(), DaemonError> {
        let path = plist_path()?;
        if !path.exists() {
            return Err(service_error(format!(
                "no launch agent at {} — run `mcpgw daemon install` first",
                path.display()
            )));
        }
        // `stop` boots the job out of the domain (see below), so the common
        // start is a fresh bootstrap; `RunAtLoad` runs the gateway as it
        // lands. A job that is still loaded only needs the kick.
        if !is_loaded()? {
            launchctl_ok(&["bootstrap".into(), domain_target()?, os(&path)], "load")?;
        }
        launchctl_ok(&["kickstart".into(), service_target()?], "start")
    }

    fn stop(&self) -> Result<(), DaemonError> {
        // Not `launchctl stop`: that sends SIGTERM and leaves the job loaded,
        // and a gateway killed by a signal did not exit successfully — which
        // is exactly the condition `KeepAlive { SuccessfulExit = false }`
        // restarts on, so the service would come straight back. Booting the
        // job out of the domain takes it away from the supervisor instead.
        // The plist stays on disk, so this is still "installed, stopped" and
        // `start` loads it again.
        bootout()
    }

    fn query(&self) -> Result<ServiceStatus, DaemonError> {
        let path = plist_path()?;
        // The plist is what makes the agent come back at login, so it is what
        // "installed" means. Asking launchd about a label whose file is gone
        // could only ever report someone else's leftovers.
        if !path.exists() {
            return Ok(ServiceStatus::default());
        }
        let Some(printed) = print_job()? else {
            return Ok(ServiceStatus {
                installed: true,
                running: false,
                unit_path: Some(path),
                detail: Some("loaded at your next login; `mcpgw daemon start` loads it now".into()),
            });
        };

        let state = field(&printed, "state");
        let running = state.as_deref() == Some("running");
        let detail = if running {
            field(&printed, "pid").map(|pid| format!("pid {pid}"))
        } else {
            field(&printed, "last exit code")
                .map(|code| format!("last exit code {code}"))
                .or(state)
        };
        Ok(ServiceStatus {
            installed: true,
            running,
            unit_path: Some(path),
            detail,
        })
    }
}

/// The plist an install writes, in the exact bytes it writes them.
///
/// Separate from the install so the file that decides what launchd runs can
/// be read, diffed and tested without touching a machine's launchd domain.
/// `path_env` is the `PATH` the service should run with — see
/// [`inherited_path`].
#[must_use]
pub fn render_plist(spec: &DaemonSpec, path_env: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
    );
    out.push_str("<plist version=\"1.0\">\n<dict>\n");

    entry(&mut out, "Label", LABEL);

    out.push_str("\t<key>ProgramArguments</key>\n\t<array>\n");
    string_item(&mut out, 2, &spec.exe.to_string_lossy());
    for arg in spec.serve_args() {
        string_item(&mut out, 2, &arg);
    }
    out.push_str("\t</array>\n");

    // The agent starts from launchd's environment, not a login shell's: it
    // has no `MCPGW_CONFIG`, so without these it would serve whatever is at
    // the default paths rather than what the install was pointed at.
    out.push_str("\t<key>EnvironmentVariables</key>\n\t<dict>\n");
    pair(
        &mut out,
        2,
        crate::paths::CONFIG_ENV,
        &spec.config_path.to_string_lossy(),
    );
    pair(
        &mut out,
        2,
        crate::paths::STATE_ENV,
        &spec.state_dir.to_string_lossy(),
    );
    if let Some(path_env) = path_env {
        pair(&mut out, 2, "PATH", path_env);
    }
    out.push_str("\t</dict>\n");

    out.push_str("\t<key>RunAtLoad</key>\n\t<true/>\n");
    // A dictionary rather than `<true/>`: restart the gateway when it dies,
    // but treat a clean exit as a decision and leave it stopped.
    out.push_str("\t<key>KeepAlive</key>\n\t<dict>\n");
    out.push_str("\t\t<key>SuccessfulExit</key>\n\t\t<false/>\n");
    out.push_str("\t</dict>\n");

    entry(
        &mut out,
        "StandardOutPath",
        &spec.logs.stdout.to_string_lossy(),
    );
    entry(
        &mut out,
        "StandardErrorPath",
        &spec.logs.stderr.to_string_lossy(),
    );

    out.push_str("</dict>\n</plist>\n");
    out
}

/// The `PATH` an install should bake into the plist: the one it was run with.
///
/// A launch agent otherwise inherits `/usr/bin:/bin:/usr/sbin:/sbin`, and
/// almost every stdio MCP server is an `npx`, `uvx` or `bunx` command living
/// somewhere else — so a gateway that works in a terminal would come up under
/// launchd with every stdio server failing to spawn. The cost is that the
/// `PATH` is frozen at install time, which the install notes say out loud.
#[must_use]
pub fn inherited_path() -> Option<String> {
    std::env::var("PATH").ok().filter(|path| !path.is_empty())
}

/// The `PATH` an installed plist runs the gateway with, read back out of the
/// file [`render_plist`] wrote.
///
/// The counterpart to [`inherited_path`]: the value baked in at install time
/// is the only record of what the service can actually resolve, and reading
/// it is what lets `doctor` and `add` tell "that command does not exist" from
/// "that command exists, but not for the daemon". Line-oriented rather than a
/// plist parser, because the shape it looks for is the one this file emits;
/// `None` for a definition written by hand in some other shape is the honest
/// answer, and every caller already has to handle the no-service case.
#[must_use]
pub fn plist_path_env(plist: &str) -> Option<String> {
    let mut lines = plist.lines().map(str::trim);
    while let Some(line) = lines.next() {
        if line == "<key>PATH</key>" {
            let value = lines.next()?.trim();
            let value = value.strip_prefix("<string>")?.strip_suffix("</string>")?;
            return Some(unescape(value)).filter(|path| !path.is_empty());
        }
    }
    None
}

/// Where the launch agent's plist belongs for the user running mcpgw.
///
/// # Errors
///
/// [`DaemonError::Service`] when there is no home directory to put it in.
pub fn plist_path() -> Result<PathBuf, DaemonError> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .ok_or_else(|| {
            service_error("HOME is not set, so there is no ~/Library/LaunchAgents to install into")
        })?;
    Ok(PathBuf::from(home)
        .join(LAUNCH_AGENTS)
        .join(format!("{LABEL}.plist")))
}

/// What `install` tells the user they still have to know.
fn notes(spec: &DaemonSpec) -> Vec<String> {
    vec![
        "macOS will show a \"Background Items Added\" notification and list mcpgw under System \
         Settings › General › Login Items & Extensions — leave it enabled, or the gateway will \
         not come back at your next login"
            .to_owned(),
        format!(
            "it serves {} and runs with the PATH you installed from, so re-run `mcpgw daemon \
             install` if either moves",
            spec.config_path.display()
        ),
        "its output goes to the daemon logs — `mcpgw daemon logs --follow` reads both streams"
            .to_owned(),
    ]
}

/// `gui/<uid>`, the domain a login session's agents live in.
fn domain_target() -> Result<std::ffi::OsString, DaemonError> {
    Ok(format!("gui/{}", uid()?).into())
}

/// `gui/<uid>/<label>`, one job inside that domain.
fn service_target() -> Result<std::ffi::OsString, DaemonError> {
    Ok(format!("gui/{}/{LABEL}", uid()?).into())
}

/// The real user id, from the tool that prints it.
///
/// std has no `getuid`, and core takes no libc dependency to read one integer
/// that `launchctl` itself would print back at us.
fn uid() -> Result<u32, DaemonError> {
    let output = Command::new("/usr/bin/id")
        .arg("-u")
        .output()
        .map_err(|err| service_error(format!("cannot run `id -u`: {err}")))?;
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .map_err(|_| {
            service_error("`id -u` did not print a user id, so the launchd domain is unknown")
        })
}

/// Removes the job from the domain, tolerating one that is not in it.
fn bootout() -> Result<(), DaemonError> {
    let output = launchctl(&["bootout".into(), service_target()?])?;
    // Racing a login, a crash or a second `daemon stop` all end with the job
    // already gone, which is the state that was asked for.
    if output.status.success() || not_loaded(&output) {
        return Ok(());
    }
    Err(failed("unload", &output))
}

/// Whether launchd currently holds the job.
fn is_loaded() -> Result<bool, DaemonError> {
    Ok(print_job()?.is_some())
}

/// `launchctl print` for the job, or `None` when launchd does not have it.
fn print_job() -> Result<Option<String>, DaemonError> {
    let output = launchctl(&["print".into(), service_target()?])?;
    if !output.status.success() {
        // Every failure reads as "not loaded" on purpose: the caller has
        // already established that the plist exists, and the only other thing
        // `print` can fail with is a domain that has no session — which is
        // still a gateway that is not running.
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()))
}

fn launchctl(args: &[std::ffi::OsString]) -> Result<Output, DaemonError> {
    Command::new("/bin/launchctl")
        .args(args)
        .output()
        .map_err(|err| service_error(format!("cannot run launchctl: {err}")))
}

fn launchctl_ok(args: &[std::ffi::OsString], action: &str) -> Result<(), DaemonError> {
    let output = launchctl(args)?;
    if output.status.success() {
        return Ok(());
    }
    Err(failed(action, &output))
}

/// launchd's own words about a refusal, which carry the errno a user can look
/// up ("Bootstrap failed: 5: Input/output error").
fn failed(action: &str, output: &Output) -> DaemonError {
    let said = [&output.stderr, &output.stdout]
        .into_iter()
        .map(|stream| String::from_utf8_lossy(stream).trim().to_owned())
        .find(|text| !text.is_empty())
        .unwrap_or_else(|| match output.status.code() {
            Some(code) => format!("launchctl exited {code}"),
            None => "launchctl was killed by a signal".to_owned(),
        });
    service_error(format!("cannot {action} the gateway service: {said}"))
}

/// Whether launchd's refusal was only "there is no such job".
fn not_loaded(output: &Output) -> bool {
    let said = String::from_utf8_lossy(&output.stderr);
    // Errno 3 is `ESRCH`; 113 is launchd's own "could not find service".
    matches!(output.status.code(), Some(3 | 113))
        || said.contains("No such process")
        || said.contains("Could not find")
}

/// The value of a `key = value` line in `launchctl print` output.
fn field(printed: &str, key: &str) -> Option<String> {
    printed.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        (name.trim() == key).then(|| value.trim().to_owned())
    })
}

fn service_error(message: impl Into<String>) -> DaemonError {
    DaemonError::Service {
        manager: MANAGER,
        message: message.into(),
    }
}

fn set_mode(path: &Path, mode: u32) -> Result<(), DaemonError> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|source| {
        DaemonError::Io {
            action: "harden",
            path: path.to_owned(),
            source,
        }
    })
}

fn os(path: &Path) -> std::ffi::OsString {
    path.as_os_str().to_owned()
}

fn entry(out: &mut String, key: &str, value: &str) {
    pair(out, 1, key, value);
}

fn pair(out: &mut String, depth: usize, key: &str, value: &str) {
    let indent = "\t".repeat(depth);
    write(out, format_args!("{indent}<key>{}</key>\n", escape(key)));
    string_item(out, depth, value);
}

fn string_item(out: &mut String, depth: usize, value: &str) {
    let indent = "\t".repeat(depth);
    write(
        out,
        format_args!("{indent}<string>{}</string>\n", escape(value)),
    );
}

/// Appending to a `String` cannot fail, so the plist is built without an
/// error path that would only ever be threaded through and unwrapped.
fn write(out: &mut String, args: std::fmt::Arguments<'_>) {
    use std::fmt::Write as _;
    out.write_fmt(args)
        .expect("writing to a String cannot fail");
}

/// XML-escapes a plist value. Home directories and server commands are user
/// text, and one `&` in a path is enough to make launchd reject the whole
/// file with a parse error it does not explain.
fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The inverse of [`escape`], for a value read back out of a plist. `&amp;`
/// is undone last so an escaped `&amp;lt;` does not come back as `<`.
fn unescape(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}
