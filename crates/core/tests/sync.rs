use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use mcpgw_core::state::ManagedState;
use mcpgw_core::sync::{apply_plan, client_entry, gateway_server, plan_sync};
use mcpgw_core::{ClientKind, Config, backup};

fn canonical() -> BTreeMap<String, mcpgw_core::Server> {
    Config::parse(
        r#"
version = 1

[servers.github]
type = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { TOKEN = "t" }

[servers.linear]
type = "http"
url = "https://mcp.linear.app/mcp"

[servers.parked]
type = "stdio"
command = "npx"
enabled = false
"#,
        Path::new("c.toml"),
    )
    .unwrap()
    .servers
}

fn managed(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|&n| n.to_owned()).collect()
}

#[test]
fn plan_covers_all_categories() {
    // Client currently has: an outdated managed github, a managed entry
    // whose canonical source is now disabled, a user's own linear
    // (conflict), and an unrelated foreign entry.
    let current_json = serde_json::json!({
        "github": { "command": "npx", "args": ["old"] },
        "parked": { "command": "npx" },
        "linear": { "url": "https://user-added.example/mcp" },
        "users-own": { "command": "deno" }
    });
    let current = current_json.as_object().unwrap();
    let plan = plan_sync(
        ClientKind::Cursor,
        current,
        &canonical(),
        &managed(&["github", "parked"]),
    );
    assert_eq!(plan.updates, ["github"]);
    assert_eq!(plan.removes, ["parked"]);
    assert_eq!(plan.conflicts, ["linear"]);
    assert_eq!(plan.foreign, ["users-own"]);
    assert!(plan.adds.is_empty());
    assert!(plan.has_changes());
    // The conflicted name must not become managed.
    assert_eq!(plan.managed_after(), managed(&["github"]));
}

#[test]
fn plan_is_idempotent_after_apply() {
    let mut root = serde_json::json!({
        "otherSetting": true,
        "mcpServers": { "users-own": { "command": "deno" } }
    });
    let plan = plan_sync(
        ClientKind::Cursor,
        root["mcpServers"].as_object().unwrap(),
        &canonical(),
        &managed(&[]),
    );
    assert_eq!(plan.adds, ["github", "linear"]);
    apply_plan(ClientKind::Cursor, &mut root, &plan);

    // Foreign entry and unrelated root keys survive.
    assert_eq!(root["otherSetting"], true);
    assert_eq!(root["mcpServers"]["users-own"]["command"], "deno");
    insta::assert_snapshot!(serde_json::to_string_pretty(&root).unwrap());

    // A second plan over the applied state sees nothing to do.
    let again = plan_sync(
        ClientKind::Cursor,
        root["mcpServers"].as_object().unwrap(),
        &canonical(),
        &plan.managed_after(),
    );
    assert!(!again.has_changes());
}

#[test]
fn apply_creates_root_key_in_empty_document() {
    let mut root = serde_json::json!({});
    let plan = plan_sync(
        ClientKind::VsCode,
        &serde_json::Map::new(),
        &canonical(),
        &managed(&[]),
    );
    apply_plan(ClientKind::VsCode, &mut root, &plan);
    assert!(root["servers"]["github"].is_object());
}

