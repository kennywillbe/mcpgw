//! The macOS launch agent: the exact plist launchd is handed, and the
//! read-only half of the manager that is safe to run on any Mac.
//!
//! The install/start/stop cycle against the real launchd domain is not
//! exercised here — it bootstraps a real job into the running user's domain,
//! so it lives in the CLI's `daemon_launchd` test behind `MCPGW_DAEMON_LIVE=1`
//! and never runs in CI. What is exercised here is the exact `launchctl`
//! command line each operation produces, through the injected runner, which
//! needs no domain at all.

#![cfg(target_os = "macos")]

use std::cell::RefCell;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use mcpgw_core::daemon::launchd::{
    LABEL, Launchd, Ran, install_with, plist_path, plist_path_env, plist_path_with, query_with,
    render_plist, start_with, stop_with, uninstall_with,
};
use mcpgw_core::daemon::{DaemonSpec, LogPaths, ServiceManager as _, ServiceStatus};

fn spec() -> DaemonSpec {
    let state_dir = PathBuf::from("/Users/u/.local/share/mcpgw");
    DaemonSpec {
        exe: PathBuf::from("/usr/local/bin/mcpgw"),
        config_path: PathBuf::from("/Users/u/.config/mcpgw/config.toml"),
        logs: LogPaths::under_state_dir(&state_dir),
        state_dir,
        bind: "127.0.0.1".to_owned(),
        port: 8137,
    }
}

/// The whole file, byte for byte: a plist is read by launchd and by the user
/// debugging it, and every key in here was a decision.
#[test]
fn the_plist_is_the_one_launchd_is_handed() {
    let rendered = render_plist(&spec(), Some("/opt/homebrew/bin:/usr/bin:/bin"));
    assert_eq!(rendered, EXPECTED_PLIST, "\n--- rendered ---\n{rendered}");
}

const EXPECTED_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>io.mcpgw.gateway</string>
	<key>ProgramArguments</key>
	<array>
		<string>/usr/local/bin/mcpgw</string>
		<string>serve</string>
		<string>--bind</string>
		<string>127.0.0.1</string>
		<string>--port</string>
		<string>8137</string>
		<string>--supervised</string>
	</array>
	<key>EnvironmentVariables</key>
	<dict>
		<key>MCPGW_CONFIG</key>
		<string>/Users/u/.config/mcpgw/config.toml</string>
		<key>MCPGW_STATE_DIR</key>
		<string>/Users/u/.local/share/mcpgw</string>
		<key>PATH</key>
		<string>/opt/homebrew/bin:/usr/bin:/bin</string>
	</dict>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>
	<key>StandardOutPath</key>
	<string>/Users/u/.local/share/mcpgw/logs/daemon.out.log</string>
	<key>StandardErrorPath</key>
	<string>/Users/u/.local/share/mcpgw/logs/daemon.err.log</string>
</dict>
</plist>
"#;

/// `KeepAlive` is the reason this plist is hand-written rather than generated:
/// as a bare `<true/>` it would restart the gateway after every deliberate
/// `mcpgw daemon stop`.
#[test]
fn keep_alive_restarts_a_crash_and_only_a_crash() {
    let rendered = render_plist(&spec(), None);
    assert!(
        rendered.contains(
            "\t<key>KeepAlive</key>\n\t<dict>\n\t\t<key>SuccessfulExit</key>\n\t\t<false/>\n"
        ),
        "{rendered}"
    );
    assert!(
        !rendered.contains("<key>KeepAlive</key>\n\t<true/>"),
        "{rendered}"
    );
}

/// The gateway is only worth supervising if it serves the config the install
/// was pointed at, and a launch agent starts with none of the environment the
/// installing shell had.
#[test]
fn the_agent_is_told_which_config_and_state_dir_to_use() {
    let rendered = render_plist(&spec(), None);
    assert!(rendered.contains("<key>MCPGW_CONFIG</key>"), "{rendered}");
    assert!(
        rendered.contains("<string>/Users/u/.config/mcpgw/config.toml</string>"),
        "{rendered}"
    );
    assert!(
        rendered.contains("<key>MCPGW_STATE_DIR</key>"),
        "{rendered}"
    );
    // No PATH to inherit means no PATH key, rather than an empty one that
    // would leave stdio servers with nothing to search at all.
    assert!(!rendered.contains("<key>PATH</key>"), "{rendered}");
}

/// One `&` in a home directory is enough for launchd to reject the whole file
/// with a parse error it does not explain.
#[test]
fn a_path_with_xml_metacharacters_survives_as_itself() {
    let mut spec = spec();
    spec.exe = PathBuf::from("/Users/a&b/<bin>/mcpgw");
    let rendered = render_plist(&spec, None);
    assert!(
        rendered.contains("<string>/Users/a&amp;b/&lt;bin&gt;/mcpgw</string>"),
        "{rendered}"
    );
    assert!(!rendered.contains("a&b"), "{rendered}");
}

