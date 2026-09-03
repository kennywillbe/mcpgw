//! End-to-end coverage for the first-run wizard: what a bare `mcpgw` does,
//! what `mcpgw init --yes` does, and what a machine that is already set up
//! is told instead.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

mod util;
use util::fixture_binary;

/// How long a wizard run is given before the test calls it hung. Generous:
/// the only thing this bounds is a genuine deadlock on stdin, and a loaded
/// CI runner is slow, not stuck.
const DEADLINE: Duration = Duration::from_secs(90);

/// A config with one healthy fixture server.
fn config() -> String {
    format!(
        "version = 1\n\n[servers.fx1]\ntype = \"stdio\"\ncommand = '{}'\nargs = [\"healthy\"]\n",
        fixture_binary().display()
    )
}

/// A command that really resolves on this machine, JSON-quoted so it can be
/// dropped straight into a client file. The import step now brings in
/// anything it cannot find on PATH switched off, so a test about dedupe or
/// about what sync does next has to name a command that is actually there.
fn real_command() -> String {
    serde_json::to_string(&fixture_binary().to_string_lossy()).unwrap()
}

/// A `mcpgw` invocation pointed at `home` and nothing of the real machine:
/// its own config, its own state directory, and no XDG override leaking in
/// from the environment the test itself was started in.
fn command(home: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(assert_cmd::cargo::cargo_bin("mcpgw"));
    command
        .env("MCPGW_NO_UPDATE_CHECK", "1")
        .env("MCPGW_CONFIG", home.join("config.toml"))
        .env("MCPGW_STATE_DIR", home.join("state"))
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("APPDATA", home.join("AppData"))
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME");
    command
}

