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
fn dedup_merges_metadata_instead_of_keeping_only_the_first_source() {
    // Same transport in both clients, but VS Code has it disabled and reads
    // it through the lossy sse mapping.
    let cursor = read(
        ClientKind::Cursor,
        r#"{"mcpServers": {"linear": {"type": "http", "url": "https://mcp.linear.app/mcp"}}}"#,
    );
    let vscode = read(
        ClientKind::VsCode,
        r#"{"servers": {"linear": {"type": "sse", "url": "https://mcp.linear.app/mcp",
            "disabled": true}}}"#,
    );
    let plan = plan_import(
        &[("cursor".into(), cursor), ("vscode".into(), vscode)],
        &BTreeMap::new(),
    );

    assert_eq!(plan.new.len(), 1);
    let candidate = &plan.new[0];
    assert_eq!(candidate.origins.len(), 2);
    // Disabled anywhere wins, and the second source's note is not lost.
    assert!(!candidate.server.enabled);
    assert_eq!(candidate.notes.len(), 1);
    assert!(candidate.notes[0].contains("sse"), "{:?}", candidate.notes);
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

/// Case in the scheme and host and one trailing slash are the same endpoint
/// spelled two ways, and two clients routinely spell it both ways. Merging
/// them is the difference between one server in the config and two.
#[test]
fn http_urls_that_differ_only_in_case_or_a_trailing_slash_dedup() {
    let cursor = read(
        ClientKind::Cursor,
        r#"{"mcpServers": {"ctx7": {"type": "http", "url": "HTTPS://MCP.Context7.com/mcp/"}}}"#,
    );
    let vscode = read(
        ClientKind::VsCode,
        r#"{"servers": {"context7": {"type": "http", "url": "https://mcp.context7.com/mcp"}}}"#,
    );
    let plan = plan_import(
        &[("cursor".into(), cursor), ("vscode".into(), vscode)],
        &BTreeMap::new(),
    );

    assert_eq!(plan.new.len(), 1);
    assert_eq!(plan.new[0].origins.len(), 2);
}

/// A host is case-insensitive; a path, a query and a port are not. Nothing
/// past the authority is normalized, because everything past it can change
/// which server answers.
#[test]
fn urls_that_differ_past_the_host_stay_separate() {
    for (a, b) in [
        ("https://h/mcp", "https://h/MCP"),
        ("https://h/mcp", "https://h/mcp/v2"),
        ("https://h/mcp?x=1", "https://h/mcp?x=2"),
        ("https://h/mcp", "https://h:8443/mcp"),
        ("https://h/mcp", "http://h/mcp"),
        ("https://h/mcp//", "https://h/mcp"),
    ] {
        let cursor = read(
            ClientKind::Cursor,
            &format!(r#"{{"mcpServers": {{"one": {{"type": "http", "url": "{a}"}}}}}}"#),
        );
        let vscode = read(
            ClientKind::VsCode,
            &format!(r#"{{"servers": {{"two": {{"type": "http", "url": "{b}"}}}}}}"#),
        );
        let plan = plan_import(
            &[("cursor".into(), cursor), ("vscode".into(), vscode)],
            &BTreeMap::new(),
        );
        assert_eq!(plan.new.len(), 2, "{a} and {b} were merged");
    }
}

/// The same headers under two spellings of one URL still merge; different
/// headers are a different server however the URL is spelled, because the
/// surviving copy's definition is the one that runs.
#[test]
fn header_differences_survive_url_canonicalization() {
    let cursor = read(
        ClientKind::Cursor,
        r#"{"mcpServers": {"a": {"type": "http", "url": "https://H/mcp/",
            "headers": {"X-Key": "one"}}}}"#,
    );
    let vscode = read(
        ClientKind::VsCode,
        r#"{"servers": {"b": {"type": "http", "url": "https://h/mcp",
            "headers": {"X-Key": "two"}}}}"#,
    );
    let plan = plan_import(
        &[("cursor".into(), cursor), ("vscode".into(), vscode)],
        &BTreeMap::new(),
    );
    assert_eq!(plan.new.len(), 2);
}

/// Stdio is compared byte for byte, argument order included: two commands
/// with the same arguments in a different order can behave differently, and
/// a merge picks one of them for the user without saying so.
#[test]
fn stdio_is_untouched_by_url_canonicalization() {
    let cursor = read(
        ClientKind::Cursor,
        r#"{"mcpServers": {"one": {"command": "NPX", "args": ["--a", "--b"]}}}"#,
    );
    let vscode = read(
        ClientKind::VsCode,
        r#"{"servers": {"two": {"type": "stdio", "command": "npx", "args": ["--b", "--a"]}}}"#,
    );
    let plan = plan_import(
        &[("cursor".into(), cursor), ("vscode".into(), vscode)],
        &BTreeMap::new(),
    );
    assert_eq!(plan.new.len(), 2);
}
