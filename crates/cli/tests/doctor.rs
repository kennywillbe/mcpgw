use std::path::Path;
use std::process::Output;

use assert_cmd::Command;

/// Runs doctor in a fully sandboxed home: every env key any adapter looks at
/// points into the temp dir, so the real machine never leaks into the test.
fn run_doctor(home: &Path, config_text: Option<&str>, args: &[&str]) -> Output {
    let config = home.join("config.toml");
    if let Some(text) = config_text {
        std::fs::write(&config, text).unwrap();
    }
    Command::cargo_bin("mcpgw")
        .unwrap()
        .arg("doctor")
        .args(args)
        .env("MCPGW_CONFIG", &config)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("APPDATA", home.join("AppData"))
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

// `cargo` is the one command guaranteed to exist wherever these tests run.
const HEALTHY: &str = r#"
version = 1
[servers.build]
type = "stdio"
command = "cargo"
"#;

const BROKEN_COMMAND: &str = r#"
version = 1
[servers.ghost]
type = "stdio"
command = "definitely-not-a-real-command-mcpgw"
"#;

#[test]
fn healthy_config_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_doctor(dir.path(), Some(HEALTHY), &[]);
    assert!(out.status.success(), "{}", stdout(&out));
    assert!(stdout(&out).contains("0 errors"));
}

#[test]
fn unresolvable_command_is_an_error_exit() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_doctor(dir.path(), Some(BROKEN_COMMAND), &[]);
    assert!(!out.status.success());
    assert!(stdout(&out).contains("not found in PATH"));
}

#[test]
fn missing_canonical_config_is_fine() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_doctor(dir.path(), None, &[]);
    assert!(out.status.success(), "{}", stdout(&out));
    assert!(stdout(&out).contains("not created yet"));
}

#[test]
fn client_warnings_do_not_fail_the_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    // A configured Cursor with an sse entry: lossy note -> warning only.
    let cursor = dir.path().join(".cursor");
    std::fs::create_dir_all(&cursor).unwrap();
    std::fs::write(
        cursor.join("mcp.json"),
        r#"{"mcpServers": {"linear": {"type": "sse", "url": "https://mcp.linear.app/sse"}}}"#,
    )
    .unwrap();

    let out = run_doctor(dir.path(), Some(HEALTHY), &[]);
    assert!(out.status.success(), "{}", stdout(&out));
    let text = stdout(&out);
    assert!(text.contains("legacy `sse`"));
    assert!(text.contains("1 warnings"));
}

#[test]
fn broken_client_entry_fails_the_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let cursor = dir.path().join(".cursor");
    std::fs::create_dir_all(&cursor).unwrap();
    std::fs::write(
        cursor.join("mcp.json"),
        r#"{"mcpServers": {"husk": {"env": {"A": "B"}}}}"#,
    )
    .unwrap();

    let out = run_doctor(dir.path(), Some(HEALTHY), &[]);
    assert!(!out.status.success());
    assert!(stdout(&out).contains("neither `command` nor `url`"));
}

#[test]
fn json_output_carries_findings_and_counts() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_doctor(dir.path(), Some(BROKEN_COMMAND), &["--json"]);
    assert!(!out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["errors"], 1);
    assert_eq!(value["findings"][0]["severity"], "error");
    assert_eq!(value["findings"][0]["server"], "ghost");
    let clients = value["clients"].as_array().unwrap();
    assert_eq!(clients.len(), 11);
    for name in [
        "Gemini CLI",
        "Codex CLI",
        "opencode",
        "Windsurf",
        "Zed",
        "Cline",
        "Cline CLI",
    ] {
        assert!(clients.iter().any(|c| c["client"] == name), "{clients:?}");
    }
}

#[test]
fn probe_reports_handshake_failure_with_nonzero_exit() {
    let dir = tempfile::tempdir().unwrap();
    // `cargo` exists (so static doctor is green) but speaks no MCP: the
    // static pass alone exits 0, the probe pass must exit 1.
    let static_out = run_doctor(dir.path(), Some(HEALTHY), &[]);
    assert!(static_out.status.success());

    let out = run_doctor(dir.path(), Some(HEALTHY), &["--probe", "--timeout", "15"]);
    assert!(!out.status.success());
    let text = stdout(&out);
    assert!(text.contains("probes"), "{text}");
    assert!(text.contains("build (canonical)"), "{text}");
}

#[test]
fn probe_reaches_http_servers_and_reports_failures() {
    let dir = tempfile::tempdir().unwrap();
    // Port 1 on loopback refuses instantly: the static pass is green, the
    // probe pass must reach out and fail loudly.
    let config = r#"
version = 1
[servers.remote]
type = "http"
url = "http://127.0.0.1:1/mcp"
"#;
    let out = run_doctor(dir.path(), Some(config), &["--probe", "--timeout", "15"]);
    assert!(!out.status.success(), "{}", stdout(&out));
    let text = stdout(&out);
    assert!(text.contains("remote (canonical)"), "{text}");
    assert!(text.contains("1 errors"), "{text}");
}

#[test]
fn probe_json_carries_results() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_doctor(
        dir.path(),
        Some(HEALTHY),
        &["--probe", "--timeout", "15", "--json"],
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let probes = value["probes"].as_object().unwrap();
    assert!(!probes.contains_key("skipped_http"));
    let results = probes["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["ok"], false);
}
