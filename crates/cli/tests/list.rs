use std::path::{Path, PathBuf};
use std::process::Output;

use assert_cmd::Command;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn run_list(config: &Path, extra: &[&str]) -> Output {
    Command::cargo_bin("mcpgw")
        .unwrap()
        .arg("list")
        .args(extra)
        .env("MCPGW_CONFIG", config)
        .output()
        .unwrap()
}

#[test]
fn lists_servers_from_env_override() {
    let out = run_list(&fixture("basic.toml"), &[]);
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("github"));
    assert!(stdout.contains("https://mcp.linear.app/mcp"));
}

#[test]
fn json_output_is_parseable() {
    let out = run_list(&fixture("basic.toml"), &["--json"]);
    assert!(out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["version"], 1);
    assert_eq!(value["servers"]["github"]["type"], "stdio");
}

#[test]
fn missing_config_is_empty_not_an_error() {
    let out = run_list(Path::new("/nonexistent/mcpgw.toml"), &["--json"]);
    assert!(out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["servers"], serde_json::json!({}));
}

#[test]
fn broken_config_fails_with_nonzero_exit() {
    let out = run_list(&fixture("broken.toml"), &[]);
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("broken.toml"));
}
