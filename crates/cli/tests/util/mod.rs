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

/// A file that runs and answers `--version` the way mcpgw does, and does
/// nothing else, written at `path` and made executable.
///
/// Most of the upgrade tests never run the binary they publish as anything
/// but a `--version`: the supervisor that would relaunch it is the test
/// itself, and what is under test is which line the watcher prints. Forking
/// a full copy of the real CLI for that puts the pre-flight check in
/// competition with every `rustc` the machine is running, which is how those
/// tests came to time out (#218). `verify_runs` only asks for a zero exit
/// and a line starting `mcpgw `, and a shell script answers both in the time
/// it takes to fork.
///
/// `marker` goes into the file, so two stubs published one after the other
/// differ in the length the watcher stats.
#[allow(dead_code)]
pub fn stub_binary(path: &Path, marker: &str) -> PathBuf {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, stub_bytes(marker)).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path.to_owned()
}

/// A stub published over a path that already holds a binary, by the same
/// rename-into-place [`publish_binary`] does.
#[allow(dead_code)]
pub fn publish_stub(path: &Path, marker: &str) {
    publish_binary(path, &stub_bytes(marker));
}

/// Windows has no shebang, so there the cheap stub is the real binary with
/// the marker appended — which is what these tests published before, and
/// costs a fork of the full CLI on the one platform where the pre-flight
/// check has never been the slow part of the run.
fn stub_bytes(marker: &str) -> Vec<u8> {
    #[cfg(unix)]
    {
        format!("#!/bin/sh\n# {marker}\necho \"mcpgw 0.0.0-stub\"\n").into_bytes()
    }
    #[cfg(not(unix))]
    {
        let mut bytes = std::fs::read(assert_cmd::cargo::cargo_bin("mcpgw")).unwrap();
        bytes.extend_from_slice(marker.as_bytes());
        bytes
    }
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

/// A spawned mcpgw process that is killed when the test drops it, however
/// the test ends.
///
/// Every one of these holds a port, and the gateways among them have stdio
/// fixture servers of their own underneath. Killing at the end of the test
/// body — which is what every call site used to do — only runs when the body
/// reaches its end, so one failed assertion left a gateway (and a fixture
/// server parked for an hour) behind for the rest of the run to collide
/// with. A `Drop` runs on the unwind too.
///
/// Derefs to the child, so a test can still wait on it, read its id or take
/// its pipes; what it cannot do is forget to kill it.
#[allow(dead_code)]
pub struct Spawned {
    /// `None` only between [`Spawned::stop`] taking the child and the guard
    /// itself being dropped.
    child: Option<tokio::process::Child>,
}

#[allow(dead_code)]
impl Spawned {
    /// Takes ownership of a child that was spawned with `kill_on_drop`.
    pub fn new(child: tokio::process::Child) -> Self {
        Self { child: Some(child) }
    }

    /// Stops the process the way a supervisor does and waits for it to be
    /// gone.
    ///
    /// For the assertions that are about what happens *after* the process
    /// ends — a released port, a record left behind, a traffic log — where
    /// the guard's own kill would come too late to be observed.
    ///
    /// SIGTERM rather than SIGKILL, because a gateway does its withdrawing
    /// and its last flush on the shutdown path, and only a signal it can
    /// catch reaches that path: a hard kill races the writer thread for
    /// whatever is still queued. The kill is still there as the backstop for
    /// a process that will not go, so a wedged gateway is a failed assertion
    /// rather than a suite that hangs. Windows has no SIGTERM to send.
    pub async fn stop(mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        #[cfg(unix)]
        if let Some(pid) = child.id() {
            // Signalling a pid we own and have not yet reaped, so it cannot
            // have been recycled onto somebody else's process.
            #[allow(clippy::cast_possible_wrap)]
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
            if tokio::time::timeout(STOP_DEADLINE, child.wait())
                .await
                .is_ok()
            {
                return;
            }
        }
        let _ = child.kill().await;
    }

    /// Ends the process the way a crash does: SIGKILL, no shutdown path.
    ///
    /// For the handful of tests whose subject *is* what an unclean end leaves
    /// behind — a runtime record nobody withdrew, say. Everything else wants
    /// [`Spawned::stop`].
    pub async fn stop_hard(mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
        }
    }
}

impl std::ops::Deref for Spawned {
    type Target = tokio::process::Child;

    fn deref(&self) -> &Self::Target {
        self.child.as_ref().expect("the child was already taken")
    }
}

impl std::ops::DerefMut for Spawned {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.child.as_mut().expect("the child was already taken")
    }
}

impl Drop for Spawned {
    fn drop(&mut self) {
        // Signal only: a drop cannot await the wait, and the `kill_on_drop`
        // every call site spawns with is what reaps the corpse.
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
    }
}

/// The same guard around a process spawned by a synchronous test.
#[allow(dead_code)]
pub struct SpawnedBlocking {
    child: std::process::Child,
}

#[allow(dead_code)]
impl SpawnedBlocking {
    pub fn new(child: std::process::Child) -> Self {
        Self { child }
    }
}

impl std::ops::Deref for SpawnedBlocking {
    type Target = std::process::Child;

    fn deref(&self) -> &Self::Target {
        &self.child
    }
}

impl std::ops::DerefMut for SpawnedBlocking {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.child
    }
}

