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

fn pin_path(home: &Path) -> std::path::PathBuf {
    home.join("state").join("pins").join("fx.json")
}

#[test]
fn pin_writes_the_definitions_and_show_reads_them_back() {
    let dir = home();
    let out = tools(dir.path(), &["fx", "--timeout", "60", "pin"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout(&out).contains("pinned 2 tool definition(s)"),
        "{}",
        stdout(&out)
    );
    let file = std::fs::read_to_string(pin_path(dir.path())).unwrap();
    assert!(file.contains(r#""server": "fx""#), "{file}");
    assert!(file.contains(r#""echo""#), "{file}");
    // Hashes and lengths only: the pin file never holds a description.
    assert!(!file.contains("echoes input"), "{file}");

    let shown = stdout(&tools(dir.path(), &["fx", "pin", "--show"]));
    assert!(shown.contains("2 pinned tool(s)"), "{shown}");
    assert!(shown.contains("echo"), "{shown}");
    assert!(shown.contains("no drift since"), "{shown}");
}

#[test]
fn the_listing_carries_a_drift_column() {
    let dir = home();
    let unpinned = stdout(&tools(dir.path(), &["fx", "--timeout", "60"]));
    assert!(
        unpinned.contains("echo     allowed   unpinned"),
        "{unpinned}"
    );

    tools(dir.path(), &["fx", "--timeout", "60", "pin"]);
    let pinned = stdout(&tools(dir.path(), &["fx", "--timeout", "60"]));
    assert!(pinned.contains("echo     allowed   pinned"), "{pinned}");
}

#[test]
fn unpin_deletes_the_file_and_says_what_that_means() {
    let dir = home();
    tools(dir.path(), &["fx", "--timeout", "60", "pin"]);
    assert!(pin_path(dir.path()).exists());

    let out = tools(dir.path(), &["fx", "unpin"]);
    assert!(out.status.success());
    assert!(stdout(&out).contains("pins afresh"), "{}", stdout(&out));
    assert!(!pin_path(dir.path()).exists());

    // Idempotent, and honest about having found nothing.
    let again = tools(dir.path(), &["fx", "unpin"]);
    assert!(again.status.success());
    assert!(
        stdout(&again).contains("had no pinned"),
        "{}",
        stdout(&again)
    );
}

/// The offline half: `pin --show` answers from the file, so a server that is
/// down does not stop anyone asking what it used to say.
#[test]
fn show_on_a_server_that_was_never_pinned_says_where_pins_come_from() {
    let dir = home();
    let out = tools(dir.path(), &["fx", "pin", "--show"]);
    assert!(out.status.success());
    assert!(
        stdout(&out).contains("no pinned tool definitions"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn drift_off_is_written_into_the_table_and_survives_a_list_edit() {
    let dir = home();
    let config = dir.path().join("config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("\n[servers.fx.tools]\ndrift = \"off\"\n");
    std::fs::write(&config, text).unwrap();

    tools(dir.path(), &["fx", "allow", "echo"]);
    let after = config_text(dir.path());
    assert!(after.contains(r#"drift = "off""#), "{after}");
    assert!(after.contains(r#"allow = ["echo"]"#), "{after}");

    // And a server whose definitions are not watched says so in the header.
    let listed = stdout(&tools(dir.path(), &["fx", "--timeout", "60"]));
    assert!(listed.contains(r#"drift = "off""#), "{listed}");
}
