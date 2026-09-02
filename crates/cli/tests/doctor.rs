use std::path::Path;
use std::process::Output;

use assert_cmd::Command;

mod util;
use util::fixture_binary;

/// Runs doctor in a fully sandboxed home: every env key any adapter looks at
/// points into the temp dir, so the real machine never leaks into the test.
fn run_doctor(home: &Path, config_text: Option<&str>, args: &[&str]) -> Output {
    let config = home.join("config.toml");
    if let Some(text) = config_text {
        std::fs::write(&config, text).unwrap();
    }
    Command::cargo_bin("mcpgw")
        .unwrap()
        // Hermetic: no test may phone home for a version notice.
        .env("MCPGW_NO_UPDATE_CHECK", "1")
        .arg("doctor")
        .args(args)
        .env("MCPGW_CONFIG", &config)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("APPDATA", home.join("AppData"))
        // The managed-state file lives here; pinning it keeps a developer's
        // real one out of the gateway pass.
        .env("MCPGW_STATE_DIR", home.join("state"))
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .output()
        .unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

// `cargo` is the one command guaranteed to exist wherever these tests run.
const HEALTHY: &str = r#"
version = 1
[servers.build]
type = "stdio"
command = "cargo"
"#;

const BROKEN_COMMAND: &str = r#"
version = 1
[servers.ghost]
type = "stdio"
command = "definitely-not-a-real-command-mcpgw"
"#;

#[test]
fn healthy_config_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_doctor(dir.path(), Some(HEALTHY), &[]);
    assert!(out.status.success(), "{}", stdout(&out));
    assert!(stdout(&out).contains("0 errors"));
}

#[test]
fn unresolvable_command_is_an_error_exit() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_doctor(dir.path(), Some(BROKEN_COMMAND), &[]);
    assert!(!out.status.success());
    assert!(stdout(&out).contains("not found in PATH"));
}

#[test]
fn missing_canonical_config_is_fine() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_doctor(dir.path(), None, &[]);
    assert!(out.status.success(), "{}", stdout(&out));
    assert!(stdout(&out).contains("not created yet"));
}

#[test]
fn client_warnings_do_not_fail_the_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    // A configured Cursor with an sse entry: lossy note -> warning only.
    let cursor = dir.path().join(".cursor");
    std::fs::create_dir_all(&cursor).unwrap();
    std::fs::write(
        cursor.join("mcp.json"),
        r#"{"mcpServers": {"linear": {"type": "sse", "url": "https://mcp.linear.app/sse"}}}"#,
    )
    .unwrap();

    let out = run_doctor(dir.path(), Some(HEALTHY), &[]);
    assert!(out.status.success(), "{}", stdout(&out));
    let text = stdout(&out);
    assert!(text.contains("legacy `sse`"));
    assert!(text.contains("1 warnings"));
}

#[test]
fn broken_client_entry_fails_the_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let cursor = dir.path().join(".cursor");
    std::fs::create_dir_all(&cursor).unwrap();
    std::fs::write(
        cursor.join("mcp.json"),
        r#"{"mcpServers": {"husk": {"env": {"A": "B"}}}}"#,
    )
    .unwrap();

    let out = run_doctor(dir.path(), Some(HEALTHY), &[]);
    assert!(!out.status.success());
    assert!(stdout(&out).contains("neither `command` nor `url`"));
}

#[test]
fn json_output_carries_findings_and_counts() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_doctor(dir.path(), Some(BROKEN_COMMAND), &["--json"]);
    assert!(!out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["errors"], 1);
    assert_eq!(value["findings"][0]["severity"], "error");
    assert_eq!(value["findings"][0]["server"], "ghost");
    let clients = value["clients"].as_array().unwrap();
    assert_eq!(clients.len(), 13);
    for name in [
        "Gemini CLI",
        "Codex CLI",
        "opencode",
        "Windsurf",
        "Zed",
        "Cline",
        "Cline CLI",
        "Amp",
        "Zoo Code",
    ] {
        assert!(clients.iter().any(|c| c["client"] == name), "{clients:?}");
    }
}

#[test]
fn probe_reports_handshake_failure_with_nonzero_exit() {
    let dir = tempfile::tempdir().unwrap();
    // `cargo` exists (so static doctor is green) but speaks no MCP: the
    // static pass alone exits 0, the probe pass must exit 1.
    let static_out = run_doctor(dir.path(), Some(HEALTHY), &[]);
    assert!(static_out.status.success());

    let out = run_doctor(dir.path(), Some(HEALTHY), &["--probe", "--timeout", "15"]);
    assert!(!out.status.success());
    let text = stdout(&out);
    assert!(text.contains("probes"), "{text}");
    assert!(text.contains("build (canonical)"), "{text}");
}