impl Drop for SpawnedBlocking {
    fn drop(&mut self) {
        // Nothing here is async, so this one can reap what it kills.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// How long [`Spawned::stop`] lets a signalled process take to end before it
/// resorts to a kill. Generous for the same reason the banner deadline is:
/// the shutdown drains upstreams and flushes a traffic log on a runner that
/// is busy with the rest of the suite.
const STOP_DEADLINE: Duration = Duration::from_secs(10);

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
pub async fn serve(home: &Path, args: &[&str]) -> (Spawned, String, String) {
    serve_with(home, &["--no-capture"], args).await
}

/// The same, with the capture flags the caller chooses instead of the
/// `--no-capture` every other test wants. Split out rather than made an
/// argument of [`serve`] so no test starts writing a traffic log by accident.
#[allow(dead_code)]
pub async fn serve_with(home: &Path, capture: &[&str], args: &[&str]) -> (Spawned, String, String) {
    let mut command = tokio::process::Command::from(mcpgw(home));
    let mut child = spawn_retrying_while_busy(
        command
            .arg("serve")
            .args(["--port", "0"])
            .args(capture)
            .args(args)
            .stdout(Stdio::piped())
            .kill_on_drop(true),
    );

    let (addr, endpoints) = banner(&mut child).await;
    (Spawned::new(child), addr, endpoints)
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
    Spawned,
    String,
    String,
    std::sync::Arc<std::sync::Mutex<String>>,
) {
    serve_binary_with_env(exe, home, args, &[]).await
}

/// [`serve_binary`], writing a traffic log instead of the `--no-capture` the
/// upgrade tests otherwise all want. Split out for the reason [`serve_with`]
/// is: no test starts logging traffic by accident.
#[allow(dead_code)]
pub async fn serve_binary_capturing(
    exe: &Path,
    home: &Path,
    args: &[&str],
    env: &[(&str, &str)],
) -> (
    Spawned,
    String,
    String,
    std::sync::Arc<std::sync::Mutex<String>>,
) {
    serve_binary_inner(exe, home, &[], args, env).await
}

/// How long a gateway started by the suite gives a replacement to answer
/// `--version`.
///
/// The default is five seconds of wall clock, which is generous for a
/// `println` and not generous at all on a machine that is compiling the rest
/// of the workspace at the same time: the fork can wait that long just to be
/// scheduled, and the gateway then reports a good binary as one that does
/// not run (#218). Nothing here is measuring how fast the check is, so the
/// suite buys room it will not normally use.
#[allow(dead_code)]
pub const TEST_VERIFY_TIMEOUT_SECS: &str = "60";

/// [`serve_binary`], with environment of the caller's choosing on top of the
/// sandbox — including the verify timeout, for the one test that is about
/// the deadline itself rather than working around it.
#[allow(dead_code)]
pub async fn serve_binary_with_env(
    exe: &Path,
    home: &Path,
    args: &[&str],
    env: &[(&str, &str)],
) -> (
    Spawned,
    String,
    String,
    std::sync::Arc<std::sync::Mutex<String>>,
) {
    serve_binary_inner(exe, home, &["--no-capture"], args, env).await
}

async fn serve_binary_inner(
    exe: &Path,
    home: &Path,
    capture: &[&str],
    args: &[&str],
    env: &[(&str, &str)],
) -> (
    Spawned,
    String,
    String,
    std::sync::Arc<std::sync::Mutex<String>>,
) {
    let mut command = tokio::process::Command::from(mcpgw_binary(exe, home));
    command.env(
        mcpgw_core::upgrade::VERIFY_TIMEOUT_ENV,
        TEST_VERIFY_TIMEOUT_SECS,
    );
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = spawn_retrying_while_busy(
        command
            .arg("serve")
            .args(["--port", "0"])
            .args(capture)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true),
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
    (Spawned::new(child), addr, endpoints, errors)
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

/// Writes the service definition this platform's supervisor reads, with
/// `path_env` baked into it the way `mcpgw daemon install` bakes the
/// installing shell's `PATH`.
///
/// Rendered by the same writers the install uses rather than hand-typed, so a
/// change to either format cannot leave a fixture behind claiming a `PATH`
/// mcpgw would no longer read. Unix only: the Windows service records no
/// `PATH` of its own, so there is nothing there for a test to stale.
#[allow(dead_code)]
#[cfg(unix)]
pub fn install_fixture_service(home: &Path, path_env: &str) {
    let state_dir = home.join("state");
    let spec = mcpgw_core::daemon::DaemonSpec {
        exe: home.join("bin/mcpgw"),
        config_path: home.join("config.toml"),
        logs: mcpgw_core::daemon::LogPaths::under_state_dir(&state_dir),
        state_dir,
        bind: "127.0.0.1".to_owned(),
        port: 8137,
    };
    #[cfg(target_os = "macos")]
    let (path, text) = (
        home.join("Library/LaunchAgents/io.mcpgw.gateway.plist"),
        mcpgw_core::daemon::launchd::render_plist(&spec, Some(path_env)),
    );
    #[cfg(not(target_os = "macos"))]
    let (path, text) = (
        home.join(".config/systemd/user/mcpgw.service"),
        mcpgw_core::daemon::systemd::render_unit(&spec, Some(path_env)),
    );
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}