/// The port and bind the install was given, spelled out in the arguments, so
/// a later change to mcpgw's defaults cannot move an installed service.
#[test]
fn the_program_arguments_are_the_specs_serve_arguments() {
    let mut spec = spec();
    spec.bind = "::1".to_owned();
    spec.port = 9137;
    let rendered = render_plist(&spec, None);
    for arg in spec.serve_args() {
        assert!(
            rendered.contains(&format!("<string>{arg}</string>")),
            "{arg}"
        );
    }
}

#[test]
fn the_agent_goes_where_launchd_looks_for_per_user_agents() {
    let path = plist_path().unwrap();
    assert!(
        path.ends_with(format!("Library/LaunchAgents/{LABEL}.plist")),
        "{}",
        path.display()
    );
    assert!(path.is_absolute(), "{}", path.display());
}

/// The read half of the same file: what a later `doctor` or `add` learns
/// about the PATH the service actually runs with, XML escapes undone.
#[test]
fn the_baked_path_is_read_back_out_of_the_plist() {
    let plain = render_plist(&spec(), Some("/opt/homebrew/bin:/usr/bin"));
    assert_eq!(
        plist_path_env(&plain).as_deref(),
        Some("/opt/homebrew/bin:/usr/bin")
    );

    let awkward = render_plist(&spec(), Some("/Users/u/Ben & Co/bin:/usr/bin"));
    assert_eq!(
        plist_path_env(&awkward).as_deref(),
        Some("/Users/u/Ben & Co/bin:/usr/bin")
    );

    // Nothing baked, and something that is not a plist at all.
    assert_eq!(plist_path_env(&render_plist(&spec(), None)), None);
    assert_eq!(plist_path_env("<key>PATH</key>\n<string></string>"), None);
    assert_eq!(plist_path_env("hello"), None);
}

/// Read-only, so it is safe on a developer's machine whether or not they have
/// the agent installed: the plist on disk is what "installed" means, and the
/// answer has to match it either way.
#[test]
fn a_query_answers_for_the_plist_that_is_actually_there() {
    let status = Launchd::new().query().unwrap();
    let path = plist_path().unwrap();

    assert_eq!(status.installed, path.exists(), "{}", path.display());
    if status.installed {
        assert_eq!(status.unit_path.as_deref(), Some(path.as_path()));
    } else {
        assert_eq!(status, ServiceStatus::default());
        // Nothing installed is a status, not an error, and `daemon status`
        // prints it as "not installed under launchd".
        assert!(!status.running);
    }
}

