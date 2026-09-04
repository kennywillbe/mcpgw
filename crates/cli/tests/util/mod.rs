//! Helpers shared by the CLI's end-to-end tests.
//!
//! Every file under `tests/` is its own binary with its own copy of this
//! module, so a helper only some of them need is dead code in the rest. That
//! is what the `#[allow(dead_code)]` below are for, and the reason is the
//! same for each one.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt as _, BufReader};

/// The scripted fixture server lives in a sibling package, so `CARGO_BIN_EXE`
/// cannot name it here; it sits next to this test executable's parent
/// (`target/<profile>/`), which holds for every cargo layout the suite runs
/// under and CI always builds the whole workspace.
#[allow(dead_code)]
pub fn fixture_binary() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap();
    let path = dir.join(format!("mcpgw-test-server{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.exists(),
        "fixture binary missing at {} — build the workspace first",
        path.display()
    );
    path
}

/// A config with one healthy fixture server per name.
#[allow(dead_code)]
pub fn fixture_config(names: &[&str]) -> String {
    use std::fmt::Write as _;

    let fixture = fixture_binary();
    let mut text = "version = 1\n".to_owned();
    for name in names {
        let _ = write!(
            text,
            "\n[servers.{name}]\ntype = \"stdio\"\ncommand = '{}'\nargs = [\"healthy\"]\n",
            fixture.display()
        );
    }
    text
}

/// A `mcpgw` invocation pointed at `home` and nothing of the real machine:
/// its own config, its own state directory, every per-platform home key
/// redirected into it, and no XDG override leaking in from the environment
/// the test itself was started in.
#[allow(dead_code)]
pub fn mcpgw(home: &Path) -> Command {
    mcpgw_binary(&assert_cmd::cargo::cargo_bin("mcpgw"), home)
}

/// The same sandbox around a different `mcpgw` on disk.
///
/// Only the upgrade tests want this: the file they replace has to be a copy,
/// because the binary cargo built is the one every other test in the run is
/// about to execute.
#[allow(dead_code)]
pub fn mcpgw_binary(exe: &Path, home: &Path) -> Command {
    let mut command = sandboxed(exe, home);
    command
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("APPDATA", home.join("AppData"))
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME");
    command
}

/// The same sandbox for the config and the state directory, with the home of
/// the machine left alone. Only the systemd test wants this: `systemctl
/// --user` reads the unit directory its *manager* was started with, so a unit
/// written under a temp `HOME` — or a temp `XDG_CONFIG_HOME` — is a unit
/// systemctl never sees.
#[allow(dead_code)]
pub fn mcpgw_keeping_the_real_home(sandbox: &Path) -> Command {
    mcpgw_binary_keeping_the_real_home(&assert_cmd::cargo::cargo_bin("mcpgw"), sandbox)
}

/// The real home again, around a different `mcpgw` on disk — the systemd
/// live cycle, which needs both halves at once: the manager's own unit
/// directory, and a binary it may replace.
#[allow(dead_code)]
pub fn mcpgw_binary_keeping_the_real_home(exe: &Path, sandbox: &Path) -> Command {
    sandboxed(exe, sandbox)
}

/// The config and state redirection every invocation gets, whichever binary
/// is being run and whatever is done about the home directory.
fn sandboxed(exe: &Path, sandbox: &Path) -> Command {
    let mut command = Command::new(exe);
    command
        // Hermetic: no test may phone home for a version notice.
        .env("MCPGW_NO_UPDATE_CHECK", "1")
        .env("MCPGW_CONFIG", sandbox.join("config.toml"))
        .env("MCPGW_STATE_DIR", sandbox.join("state"));
    command
}

/// `mcpgw daemon`, run to completion in the sandbox `mcpgw` builds.
#[allow(dead_code)]
pub fn daemon(home: &Path, args: &[&str]) -> Output {
    run_daemon(mcpgw(home), args)
}

/// The same, for the one caller that has to keep the real `HOME` — see
/// [`mcpgw_keeping_the_real_home`].
#[allow(dead_code)]
pub fn daemon_keeping_the_real_home(sandbox: &Path, args: &[&str]) -> Output {
    run_daemon(mcpgw_keeping_the_real_home(sandbox), args)
}

/// How long a spawn waits out an executable somebody else is holding open,
/// and how often it tries again in the meantime.
const BUSY_DEADLINE: Duration = Duration::from_secs(10);
const BUSY_POLL: Duration = Duration::from_millis(50);

