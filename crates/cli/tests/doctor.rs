use std::path::Path;
use std::process::Output;

use assert_cmd::Command;

mod util;
use util::fixture_binary;

/// Runs doctor in a fully sandboxed home: every env key any adapter looks at
/// points into the temp dir, so the real machine never leaks into the test.
fn run_doctor(home: &Path, config_text: Option<&str>, args: &[&str]) -> Output {
    run_doctor_in(home, home, config_text, args)
}

/// The same run from a working directory of the test's choosing — the
/// project-config pass reports what is around the cwd, so a test about it
/// has to say where the process stands.
fn run_doctor_in(home: &Path, cwd: &Path, config_text: Option<&str>, args: &[&str]) -> Output {
    let config = home.join("config.toml");
    if let Some(text) = config_text {
        std::fs::write(&config, text).unwrap();
    }
    Command::cargo_bin("mcpgw")
        .unwrap()
        .current_dir(cwd)
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

/// A service still pointed at a binary that is gone is a warning, not an
/// error: the gateway may be answering perfectly on it. Reported without
/// `--probe`, because no dial could reveal it.
#[test]
fn json_warns_about_a_service_installed_from_a_binary_that_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let gone = dir.path().join("cargo").join("bin").join("mcpgw");
    util::record_installed_spec(dir.path(), &gone, "127.0.0.1", 18137);

    let out = run_doctor(dir.path(), Some(HEALTHY), &["--json"]);
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let findings = value["findings"].as_array().unwrap();
    let stale = findings
        .iter()
        .find(|f| f["message"].as_str().unwrap().contains("which is gone"))
        .unwrap_or_else(|| panic!("no stale-service finding: {findings:?}"));
    assert_eq!(stale["severity"], "warning");
    assert!(
        stale["message"]
            .as_str()
            .unwrap()
            .contains("`mcpgw daemon install`"),
        "{stale:?}"
    );
    // Nothing here is broken, so doctor still exits zero.
    assert!(out.status.success(), "{}", stdout(&out));
}

/// The upgrade that changed nothing: a service installed on this port, a
/// gateway answering there, and the build it is answering on is not the one
/// running `doctor`. Needs all three — the record alone is a file a crash
/// could have left.
#[tokio::test]
async fn json_warns_about_a_gateway_answering_on_another_build() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        util::fixture_config(&["fx1"]),
    )
    .unwrap();
    let (mut child, addr, _endpoints) = util::serve(dir.path(), &[]).await;
    let url = format!("http://{addr}/mcp");
    util::record_installed_spec(
        dir.path(),
        &fixture_binary(),
        "127.0.0.1",
        mcpgw_core::daemon_check::url_port(&url).unwrap(),
    );
    util::rewrite_record_version(dir.path(), &url, "0.0.1").await;

    let home = dir.path().to_owned();
    let out = tokio::task::spawn_blocking(move || run_doctor(&home, None, &["--json"]))
        .await
        .unwrap();
    child.kill().await.unwrap();

    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let findings = value["findings"].as_array().unwrap();
    let stale = findings
        .iter()
        .find(|f| f["message"].as_str().unwrap().contains("runs mcpgw 0.0.1"))
        .unwrap_or_else(|| panic!("no stale-version finding: {findings:?}"));
    assert_eq!(stale["severity"], "warning");
    assert_eq!(
        stale["message"],
        format!(
            "the gateway service runs mcpgw 0.0.1; you are running {} — run \
             `mcpgw daemon install` to restart it on this build",
            env!("CARGO_PKG_VERSION")
        )
    );
    // A warning, so doctor still exits zero.
    assert!(out.status.success(), "{}", stdout(&out));
}

/// No service recorded, nothing to say about one.
#[test]
fn a_machine_with_no_service_gets_no_stale_binary_warning() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_doctor(dir.path(), Some(HEALTHY), &["--json"]);
    assert!(
        !stdout(&out).contains("the gateway service"),
        "{}",
        stdout(&out)
    );
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

    let out = run_doctor(dir.path(), Some(HEALTHY), &["--probe", "--timeout", "60"]);
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
    let out = run_doctor(dir.path(), Some(config), &["--probe", "--timeout", "60"]);
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
        &["--probe", "--timeout", "60", "--json"],
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
            "60",
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
        &["--probe", "--timeout", "60", "--gateway-url", &base],
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
        &["--probe", "--timeout", "60", "--gateway-url", &base],
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
            "60",
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
        &["--probe", "--timeout", "60", "--json"],
    );
    let value = json_of(&out);
    // Nothing points at a gateway, so nothing is said about one — a user who
    // never ran `sync` gets no new noise.
    assert!(value.get("gateway").is_none(), "{value}");
    assert_eq!(value["errors"], 0);
}

/// A repo holding one entry the canonical config already speaks for and one
/// it does not — the split the project section exists to report.
fn fake_repo(root: &Path) -> std::path::PathBuf {
    let repo = root.join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::write(
        repo.join(".mcp.json"),
        r#"{"mcpServers": {"build": {"command": "cargo"}, "scratch": {"command": "cargo", "args": ["run"]}}}"#,
    )
    .unwrap();
    repo
}

