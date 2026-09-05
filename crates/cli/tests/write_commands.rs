use std::path::{Path, PathBuf};
use std::process::Output;

use assert_cmd::Command;

mod util;

/// `config` always sits in a temp directory of the test's own, and that
/// directory is the home every run gets: `add`, `remove`, `enable` and
/// `disable` sync the clients now, so a run that inherited the real home
/// would rewrite the client files of whoever is running the suite.
fn mcpgw(config: &Path, args: &[&str]) -> Output {
    let home = config.parent().unwrap();
    Command::cargo_bin("mcpgw")
        .unwrap()
        // Hermetic: no test may phone home for a version notice.
        .env("MCPGW_NO_UPDATE_CHECK", "1")
        .args(args)
        .env("MCPGW_CONFIG", config)
        .env("MCPGW_STATE_DIR", home.join("state"))
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("APPDATA", home.join("AppData"))
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .output()
        .unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).unwrap()
}

fn list_json(config: &Path) -> serde_json::Value {
    list_json_with(config, &[])
}

fn list_json_with(config: &Path, extra: &[&str]) -> serde_json::Value {
    let mut args = vec!["list", "--json"];
    args.extend_from_slice(extra);
    let out = mcpgw(config, &args);
    assert!(out.status.success());
    serde_json::from_slice(&out.stdout).unwrap()
}

fn temp_config() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    (dir, path)
}

#[test]
fn add_creates_config_from_template() {
    let (_dir, config) = temp_config();
    let out = mcpgw(
        &config,
        &[
            "add",
            "github",
            "--env",
            "TOKEN=t",
            "--tag",
            "work",
            "--",
            "npx",
            "-y",
            "@modelcontextprotocol/server-github",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));

    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.starts_with("# mcpgw canonical config"));

    let json = list_json(&config);
    let github = &json["servers"]["github"];
    assert_eq!(github["type"], "stdio");
    assert_eq!(github["command"], "npx");
    // Masked by default; the key stays so the entry is still readable.
    assert_eq!(github["env"]["TOKEN"], "***");
    assert_eq!(github["enabled"], true);
    assert_eq!(github["tags"][0], "work");

    let unmasked = list_json_with(&config, &["--show-secrets"]);
    assert_eq!(unmasked["servers"]["github"]["env"]["TOKEN"], "t");
}

/// The warning that turns "the daemon says connection closed" into a
/// sentence: the command runs here, and the PATH the service was installed
/// with cannot find it.
#[cfg(unix)]
#[test]
fn add_warns_when_the_daemon_could_not_start_the_command() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    let empty = dir.path().join("empty-bin");
    std::fs::create_dir(&empty).unwrap();
    util::install_fixture_service(dir.path(), &empty.display().to_string());

    let out = add_in_home(dir.path(), &config, &["add", "build", "--", "cargo", "mcp"]);
    let said = stderr(&out);
    assert!(out.status.success(), "{said}");
    assert!(said.contains("resolves in your shell"), "{said}");
    assert!(said.contains("`mcpgw daemon install`"), "{said}");
    assert!(said.contains("--env PATH="), "{said}");

    // The entry is still written exactly as it was asked for: the daemon's
    // PATH is a fact about this machine, not a reason to refuse a command.
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.contains("command = \"cargo\""), "{text}");
    assert!(text.contains("args = [\"mcp\"]"), "{text}");

    // A server carrying its own PATH reaches the child whatever the service
    // was installed with, so there is nothing to warn about.
    let out = add_in_home(
        dir.path(),
        &config,
        &[
            "add",
            "build",
            "--force",
            "--env",
            "PATH=/whatever",
            "--",
            "cargo",
            "mcp",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        !stderr(&out).contains("resolves in your shell"),
        "{}",
        stderr(&out)
    );
}

/// `add` in a home of the test's own, which is where the service definition
/// it reads has to live.
#[cfg(unix)]
fn add_in_home(home: &Path, config: &Path, args: &[&str]) -> Output {
    Command::cargo_bin("mcpgw")
        .unwrap()
        .env("MCPGW_NO_UPDATE_CHECK", "1")
        .args(args)
        .env("MCPGW_CONFIG", config)
        .env("HOME", home)
        .env("MCPGW_STATE_DIR", home.join("state"))
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .output()
        .unwrap()
}