/// Runs `attempt` until it stops failing with `ETXTBSY`, and returns what it
/// produced.
///
/// Several tests copy the mcpgw binary into a tempdir of their own and
/// execute the copy. Nothing coordinates them and they share one process: a
/// sibling test forking while a copy's write handle is still open leaves
/// that child holding the descriptor, and Linux refuses to execute a file
/// anyone has open for writing. So a spawn loses a race it took no part in
/// (#74), against a condition that clears by itself the moment the
/// descriptor is gone — which is why this waits rather than reports.
///
/// Only that one error. Anything else is the failure the test is about, and
/// is raised on the first try rather than retried for ten seconds.
#[allow(dead_code)]
pub fn retrying_while_busy<T>(exe: &Path, mut attempt: impl FnMut() -> std::io::Result<T>) -> T {
    let deadline = std::time::Instant::now() + BUSY_DEADLINE;
    loop {
        match attempt() {
            Ok(produced) => return produced,
            Err(err) if err.kind() == std::io::ErrorKind::ExecutableFileBusy => assert!(
                std::time::Instant::now() < deadline,
                "{} was still busy after {BUSY_DEADLINE:?}",
                exe.display()
            ),
            Err(err) => panic!("{}: {err}", exe.display()),
        }
        std::thread::sleep(BUSY_POLL);
    }
}

/// `command.output()`, waiting out a busy executable — see
/// [`retrying_while_busy`].
#[allow(dead_code)]
pub fn output_retrying_while_busy(command: &mut Command) -> Output {
    let exe = PathBuf::from(command.get_program());
    retrying_while_busy(&exe, || command.output())
}

/// `command.spawn()`, waiting out a busy executable — see
/// [`retrying_while_busy`].
///
/// The wait between tries blocks the runtime thread rather than yielding to
/// it. `spawn` is a synchronous call on an async `Command`, the wait is
/// bounded, and a test whose gateway has not started yet has nothing else
/// for that thread to be doing.
#[allow(dead_code)]
pub fn spawn_retrying_while_busy(command: &mut tokio::process::Command) -> tokio::process::Child {
    let exe = PathBuf::from(command.as_std().get_program());
    retrying_while_busy(&exe, || command.spawn())
}

/// `daemon` against a sandbox the caller built itself.
///
/// The live cycles run every command through a copy of mcpgw rather than
/// through the binary cargo built, and each of them sandboxes differently —
/// so what they share is this last step and not the `Command` that reaches
/// it.
#[allow(dead_code)]
pub fn run_daemon(mut mcpgw: Command, args: &[&str]) -> Output {
    output_retrying_while_busy(mcpgw.arg("daemon").args(args))
}