/// Waits for `child` to exit, polling to a deadline rather than blocking:
/// the point of these tests is that the wizard never waits for stdin, and a
/// blocking wait would express that as a hung suite instead of a failure.
async fn finish(mut child: tokio::process::Child, what: &str) -> std::process::Output {
    let deadline = Instant::now() + DEADLINE;
    loop {
        if let Some(_status) = child.try_wait().unwrap() {
            return child.wait_with_output().await.unwrap();
        }
        if Instant::now() >= deadline {
            child.kill().await.unwrap();
            panic!("{what} never exited — it is waiting for something");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Off a terminal there is nobody to answer the wizard's questions, so a
/// bare `mcpgw` is the same missing-subcommand failure it has always been.
#[test]
fn a_bare_run_off_a_terminal_prints_help_and_exits_2() {
    let dir = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("mcpgw"))
        .env("MCPGW_NO_UPDATE_CHECK", "1")
        .env("MCPGW_CONFIG", dir.path().join("config.toml"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
        .wait_with_output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Usage: mcpgw"), "{stderr}");
    assert!(stderr.contains("Commands:"), "{stderr}");
    // The wizard's own opening must not appear on a path that cannot ask.
    assert!(!stderr.contains("let's get your MCP servers"), "{stderr}");
}

/// `--yes` walks the whole wizard with its stdin held open and never
/// written to: if any step reached for a line, this test would hang.
#[tokio::test]
async fn init_yes_walks_every_step_without_reading_stdin() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), config()).unwrap();

    // A port held open by a socket that will never answer HTTP, so the
    // daemon step reports "not running" for a reason the test owns. Pointing
    // at the default 127.0.0.1:8137 instead would read whatever gateway the
    // developer or the runner happens to have up.
    let blocked = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/mcp", blocked.local_addr().unwrap());

    let child = command(dir.path())
        .args(["init", "--yes", "--gateway-url", &url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let output = finish(child, "`mcpgw init --yes`").await;

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("let's get your MCP servers running through one gateway"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Nothing is written until you say yes"),
        "{stdout}"
    );
    // The config already has a server, so the survey is skipped.
    assert!(stdout.contains("skipping the survey"), "{stdout}");
    // The daemon step announces itself and then declines to fight for a port
    // somebody else holds — or, on a platform whose installer has not shipped,
    // says so. Both end at the same offer, which is the assertion that holds
    // on every runner in the matrix.
    assert!(stdout.contains("keep the gateway running"), "{stdout}");
    assert!(stdout.contains("mcpgw serve"), "{stdout}");
    assert!(!stdout.contains("installed at"), "{stdout}");
    // No client is installed under this sandbox home, so the sync step has
    // nowhere to push and writes nothing — but it still closes the wizard.
    assert!(stdout.contains("no MCP client here"), "{stdout}");
    assert!(stdout.contains("Restart your clients"), "{stdout}");
    assert!(!dir.path().join("state").join("managed.json").exists());
}

/// Every step done — servers configured, a gateway answering, a client
/// already synced — and the wizard has nothing to walk anyone through.
#[tokio::test]
async fn an_already_finished_machine_gets_the_status_card() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), config()).unwrap();
    std::fs::create_dir_all(dir.path().join("state")).unwrap();
    std::fs::write(
        dir.path().join("state").join("managed.json"),
        r#"{"clients":{"cursor":["fx1"]}}"#,
    )
    .unwrap();
    // A service installed from an mcpgw that has since been uninstalled: the
    // machine really is set up, so the card still says so and only adds the
    // one line about what the service is pointed at.
    let gone = dir.path().join("cargo").join("bin").join("mcpgw");
    util::record_installed_spec(dir.path(), &gone, "127.0.0.1", 18137);

    // Port 0 and the banner rather than a number of our own: a fixed port
    // is a race against every other test in the suite (#54, #83).
    let mut gateway = command(dir.path())
        .args(["serve", "--port", "0", "--no-capture"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let url = gateway_url(&mut gateway).await;
    // And answering on a build that is not this one, which is the other
    // half the card reports: the two are different facts and both print.
    util::rewrite_record_version(dir.path(), &url, "0.0.1").await;

    let child = command(dir.path())
        .args(["init", "--yes", "--gateway-url", &url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let output = finish(child, "`mcpgw init --yes` against a live gateway").await;
    gateway.kill().await.unwrap();

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("everything is set up"), "{stdout}");
    assert!(!stdout.contains("let's get your MCP servers"), "{stdout}");
    assert!(stdout.contains("1 configured, 1 enabled"), "{stdout}");
    // One per-server endpoint plus the aggregate.
    assert!(stdout.contains("2 endpoints"), "{stdout}");
    assert!(stdout.contains("Cursor"), "{stdout}");
    assert!(stdout.contains("which is gone"), "{stdout}");
    assert!(stdout.contains("mcpgw daemon install"), "{stdout}");
    assert!(
        stdout.contains(&format!(
            "runs mcpgw 0.0.1; you are running {}",
            env!("CARGO_PKG_VERSION")
        )),
        "{stdout}"
    );
    for suggestion in ["mcpgw list", "mcpgw watch", "mcpgw doctor --probe"] {
        assert!(stdout.contains(suggestion), "{stdout}");
    }
}

/// A gateway that is already answering is one the wizard has nothing to add
/// to: the daemon step does not run, and `--yes` does not turn "already
/// working" into a login item nobody asked for.
#[tokio::test]
async fn a_running_gateway_is_left_alone_and_no_service_is_installed() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), config()).unwrap();

    // No `managed.json`, so the last step still has something to say and the
    // wizard walks its steps rather than printing the status card.
    let mut gateway = command(dir.path())
        .args(["serve", "--port", "0", "--no-capture"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let url = gateway_url(&mut gateway).await;

    let child = command(dir.path())
        .args(["init", "--yes", "--gateway-url", &url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let output = finish(child, "`mcpgw init --yes` beside a running gateway").await;
    gateway.kill().await.unwrap();

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("the gateway is already answering"),
        "{stdout}"
    );
    // Not even the offer: the step is skipped outright.
    assert!(!stdout.contains("keep the gateway running"), "{stdout}");
    assert_no_service(dir.path());
}

/// "No" to the login service is an answer, not an error: the step prints the
/// alternative and the wizard carries on.
#[tokio::test]
async fn declining_the_service_prints_the_alternative_and_is_not_a_failure() {
    use tokio::io::AsyncWriteExt as _;

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), config()).unwrap();

    // An address the OS handed out and nothing holds, so the step gets as far
    // as its offer on a platform whose installer has shipped. Asked for rather
    // than picked: a fixed port is a race against the rest of the suite
    // (#54, #83).
    let free = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let url = format!("http://{free}/mcp");

    let mut child = command(dir.path())
        .args(["init", "--gateway-url", &url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // More noes than there are questions, deliberately: an exhausted stdin
    // takes the recommended answer, and the recommended answer here installs
    // a real login service on the machine running the tests.
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all("n\n".repeat(8).as_bytes()).await.unwrap();
    drop(stdin);
    let output = finish(child, "`mcpgw init` answered with no").await;

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("keep the gateway running"), "{stdout}");
    // Whichever way the step ended — declined, or a platform that cannot
    // install one — the user leaves knowing how to run the gateway anyway.
    assert!(stdout.contains("mcpgw serve"), "{stdout}");
    assert!(!stdout.contains("installed at"), "{stdout}");
    assert_no_service(dir.path());
}

/// No test may leave a real login service behind, so every run that could
/// have installed one checks the sandbox home it would have gone into.
/// Installing for real is the platform milestone's own env-gated test.
fn assert_no_service(home: &Path) {
    for candidate in [
        home.join("Library").join("LaunchAgents"),
        home.join(".config").join("systemd"),
    ] {
        assert!(!candidate.exists(), "{} was written", candidate.display());
    }
}

/// A client file under the sandbox home.
fn write_client(home: &Path, rel: &str, json: &str) {
    let path = home.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, json).unwrap();
}

/// An address nothing will ever answer HTTP on, so the daemon step reports
/// "not running" for a reason the test owns rather than finding whatever
/// gateway the runner happens to have up. Port 0 and a fixed port are both
/// wrong here: one is not a real address, the other is a race (#54, #83).
///
/// The listener is returned rather than dropped, and holding it is what
/// keeps these import tests from installing a login service: the daemon
/// step's preflight sees the port taken and offers nothing.
fn dead_gateway() -> (std::net::TcpListener, String) {
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/mcp", held.local_addr().unwrap());
    (held, url)
}

/// Runs the wizard against `home`, feeding it `input` and returning stdout.
async fn wizard(home: &Path, url: &str, extra: &[&str], input: &str) -> String {
    let mut child = command(home)
        .arg("init")
        .args(extra)
        .args(["--gateway-url", url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Written up front and the pipe closed: every question this wizard asks
    // is answered by the same script, and an open stdin with nothing coming
    // is how these tests hang instead of failing.
    //
    // The noes on the end are load-bearing rather than padding: an exhausted
    // stdin takes the *recommended* answer, and the recommended answer to
    // the step after this one installs a login service on the machine
    // running the tests.
    if !input.is_empty() {
        use tokio::io::AsyncWriteExt as _;
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(format!("{input}{}", "n\n".repeat(8)).as_bytes())
            .await
            .unwrap();
        stdin.shutdown().await.unwrap();
    }
    let output = finish(child, "`mcpgw init`").await;
    assert_eq!(output.status.code(), Some(0));
    assert_no_service(home);
    String::from_utf8(output.stdout).unwrap()
}

/// One server configured in two clients is one server in the config, and the
/// wizard says so before writing rather than leaving the merge to be noticed
/// in `mcpgw list` afterwards.
#[tokio::test]
async fn import_dedupes_a_server_two_clients_share_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let cmd = real_command();
    write_client(
        dir.path(),
        ".cursor/mcp.json",
        &format!(
            r#"{{"mcpServers": {{
            "github": {{"command": {cmd}, "args": ["server-github"]}},
            "notes": {{"command": {cmd}}}
        }}}}"#
        ),
    );
    write_client(
        dir.path(),
        ".claude.json",
        &format!(
            r#"{{"mcpServers": {{"github": {{"command": {cmd}, "args": ["server-github"]}}}}}}"#
        ),
    );

    let (_held, url) = dead_gateway();
    let stdout = wizard(dir.path(), &url, &["--yes"], "").await;

    // One shared server, so the whole sentence has to be in the singular —
    // this line is on the most-read screen mcpgw has.
    assert!(
        stdout.contains(
            "1 of them is the same server configured in more than one place — I'll keep one copy:"
        ),
        "{stdout}"
    );
    assert!(stdout.contains("github — from"), "{stdout}");
    assert!(stdout.contains("Claude Code"), "{stdout}");
    assert!(stdout.contains("Cursor"), "{stdout}");
    assert!(
        stdout.contains("The rest come across as they are: notes."),
        "{stdout}"
    );

    // `--yes` never read a line, and still wrote both servers and adopted
    // all three client entries.
    let config = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
    assert!(config.contains("[servers.github]"), "{config}");
    assert!(config.contains("[servers.notes]"), "{config}");
    let state = std::fs::read_to_string(dir.path().join("state").join("managed.json")).unwrap();
    assert!(state.contains("claude-code"), "{state}");
    assert!(state.contains("cursor"), "{state}");
}

