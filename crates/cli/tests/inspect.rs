//! End-to-end coverage for `mcpgw inspect`: the real binary spawns the
//! scripted fixture server and reports what it advertises.

use std::path::Path;
use std::process::Output;

use assert_cmd::Command;

mod util;
use util::fixture_binary;

/// Runs inspect in a sandboxed home against a config naming the fixture.
fn run_inspect(home: &Path, config_text: &str, args: &[&str]) -> Output {
    let config = home.join("config.toml");
    std::fs::write(&config, config_text).unwrap();
    Command::cargo_bin("mcpgw")
        .unwrap()
        .arg("inspect")
        .args(args)
        .env("MCPGW_CONFIG", &config)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .unwrap()
}

fn config() -> String {
    format!(
        r#"
version = 1

[servers.fx]
type = "stdio"
command = "{}"
args = ["healthy"]

[servers.other]
type = "stdio"
command = "cargo"
"#,
        fixture_binary().display()
    )
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

#[test]
fn tables_the_tools_a_live_server_advertises() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_inspect(dir.path(), &config(), &["fx"]);
    let text = stdout(&out);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("mcpgw-test-server 9.9.9"), "{text}");
    assert!(text.contains("echo") && text.contains("reverse"), "{text}");
    assert!(text.contains("echoes input"), "{text}");
    // The fixture advertises tools only, which must read as a plain fact
    // rather than as a failed listing.
    assert!(text.contains("resources: not supported"), "{text}");
}

#[test]
fn json_carries_the_full_listing() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_inspect(dir.path(), &config(), &["fx", "--json"]);
    assert!(out.status.success(), "{}", stdout(&out));
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(value["server_name"], "mcpgw-test-server");
    assert_eq!(value["tools"].as_array().unwrap().len(), 2);
    assert_eq!(value["tools"][0]["name"], "echo");
    assert_eq!(value["supports_resources"], false);
    assert_eq!(value["resources"].as_array().unwrap().len(), 0);
}

#[test]
fn unknown_server_names_the_known_ones() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_inspect(dir.path(), &config(), &["nope"]);
    assert!(!out.status.success());
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("no server named \"nope\""), "{err}");
    assert!(err.contains("fx") && err.contains("other"), "{err}");
}

#[test]
fn an_unreachable_server_fails_loudly() {
    let dir = tempfile::tempdir().unwrap();
    let config = r#"
version = 1

[servers.ghost]
type = "stdio"
command = "definitely-not-a-real-command-mcpgw"
"#;
    let out = run_inspect(dir.path(), config, &["ghost", "--timeout", "5"]);
    assert!(!out.status.success());
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("cannot inspect server \"ghost\""), "{err}");
}