/// A recording runner: answers from `replies` (matched on the verb, which is
/// the first argument of every command this file runs) and remembers every
/// command line it was handed.
struct Fake {
    calls: RefCell<Vec<String>>,
    replies: Vec<(&'static str, std::io::Result<Ran>)>,
}

impl Fake {
    fn new(replies: Vec<(&'static str, std::io::Result<Ran>)>) -> Self {
        Self {
            calls: RefCell::new(Vec::new()),
            replies,
        }
    }

    fn exec(&self) -> impl Fn(&str, &[OsString]) -> std::io::Result<Ran> + '_ {
        move |program, args| {
            let rendered: Vec<String> = args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            self.calls
                .borrow_mut()
                .push(format!("{program} {}", rendered.join(" ")));
            // `id -u` is asked before every target is built, and answering it
            // here rather than in every test keeps the replies about launchd.
            if program.ends_with("/id") {
                return Ok(Ran::ok("501\n"));
            }
            let verb = rendered.first().map_or("", String::as_str);
            for (key, reply) in &self.replies {
                if *key == verb {
                    return match reply {
                        Ok(ran) => Ok(ran.clone()),
                        Err(err) => Err(std::io::Error::new(err.kind(), err.to_string())),
                    };
                }
            }
            Ok(Ran::ok(""))
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}

/// The two errnos launchd says "there is no such job" with, and the plain
/// refusal that is not one.
fn no_such_job() -> Ran {
    Ran::failed(113, "Could not find service \"io.mcpgw.gateway\" in domain")
}

fn missing() -> std::io::Result<Ran> {
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "No such file or directory (os error 2)",
    ))
}

fn under(dir: &Path) -> PathBuf {
    dir.join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

/// Install is three commands in one order: boot the old job out, then
/// bootstrap the new plist into the GUI domain. Anything else — a `load`, a
/// `start`, a bootstrap before the bootout — is a different installer.
#[test]
fn install_boots_the_old_job_out_before_bootstrapping_the_new_plist() {
    let home = tempfile::tempdir().unwrap();
    let path = under(home.path());
    let fake = Fake::new(vec![("bootout", Ok(no_such_job()))]);

    let installed = install_with(&spec(), &path, &fake.exec(), |key| {
        (key == "PATH").then(|| OsString::from("/opt/homebrew/bin"))
    })
    .unwrap();

    assert_eq!(
        fake.calls(),
        [
            "/usr/bin/id -u".to_owned(),
            "/bin/launchctl bootout gui/501/io.mcpgw.gateway".to_owned(),
            "/usr/bin/id -u".to_owned(),
            format!("/bin/launchctl bootstrap gui/501 {}", path.display()),
        ]
    );
    assert_eq!(installed.unit_path, path);
    // Written, and with the PATH the injected environment gave it.
    let plist = std::fs::read_to_string(&path).unwrap();
    assert!(
        plist.contains("<string>/opt/homebrew/bin</string>"),
        "{plist}"
    );
}

/// The mode is part of the install: this file names the program launchd runs
/// as the user, so it must not be group- or world-writable.
#[test]
fn the_installed_plist_is_not_writable_by_anyone_else() {
    use std::os::unix::fs::PermissionsExt as _;

    let home = tempfile::tempdir().unwrap();
    let path = under(home.path());
    let fake = Fake::new(vec![]);
    install_with(&spec(), &path, &fake.exec(), |_| None).unwrap();

    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o644, "{mode:o}");
}

/// A refusal that is not "no such job" is a failed install, quoted back.
#[test]
fn install_reports_what_launchd_refused_with() {
    let home = tempfile::tempdir().unwrap();
    let fake = Fake::new(vec![(
        "bootstrap",
        Ok(Ran::failed(5, "Bootstrap failed: 5: Input/output error")),
    )]);

    let err = install_with(&spec(), &under(home.path()), &fake.exec(), |_| None).unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("cannot load the gateway service"),
        "{message}"
    );
    assert!(message.contains("Bootstrap failed: 5"), "{message}");
}

/// Uninstall promises an end state, so a job launchd never had is not a
/// failure — and neither is a plist that is already gone.
#[test]
fn uninstall_boots_the_job_out_and_removes_the_plist() {
    let home = tempfile::tempdir().unwrap();
    let path = under(home.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "<plist/>").unwrap();
    let fake = Fake::new(vec![("bootout", Ok(no_such_job()))]);

    uninstall_with(&path, &fake.exec()).unwrap();

    assert_eq!(
        fake.calls(),
        [
            "/usr/bin/id -u".to_owned(),
            "/bin/launchctl bootout gui/501/io.mcpgw.gateway".to_owned(),
        ]
    );
    assert!(!path.exists(), "the plist survived the uninstall");

    // And again, over nothing at all.
    let fake = Fake::new(vec![("bootout", Ok(no_such_job()))]);
    uninstall_with(&path, &fake.exec()).unwrap();
}

/// Stop takes the job away from the supervisor rather than signalling it: a
/// `launchctl stop` is a non-zero exit, which is exactly what `KeepAlive`
/// restarts on.
#[test]
fn stop_boots_the_job_out_and_never_signals_it() {
    let fake = Fake::new(vec![]);
    stop_with(&fake.exec()).unwrap();

    assert_eq!(
        fake.calls(),
        [
            "/usr/bin/id -u".to_owned(),
            "/bin/launchctl bootout gui/501/io.mcpgw.gateway".to_owned(),
        ]
    );
}

/// The common start is after a stop, which left the job booted out — so it
/// is bootstrapped again and then kicked.
#[test]
fn start_bootstraps_a_job_launchd_no_longer_holds_and_kickstarts_it() {
    let home = tempfile::tempdir().unwrap();
    let path = under(home.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "<plist/>").unwrap();
    let fake = Fake::new(vec![("print", Ok(no_such_job()))]);

    start_with(&path, &fake.exec()).unwrap();

    assert_eq!(
        fake.calls(),
        [
            "/usr/bin/id -u".to_owned(),
            "/bin/launchctl print gui/501/io.mcpgw.gateway".to_owned(),
            "/usr/bin/id -u".to_owned(),
            format!("/bin/launchctl bootstrap gui/501 {}", path.display()),
            "/usr/bin/id -u".to_owned(),
            "/bin/launchctl kickstart gui/501/io.mcpgw.gateway".to_owned(),
        ]
    );
}