/// Two clients using one name for two different servers is the case where
/// the plan quietly invents a name, so both survivors are shown with the
/// client they came from and what each one actually runs — key names only,
/// never the token that sits in a header or an env var.
#[tokio::test]
async fn import_shows_both_sides_of_a_name_two_clients_disagree_on() {
    let dir = tempfile::tempdir().unwrap();
    write_client(
        dir.path(),
        ".cursor/mcp.json",
        r#"{"mcpServers": {
            "db": {"command": "db-mcp", "args": ["--local"], "env": {"DB_TOKEN": "hunter2"}}
        }}"#,
    );
    write_client(
        dir.path(),
        ".claude.json",
        r#"{"mcpServers": {"db": {"type": "http", "url": "https://db.example.com/mcp",
            "headers": {"Authorization": "Bearer hunter2"}}}}"#,
    );

    let (_held, url) = dead_gateway();
    let stdout = wizard(dir.path(), &url, &["--yes"], "").await;

    assert!(
        stdout.contains("but configure it differently, so both are kept"),
        "{stdout}"
    );
    assert!(stdout.contains("db-mcp --local"), "{stdout}");
    assert!(stdout.contains("https://db.example.com/mcp"), "{stdout}");
    assert!(stdout.contains("db-2"), "{stdout}");
    // The names of an env var and a header are context; their values are
    // credentials, and this transcript ends up in bug reports.
    assert!(stdout.contains("(env: DB_TOKEN)"), "{stdout}");
    assert!(stdout.contains("(headers: Authorization)"), "{stdout}");
    assert!(!stdout.contains("hunter2"), "{stdout}");

    let config = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
    assert!(config.contains("[servers.db]"), "{config}");
    assert!(config.contains("[servers.db-2]"), "{config}");
}

