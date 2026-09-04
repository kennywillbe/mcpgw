//! `mcpgw clients`: the listing, the two edits, and what the config file
//! looks like afterwards.

use std::path::Path;
use std::process::Output;

use assert_cmd::Command;

fn clients(home: &Path, args: &[&str]) -> Output {
    Command::cargo_bin("mcpgw")
        .unwrap()
        .env("MCPGW_NO_UPDATE_CHECK", "1")
        .env("MCPGW_CONFIG", home.join("config.toml"))
        .env("MCPGW_STATE_DIR", home.join("state"))
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("APPDATA", home.join("AppData"))
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .arg("clients")
        .args(args)
        .output()
        .unwrap()
}

fn ok(home: &Path, args: &[&str]) -> String {
    let out = clients(home, args);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

const CONFIG: &str = r#"version = 1

[servers.github]
type = "stdio"
command = "npx"

[servers.linear]
type = "http"
url = "https://mcp.linear.app/mcp"
"#;

fn home() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), CONFIG).unwrap();
    dir
}

fn config_text(home: &Path) -> String {
    std::fs::read_to_string(home.join("config.toml")).unwrap()
}

#[test]
fn the_listing_says_what_every_client_is_given() {
    let dir = home();
    let text = ok(dir.path(), &[]);
    assert!(text.contains("cursor — all 2 servers"), "{text}");
    assert!(text.contains("windsurf — all 2 servers"), "{text}");
    assert!(text.contains("no scope is given every server"), "{text}");
}

#[test]
fn narrowing_the_servers_writes_the_table_and_shows_up_in_the_listing() {
    let dir = home();
    let out = ok(dir.path(), &["cursor", "servers", "github"]);
    assert!(out.contains("mcpgw sync --client cursor"), "{out}");
    let text = config_text(dir.path());
    assert!(text.contains("[clients.cursor]"), "{text}");
    assert!(text.contains(r#"servers = ["github"]"#), "{text}");

    let listing = ok(dir.path(), &["cursor"]);
    assert!(listing.contains("cursor — 1 of 2 servers"), "{listing}");
    assert!(listing.contains(r#"servers = ["github"]"#), "{listing}");

    // `all` is how a scope is widened again, and it takes the whole table
    // with it when nothing else is in it.
    ok(dir.path(), &["cursor", "servers", "all"]);
    assert!(!config_text(dir.path()).contains("clients"), "{text}");
}

#[test]
fn tool_lists_are_added_moved_between_and_cleared() {
    let dir = home();
    ok(dir.path(), &["cursor", "tools", "deny", "delete_*"]);
    let text = config_text(dir.path());
    assert!(text.contains("[clients.cursor.tools]"), "{text}");
    assert!(text.contains(r#"deny = ["delete_*"]"#), "{text}");

    // Allowing a name the deny list holds moves it, rather than leaving a
    // rule that quietly wins over the one just written.
    ok(dir.path(), &["cursor", "tools", "allow", "delete_*"]);
    let text = config_text(dir.path());
    assert!(text.contains(r#"allow = ["delete_*"]"#), "{text}");
    assert!(!text.contains("deny"), "{text}");

    ok(dir.path(), &["cursor", "tools", "clear"]);
    assert!(!config_text(dir.path()).contains("clients"), "{text}");
}

#[test]
fn a_scope_naming_a_server_that_is_not_there_is_refused_before_it_is_written() {
    let dir = home();
    let out = clients(dir.path(), &["cursor", "servers", "ghost"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("ghost"), "{err}");
    assert!(err.contains("github"), "{err}");
    assert!(!config_text(dir.path()).contains("clients"));
}

#[test]
fn an_unknown_client_names_the_ones_that_exist() {
    let dir = home();
    let out = clients(dir.path(), &["cursorr", "servers", "github"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("cursor"), "{err}");
}
