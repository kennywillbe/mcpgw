//! `mcpgw tools`: the listing, the three edits, and what the config file
//! looks like afterwards.

use std::path::Path;
use std::process::Output;

use assert_cmd::Command;

mod util;
use util::fixture_config;

fn tools(home: &Path, args: &[&str]) -> Output {
    Command::cargo_bin("mcpgw")
        .unwrap()
        .env("MCPGW_NO_UPDATE_CHECK", "1")
        .env("MCPGW_CONFIG", home.join("config.toml"))
        .env("MCPGW_STATE_DIR", home.join("state"))
        .arg("tools")
        .args(args)
        .output()
        .unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn home() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), fixture_config(&["fx"])).unwrap();
    dir
}

fn config_text(home: &Path) -> String {
    std::fs::read_to_string(home.join("config.toml")).unwrap()
}

#[test]
fn the_listing_reaches_the_server_and_marks_every_tool() {
    let dir = home();
    let out = tools(dir.path(), &["fx", "--timeout", "60"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = stdout(&out);
    assert!(text.contains("every tool is allowed"), "{text}");
    assert!(text.contains("echo"), "{text}");
    assert!(text.contains("reverse"), "{text}");
    assert!(!text.contains("denied"), "{text}");
}

#[test]
fn allow_then_the_listing_says_what_is_denied() {
    let dir = home();
    let out = tools(dir.path(), &["fx", "allow", "echo"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(config_text(dir.path()).contains("[servers.fx.tools]"));
    assert!(config_text(dir.path()).contains(r#"allow = ["echo"]"#));

    let text = stdout(&tools(dir.path(), &["fx", "--timeout", "60"]));
    assert!(text.contains("echo     allowed"), "{text}");
    assert!(text.contains("reverse  denied"), "{text}");
}

#[test]
fn deny_appends_and_takes_the_name_out_of_the_allow_list() {
    let dir = home();
    tools(dir.path(), &["fx", "allow", "echo", "reverse"]);
    tools(dir.path(), &["fx", "deny", "reverse"]);
    let text = config_text(dir.path());
    assert!(text.contains(r#"allow = ["echo"]"#), "{text}");
    assert!(text.contains(r#"deny = ["reverse"]"#), "{text}");
}

#[test]
fn clear_removes_the_table_again() {
    let dir = home();
    tools(dir.path(), &["fx", "allow", "echo"]);
    let out = tools(dir.path(), &["fx", "clear"]);
    assert!(out.status.success());
    assert!(stdout(&out).contains("every tool is allowed again"));
    assert!(
        !config_text(dir.path()).contains("tools"),
        "{}",
        config_text(dir.path())
    );
}

#[test]
fn an_unknown_server_is_an_error_that_lists_the_real_ones() {
    let dir = home();
    let out = tools(dir.path(), &["ghost", "allow", "echo"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("ghost"), "{stderr}");
    assert!(stderr.contains("fx"), "{stderr}");
}