#[test]
fn probe_reaches_http_servers_and_reports_failures() {
    let dir = tempfile::tempdir().unwrap();
    // Port 1 on loopback refuses instantly: the static pass is green, the
    // probe pass must reach out and fail loudly.
    let config = r#"
version = 1
[servers.remote]
type = "http"
url = "http://127.0.0.1:1/mcp"
"#;
    let out = run_doctor(dir.path(), Some(config), &["--probe", "--timeout", "15"]);
    assert!(!out.status.success(), "{}", stdout(&out));
    let text = stdout(&out);
    assert!(text.contains("remote (canonical)"), "{text}");
    assert!(text.contains("1 errors"), "{text}");
}

#[test]
fn probe_json_carries_results() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_doctor(
        dir.path(),
        Some(HEALTHY),
        &["--probe", "--timeout", "15", "--json"],
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let probes = value["probes"].as_object().unwrap();
    assert!(!probes.contains_key("skipped_http"));
    let results = probes["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["ok"], false);
}

/// Serves `names` — each one the healthy fixture — on an ephemeral port, one
/// endpoint per name, exactly as `mcpgw serve --per-server` would.
///
/// The runtime comes back with the URL because it owns the server task: drop
/// it and the gateway goes away mid-test.
fn fixture_gateway(names: &[&str]) -> (tokio::runtime::Runtime, String) {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use mcpgw_core::endpoints::{EndpointTable, Endpoints};
    use mcpgw_core::gateway::{Gateway, serve_http_with};
    use mcpgw_core::upstream::UpstreamManager;

    let servers: BTreeMap<String, mcpgw_core::Server> = names
        .iter()
        .map(|name| ((*name).to_owned(), fixture_server()))
        .collect();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let url = runtime.block_on(async {
        let manager = Arc::new(UpstreamManager::new(servers));
        let aggregate = Gateway::aggregate(
            Arc::clone(&manager),
            names.iter().map(|n| (*n).to_owned()).collect(),
        );
        let pipes: Vec<_> = names
            .iter()
            .map(|name| {
                (
                    (*name).to_owned(),
                    Gateway::new(Arc::clone(&manager), (*name).to_owned()),
                )
            })
            .collect();
        let endpoints = Endpoints::new(EndpointTable::new(pipes));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve_http_with(
            aggregate,
            Some(endpoints),
            listener,
            std::future::pending(),
        ));
        format!("http://{addr}/mcp")
    });
    (runtime, url)
}

fn fixture_server() -> mcpgw_core::Server {
    mcpgw_core::Server {
        enabled: true,
        tags: Vec::new(),
        transport: mcpgw_core::Transport::Stdio {
            command: fixture_binary().to_string_lossy().into_owned(),
            args: vec!["healthy".to_owned()],
            env: std::collections::BTreeMap::new(),
        },
    }
}

/// A canonical config whose one server is the healthy fixture, so the direct
/// probe pass stays green and anything red comes from the gateway pass.
fn canonical_fixture() -> String {
    format!(
        "version = 1\n[servers.fx]\ntype = \"stdio\"\ncommand = {:?}\nargs = [\"healthy\"]\n",
        fixture_binary().to_string_lossy()
    )
}

/// Writes a Cursor config holding `entries` (name → url) and records every one
/// of them as mcpgw-managed — which is what makes doctor treat them as the
/// path its own clients take rather than as the user's business.
fn managed_cursor(home: &Path, entries: &[(&str, &str)]) {
    let cursor = home.join(".cursor");
    std::fs::create_dir_all(&cursor).unwrap();
    let servers: serde_json::Map<String, serde_json::Value> = entries
        .iter()
        .map(|(name, url)| {
            (
                (*name).to_owned(),
                serde_json::json!({ "type": "http", "url": url }),
            )
        })
        .collect();
    std::fs::write(
        cursor.join("mcp.json"),
        serde_json::to_string(&serde_json::json!({ "mcpServers": servers })).unwrap(),
    )
    .unwrap();

    let state = home.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let names: Vec<&str> = entries.iter().map(|(name, _)| *name).collect();
    std::fs::write(
        state.join("managed.json"),
        serde_json::to_string(&serde_json::json!({ "clients": { "cursor": names } })).unwrap(),
    )
    .unwrap();
}

