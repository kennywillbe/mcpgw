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
    let mut command = mcpgw_keeping_the_real_home(home);
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
    let mut command = Command::new(assert_cmd::cargo::cargo_bin("mcpgw"));
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
    mcpgw(home).arg("daemon").args(args).output().unwrap()
}

/// The same, for the one caller that has to keep the real `HOME` — see
/// [`mcpgw_keeping_the_real_home`].
#[allow(dead_code)]
pub fn daemon_keeping_the_real_home(sandbox: &Path, args: &[&str]) -> Output {
    mcpgw_keeping_the_real_home(sandbox)
        .arg("daemon")
        .args(args)
        .output()
        .unwrap()
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
    let mut command = tokio::process::Command::from(mcpgw(home));
    let mut child = command
        .arg("serve")
        .args(["--port", "0", "--no-capture"])
        .args(args)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

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
    (child, addr, endpoints)
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