/// The escape hatch: one line of names instead of yes, and those servers are
/// left where they are — not written, and not adopted either, so nothing
/// claims to manage a client entry the user kept for themselves.
#[tokio::test]
async fn import_leaves_out_the_names_you_type() {
    let dir = tempfile::tempdir().unwrap();
    write_client(
        dir.path(),
        ".cursor/mcp.json",
        r#"{"mcpServers": {
            "github": {"command": "npx", "args": ["server-github"]},
            "notes": {"command": "notes-mcp"},
            "linear": {"type": "http", "url": "https://mcp.linear.app/mcp"}
        }}"#,
    );

    let (_held, url) = dead_gateway();
    // `y` for the survey, then the two names to skip.
    let stdout = wizard(dir.path(), &url, &[], "y\ngithub, linear\n").await;

    assert!(
        stdout.contains("or type names to leave out, comma-separated"),
        "{stdout}"
    );
    assert!(stdout.contains("Imported 1 server."), "{stdout}");

    let config = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
    assert!(config.contains("[servers.notes]"), "{config}");
    assert!(!config.contains("[servers.github]"), "{config}");
    assert!(!config.contains("[servers.linear]"), "{config}");
    let state = std::fs::read_to_string(dir.path().join("state").join("managed.json")).unwrap();
    assert!(state.contains("notes"), "{state}");
    assert!(!state.contains("github"), "{state}");
}

/// A name that is not in the plan is a typo, not an instruction: the wizard
/// says which names it has and asks again rather than importing everything.
#[tokio::test]
async fn a_name_that_is_not_in_the_plan_is_asked_again() {
    let dir = tempfile::tempdir().unwrap();
    write_client(
        dir.path(),
        ".cursor/mcp.json",
        r#"{"mcpServers": {"github": {"command": "npx", "args": ["server-github"]}}}"#,
    );

    let (_held, url) = dead_gateway();
    let stdout = wizard(dir.path(), &url, &[], "y\ngithbu\ngithub\n").await;

    assert!(stdout.contains("I don't have a server called"), "{stdout}");
    assert!(stdout.contains("the names are: github"), "{stdout}");
    assert!(stdout.contains("Imported 0 servers."), "{stdout}");
    let config = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
    assert!(!config.contains("[servers.github]"), "{config}");
}

/// The seam between this step and the one that pushes: importing a client's
/// servers adopts that client's entries, and adoption must not be mistaken
/// for "already synced". A first run that imports has to go on to point the
/// clients at the gateway, or it leaves the machine half set up.
#[tokio::test]
async fn what_the_import_step_adopts_is_still_pushed_by_the_sync_step() {
    let dir = tempfile::tempdir().unwrap();
    write_client(
        dir.path(),
        ".cursor/mcp.json",
        &format!(
            r#"{{"mcpServers": {{"notes": {{"command": {}}}}}}}"#,
            real_command()
        ),
    );

    let (_held, url) = dead_gateway();
    let stdout = wizard(dir.path(), &url, &["--yes"], "").await;

    assert!(stdout.contains("Imported 1 server."), "{stdout}");
    assert!(!stdout.contains("nothing to push"), "{stdout}");
    assert!(
        stdout.contains("Pointing your clients at the gateway"),
        "{stdout}"
    );

    // The entry the wizard adopted a moment ago now points at the gateway,
    // which is the whole promise of a first run.
    let entries: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join(".cursor/mcp.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        entries["mcpServers"]["notes"]["url"],
        url.replace("/mcp", "/s/notes")
    );
}

/// Re-running over a config that already holds a *different* server under
/// one of the names keeps what is already there. The wizard never overwrites
/// the canonical config — that decision belongs to `mcpgw import`.
#[tokio::test]
async fn import_never_overwrites_what_the_config_already_says() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        "version = 1\n\n[servers.github]\ntype = \"stdio\"\ncommand = \"mine\"\n",
    )
    .unwrap();
    write_client(
        dir.path(),
        ".cursor/mcp.json",
        r#"{"mcpServers": {
            "github": {"command": "npx", "args": ["server-github"]},
            "notes": {"command": "notes-mcp"}
        }}"#,
    );

    let (_held, url) = dead_gateway();
    let stdout = wizard(dir.path(), &url, &["--yes"], "").await;

    assert!(
        stdout.contains("differ from what your config already says"),
        "{stdout}"
    );
    assert!(stdout.contains("github left alone"), "{stdout}");

    let config = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
    assert!(config.contains("mine"), "{config}");
    assert!(!config.contains("server-github"), "{config}");
    assert!(config.contains("[servers.notes]"), "{config}");
}

