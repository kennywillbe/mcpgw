use std::collections::BTreeMap;
use std::path::Path;

use mcpgw_core::import::{plan_import, slugify};
use mcpgw_core::{ClientKind, ClientRead, Config};

fn read(kind: ClientKind, json: &str) -> ClientRead {
    kind.read_text(json, Path::new("x.json")).unwrap()
}

fn canonical(toml: &str) -> BTreeMap<String, mcpgw_core::Server> {
    Config::parse(toml, Path::new("c.toml")).unwrap().servers
}

#[test]
fn slugify_cases() {
    assert_eq!(slugify("My Server"), "my-server");
    assert_eq!(slugify("GitHub_Tools"), "github_tools");
    assert_eq!(slugify("çok--Güzel!!"), "ok-g-zel");
    assert_eq!(slugify("!!!"), "imported-server");
}

#[test]
fn identical_definitions_dedup_across_clients() {
    let cursor = read(
        ClientKind::Cursor,
        r#"{"mcpServers": {"github": {"command": "npx", "args": ["server-github"]}}}"#,
    );
    let vscode = read(
        ClientKind::VsCode,
        r#"{"servers": {"github": {"type": "stdio", "command": "npx", "args": ["server-github"]}}}"#,
    );
    let plan = plan_import(
        &[("cursor".into(), cursor), ("vscode".into(), vscode)],
        &BTreeMap::new(),
    );
    assert_eq!(plan.new.len(), 1);
    assert_eq!(plan.new[0].origins.len(), 2);
    assert!(!plan.new[0].renamed);
}

#[test]
fn cross_client_name_clash_suffixes_the_later_one() {
    let cursor = read(
        ClientKind::Cursor,
        r#"{"mcpServers": {"tools": {"command": "npx"}}}"#,
    );
    let vscode = read(
        ClientKind::VsCode,
        r#"{"servers": {"tools": {"type": "http", "url": "https://x/mcp"}}}"#,
    );
    let plan = plan_import(
        &[("cursor".into(), cursor), ("vscode".into(), vscode)],
        &BTreeMap::new(),
    );
    let names: Vec<&str> = plan.new.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["tools", "tools-2"]);
    assert!(plan.new[1].renamed);
}

#[test]
fn invalid_names_are_slugified_and_avoid_canonical_collisions() {
    let cursor = read(
        ClientKind::Cursor,
        r#"{"mcpServers": {"My Server": {"command": "npx"}}}"#,
    );
    let canonical = canonical(
        r#"
version = 1
[servers.my-server]
type = "http"
url = "https://occupied/mcp"
"#,
    );
    let plan = plan_import(&[("cursor".into(), cursor)], &canonical);
    assert_eq!(plan.new.len(), 1);
    assert_eq!(plan.new[0].name, "my-server-2");
    assert!(plan.new[0].renamed);
}

#[test]
fn classification_against_canonical() {
    let cursor = read(
        ClientKind::Cursor,
        r#"{"mcpServers": {
            "same": {"command": "npx", "args": ["a"]},
            "differs": {"command": "npx", "args": ["client-version"]},
            "fresh": {"url": "https://fresh/mcp"}
        }}"#,
    );
    let canonical = canonical(
        r#"
version = 1
[servers.same]
type = "stdio"
command = "npx"
args = ["a"]
[servers.differs]
type = "stdio"
command = "npx"
args = ["canonical-version"]
"#,
    );
    let plan = plan_import(&[("cursor".into(), cursor)], &canonical);
    assert_eq!(plan.already.len(), 1);
    assert_eq!(plan.already[0].name, "same");
    assert_eq!(plan.conflicts.len(), 1);
    assert_eq!(plan.conflicts[0].name, "differs");
    assert_eq!(plan.new.len(), 1);
    assert_eq!(plan.new[0].name, "fresh");
}

#[test]
fn sse_note_travels_with_the_candidate() {
    let cursor = read(
        ClientKind::Cursor,
        r#"{"mcpServers": {"linear": {"type": "sse", "url": "https://l/sse"}}}"#,
    );
    let plan = plan_import(&[("cursor".into(), cursor)], &BTreeMap::new());
    assert_eq!(plan.new[0].notes.len(), 1);
    assert!(plan.new[0].notes[0].contains("sse"));
}
