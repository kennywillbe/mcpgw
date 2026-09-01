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

#[test]
fn json_output_masks_env_and_header_values_unless_asked() {
    let out = run_list(&fixture("secrets.toml"), &["--json"]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(!text.contains("ghp_realsecret"), "{text}");
    assert!(!text.contains("t0ken"), "{text}");
    // The names stay: which variables a server needs is the useful half.
    assert!(text.contains("GITHUB_TOKEN"), "{text}");
    assert!(text.contains("Authorization"), "{text}");

    let out = run_list(&fixture("secrets.toml"), &["--json", "--show-secrets"]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("ghp_realsecret"), "{text}");
    assert!(text.contains("Bearer t0ken"), "{text}");
}

#[test]
fn the_human_table_never_carried_secrets_to_begin_with() {
    let out = run_list(&fixture("secrets.toml"), &[]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("github") && text.contains("linear"), "{text}");
    assert!(!text.contains("ghp_realsecret"), "{text}");
    assert!(!text.contains("t0ken"), "{text}");
}
