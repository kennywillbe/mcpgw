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
        // Hermetic: no test may phone home for a version notice.
        .env("MCPGW_NO_UPDATE_CHECK", "1")
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

#[test]
fn json_output_carries_the_tool_lists() {
    let out = run_list(&fixture("tools.toml"), &["--json"]);
    assert!(out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        value["servers"]["github"]["tools"]["allow"],
        serde_json::json!(["search_repositories"])
    );
    assert_eq!(
        value["servers"]["github"]["tools"]["deny"],
        serde_json::json!(["delete_*"])
    );
    // A server with no table has no key, rather than an empty one that would
    // read as a list allowing nothing.
    assert!(value["servers"]["linear"].get("tools").is_none());
}

/// The secret material here is in `args` and in a query string, which is
/// where the `env`/`headers` masking never reached.
#[test]
fn the_table_redacts_secrets_in_args_and_urls() {
    let out = run_list(&fixture("target-secrets.toml"), &[]);
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(!text.contains("notarealkeyjustashape"), "{text}");
    assert!(!text.contains("notarealtokenjustashape"), "{text}");
    // What is left is still a readable target: the command, the host, and
    // the name of the parameter that carried the credential.
    assert!(text.contains("npx -y server-github"), "{text}");
    assert!(text.contains("https://mcp.linear.app/mcp?token="), "{text}");
}

#[test]
fn the_table_shows_args_and_url_secrets_when_asked() {
    let out = run_list(&fixture("target-secrets.toml"), &["--show-secrets"]);
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains("--api-key=sk-notarealkeyjustashape"),
        "{text}"
    );
    assert!(text.contains("?token=notarealtokenjustashape"), "{text}");
}

#[test]
fn json_output_redacts_secrets_in_args_and_urls() {
    let out = run_list(&fixture("target-secrets.toml"), &["--json"]);
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(!text.contains("notarealkeyjustashape"), "{text}");
    assert!(!text.contains("notarealtokenjustashape"), "{text}");
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        value["servers"]["linear"]["url"],
        "https://mcp.linear.app/mcp?token=[redacted]"
    );
    assert_eq!(value["servers"]["github"]["args"][1], "server-github");
}

#[test]
fn json_output_shows_args_and_url_secrets_when_asked() {
    let out = run_list(
        &fixture("target-secrets.toml"),
        &["--json", "--show-secrets"],
    );
    assert!(out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        value["servers"]["linear"]["url"],
        "https://mcp.linear.app/mcp?token=notarealtokenjustashape"
    );
    assert_eq!(
        value["servers"]["github"]["args"][2],
        "--api-key=sk-notarealkeyjustashape"
    );
}