/// The third answer #82 added: keep the canonical entry *and* bring the
/// client's differing one in beside it, so the client stops being an
/// unmanaged entry talking to its origin behind the gateway's back.
#[tokio::test]
async fn a_conflict_can_be_kept_both_ways() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        "version = 1\n\n[servers.github]\ntype = \"stdio\"\ncommand = \"mine\"\n",
    )
    .unwrap();
    write_client(
        dir.path(),
        ".cursor/mcp.json",
        r#"{"mcpServers": {
            "github": {"command": "npx", "args": ["server-github"]},
            "notes": {"command": "notes-mcp"}
        }}"#,
    );

    let (_held, url) = dead_gateway();
    // Yes to the survey, yes to the import, then the second option: keep both.
    let stdout = wizard(dir.path(), &url, &[], "y\ny\n2\n").await;

    assert!(
        stdout.contains("Keep both — bring your client's copy in as github-2"),
        "{stdout}"
    );
    assert!(
        stdout.contains("github-2 brought in — your github is untouched"),
        "{stdout}"
    );

    let config = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
    assert!(config.contains("command = \"mine\""), "{config}");
    assert!(config.contains("[servers.github-2]"), "{config}");
    assert!(config.contains("server-github"), "{config}");
    // Adopted, which is the whole point: the next sync owns that entry and
    // points it at the gateway.
    let state = std::fs::read_to_string(dir.path().join("state").join("managed.json")).unwrap();
    assert!(state.contains("github"), "{state}");
}

/// The same question's third answer, which is the one that loses data — so
/// it is last in the list and has to be picked on purpose.
#[tokio::test]
async fn a_conflict_can_be_overwritten_on_purpose() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        "version = 1\n\n[servers.github]\ntype = \"stdio\"\ncommand = \"mine\"\n",
    )
    .unwrap();
    write_client(
        dir.path(),
        ".cursor/mcp.json",
        r#"{"mcpServers": {
            "github": {"command": "npx", "args": ["server-github"]},
            "notes": {"command": "notes-mcp"}
        }}"#,
    );

    let (_held, url) = dead_gateway();
    let stdout = wizard(dir.path(), &url, &[], "y\ny\n3\n").await;

    assert!(
        stdout.contains("github replaced with your client's copy"),
        "{stdout}"
    );
    let config = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
    assert!(config.contains("server-github"), "{config}");
    assert!(!config.contains("command = \"mine\""), "{config}");
    assert!(!config.contains("[servers.github-2]"), "{config}");
}

/// Reads the served address out of the gateway's own banner and keeps
/// draining its stdout, so a later banner line cannot hit a closed pipe.
async fn gateway_url(child: &mut tokio::process::Child) -> String {
    use tokio::io::{AsyncBufReadExt as _, BufReader};

    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let banner = lines.next_line().await.unwrap().unwrap();
    let url = banner
        .split_whitespace()
        .find(|word| word.starts_with("http://"))
        .unwrap_or_else(|| panic!("no address in banner: {banner}"))
        .to_owned();
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    url
}

/// A Cursor config with an entry mcpgw did not write. It has to survive the
/// wizard untouched and be named as somebody else's on the way past.
const CURSOR_WITH_A_HAND_MADE_ENTRY: &str = r#"{
  "mcpServers": {
    "notes": { "command": "notes-mcp" }
  }
}"#;

/// Mirrors `ClientKind::config_path` for Claude Desktop under the sandbox
/// environment [`command`] builds.
fn claude_desktop_config(home: &Path) -> std::path::PathBuf {
    let app_data = if cfg!(target_os = "macos") {
        home.join("Library/Application Support")
    } else if cfg!(windows) {
        home.join("AppData")
    } else {
        home.join(".config")
    };
    app_data.join("Claude/claude_desktop_config.json")
}

/// Two clients on the machine: Cursor, which holds http entries and already
/// has a hand-made one, and Claude Desktop, which cannot and gets the stdio
/// bridge. Claude Desktop is installed but unconfigured, so the wizard has to
/// create its file as well as write into one.
///
/// The config gains a *disabled* `notes` so these tests are about the sync
/// step and nothing else. A hand-made entry the canonical config has never
/// heard of is something the import step now adopts, one step earlier — the
/// entry would arrive here as mcpgw's own and there would be no foreign
/// entry left to leave untouched. Disabled means known, so import has
/// nothing to do, and not published, so sync has no reason to write it.
fn install_two_clients(home: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let cursor = home.join(".cursor/mcp.json");
    std::fs::create_dir_all(cursor.parent().unwrap()).unwrap();
    std::fs::write(&cursor, CURSOR_WITH_A_HAND_MADE_ENTRY).unwrap();

    let config = home.join("config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str(
        "\n[servers.notes]\nenabled = false\ntype = \"stdio\"\ncommand = \"notes-mcp\"\n",
    );
    std::fs::write(&config, text).unwrap();

    let claude = claude_desktop_config(home);
    std::fs::create_dir_all(claude.parent().unwrap()).unwrap();
    (cursor, claude)
}