/// A job launchd still holds only needs the kick. Bootstrapping it a second
/// time is an error, so the extra command would break every start.
#[test]
fn start_only_kickstarts_a_job_that_is_still_loaded() {
    let home = tempfile::tempdir().unwrap();
    let path = under(home.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "<plist/>").unwrap();
    let fake = Fake::new(vec![("print", Ok(Ran::ok("\tstate = running\n")))]);

    start_with(&path, &fake.exec()).unwrap();

    let calls = fake.calls();
    assert!(
        !calls.iter().any(|call| call.contains("bootstrap")),
        "{calls:?}"
    );
    assert!(
        calls
            .last()
            .unwrap()
            .contains("kickstart gui/501/io.mcpgw.gateway"),
        "{calls:?}"
    );
}

/// Nothing installed is told to install rather than handed to launchd, which
/// would only fail with launchd's own words about a path.
#[test]
fn start_without_a_plist_says_to_install_first() {
    let home = tempfile::tempdir().unwrap();
    let fake = Fake::new(vec![]);

    let err = start_with(&under(home.path()), &fake.exec()).unwrap_err();
    assert!(err.to_string().contains("mcpgw daemon install"), "{err}");
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
}

/// `launchctl print` is the whole of what status knows, and the two fields it
/// reads are the two a user asks about.
#[test]
fn a_running_job_is_reported_with_the_pid_launchctl_printed() {
    let home = tempfile::tempdir().unwrap();
    let path = under(home.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "<plist/>").unwrap();
    let fake = Fake::new(vec![(
        "print",
        Ok(Ran::ok("\tstate = running\n\tpid = 4242\n")),
    )]);

    let status = query_with(&path, &fake.exec()).unwrap();

    assert_eq!(
        fake.calls(),
        [
            "/usr/bin/id -u".to_owned(),
            "/bin/launchctl print gui/501/io.mcpgw.gateway".to_owned(),
        ]
    );
    assert!(status.installed);
    assert!(status.running);
    assert_eq!(status.detail.as_deref(), Some("pid 4242"));
    assert_eq!(status.unit_path.as_deref(), Some(path.as_path()));
}

/// A plist launchd does not hold is installed and not running — the state a
/// `daemon stop` leaves, and the one a fresh install has before login.
#[test]
fn a_plist_launchd_does_not_hold_is_installed_but_not_running() {
    let home = tempfile::tempdir().unwrap();
    let path = under(home.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "<plist/>").unwrap();
    let fake = Fake::new(vec![("print", Ok(no_such_job()))]);

    let status = query_with(&path, &fake.exec()).unwrap();

    assert!(status.installed);
    assert!(!status.running);
    assert!(
        status.detail.unwrap().contains("mcpgw daemon start"),
        "the status did not say how to load it now"
    );
}

/// The exit code a stopped job left behind is the line a user is looking for,
/// and it must not be reported as a pid.
#[test]
fn a_stopped_job_reports_the_code_it_stopped_with() {
    let home = tempfile::tempdir().unwrap();
    let path = under(home.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "<plist/>").unwrap();
    let fake = Fake::new(vec![(
        "print",
        Ok(Ran::ok("\tstate = not running\n\tlast exit code = 2\n")),
    )]);

    let status = query_with(&path, &fake.exec()).unwrap();

    assert!(!status.running);
    assert_eq!(status.detail.as_deref(), Some("last exit code 2"));
}

/// No plist is "not installed", and launchd is not asked at all: the answer
/// could only ever be somebody else's leftovers under the same label.
#[test]
fn a_missing_plist_never_asks_launchd_anything() {
    let home = tempfile::tempdir().unwrap();
    let fake = Fake::new(vec![]);

    let status = query_with(&under(home.path()), &fake.exec()).unwrap();

    assert_eq!(status, ServiceStatus::default());
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
}

/// A machine with no `launchctl` gets a sentence about that, not an errno.
#[test]
fn a_machine_without_launchctl_says_so() {
    let fake = Fake::new(vec![("bootout", missing())]);

    let err = stop_with(&fake.exec()).unwrap_err();
    assert!(err.to_string().contains("cannot run launchctl"), "{err}");
}

/// The plist path is read out of the environment, so a sandboxed HOME is a
/// sandboxed install — which is what the live cycle relies on.
#[test]
fn the_plist_path_follows_the_home_it_is_given() {
    let path = plist_path_with(|key| (key == "HOME").then(|| OsString::from("/Users/ada")));
    assert_eq!(
        path,
        Some(PathBuf::from(format!(
            "/Users/ada/Library/LaunchAgents/{LABEL}.plist"
        )))
    );
    // An empty HOME is no HOME: joining onto it would write into the
    // working directory.
    assert_eq!(plist_path_with(|_| Some(OsString::from(""))), None);
}