fn json_of(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|err| panic!("{err}: {}", stdout(out)))
}

#[test]
fn a_served_endpoint_reports_reachable_through_the_gateway() {
    let dir = tempfile::tempdir().unwrap();
    let (_runtime, base) = fixture_gateway(&["fx"]);
    let endpoint = mcpgw_core::endpoints::per_server_url(&base, "fx").unwrap();
    managed_cursor(dir.path(), &[("fx", &endpoint)]);

    let out = run_doctor(
        dir.path(),
        Some(&canonical_fixture()),
        &[
            "--probe",
            "--timeout",
            "15",
            "--gateway-url",
            &base,
            "--json",
        ],
    );
    let value = json_of(&out);
    assert!(out.status.success(), "{}", stdout(&out));
    let gateway = &value["gateway"];
    assert_eq!(gateway["reachable"], true);
    let results = gateway["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["ok"], true);
    assert_eq!(results[0]["url"], endpoint.as_str());
    assert_eq!(results[0]["server"], "fx");
    assert_eq!(results[0]["tools"], 2);
    assert_eq!(results[0]["entries"][0]["client"], "Cursor");

    // The two passes are separate sections, and the entry is only in the
    // gateway one — the same URL under both headings would be one problem
    // reported twice.
    let text = stdout(&run_doctor(
        dir.path(),
        Some(&canonical_fixture()),
        &["--probe", "--timeout", "15", "--gateway-url", &base],
    ));
    assert!(text.contains("probes — direct to each server"), "{text}");
    assert!(
        text.contains(&format!("probes — through the gateway at {base}")),
        "{text}"
    );
    assert!(text.contains(r#"Cursor "fx""#), "{text}");
    assert!(text.contains("0 errors"), "{text}");
}

#[test]
fn an_entry_the_gateway_does_not_serve_is_an_actionable_error() {
    let dir = tempfile::tempdir().unwrap();
    let (_runtime, base) = fixture_gateway(&["fx"]);
    // A name the gateway never served: a renamed or disabled server whose
    // client entry stayed behind.
    let stale = mcpgw_core::endpoints::per_server_url(&base, "ghost").unwrap();
    managed_cursor(dir.path(), &[("ghost", &stale)]);

    let out = run_doctor(
        dir.path(),
        Some(&canonical_fixture()),
        &["--probe", "--timeout", "15", "--gateway-url", &base],
    );
    assert!(!out.status.success(), "{}", stdout(&out));
    let text = stdout(&out);
    assert!(text.contains("does not serve"), "{text}");
    // Actionable means naming what it does serve.
    assert!(text.contains("/s/fx"), "{text}");
    assert!(text.contains("1 errors"), "{text}");
}

#[test]
fn a_down_gateway_is_reported_once_however_many_entries_point_at_it() {
    let dir = tempfile::tempdir().unwrap();
    // Nothing is listening on port 1, so the connect is refused instantly.
    let base = "http://127.0.0.1:1/mcp";
    managed_cursor(
        dir.path(),
        &[
            ("fx", "http://127.0.0.1:1/s/fx"),
            ("other", "http://127.0.0.1:1/s/other"),
            ("third", "http://127.0.0.1:1/s/third"),
        ],
    );

    let out = run_doctor(
        dir.path(),
        Some(&canonical_fixture()),
        &[
            "--probe",
            "--timeout",
            "15",
            "--gateway-url",
            base,
            "--json",
        ],
    );
    let value = json_of(&out);
    assert!(!out.status.success(), "{}", stdout(&out));
    assert_eq!(value["gateway"]["reachable"], false);
    assert_eq!(value["gateway"]["skipped"], 3);
    // Three entries, three endpoints, one sentence.
    assert_eq!(value["errors"], 1);
    let findings = value["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1);
    assert!(
        findings[0]["message"]
            .as_str()
            .unwrap()
            .contains("mcpgw serve"),
        "{findings:?}"
    );
}

#[test]
fn without_managed_gateway_entries_there_is_no_gateway_section() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_doctor(
        dir.path(),
        Some(&canonical_fixture()),
        &["--probe", "--timeout", "15", "--json"],
    );
    let value = json_of(&out);
    // Nothing points at a gateway, so nothing is said about one — a user who
    // never ran `sync` gets no new noise.
    assert!(value.get("gateway").is_none(), "{value}");
    assert_eq!(value["errors"], 0);
}