fn json_at(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The whole point of the step, end to end: every client ends up pointing at
/// the gateway by the server's own name, and the wizard proves it by dialing
/// the endpoint the clients were just told to use.
#[tokio::test]
async fn the_sync_step_points_every_client_at_a_live_gateway_and_checks_it() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), config()).unwrap();
    let (cursor, claude) = install_two_clients(dir.path());

    let mut gateway = command(dir.path())
        .args(["serve", "--port", "0", "--no-capture"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let url = gateway_url(&mut gateway).await;

    let child = command(dir.path())
        .args(["init", "--yes", "--gateway-url", &url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let output = finish(child, "`mcpgw init --yes` with two clients installed").await;
    gateway.kill().await.unwrap();

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();

    // The plan, per client, by name — and the entry that is not mcpgw's.
    assert!(
        stdout.contains("Pointing your clients at the gateway"),
        "{stdout}"
    );
    assert!(stdout.contains("+ fx1"), "{stdout}");
    assert!(stdout.contains("Claude Desktop"), "{stdout}");
    assert!(
        stdout.contains("notes (not mine — left untouched)"),
        "{stdout}"
    );

    // The reassurance, then exactly one question for the whole set.
    assert!(
        stdout.contains("Each server keeps its name and its entry"),
        "{stdout}"
    );
    assert!(stdout.contains("Tool names don't change"), "{stdout}");
    assert!(stdout.contains("mcpgw sync --rollback"), "{stdout}");
    assert_eq!(stdout.matches("[Y/n] y").count(), 1, "{stdout}");

    let endpoint = url.replace("/mcp", "/s/fx1");
    let entries = json_at(&cursor)["mcpServers"].clone();
    assert_eq!(entries["fx1"]["url"], endpoint);
    assert_eq!(entries["fx1"]["type"], "http");
    // The hand-made entry is exactly as it was left.
    assert_eq!(entries["notes"]["command"], "notes-mcp");

    // Claude Desktop holds no http entry, so it gets the bridge — the
    // gateway's own URL plus the server name, not a path shape.
    let bridged = json_at(&claude)["mcpServers"]["fx1"].clone();
    assert!(
        bridged["command"].as_str().unwrap().contains("mcpgw"),
        "{bridged}"
    );
    assert_eq!(
        bridged["args"],
        serde_json::json!(["connect", "--server", "fx1", "--url", url])
    );

    // mcpgw's own record of what it wrote, which is what `sync` and `doctor`
    // read to tell its entries from the user's.
    let state = json_at(&dir.path().join("state/managed.json"));
    assert_eq!(state["clients"]["cursor"], serde_json::json!(["fx1"]));
    assert_eq!(
        state["clients"]["claude-desktop"],
        serde_json::json!(["fx1"])
    );

    // And the half that decides whether any of it worked: the gateway
    // answering, the server's endpoint answering through it, and both
    // clients landing on it.
    assert!(
        stdout.contains("Checking that it actually works"),
        "{stdout}"
    );
    assert!(stdout.contains("gateway answering at"), "{stdout}");
    assert!(stdout.contains(&format!("{endpoint} — ")), "{stdout}");
    assert!(stdout.contains("tools"), "{stdout}");
    assert_eq!(
        stdout
            .matches("pointing at an endpoint that answers")
            .count(),
        2,
        "{stdout}"
    );

    // The line whose absence turns every first run into a bug report.
    assert!(
        stdout.contains("Done. Restart your clients to pick up the new config."),
        "{stdout}"
    );
    for suggestion in [
        "mcpgw watch",
        "mcpgw add",
        "mcpgw doctor --probe",
        "mcpgw eject",
    ] {
        assert!(stdout.contains(suggestion), "{stdout}");
    }
}

/// The daemon step was skipped, so there is nothing to check against. The
/// config the wizard wrote is still correct, and saying so — with the two
/// commands that finish the job — beats failing a run that did its work.
#[tokio::test]
async fn a_gateway_that_is_down_is_reported_honestly_and_does_not_fail_the_run() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), config()).unwrap();
    let (cursor, _claude) = install_two_clients(dir.path());

    // A port held open by a socket that never answers HTTP, so "down" is a
    // state this test owns rather than whatever is running on the machine.
    let blocked = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/mcp", blocked.local_addr().unwrap());

    let child = command(dir.path())
        .args(["init", "--yes", "--gateway-url", &url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let output = finish(child, "`mcpgw init --yes` against a gateway that is down").await;

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();

    // Written anyway: the entries are right, they simply have nothing to
    // reach yet.
    assert_eq!(
        json_at(&cursor)["mcpServers"]["fx1"]["url"],
        url.replace("/mcp", "/s/fx1")
    );

    assert!(
        stdout.contains("Checking that it actually works"),
        "{stdout}"
    );
    assert!(stdout.contains("nothing is answering at"), "{stdout}");
    assert!(stdout.contains("mcpgw daemon install"), "{stdout}");
    assert!(stdout.contains("mcpgw serve"), "{stdout}");
    // No endpoint was dialed, so nothing may claim one answered.
    assert!(!stdout.contains("tools"), "{stdout}");
    assert!(stdout.contains("Restart your clients"), "{stdout}");
}

/// Second time round there is nothing left to push, and the wizard says so in
/// one dim line rather than walking the step again.
#[tokio::test]
async fn a_second_run_has_nothing_left_to_push() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), config()).unwrap();
    install_two_clients(dir.path());

    // A held port rather than a running gateway, for two reasons: the daemon
    // step keeps something to say on the second run, which is what makes the
    // wizard walk its steps and print this step's dim line rather than the
    // status card — and a port somebody else holds is a port no login
    // service can be installed on, so `--yes` installs nothing.
    let blocked = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/mcp", blocked.local_addr().unwrap());

    for run in 1..=2 {
        let child = command(dir.path())
            .args(["init", "--yes", "--gateway-url", &url])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let output = finish(child, "`mcpgw init --yes`").await;
        assert_eq!(output.status.code(), Some(0));
        let stdout = String::from_utf8(output.stdout).unwrap();

        if run == 1 {
            assert!(stdout.contains("+ fx1"), "{stdout}");
        } else {
            // The gateway nobody is running keeps the daemon step pending, so
            // the wizard still walks — and the sync step is the one with
            // nothing to say.
            assert!(stdout.contains("nothing to push"), "{stdout}");
            assert!(!stdout.contains("Point them at the gateway?"), "{stdout}");
        }
        assert_no_service(dir.path());
    }
}