/// A copy of the mcpgw cargo built, under `dir`, executable and ready to be
/// installed as a service.
///
/// The upgrade tests replace the binary a gateway is running, and the file
/// cargo built is the one every other test in the run is about to execute —
/// so they replace a copy of it instead. On macOS the copy also has to sit
/// outside the folders TCC keeps a launch agent out of, which a temp
/// directory is.
#[allow(dead_code)]
pub fn binary_copy(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let copy = dir.join(format!("mcpgw{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(assert_cmd::cargo::cargo_bin("mcpgw"), &copy).unwrap();
    copy
}

/// Publishes a new binary at `path` the way an upgrade does: whole-file, so
/// the stamp the watcher sees is stable from the first tick that sees it.
///
/// Never a write into the file already there. That path is usually a running
/// image, and writing into one is `ETXTBSY` on Linux and a sharing violation
/// on Windows — which is why `brew`, `cargo install` and `self_replace` all
/// publish by renaming a sibling over the path, and why this does too.
/// Windows will not rename *over* a running image but will rename it aside,
/// so there the old one is moved out of the way first.
///
/// The new bytes are the old ones with a few appended. A Mach-O, an ELF and
/// a PE are all described by their headers rather than by their length, so
/// trailing bytes change the stamp without changing what runs — which the
/// caller is told to confirm, because a binary that no longer executes would
/// otherwise show up as a service that mysteriously never came back.
#[allow(dead_code)]
pub fn replace_binary(path: &Path) {
    let mut bytes = std::fs::read(path).unwrap();
    bytes.extend_from_slice(b"an upgrade");
    publish_binary(path, &bytes);
}

/// [`replace_binary`], with the caller saying what the new file holds.
///
/// The tests that publish something which is *not* a working binary need
/// the same rename-into-place, so that what is under test is the gateway
/// running the replacement rather than the gateway catching a half-written
/// file mid-copy.
#[allow(dead_code)]
pub fn publish_binary(path: &Path, bytes: &[u8]) {
    let published = path.with_extension("new");
    std::fs::write(&published, bytes).unwrap();
    // Carried over so the replacement is a plausible binary rather than a
    // 0644 file wearing its name.
    let mode = std::fs::metadata(path).unwrap().permissions();
    std::fs::set_permissions(&published, mode).unwrap();
    if cfg!(windows) {
        let aside = path.with_extension("old");
        let _ = std::fs::remove_file(&aside);
        std::fs::rename(path, &aside).unwrap();
    }
    std::fs::rename(&published, path).unwrap();
}

/// Waits for the gateway on `port` to be a different process that got there
/// by standing aside for a replaced binary, and returns the record it
/// published.
///
/// Both halves are asserted because either alone is a weaker claim: a new pid
/// is any restart at all, and a recorded restart is what the *outgoing*
/// gateway writes on its way out. Only the two together say the supervisor
/// relaunched the replacement.
#[allow(dead_code)]
pub fn wait_for_an_upgrade_restart(
    state: &Path,
    port: u16,
    previous_pid: u32,
    timeout: Duration,
) -> mcpgw_core::runtime::GatewayRecord {
    let deadline = std::time::Instant::now() + timeout;
    let mut last = None;
    loop {
        let record = mcpgw_core::runtime::read_record(state, port).unwrap();
        if let Some(record) = record {
            if record.pid != previous_pid && record.last_upgrade_restart.is_some() {
                return record;
            }
            last = Some(record);
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the gateway on port {port} did not restart onto the replaced binary within \
             {timeout:?} — the record is now {last:?}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[allow(dead_code)]
pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[allow(dead_code)]
pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A loopback port free right now, and deliberately not 8137: whoever runs
/// these very likely has a foreground gateway on the default port.
///
/// Asking for port 0 and dropping the listener leaves the number free for
/// anything else in the run to take, so the candidate is bound a second time
/// before it is handed out and one that has already gone is discarded.
#[allow(dead_code)]
pub fn free_port() -> u16 {
    for _ in 0..64 {
        let candidate = {
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            listener.local_addr().unwrap().port()
        };
        if std::net::TcpListener::bind(("127.0.0.1", candidate)).is_ok() {
            return candidate;
        }
    }
    panic!("no loopback port stayed free long enough to be used");
}

/// Installs a service on a port that was free when it was picked, returning
/// the port and what install said.
///
/// `daemon install` cannot be given port 0 the way `serve` can, so the number
/// has to be chosen before the process starts and the gap between choosing it
/// and binding it belongs to everything else the run is doing. Losing that
/// race is a refusal that names itself, so it is retried with a fresh port
/// rather than reported as a broken installer.
#[allow(dead_code)]
pub fn install_on_a_free_port(mut install: impl FnMut(u16) -> Output) -> (u16, Output) {
    for _ in 0..8 {
        let port = free_port();
        let output = install(port);
        if output.status.success() || !stderr(&output).contains("something already listens") {
            return (port, output);
        }
    }
    panic!("`daemon install` lost the port it picked eight times running");
}

/// How long a banner line may take to arrive. Generous because it covers a
/// cold process start on a runner that is compiling and testing everything
/// else at the same time.
const BANNER_DEADLINE: Duration = Duration::from_secs(60);

/// Spawns a real foreground gateway on the config already written to
/// `home/config.toml`, returning it with the address read off its banner and
/// the banner line that lists the per-server endpoints.
///
/// The port asked for is 0 and the real one is read back, so the address is
/// never guessed and two tests running at once cannot collide.
#[allow(dead_code)]
pub async fn serve(home: &Path, args: &[&str]) -> (tokio::process::Child, String, String) {
    serve_with(home, &["--no-capture"], args).await
}

/// The same, with the capture flags the caller chooses instead of the
/// `--no-capture` every other test wants. Split out rather than made an
/// argument of [`serve`] so no test starts writing a traffic log by accident.
#[allow(dead_code)]
pub async fn serve_with(
    home: &Path,
    capture: &[&str],
    args: &[&str],
) -> (tokio::process::Child, String, String) {
    let mut command = tokio::process::Command::from(mcpgw(home));
    let mut child = spawn_retrying_while_busy(
        command
            .arg("serve")
            .args(["--port", "0"])
            .args(capture)
            .args(args)
            .stdout(Stdio::piped()),
    );

    let (addr, endpoints) = banner(&mut child).await;
    (child, addr, endpoints)
}

/// The same gateway, run from `exe` and with its stderr collected into a
/// string the test can read while it is still running.
///
/// Both halves are what the upgrade tests need: the binary they replace must
/// not be the one the rest of the suite runs, and the line the gateway
/// prints about that replacement is the thing under test.
#[allow(dead_code)]
pub async fn serve_binary(
    exe: &Path,
    home: &Path,
    args: &[&str],
) -> (
    tokio::process::Child,
    String,
    String,
    std::sync::Arc<std::sync::Mutex<String>>,
) {
    let mut command = tokio::process::Command::from(mcpgw_binary(exe, home));
    let mut child = spawn_retrying_while_busy(
        command
            .arg("serve")
            .args(["--port", "0", "--no-capture"])
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    );

    let errors = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    tokio::spawn({
        let errors = std::sync::Arc::clone(&errors);
        async move {
            while let Ok(Some(line)) = lines.next_line().await {
                let mut errors = errors.lock().unwrap();
                errors.push_str(&line);
                errors.push('\n');
            }
        }
    });
    let (addr, endpoints) = banner(&mut child).await;
    (child, addr, endpoints, errors)
}

/// Reads the two banner lines off a freshly spawned gateway and returns the
/// address it bound with the endpoint line, leaving stdout drained.
async fn banner(child: &mut tokio::process::Child) -> (String, String) {
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let listening = banner_line(&mut lines).await;
    let endpoints = banner_line(&mut lines).await;
    let addr = listening
        .split("http://")
        .nth(1)
        .and_then(|rest| rest.split("/mcp").next())
        .unwrap_or_else(|| panic!("no address in banner: {listening}"))
        .to_owned();
    // Kept draining for the life of the gateway. Dropping the read end here
    // would leave the child writing into a closed pipe, and a `println!` that
    // hits EPIPE panics — which killed the gateway mid-test whenever the
    // third banner line happened to land after this function returned.
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    (addr, endpoints)
}

/// One line of the startup banner, waited for to a deadline.
///
/// The child has to spawn, bind and flush before its first line lands, and on
/// a loaded runner none of that is instant. A deadline turns "slower than the
/// test expected" into "waited a minute", and only a genuinely wedged gateway
/// still fails — with a message that says so.
async fn banner_line(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
) -> String {
    tokio::time::timeout(BANNER_DEADLINE, lines.next_line())
        .await
        .expect("the gateway printed no banner line before the deadline")
        .expect("reading the gateway banner")
        .expect("the gateway closed stdout before finishing its banner")
}

/// Writes the record `daemon install` leaves behind, without installing a
/// service: a live install would register a launch agent on whatever machine
/// ran the suite, and the behaviour under test is entirely about what the
/// read-only commands make of what they read back out of it.
///
/// `exe` is a parameter because the two states worth reporting are both
/// about which binary the record names — one that is gone, and one that is
/// not the mcpgw being run.
#[allow(dead_code)]
pub fn record_installed_spec(home: &Path, exe: &Path, bind: &str, port: u16) {
    let state = home.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let path = |name: &str| state.join(name).display().to_string();
    std::fs::write(
        state.join("daemon.json"),
        format!(
            r#"{{"exe":{:?},"config_path":{:?},"state_dir":{:?},
                 "bind":"{bind}","port":{port},
                 "logs":{{"stdout":{:?},"stderr":{:?}}}}}"#,
            exe.display().to_string(),
            home.join("config.toml").display().to_string(),
            state.display().to_string(),
            path("logs/daemon.out.log"),
            path("logs/daemon.err.log"),
        ),
    )
    .unwrap();
}

/// Rewrites the version in the record the gateway at `url` published, so a
/// test can meet an upgraded machine without owning two builds of mcpgw.
///
/// The record lands around the bind rather than before the banner, so it is
/// polled for first — rewriting a file that is not there yet would be a
/// gateway that publishes over the doctored version a moment later.
#[allow(dead_code)]
pub async fn rewrite_record_version(home: &Path, url: &str, version: &str) {
    let state = home.join("state");
    let port = mcpgw_core::daemon_check::url_port(url).unwrap();
    let deadline = std::time::Instant::now() + BANNER_DEADLINE;
    loop {
        if let Some(mut record) = mcpgw_core::runtime::read_record(&state, port).unwrap() {
            version.clone_into(&mut record.version);
            mcpgw_core::runtime::write_record(&state, &record).unwrap();
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no gateway record for port {port} within {BANNER_DEADLINE:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
