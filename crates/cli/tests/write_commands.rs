use std::path::{Path, PathBuf};
use std::process::Output;

use assert_cmd::Command;

fn mcpgw(config: &Path, args: &[&str]) -> Output {
    Command::cargo_bin("mcpgw")
        .unwrap()
        // Hermetic: no test may phone home for a version notice.
        .env("MCPGW_NO_UPDATE_CHECK", "1")
        .args(args)
        .env("MCPGW_CONFIG", config)
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