/// Two clients holding one remote server under two tokens: structurally two
/// servers, in practice one. The step says what it sees before it writes
/// anything — and never what the tokens are.
const SHARED_ADDRESS_EXPLANATION: &str = "points at the same address as context7, \
     also being imported, with different credentials — probably the same server.";

fn shared_address_clients(home: &Path) {
    write_client(
        home,
        ".cursor/mcp.json",
        r#"{"mcpServers": {"context7": {"type": "http", "url": "https://mcp.context7.com/mcp",
            "headers": {"Authorization": "Bearer cursor-secret"}}}}"#,
    );
    write_client(
        home,
        ".claude.json",
        r#"{"mcpServers": {"context7": {"type": "http", "url": "https://mcp.context7.com/mcp",
            "headers": {"Authorization": "Bearer claude-secret"}}}}"#,
    );
}

/// `--yes` cannot ask, so it keeps both — what every earlier release did —
/// but it still prints the explanation, because a user who is later surprised
/// by `context7-2` deserves to find the reason in their scrollback.
#[tokio::test]
async fn a_shared_address_is_explained_and_yes_keeps_both() {
    let dir = tempfile::tempdir().unwrap();
    shared_address_clients(dir.path());

    let (_held, url) = dead_gateway();
    let stdout = wizard(dir.path(), &url, &["--yes"], "").await;

    assert!(stdout.contains(SHARED_ADDRESS_EXPLANATION), "{stdout}");
    assert!(stdout.contains("Keep both"), "{stdout}");
    assert!(!stdout.contains("secret"), "{stdout}");

    let config = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
    assert!(config.contains("[servers.context7]"), "{config}");
    assert!(config.contains("[servers.context7-2]"), "{config}");
}

/// Answered by a human who recognises their own server, the second copy is
/// left where it is: not written, and not adopted either, so nothing claims
/// to manage a client entry the user kept for themselves.
#[tokio::test]
async fn a_shared_address_answered_keep_one_skips_the_incoming_copy() {
    let dir = tempfile::tempdir().unwrap();
    shared_address_clients(dir.path());

    let (_held, url) = dead_gateway();
    // Yes to the survey, yes to the import, then the second option: keep only
    // the copy that is already coming in.
    let stdout = wizard(dir.path(), &url, &[], "y\ny\n2\n").await;

    assert!(stdout.contains(SHARED_ADDRESS_EXPLANATION), "{stdout}");
    assert!(stdout.contains("Keep just context7 —"), "{stdout}");
    assert!(stdout.contains("Imported 1 server."), "{stdout}");

    let config = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
    assert!(config.contains("[servers.context7]"), "{config}");
    assert!(!config.contains("[servers.context7-2]"), "{config}");
    // One of the two client entries stays the user's own.
    let state = std::fs::read_to_string(dir.path().join("state").join("managed.json")).unwrap();
    assert_eq!(state.matches("context7").count(), 1, "{state}");
}

/// The first-run bug this step now guards against: a client carrying an entry
/// for something that is not installed here. Importing it enabled put it in
/// front of every client and turned one stale entry into one verify failure
/// per client, so it comes in switched off — and the plan says so before the
/// question, not afterwards.
#[tokio::test]
async fn a_server_this_machine_cannot_start_is_imported_switched_off() {
    let dir = tempfile::tempdir().unwrap();
    write_client(
        dir.path(),
        ".cursor/mcp.json",
        &format!(
            r#"{{"mcpServers": {{
            "node_repl": {{"command": "/Applications/Gone.app/cua_node/bin/node_repl"}},
            "notes": {{"command": {}}}
        }}}}"#,
            real_command()
        ),
    );

    let (_held, url) = dead_gateway();
    let stdout = wizard(dir.path(), &url, &["--yes"], "").await;

    assert!(
        stdout.contains(
            "node_repl — command not found on this machine, importing disabled \
             (enable later: mcpgw toggle node_repl)"
        ),
        "{stdout}"
    );
    // Explained on its own line, so it is not also listed among the entries
    // that need no explanation.
    assert!(
        stdout.contains("The rest come across as they are: notes."),
        "{stdout}"
    );

    let config = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
    assert!(config.contains("[servers.node_repl]"), "{config}");
    assert!(config.contains("enabled = false"), "{config}");

    // And the point of all of it: the client is never pointed at an endpoint
    // the gateway does not publish.
    let entries = json_at(&dir.path().join(".cursor/mcp.json"))["mcpServers"].clone();
    assert!(entries["node_repl"].get("url").is_none(), "{entries}");
    assert_eq!(entries["notes"]["url"], url.replace("/mcp", "/s/notes"));
}