#[test]
fn entry_shapes_per_client() {
    let canonical = canonical();
    let vs_stdio = client_entry(ClientKind::VsCode, &canonical["github"]);
    let cursor_stdio = client_entry(ClientKind::Cursor, &canonical["github"]);
    let cursor_http = client_entry(ClientKind::Cursor, &canonical["linear"]);
    // VS Code carries an explicit type on stdio; mcpServers clients don't.
    assert_eq!(vs_stdio["type"], "stdio");
    assert!(cursor_stdio.get("type").is_none());
    assert_eq!(cursor_http["type"], "http");

    // Gemini has no `type`, and its remote field must be `httpUrl`: writing
    // `url` there would configure the legacy SSE transport instead.
    let gemini_stdio = client_entry(ClientKind::Gemini, &canonical["github"]);
    let gemini_http = client_entry(ClientKind::Gemini, &canonical["linear"]);
    assert_eq!(gemini_stdio, cursor_stdio);
    assert_eq!(gemini_http["httpUrl"], "https://mcp.linear.app/mcp");
    assert!(gemini_http.get("url").is_none());
    assert!(gemini_http.get("type").is_none());

    // Codex spells remote entries `url` + `http_headers`, and its stdio
    // shape is the plain one — the TOML rendering is the codec's job.
    let codex_stdio = client_entry(ClientKind::Codex, &canonical["github"]);
    let codex_http = client_entry(ClientKind::Codex, &canonical["linear"]);
    assert_eq!(codex_stdio, cursor_stdio);
    assert_eq!(codex_http["url"], "https://mcp.linear.app/mcp");
    assert!(codex_http.get("type").is_none());
    assert!(codex_http.get("headers").is_none());
    let with_headers = client_entry(
        ClientKind::Codex,
        &mcpgw_core::Server {
            enabled: true,
            tags: Vec::new(),
            transport: mcpgw_core::Transport::Http {
                url: "https://h.example/mcp".to_owned(),
                headers: [("Authorization".to_owned(), "Bearer t".to_owned())]
                    .into_iter()
                    .collect(),
            },
        },
    );
    assert_eq!(with_headers["http_headers"]["Authorization"], "Bearer t");

    // opencode spells the transport `local`/`remote`, folds the program and
    // its arguments into one array, and calls the variables `environment`.
    let opencode_stdio = client_entry(ClientKind::Opencode, &canonical["github"]);
    let opencode_http = client_entry(ClientKind::Opencode, &canonical["linear"]);
    assert_eq!(opencode_stdio["type"], "local");
    assert_eq!(
        opencode_stdio["command"],
        serde_json::json!(["npx", "-y", "@modelcontextprotocol/server-github"])
    );
    assert!(opencode_stdio.get("args").is_none());
    assert_eq!(opencode_stdio["environment"]["TOKEN"], "t");
    assert!(opencode_stdio.get("env").is_none());
    assert_eq!(opencode_http["type"], "remote");
    assert_eq!(opencode_http["url"], "https://mcp.linear.app/mcp");
    assert!(opencode_http.get("headers").is_none());

    // Windsurf is the shared stdio shape, and `serverUrl` for remote — a
    // plain `url` there is a field Windsurf does not read.
    let windsurf_stdio = client_entry(ClientKind::Windsurf, &canonical["github"]);
    let windsurf_http = client_entry(ClientKind::Windsurf, &canonical["linear"]);
    assert_eq!(windsurf_stdio, cursor_stdio);
    assert_eq!(windsurf_http["serverUrl"], "https://mcp.linear.app/mcp");
    assert!(windsurf_http.get("url").is_none());
    assert!(windsurf_http.get("type").is_none());

    // Zed is the shared shape plus the `source` Zed silently requires; an
    // entry written without it is one Zed drops on the floor.
    let zed_stdio = client_entry(ClientKind::Zed, &canonical["github"]);
    let zed_http = client_entry(ClientKind::Zed, &canonical["linear"]);
    assert_eq!(zed_stdio["source"], "custom");
    assert_eq!(zed_http["source"], "custom");
    assert_eq!(zed_stdio["command"], "npx");
    assert_eq!(zed_stdio["env"]["TOKEN"], "t");
    assert_eq!(zed_http["url"], "https://mcp.linear.app/mcp");
    assert!(zed_http.get("type").is_none());

    // Cline is the shared stdio shape; a remote entry needs its camelCase
    // `type`, because an untyped one means the legacy SSE transport there.
    let cline_stdio = client_entry(ClientKind::Cline, &canonical["github"]);
    let cline_http = client_entry(ClientKind::Cline, &canonical["linear"]);
    assert_eq!(cline_stdio, cursor_stdio);
    assert_eq!(cline_http["type"], "streamableHttp");
    assert_eq!(cline_http["url"], "https://mcp.linear.app/mcp");
    // `disabled` and `autoApprove` are Cline's own; mcpgw writes neither.
    assert!(cline_http.get("disabled").is_none());
    assert!(cline_stdio.get("autoApprove").is_none());
    // Both surfaces write the same bytes.
    assert_eq!(
        client_entry(ClientKind::ClineCli, &canonical["linear"]),
        cline_http
    );

    insta::assert_snapshot!(serde_json::to_string_pretty(&vs_stdio).unwrap());
}

/// The clients whose entry shape is rewritten rather than passed through:
/// opencode splits and rejoins the `command` array, Windsurf renames the
/// remote URL, Zed adds a `source` its reader has to ignore, Cline spells the
/// remote type its own way. Emitting and re-reading has to give back the
/// server that went in — for every client, but for these it is load bearing
/// rather than incidental.
#[test]
fn emitting_and_re_reading_an_entry_returns_the_same_server() {
    let mut servers: Vec<mcpgw_core::Server> = canonical().into_values().collect();
    // Disabled is a canonical fact, not an emitted one: a client only ever
    // receives entries that are on.
    for server in &mut servers {
        server.enabled = true;
    }
    servers.push(mcpgw_core::Server {
        enabled: true,
        tags: Vec::new(),
        transport: mcpgw_core::Transport::Http {
            url: "https://mcp.linear.app/mcp".to_owned(),
            headers: [(
                "Authorization".to_owned(),
                "Bearer {env:LINEAR_TOKEN}".to_owned(),
            )]
            .into_iter()
            .collect(),
        },
    });
    // A bare command with no arguments: the array must not collapse.
    servers.push(mcpgw_core::Server {
        enabled: true,
        tags: Vec::new(),
        transport: mcpgw_core::Transport::Stdio {
            command: "notes-mcp".to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
        },
    });

    for kind in [
        ClientKind::Opencode,
        ClientKind::Windsurf,
        ClientKind::Zed,
        ClientKind::Cline,
        ClientKind::ClineCli,
    ] {
        let entries = kind.codec().entries;
        for server in &servers {
            let emitted = entries.emit(server);
            let (parsed, note) = entries.parse(&emitted).unwrap();
            assert_eq!(
                &parsed,
                server,
                "{} round trip changed {emitted}",
                kind.id()
            );
            assert_eq!(note, None);
        }
    }
}

