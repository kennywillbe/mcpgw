use std::path::Path;

use mcpgw_core::doctor::{Severity, check_server, classify_problems};
use mcpgw_core::{ClientKind, Config};

fn parse(text: &str) -> Config {
    Config::parse(text, Path::new("t.toml")).unwrap()
}

#[test]
fn stdio_command_resolution_drives_findings() {
    let config = parse(
        r#"
version = 1
[servers.good]
type = "stdio"
command = "present"
[servers.bad]
type = "stdio"
command = "absent"
"#,
    );
    let exists = |cmd: &str| cmd == "present";
    let mut findings = Vec::new();
    for (name, server) in &config.servers {
        findings.extend(check_server(None, name, server, &exists));
    }
    insta::assert_debug_snapshot!(findings);
}

#[test]
fn disabled_servers_are_skipped() {
    let config = parse(
        r#"
version = 1
[servers.parked]
type = "stdio"
command = "absent"
enabled = false
"#,
    );
    let exists = |_: &str| false;
    let server = &config.servers["parked"];
    assert!(check_server(None, "parked", server, &exists).is_empty());
}

#[test]
fn url_syntax_and_scheme_checks() {
    let config = parse(
        r#"
version = 1
[servers.ok]
type = "http"
url = "https://mcp.example.com/mcp"
[servers.broken]
type = "http"
url = "not a url"
[servers.odd]
type = "http"
url = "ftp://mcp.example.com"
"#,
    );
    let exists = |_: &str| true;
    let sev = |name: &str| {
        check_server(None, name, &config.servers[name], &exists)
            .first()
            .map(|f| f.severity)
    };
    assert_eq!(sev("ok"), None);
    assert_eq!(sev("broken"), Some(Severity::Error));
    assert_eq!(sev("odd"), Some(Severity::Warning));
}

#[test]
fn surviving_entries_yield_warnings_dropped_yield_errors() {
    let text = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cursor_mcp.json"),
    )
    .unwrap();
    let read = ClientKind::Cursor
        .read_text(&text, Path::new("c.json"))
        .unwrap();
    // linear survives with an sse note -> warning
    let findings = classify_problems("Cursor", &read);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].severity, Severity::Warning);

    let text = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/messy.json"),
    )
    .unwrap();
    let read = ClientKind::ClaudeDesktop
        .read_text(&text, Path::new("m.json"))
        .unwrap();
    let findings = classify_problems("Claude Desktop", &read);
    assert_eq!(findings.len(), 6);
    assert!(findings.iter().all(|f| f.severity == Severity::Error));
}