#[test]
fn add_disabled_then_enable() {
    let (_dir, config) = temp_config();
    let out = mcpgw(
        &config,
        &["add", "linear", "--disabled", "--url", "https://x"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(list_json(&config)["servers"]["linear"]["enabled"], false);

    let out = mcpgw(&config, &["enable", "linear"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(list_json(&config)["servers"]["linear"]["enabled"], true);
}

#[test]
fn duplicate_add_piped_needs_force() {
    let (_dir, config) = temp_config();
    assert!(
        mcpgw(&config, &["add", "a", "--url", "https://x"])
            .status
            .success()
    );

    // stdin is piped in tests, so the interactive prompt must not trigger.
    let out = mcpgw(&config, &["add", "a", "--url", "https://y"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("--force"));
    assert_eq!(list_json(&config)["servers"]["a"]["url"], "https://x");

    let out = mcpgw(&config, &["add", "a", "--force", "--url", "https://y"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("updated"));
    assert_eq!(list_json(&config)["servers"]["a"]["url"], "https://y");
}

#[test]
fn remove_piped_needs_yes() {
    let (_dir, config) = temp_config();
    assert!(
        mcpgw(&config, &["add", "a", "--url", "https://x"])
            .status
            .success()
    );

    let out = mcpgw(&config, &["remove", "a"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("--yes"));

    let out = mcpgw(&config, &["remove", "a", "--yes"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(list_json(&config)["servers"], serde_json::json!({}));
}

#[test]
fn url_and_command_together_rejected() {
    let (_dir, config) = temp_config();
    let out = mcpgw(&config, &["add", "a", "--url", "https://x", "--", "npx"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("not both"));
}

#[test]
fn missing_target_rejected() {
    let (_dir, config) = temp_config();
    let out = mcpgw(&config, &["add", "a"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("missing server target"));
}

#[test]
fn malformed_env_rejected() {
    let (_dir, config) = temp_config();
    let out = mcpgw(&config, &["add", "a", "--env", "NOEQUALS", "--", "npx"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("KEY=VALUE"));
}

#[test]
fn enable_unknown_lists_known_servers() {
    let (_dir, config) = temp_config();
    assert!(
        mcpgw(&config, &["add", "github", "--url", "https://x"])
            .status
            .success()
    );
    let out = mcpgw(&config, &["enable", "nope"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("github"));
}

#[test]
fn toggle_without_config_reports_missing_file() {
    let (_dir, config) = temp_config();
    let out = mcpgw(&config, &["enable", "x"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("no config file"));
}

/// `--headers-command` is one typed line, stored as argv and never run here.
/// The token it would print is not in the config at all, which is the whole
/// reason to use it — so `list` has a command to show and no value to mask.
#[test]
fn add_stores_a_headers_command_as_argv() {
    let (_dir, config) = temp_config();
    let out = mcpgw(
        &config,
        &[
            "add",
            "corp",
            "--url",
            "https://mcp.corp.example/mcp",
            "--headers-command",
            "corp-auth print-mcp-headers",
            "--header",
            "X-Team=platform",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));

    let text = std::fs::read_to_string(&config).unwrap();
    assert!(
        text.contains(r#"headers_command = ["corp-auth", "print-mcp-headers"]"#),
        "{text}"
    );

    let json = list_json(&config);
    let corp = &json["servers"]["corp"];
    assert_eq!(corp["headers_command"][0], "corp-auth");
    assert_eq!(corp["headers_command"][1], "print-mcp-headers");
    // The command is not a secret and is shown whole; the header beside it
    // is still masked, because that one is a value.
    assert_eq!(corp["headers"]["X-Team"], "***");
    let printed = String::from_utf8(mcpgw(&config, &["list", "--json"]).stdout).unwrap();
    assert!(!printed.contains("platform"), "{printed}");
}

#[test]
fn a_headers_command_is_refused_on_a_stdio_server() {
    let (_dir, config) = temp_config();
    let out = mcpgw(
        &config,
        &[
            "add",
            "github",
            "--headers-command",
            "corp-auth",
            "--",
            "npx",
        ],
    );
    assert!(!out.status.success());
    assert!(stderr(&out).contains("--headers-command is for http servers"));
}

/// A config directory that is also a home with one client installed in it,
/// so the sync these commands now run has somewhere to write.
fn temp_config_with_cursor() -> (tempfile::TempDir, PathBuf) {
    let (dir, config) = temp_config();
    std::fs::create_dir_all(dir.path().join(".cursor")).unwrap();
    std::fs::write(
        dir.path().join(".cursor/mcp.json"),
        "{\n  \"mcpServers\": {}\n}\n",
    )
    .unwrap();
    (dir, config)
}

fn cursor_entries(home: &Path) -> String {
    std::fs::read_to_string(home.join(".cursor/mcp.json")).unwrap()
}

#[test]
fn add_writes_the_new_server_into_the_clients_itself() {
    let (dir, config) = temp_config_with_cursor();
    let out = mcpgw(
        &config,
        &["add", "linear", "--url", "https://mcp.linear.app/mcp"],
    );
    assert!(out.status.success(), "{}", stderr(&out));

    // The per-client lines `mcpgw sync` prints, from the add itself.
    let said = String::from_utf8(out.stdout).unwrap();
    assert!(said.contains("Cursor —"), "{said}");
    assert!(said.contains("+ linear"), "{said}");

    // And no second command was needed to make them true.
    let written = cursor_entries(dir.path());
    assert!(written.contains("linear"), "{written}");
}

#[test]
fn add_no_sync_leaves_the_clients_where_they_were() {
    let (dir, config) = temp_config_with_cursor();
    let out = mcpgw(
        &config,
        &[
            "add",
            "--no-sync",
            "linear",
            "--url",
            "https://mcp.linear.app/mcp",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    // Said, because the two halves are now out of step until it runs.
    let said = String::from_utf8(out.stdout).unwrap();
    assert!(said.contains("mcpgw sync"), "{said}");

    assert_eq!(list_json(&config)["servers"]["linear"]["type"], "http");
    let written = cursor_entries(dir.path());
    assert!(!written.contains("linear"), "{written}");
}

#[test]
fn remove_takes_the_entry_out_of_the_clients_itself() {
    let (dir, config) = temp_config_with_cursor();
    mcpgw(
        &config,
        &["add", "linear", "--url", "https://mcp.linear.app/mcp"],
    );
    assert!(cursor_entries(dir.path()).contains("linear"));

    let out = mcpgw(&config, &["remove", "linear", "--yes"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let said = String::from_utf8(out.stdout).unwrap();
    assert!(said.contains("- linear"), "{said}");

    let written = cursor_entries(dir.path());
    assert!(!written.contains("linear"), "{written}");
}

/// A disabled server is mirrored into no client, so the switch is a client
/// edit too — and the sync that makes it one runs from `disable` itself.
#[test]
fn disable_and_enable_move_the_entry_with_them() {
    let (dir, config) = temp_config_with_cursor();
    mcpgw(
        &config,
        &["add", "linear", "--url", "https://mcp.linear.app/mcp"],
    );

    let out = mcpgw(&config, &["disable", "linear"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let written = cursor_entries(dir.path());
    assert!(!written.contains("linear"), "{written}");

    let out = mcpgw(&config, &["enable", "linear"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let written = cursor_entries(dir.path());
    assert!(written.contains("linear"), "{written}");
}

/// One line rather than one "not found, skipped" per client: on a machine
/// with no MCP client at all, that list would be the whole output and none
/// of it the answer.
#[test]
fn an_edit_on_a_machine_with_no_client_says_so_once() {
    let (_dir, config) = temp_config();
    let out = mcpgw(
        &config,
        &["add", "linear", "--url", "https://mcp.linear.app/mcp"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let said = String::from_utf8(out.stdout).unwrap();
    assert!(said.contains("no MCP client found"), "{said}");
    assert!(!said.contains("skipped"), "{said}");
}