#[test]
fn project_configs_get_their_own_section() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let repo = fake_repo(workspace.path());

    let out = run_doctor_in(home.path(), &repo, Some(HEALTHY), &[]);
    let text = stdout(&out);
    // Warnings only: the entries work, they are just nobody's to move.
    assert!(out.status.success(), "{text}");
    assert!(text.contains("project configs"), "{text}");
    assert!(text.contains(".mcp.json"), "{text}");
    assert!(text.contains("Claude Code, 2 servers"), "{text}");
    assert!(
        text.contains("build: mirrors canonical, not managed"),
        "{text}"
    );
    assert!(
        text.contains("scratch: not managed: direct entry stays live after sync"),
        "{text}"
    );
    assert!(
        text.contains("holds 1 direct MCP entry mcpgw does not manage"),
        "{text}"
    );
    assert!(
        text.contains("`mcpgw import --project` adopts them"),
        "{text}"
    );
}

/// The section is about the repo the user is standing in, so a run from
/// anywhere else must not mention it at all.
#[test]
fn a_directory_with_no_project_config_gets_no_section() {
    let home = tempfile::tempdir().unwrap();
    let empty = tempfile::tempdir().unwrap();

    let out = run_doctor_in(home.path(), empty.path(), Some(HEALTHY), &[]);
    let text = stdout(&out);
    assert!(!text.contains("project configs"), "{text}");
    assert!(text.contains("0 errors, 0 warnings"), "{text}");
}

#[test]
fn json_lists_project_configs_and_their_standing() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let repo = fake_repo(workspace.path());

    let out = run_doctor_in(home.path(), &repo, Some(HEALTHY), &["--json"]);
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    let projects = value["projects"].as_array().unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0]["client_id"], "claude-code");
    assert_eq!(projects[0]["unmanaged"], 1);
    assert_eq!(
        projects[0]["servers"],
        serde_json::json!([
            { "name": "build", "mirrors_canonical": true, "managed": false },
            { "name": "scratch", "mirrors_canonical": false, "managed": false },
        ])
    );
    // The warning joins the one findings array, the way the gateway pass's
    // findings do — a consumer counting problems reads one place.
    assert_eq!(value["warnings"], 1);
    let findings = value["findings"].as_array().unwrap();
    assert!(
        findings.iter().any(|finding| {
            finding["client"] == "Claude Code"
                && finding["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("direct MCP entry"))
        }),
        "{findings:?}"
    );
}

/// An empty array rather than a missing key: "no project configs" and "an
/// mcpgw that does not look" have to be different answers.
#[test]
fn json_always_carries_the_projects_array() {
    let home = tempfile::tempdir().unwrap();
    let empty = tempfile::tempdir().unwrap();

    let out = run_doctor_in(home.path(), empty.path(), Some(HEALTHY), &["--json"]);
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(value["projects"], serde_json::json!([]));
}

/// A bare `401` responder on an ephemeral port, and the thread serving it.
///
/// Hand-rolled rather than an MCP fixture on purpose: an OAuth-protected
/// server never gets as far as speaking MCP to a caller with no token, so a
/// challenge and nothing else is exactly what doctor has to make sense of.
fn unauthorized_server() -> String {
    use std::io::{BufRead as _, BufReader, Read as _, Write as _};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            // The whole request is read before the answer goes out: a server
            // that replies to a POST and hangs up on its unread body is a
            // connection reset on the client, not a 401.
            let mut reader = BufReader::new(&stream);
            let mut length = 0usize;
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|n| n > 0) {
                if let Some(value) = line
                    .to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(str::trim)
                {
                    length = value.parse().unwrap_or(0);
                }
                if line.trim().is_empty() {
                    break;
                }
                line.clear();
            }
            let _ = reader.take(length as u64).read_to_end(&mut Vec::new());

            let mut stream = &stream;
            let _ = stream.write_all(
                b"HTTP/1.1 401 Unauthorized\r\n\
                  WWW-Authenticate: Bearer resource_metadata=\"https://auth.example.com/\
                  .well-known/oauth-protected-resource\"\r\n\
                  Content-Length: 0\r\nConnection: close\r\n\r\n",
            );
            let _ = stream.flush();
        }
    });
    url
}