/// Runs a plain `mcpgw` subcommand against a wizard sandbox and returns its
/// stdout, so a test can carry on past `init` into the sync and eject that
/// act on what the wizard recorded.
async fn mcpgw(home: &Path, args: &[&str]) -> String {
    let output = command(home).args(args).output().await.unwrap();
    assert!(
        output.status.success(),
        "mcpgw {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// Keep-both, followed all the way to the client file — which is where the
/// answer either means what the user said or does the opposite of it.
///
/// The user was asked whether their client's `github` was the canonical
/// `github`, and said no. So their entry follows *their* server, which came
/// in as `github-2`; pointing it at `/s/github` would repoint it at the one
/// server they had just ruled out. The canonical `github` is not written into
/// this client at all — that name is spoken for here — and it is said out
/// loud rather than dropped in silence.
#[tokio::test]
async fn a_conflict_answered_keep_both_is_adopted_beside_canonical() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        "version = 1\n\n[servers.github]\ntype = \"stdio\"\ncommand = \"mine\"\n",
    )
    .unwrap();
    let client = dir.path().join(".cursor/mcp.json");
    let original = format!(
        r#"{{"mcpServers": {{"github": {{"command": {}, "args": ["healthy"]}}}}}}"#,
        real_command()
    );
    write_client(dir.path(), ".cursor/mcp.json", &original);

    let (_held, url) = dead_gateway();
    // The import step opens even though the client holds no unknown name, and
    // the conflict it opens for is answered with the second option: keep both.
    let stdout = wizard(dir.path(), &url, &[], "y\n2\n").await;
    assert!(
        stdout.contains("github-2 brought in — your github is untouched"),
        "{stdout}"
    );

    let state: serde_json::Value = json_at(&dir.path().join("state/managed.json"));
    assert_eq!(state["clients"]["cursor"][0], "github");
    assert_eq!(state["resolved"]["cursor"]["github"], "github-2");

    let out = mcpgw(dir.path(), &["sync", "--gateway-url", &url]).await;
    let entries = json_at(&client)["mcpServers"].clone();
    assert_eq!(
        entries["github"]["url"],
        url.replace("/mcp", "/s/github-2"),
        "{entries}"
    );
    // Their server, under their name, and nothing else added beside it.
    assert_eq!(entries.as_object().unwrap().len(), 1, "{entries}");
    assert!(out.contains("github not written here"), "{out}");

    // And the way back: the entry goes to the definition it stood for, under
    // the name it has always had.
    mcpgw(dir.path(), &["eject", "--yes"]).await;
    assert_eq!(
        json_at(&client),
        serde_json::from_str::<serde_json::Value>(&original).unwrap()
    );
}

/// The conflict question is asked once. An answer of "keep yours" writes
/// nothing, so the only thing that can stop the next `mcpgw init` asking
/// again is the record of having asked.
#[tokio::test]
async fn a_conflict_left_alone_is_not_asked_about_twice() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        "version = 1\n\n[servers.github]\ntype = \"stdio\"\ncommand = \"mine\"\n",
    )
    .unwrap();
    write_client(
        dir.path(),
        ".cursor/mcp.json",
        &format!(
            r#"{{"mcpServers": {{"github": {{"command": {}, "args": ["healthy"]}}}}}}"#,
            real_command()
        ),
    );

    let (_held, url) = dead_gateway();
    let first = wizard(dir.path(), &url, &[], "y\n1\n").await;
    assert!(
        first.contains("differs from the canonical entry"),
        "{first}"
    );

    let second = wizard(dir.path(), &url, &[], "y\n1\n").await;
    assert!(
        !second.contains("differs from the canonical entry"),
        "{second}"
    );
}

/// `--yes` reaches keep-canonical by taking the default, not by asking. That
/// is not an answer, so it must not be remembered as one — the next run at a
/// real terminal still owes the user the question.
#[tokio::test]
async fn yes_keeps_canonical_without_recording_an_answer() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        "version = 1\n\n[servers.github]\ntype = \"stdio\"\ncommand = \"mine\"\n",
    )
    .unwrap();
    write_client(
        dir.path(),
        ".cursor/mcp.json",
        &format!(
            r#"{{"mcpServers": {{"github": {{"command": {}, "args": ["healthy"]}}}}}}"#,
            real_command()
        ),
    );

    let (_held, url) = dead_gateway();
    wizard(dir.path(), &url, &["--yes"], "").await;

    let state: serde_json::Value = json_at(&dir.path().join("state/managed.json"));
    assert!(state["resolved"].get("cursor").is_none(), "{state}");

    let asked = wizard(dir.path(), &url, &[], "y\n1\n").await;
    assert!(
        asked.contains("differs from the canonical entry"),
        "{asked}"
    );
}
