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
fn gemini_reads_both_url_shapes_and_the_excluded_list() {
    let read = read_fixture(ClientKind::Gemini, "gemini_settings.json");

    // `httpUrl` is streamable HTTP, plain `url` is legacy SSE, and an entry
    // carrying both resolves to `httpUrl` — all three land as http.
    assert_eq!(
        read.servers["linear"].transport,
        mcpgw_core::Transport::Http {
            url: "https://mcp.linear.app/mcp".to_owned(),
            headers: [(
                "Authorization".to_owned(),
                "Bearer ${LINEAR_TOKEN}".to_owned()
            )]
            .into_iter()
            .collect(),
        }
    );
    // Env values keep their `$VAR` spelling: Gemini expands them, mcpgw
    // must not resolve them at read time.
    let mcpgw_core::Transport::Stdio { env, .. } = &read.servers["github"].transport else {
        panic!("github should be stdio");
    };
    assert_eq!(env["GITHUB_TOKEN"], "$GITHUB_TOKEN");

    // No per-entry flag: `notes` is off only because `mcp.excluded` says so.
    assert!(!read.servers["notes"].enabled);
    assert!(read.servers["github"].enabled);
    // An entry with no target field at all is a problem, not a failure.
    assert!(!read.servers.contains_key("husk"));

    insta::assert_debug_snapshot!(read);
}

#[test]
fn gemini_tolerates_a_malformed_excluded_list() {
    let read = ClientKind::Gemini
        .read_text(
            r#"{"mcp": {"excluded": "notes"}, "mcpServers": {"notes": {"command": "x"}}}"#,
            Path::new("settings.json"),
        )
        .unwrap();
    // The list is unreadable, so nothing is disabled — but the file is not
    // rejected and the problem is reported.
    assert!(read.servers["notes"].enabled);
    assert_eq!(read.problems.len(), 1);
    assert_eq!(read.problems[0].message, "`mcp.excluded` is not an array");
}

#[test]
fn codex_reads_toml_entries_and_tolerates_the_evolving_fields() {
    let read = read_fixture(ClientKind::Codex, "codex_config.toml");

    // The stdio entry's extras (env_vars, cwd, the timeouts, the per-tool
    // sub-table) have no canonical counterpart and must not break the read.
    assert_eq!(
        read.servers["github"].transport,
        mcpgw_core::Transport::Stdio {
            command: "npx".to_owned(),
            args: ["-y", "@modelcontextprotocol/server-github"]
                .map(str::to_owned)
                .to_vec(),
            env: [("GITHUB_TOKEN".to_owned(), "$GITHUB_TOKEN".to_owned())]
                .into_iter()
                .collect(),
        }
    );
    // Remote headers are `http_headers` here, not `headers`.
    assert_eq!(
        read.servers["linear"].transport,
        mcpgw_core::Transport::Http {
            url: "https://mcp.linear.app/mcp".to_owned(),
            headers: [(
                "Authorization".to_owned(),
                "Bearer ${LINEAR_TOKEN}".to_owned()
            )]
            .into_iter()
            .collect(),
        }
    );
    // Codex mints this server's credential itself, so the imported URL is
    // not the whole story — that has to reach the user as a problem.
    assert!(read.problems.iter().any(|p| {
        p.server.as_deref() == Some("figma") && p.message == "codex-managed auth not carried over"
    }));
    // Unlike Gemini, Codex does have a per-entry off switch.
    assert!(!read.servers["notes"].enabled);
    assert!(read.servers["github"].enabled);
    // An entry with no target field at all is a problem, not a failure.
    assert!(!read.servers.contains_key("husk"));
    // The non-MCP siblings are simply not servers.
    assert_eq!(read.servers.len(), 4);

    insta::assert_debug_snapshot!(read);
}

#[test]
fn opencode_reads_a_commented_file_and_both_entry_types() {
    let read = read_fixture(ClientKind::Opencode, "opencode.jsonc");

    // The command array is one field holding program *and* arguments.
    assert_eq!(
        read.servers["github"].transport,
        mcpgw_core::Transport::Stdio {
            command: "npx".to_owned(),
            args: ["-y", "@modelcontextprotocol/server-github"]
                .map(str::to_owned)
                .to_vec(),
            // Spelled `environment`, not `env`.
            env: [("GITHUB_TOKEN".to_owned(), "$GITHUB_TOKEN".to_owned())]
                .into_iter()
                .collect(),
        }
    );
    // opencode's own `{env:VAR}` interpolation is passed through verbatim.
    assert_eq!(
        read.servers["linear"].transport,
        mcpgw_core::Transport::Http {
            url: "https://mcp.linear.app/mcp".to_owned(),
            headers: [(
                "Authorization".to_owned(),
                "Bearer {env:LINEAR_TOKEN}".to_owned()
            )]
            .into_iter()
            .collect(),
        }
    );
    // opencode holds this server's OAuth tokens itself, so the imported URL
    // is not the whole story — that has to reach the user as a problem.
    assert!(read.problems.iter().any(|p| {
        p.server.as_deref() == Some("figma")
            && p.message == "opencode-managed oauth not carried over"
    }));
    assert!(!read.servers["notes"].enabled);
    assert!(read.servers["github"].enabled);
    // A local entry with an empty command array is a problem, not a failure.
    assert!(!read.servers.contains_key("husk"));
    // The non-MCP siblings ($schema, theme, model) are not servers.
    assert_eq!(read.servers.len(), 4);

    insta::assert_debug_snapshot!(read);
}

