use std::path::Path;

use mcpgw_core::{ClientKind, Detection, Error};

fn read_fixture(kind: ClientKind, name: &str) -> mcpgw_core::ClientRead {
    let full = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let text = std::fs::read_to_string(full).unwrap();
    kind.read_text(&text, Path::new(name)).unwrap()
}

#[test]
fn claude_desktop_reads_inferred_stdio() {
    insta::assert_debug_snapshot!(read_fixture(
        ClientKind::ClaudeDesktop,
        "claude_desktop.json"
    ));
}

#[test]
fn claude_code_reads_only_global_mcp_servers() {
    let read = read_fixture(ClientKind::ClaudeCode, "claude_code_state.json");
    // The project-scoped entry inside `projects` must be invisible.
    assert!(!read.servers.contains_key("project-scoped-ignored"));
    insta::assert_debug_snapshot!(read);
}

#[test]
fn cursor_maps_sse_to_http_with_note_and_honors_disabled() {
    let read = read_fixture(ClientKind::Cursor, "cursor_mcp.json");
    assert!(!read.servers["browser"].enabled);
    insta::assert_debug_snapshot!(read);
}

#[test]
fn vscode_reads_servers_root_key() {
    let read = read_fixture(ClientKind::VsCode, "vscode_mcp.json");
    assert_eq!(read.servers.len(), 2);
    assert!(read.problems.is_empty());
    insta::assert_debug_snapshot!(read);
}

#[test]
fn broken_entries_become_problems_not_failures() {
    let read = read_fixture(ClientKind::ClaudeDesktop, "messy.json");
    // Exactly one entry survives; every other becomes a reported problem.
    assert_eq!(read.servers.len(), 1);
    assert!(read.servers.contains_key("survivor"));
    assert_eq!(read.problems.len(), 6);
    insta::assert_debug_snapshot!(read.problems);
}

#[test]
fn missing_root_key_is_the_normal_empty_state() {
    let read = ClientKind::Cursor
        .read_text(r#"{"otherStuff": true}"#, Path::new("x.json"))
        .unwrap();
    assert!(read.servers.is_empty());
    assert!(read.problems.is_empty());
}

#[test]
fn invalid_json_is_a_file_level_error() {
    let err = ClientKind::Cursor
        .read_text("{ not json", Path::new("x.json"))
        .unwrap_err();
    assert!(matches!(err, Error::ClientParse { .. }));
    let err = ClientKind::Cursor
        .read_text("[1, 2]", Path::new("x.json"))
        .unwrap_err();
    insta::assert_snapshot!(err.to_string());
}

#[test]
fn detect_reports_three_states() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_owned();
    // One fake env drives every platform's lookup keys into the temp dir.
    let appdata = home.join("AppData");
    let env = move |key: &str| -> Option<std::ffi::OsString> {
        match key {
            "HOME" | "USERPROFILE" => Some(home.clone().into()),
            "APPDATA" => Some(appdata.clone().into()),
            _ => None,
        }
    };

    for kind in ClientKind::ALL {
        assert_eq!(
            kind.detect_with(&env),
            Detection::NotInstalled,
            "{} in empty home",
            kind.display_name()
        );
    }

    // Create only the install trace: detected as installed, not configured.
    let trace = ClientKind::Cursor.install_trace_with(&env).unwrap();
    std::fs::create_dir_all(&trace).unwrap();
    assert_eq!(ClientKind::Cursor.detect_with(&env), Detection::Installed);

    // Creating the config file upgrades detection to Configured.
    let config = ClientKind::Cursor.config_path_with(&env).unwrap();
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, "{}").unwrap();
    assert_eq!(
        ClientKind::Cursor.detect_with(&env),
        Detection::Configured(config)
    );
}

#[test]
fn load_missing_file_is_not_found() {
    let err = ClientKind::Cursor
        .load(Path::new("/nonexistent/mcp.json"))
        .unwrap_err();
    assert!(matches!(err, Error::NotFound { .. }));
}