#[test]
fn gateway_entry_shapes_per_client() {
    let url = "http://127.0.0.1:8137/mcp";
    let http = client_entry(
        ClientKind::Cursor,
        &gateway_server(ClientKind::Cursor, url, "mcpgw"),
    );
    assert_eq!(http["type"], "http");
    assert_eq!(http["url"], url);
    assert!(http.get("command").is_none());

    // Claude Desktop cannot take a URL, so it gets the stdio bridge.
    let stdio = client_entry(
        ClientKind::ClaudeDesktop,
        &gateway_server(ClientKind::ClaudeDesktop, url, "/opt/mcpgw"),
    );
    assert_eq!(stdio["command"], "/opt/mcpgw");
    assert_eq!(stdio["args"], serde_json::json!(["connect", "--url", url]));
    assert!(stdio.get("url").is_none());

    // Gemini takes the gateway over HTTP, spelled its own way.
    let gemini = client_entry(
        ClientKind::Gemini,
        &gateway_server(ClientKind::Gemini, url, "mcpgw"),
    );
    assert_eq!(gemini["httpUrl"], url);
    assert!(gemini.get("url").is_none());
    assert!(gemini.get("command").is_none());

    // opencode takes the gateway as a remote entry.
    let opencode = client_entry(
        ClientKind::Opencode,
        &gateway_server(ClientKind::Opencode, url, "mcpgw"),
    );
    assert_eq!(opencode["type"], "remote");
    assert_eq!(opencode["url"], url);
    assert!(opencode.get("command").is_none());

    // Codex takes the gateway over HTTP too, with no `type` to go with it.
    let codex = client_entry(
        ClientKind::Codex,
        &gateway_server(ClientKind::Codex, url, "mcpgw"),
    );
    assert_eq!(codex["url"], url);
    assert!(codex.get("type").is_none());
    assert!(codex.get("command").is_none());

    // Windsurf takes the gateway over HTTP under its own field name.
    let windsurf = client_entry(
        ClientKind::Windsurf,
        &gateway_server(ClientKind::Windsurf, url, "mcpgw"),
    );
    assert_eq!(windsurf["serverUrl"], url);
    assert!(windsurf.get("url").is_none());
    assert!(windsurf.get("command").is_none());

    // The gateway is an entry like any other, so it needs the `source` too.
    let zed = client_entry(
        ClientKind::Zed,
        &gateway_server(ClientKind::Zed, url, "mcpgw"),
    );
    assert_eq!(zed["url"], url);
    assert_eq!(zed["source"], "custom");
    assert!(zed.get("command").is_none());

    // Cline reads an untyped remote entry as SSE, so even the gateway has
    // to carry the type that says otherwise.
    let cline = client_entry(
        ClientKind::Cline,
        &gateway_server(ClientKind::Cline, url, "mcpgw"),
    );
    assert_eq!(cline["url"], url);
    assert_eq!(cline["type"], "streamableHttp");
    assert!(cline.get("command").is_none());

    for kind in ClientKind::ALL {
        assert_eq!(
            kind.supports_http_entries(),
            kind != ClientKind::ClaudeDesktop
        );
    }
}

#[test]
fn backups_prune_to_keep_and_latest_wins() {
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join("state");
    let file = dir.path().join("mcp.json");
    for i in 0..8 {
        std::fs::write(&file, format!("{{\"gen\": {i}}}")).unwrap();
        backup::backup_file(&state_dir, "cursor", &file).unwrap();
    }
    let backups: Vec<_> = std::fs::read_dir(state_dir.join("backups/cursor"))
        .unwrap()
        .collect();
    assert_eq!(backups.len(), backup::KEEP);
    let latest = backup::latest_backup(&state_dir, "cursor")
        .unwrap()
        .unwrap();
    assert_eq!(std::fs::read_to_string(latest).unwrap(), "{\"gen\": 7}");
    assert!(
        backup::latest_backup(&state_dir, "vscode")
            .unwrap()
            .is_none()
    );
}

#[test]
fn state_round_trips_and_tolerates_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("managed.json");
    assert_eq!(ManagedState::load(&path).unwrap(), ManagedState::default());

    let mut state = ManagedState::default();
    state
        .clients
        .insert("cursor".to_owned(), managed(&["github"]));
    state.save(&path).unwrap();
    assert_eq!(ManagedState::load(&path).unwrap(), state);
}

#[test]
fn client_ids_round_trip() {
    for kind in ClientKind::ALL {
        assert_eq!(ClientKind::from_id(kind.id()), Some(kind));
    }
    assert_eq!(ClientKind::from_id("emacs"), None);
}