#[test]
fn opencode_infers_the_type_and_reports_undecidable_entries() {
    let read = ClientKind::Opencode
        .read_text(
            r#"{"mcp": {
                "local-ish": {"command": ["deno"]},
                "remote-ish": {"url": "https://example.com/mcp"},
                "both": {"command": ["deno"], "url": "https://example.com/mcp"},
                "neither": {"environment": {"A": "B"}},
                "odd": {"type": "carrier-pigeon", "url": "https://example.com/mcp"}
            }}"#,
            Path::new("opencode.json"),
        )
        .unwrap();

    // opencode's schema requires `type`; a hand-written file that omits it
    // still reads, because the target field says which shape it is.
    assert!(matches!(
        read.servers["local-ish"].transport,
        mcpgw_core::Transport::Stdio { .. }
    ));
    assert!(matches!(
        read.servers["remote-ish"].transport,
        mcpgw_core::Transport::Http { .. }
    ));
    assert_eq!(read.servers.len(), 2);
    insta::assert_debug_snapshot!(read.problems);
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

    // Gemini's trace dir also holds its config file, so the two states have
    // to be distinguished by the file rather than by the directory.
    let trace = ClientKind::Gemini.install_trace_with(&env).unwrap();
    std::fs::create_dir_all(&trace).unwrap();
    assert_eq!(ClientKind::Gemini.detect_with(&env), Detection::Installed);
    let config = ClientKind::Gemini.config_path_with(&env).unwrap();
    std::fs::write(&config, "{}").unwrap();
    assert_eq!(
        ClientKind::Gemini.detect_with(&env),
        Detection::Configured(config)
    );

    // Same shape for Codex: ~/.codex exists as soon as the CLI has run,
    // config.toml only once there is something to configure.
    let trace = ClientKind::Codex.install_trace_with(&env).unwrap();
    std::fs::create_dir_all(&trace).unwrap();
    assert_eq!(ClientKind::Codex.detect_with(&env), Detection::Installed);
    let config = ClientKind::Codex.config_path_with(&env).unwrap();
    std::fs::write(&config, "model = \"gpt-5-codex\"\n").unwrap();
    assert_eq!(
        ClientKind::Codex.detect_with(&env),
        Detection::Configured(config)
    );

    // opencode accepts two filenames, so detection has to find whichever of
    // them the machine actually has.
    let trace = ClientKind::Opencode.install_trace_with(&env).unwrap();
    std::fs::create_dir_all(&trace).unwrap();
    assert_eq!(ClientKind::Opencode.detect_with(&env), Detection::Installed);
    let jsonc = trace.join("opencode.jsonc");
    std::fs::write(&jsonc, "// hi\n{}\n").unwrap();
    assert_eq!(
        ClientKind::Opencode.detect_with(&env),
        Detection::Configured(jsonc.clone())
    );
    // With both present the .json spelling wins, matching the order a fresh
    // machine gets created in.
    let json = trace.join("opencode.json");
    std::fs::write(&json, "{}\n").unwrap();
    assert_eq!(
        ClientKind::Opencode.detect_with(&env),
        Detection::Configured(json)
    );
}

#[test]
fn a_machine_with_no_opencode_config_resolves_to_the_json_spelling() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_owned();
    let env = move |key: &str| -> Option<std::ffi::OsString> {
        match key {
            "HOME" | "USERPROFILE" => Some(home.clone().into()),
            _ => None,
        }
    };

    // Both candidates are offered; nothing exists, so a write creates the
    // first of them.
    let candidates = ClientKind::Opencode.config_path_candidates_with(&env);
    assert_eq!(candidates.len(), 2);
    assert!(candidates[0].ends_with(".config/opencode/opencode.json"));
    assert!(candidates[1].ends_with(".config/opencode/opencode.jsonc"));
    assert_eq!(
        ClientKind::Opencode.config_path_with(&env).unwrap(),
        candidates[0]
    );
    // XDG layout on every platform, so this is never the app-data dir.
    assert_ne!(
        ClientKind::Opencode.config_path_with(&env),
        ClientKind::VsCode.config_path_with(&env)
    );
}

#[test]
fn an_existing_opencode_jsonc_is_the_file_that_gets_written() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_owned();
    let env = move |key: &str| -> Option<std::ffi::OsString> {
        match key {
            "HOME" | "USERPROFILE" => Some(home.clone().into()),
            _ => None,
        }
    };

    let jsonc = ClientKind::Opencode
        .install_trace_with(&env)
        .unwrap()
        .join("opencode.jsonc");
    std::fs::create_dir_all(jsonc.parent().unwrap()).unwrap();
    std::fs::write(&jsonc, "{}\n").unwrap();
    assert_eq!(ClientKind::Opencode.config_path_with(&env).unwrap(), jsonc);
}

#[test]
fn load_missing_file_is_not_found() {
    let err = ClientKind::Cursor
        .load(Path::new("/nonexistent/mcp.json"))
        .unwrap_err();
    assert!(matches!(err, Error::NotFound { .. }));
}