/// The report a user of an OAuth-protected server gets: a warning naming the
/// login, not a handshake error, and not a red exit — there is nothing wrong
/// with the machine, and nothing `--timeout` or a restart would change.
#[test]
fn a_server_behind_oauth_is_a_warning_naming_the_login() {
    let dir = tempfile::tempdir().unwrap();
    let config = format!(
        "version = 1\n[servers.linear]\ntype = \"http\"\nurl = \"{}\"\n",
        unauthorized_server()
    );

    let out = run_doctor(
        dir.path(),
        Some(&config),
        &["--probe", "--timeout", "60", "--json"],
    );
    assert!(out.status.success(), "{}", stdout(&out));
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["errors"], 0);
    assert_eq!(value["warnings"], 1);

    let finding = value["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["code"] == "needs_oauth")
        .unwrap_or_else(|| panic!("no needs_oauth finding in {value}"));
    assert_eq!(finding["severity"], "warning");
    assert_eq!(finding["server"], "linear");
    assert_eq!(
        finding["message"],
        "linear needs OAuth — the gateway cannot complete a client-side login; \
         run mcpgw auth login linear"
    );

    let row = &value["probes"]["results"][0];
    assert_eq!(row["ok"], false);
    assert_eq!(row["code"], "needs_oauth");
    assert_eq!(row["error"], finding["message"]);

    // And the same sentence in the rendered report, as a warning line.
    let text = stdout(&run_doctor(
        dir.path(),
        Some(&config),
        &["--probe", "--timeout", "60"],
    ));
    assert!(text.contains("run mcpgw auth login linear"), "{text}");
    assert!(text.contains("0 errors, 1 warnings"), "{text}");
}

/// Once `sync --project` owns an entry the section says so: "mcpgw writes
/// this" and "this is right today and nobody's to keep right" are different
/// facts, and the whole reason the section exists is to tell them apart.
#[test]
fn the_project_section_separates_managed_entries_from_the_rest() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let repo = fake_repo(workspace.path());

    // The record a `sync --project` leaves: `build` is mcpgw's, `scratch`
    // is the repo's own. Keyed through the same normalisation the product
    // keys by, which is the only spelling a state file ever holds — on
    // Windows a bare `canonicalize` here would write the verbatim
    // `\\?\C:\...` form the running process never sees for its own cwd.
    let repo = mcpgw_core::paths::normalize(&repo);
    let key = mcpgw_core::paths::normalize(&repo.join(".mcp.json"));
    let state = home.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join("managed.json"),
        serde_json::json!({
            "clients": {},
            "files": {
                key.to_string_lossy(): {
                    "client": "claude-code",
                    "managed": ["build"],
                },
            },
        })
        .to_string(),
    )
    .unwrap();

    let out = run_doctor_in(home.path(), &repo, Some(HEALTHY), &[]);
    let text = stdout(&out);
    assert!(text.contains("build: managed by sync"), "{text}");
    assert!(
        text.contains("scratch: not managed: direct entry stays live after sync"),
        "{text}"
    );

    let json = run_doctor_in(home.path(), &repo, Some(HEALTHY), &["--json"]);
    let value: serde_json::Value = serde_json::from_str(&stdout(&json)).unwrap();
    assert_eq!(
        value["projects"][0]["servers"],
        serde_json::json!([
            { "name": "build", "mirrors_canonical": true, "managed": true },
            { "name": "scratch", "mirrors_canonical": false, "managed": false },
        ])
    );
    // Still one warning: the entry nobody manages is still live.
    assert_eq!(value["warnings"], 1);
}

/// A `headers_command` is resolved like any other command mcpgw spawns, and
/// the report names it rather than the credential it would have produced.
#[test]
fn an_unresolvable_headers_command_is_an_error_finding() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_doctor(
        dir.path(),
        Some(
            r#"
version = 1
[servers.corp]
type = "http"
url = "https://mcp.corp.example/mcp"
headers_command = "definitely-not-a-real-command-mcpgw print-headers"
"#,
        ),
        &[],
    );
    // Errors mean a non-zero exit, the same as any other broken command.
    assert_eq!(out.status.code(), Some(1));
    let text = stdout(&out);
    assert!(text.contains("headers from command"), "{text}");
    assert!(
        text.contains("definitely-not-a-real-command-mcpgw print-headers"),
        "{text}"
    );
    assert!(text.contains("absolute path"), "{text}");
}

/// The rule the whole feature stands on: `--probe` runs the command for real,
/// and nothing it printed reaches the report. What does reach it is the
/// command line, which is not the secret.
#[test]
fn a_probe_runs_the_headers_command_without_printing_what_it_produced() {
    let dir = tempfile::tempdir().unwrap();
    let tokens = dir.path().join("tokens");
    std::fs::write(&tokens, "s3cret-rotating-token").unwrap();
    let config = format!(
        "version = 1\n[servers.corp]\ntype = \"http\"\n\
         # Port 1 on loopback refuses instantly, so the probe fails *after*\n\
         # the command has run and handed its headers over.\n\
         url = \"http://127.0.0.1:1/mcp\"\n\
         headers_command = ['{}', 'headers', '{}']\n",
        fixture_binary().display(),
        tokens.display()
    );

    let out = run_doctor(dir.path(), Some(&config), &["--probe"]);
    let text = stdout(&out);
    assert!(text.contains("headers from command"), "{text}");
    assert!(
        !text.contains("s3cret-rotating-token"),
        "the command's output reached the report: {text}"
    );
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("s3cret-rotating-token"),
        "the command's output reached stderr"
    );
}
