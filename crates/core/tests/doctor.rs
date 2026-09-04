use std::path::Path;

use mcpgw_core::auth::TokenState;
use mcpgw_core::doctor::{
    GatewayFault, GatewayPlan, NEEDS_OAUTH, Severity, check_server, classify_gateway_failure,
    classify_problems, gateway_unreachable, needs_oauth, unmatched_tool_rules, unserved_endpoint,
};
use mcpgw_core::{ClientKind, Config, Server, Transport};

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

fn http(url: &str) -> mcpgw_core::Server {
    mcpgw_core::Server {
        enabled: true,
        tags: Vec::new(),
        tools: None,
        transport: mcpgw_core::Transport::Http {
            url: url.to_owned(),
            headers_command: Vec::new(),
            headers: std::collections::BTreeMap::new(),
            auth: None,
        },
    }
}

fn bridge(command: &str, args: &[&str]) -> mcpgw_core::Server {
    mcpgw_core::Server {
        enabled: true,
        tags: Vec::new(),
        tools: None,
        transport: mcpgw_core::Transport::Stdio {
            command: command.to_owned(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            env: std::collections::BTreeMap::new(),
        },
    }
}

const BASE: &str = "http://127.0.0.1:8137/mcp";

#[test]
fn only_entries_aimed_at_this_gateway_are_collected() {
    let mut plan = GatewayPlan::new(BASE);
    assert!(plan.collect("Cursor", "github", &http("http://127.0.0.1:8137/s/github")));
    // Same socket, other spelling of loopback: still this gateway.
    assert!(plan.collect("Zed", "github", &http("http://localhost:8137/s/github")));
    // A different port, a hosted remote and a plain stdio server are all
    // somebody else's business.
    assert!(!plan.collect("Zed", "other", &http("http://127.0.0.1:9000/s/github")));
    assert!(!plan.collect("Zed", "linear", &http("https://mcp.linear.app/mcp")));
    assert!(!plan.collect("Zed", "local", &bridge("npx", &["-y", "pkg"])));

    let targets = plan.into_targets();
    // The two loopback spellings are one endpoint, and both entries are named
    // on it — each is a file somebody would have to go and edit.
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].server.as_deref(), Some("github"));
    assert_eq!(targets[0].entries.len(), 2);
    assert_eq!(targets[0].label(), r#"Cursor "github", Zed "github""#);
}

#[test]
fn the_stdio_bridge_resolves_the_url_connect_would_dial() {
    let mut plan = GatewayPlan::new(BASE);
    // What `sync` writes for a client with no http entries.
    assert!(plan.collect(
        "Claude Desktop",
        "github",
        &bridge(
            "/usr/local/bin/mcpgw",
            &["connect", "--server", "github", "--url", BASE],
        ),
    ));
    // The legacy entry a 0.3.x sync wrote, in the bare form that defaults
    // to the serve port: still ours, and still to be migrated.
    assert!(plan.collect("Codex CLI", "mcpgw", &bridge("mcpgw", &["connect"])));
    // Some other command called `connect` is not our bridge.
    assert!(!plan.collect("Zed", "x", &bridge("socat", &["connect"])));

    let targets = plan.into_targets();
    let urls: Vec<&str> = targets.iter().map(|t| t.url.as_str()).collect();
    assert_eq!(
        urls,
        [
            "http://127.0.0.1:8137/mcp",
            "http://127.0.0.1:8137/s/github"
        ]
    );
    // The base endpoint belongs to no single server, which is what marks
    // that entry as one to migrate rather than one that is served.
    assert_eq!(targets[0].server, None);
    assert_eq!(targets[1].server.as_deref(), Some("github"));
}

#[test]
fn disabled_entries_are_left_alone() {
    let mut plan = GatewayPlan::new(BASE);
    let mut parked = http("http://127.0.0.1:8137/s/github");
    parked.enabled = false;
    assert!(!plan.collect("Cursor", "github", &parked));
    assert!(plan.is_empty());
}

#[test]
fn a_down_gateway_is_one_finding_however_many_entries_point_at_it() {
    let mut plan = GatewayPlan::new(BASE);
    for client in ["Cursor", "Zed", "VS Code"] {
        plan.collect(client, "github", &http("http://127.0.0.1:8137/s/github"));
        plan.collect(client, "linear", &http("http://127.0.0.1:8137/s/linear"));
    }
    assert_eq!(plan.len(), 2);

    let finding = gateway_unreachable(plan.base());
    assert_eq!(finding.severity, Severity::Error);
    assert_eq!(finding.client, None);
    assert!(finding.message.contains("mcpgw serve"), "{finding:?}");
    assert!(finding.message.contains(BASE), "{finding:?}");
}

#[test]
fn an_unserved_endpoint_names_every_file_holding_it() {
    let mut plan = GatewayPlan::new(BASE);
    plan.collect("Cursor", "ghost", &http("http://127.0.0.1:8137/s/ghost"));
    plan.collect("Zed", "ghost", &http("http://127.0.0.1:8137/s/ghost"));
    let target = &plan.into_targets()[0];

    let findings = unserved_endpoint(target, "known endpoints: /s/github");
    assert_eq!(findings.len(), 2);
    assert!(findings.iter().all(|f| f.severity == Severity::Error));
    assert_eq!(findings[0].client.as_deref(), Some("Cursor"));
    assert_eq!(findings[1].client.as_deref(), Some("Zed"));
    // The message has to carry both halves: what is wrong, and what is there.
    assert!(findings[0].message.contains("/s/ghost"), "{findings:?}");
    assert!(
        findings[0].message.contains("known endpoints: /s/github"),
        "{findings:?}"
    );
}

#[test]
fn a_404_is_the_only_failure_that_means_wrong_address() {
    // Verbatim shape of an rmcp transport error over the gateway's own 404.
    let body = "MCP handshake failed: Send message error Transport error: unexpected \
        server response: HTTP 404 Not Found: no server endpoint named \"ghost\" — \
        known endpoints: /s/github\n, when send initialize request";
    assert_eq!(
        classify_gateway_failure(body),
        GatewayFault::Unserved(
            "no server endpoint named \"ghost\" — known endpoints: /s/github".to_owned()
        )
    );

    // A 404 with no body of ours — something that is not an mcpgw gateway is
    // listening on that port — still has to say something actionable.
    let bare = "MCP handshake failed: unexpected server response: HTTP 404 Not Found";
    let GatewayFault::Unserved(detail) = classify_gateway_failure(bare) else {
        panic!("a 404 is an unserved endpoint");
    };
    assert!(detail.contains("what is listening"), "{detail}");

    // Everything else is the endpoint answering badly, not the wrong address.
    assert_eq!(
        classify_gateway_failure("no response within 10s"),
        GatewayFault::Failed
    );
    assert_eq!(
        classify_gateway_failure("unexpected server response: HTTP 500"),
        GatewayFault::Failed
    );
}

/// A server behind OAuth is not a broken server: the report says so as a
/// warning, names the login, and tags the finding so a `--json` consumer can
/// act on it without matching prose.
#[test]
fn a_server_needing_oauth_is_a_tagged_warning_naming_the_login() {
    let finding = needs_oauth(None, "linear", None);
    assert_eq!(finding.severity, Severity::Warning);
    assert_eq!(finding.server.as_deref(), Some("linear"));
    assert_eq!(
        finding.message,
        "linear needs OAuth — the gateway cannot complete a client-side login; \
         run mcpgw auth login linear"
    );
    assert_eq!(finding.code, Some(NEEDS_OAUTH));

    // The same diagnosis and the same command, and a different middle clause
    // for each of the two things a reader is telling apart: a login that has
    // never happened, and one that has and stopped working.
    assert_eq!(
        needs_oauth(None, "linear", Some(TokenState::Expired)).message,
        "linear needs OAuth — the stored login expired; run mcpgw auth login linear"
    );
    assert_eq!(
        needs_oauth(None, "linear", Some(TokenState::Valid)).message,
        "linear needs OAuth — the stored login was refused; run mcpgw auth login linear"
    );

    let json = serde_json::to_value(&finding).unwrap();
    assert_eq!(json["code"], "needs_oauth");
    assert_eq!(json["severity"], "warning");
    // Every other finding stays exactly the shape it was: no code key at all.
    let plain = gateway_unreachable("http://127.0.0.1:8137");
    assert!(serde_json::to_value(&plain).unwrap().get("code").is_none());
}

/// The gateway's own answer for an upstream behind OAuth is not a broken
/// session: it is the one failure whose fix is a login, and the report has
/// to sort it out of the pile the same way it sorts a 404.
#[test]
fn a_gateway_error_naming_the_login_is_its_own_fault_kind() {
    assert_eq!(
        classify_gateway_failure(
            "when send message: upstream \"linear\" needs OAuth; \
             run mcpgw auth login linear on this machine"
        ),
        GatewayFault::NeedsOAuth
    );
    // A server that failed for any other reason still reads as a failure.
    assert_eq!(
        classify_gateway_failure("upstream \"linear\" failed after 3 attempt(s): refused"),
        GatewayFault::Failed
    );
}

/// A `headers_command` is a program mcpgw spawns, so it is resolved exactly
/// like a stdio `command` — and the advice says the one thing that is
/// different about it: whatever runs the gateway decides its PATH.
#[test]
fn a_headers_command_is_resolved_like_a_stdio_command() {
    let config = parse(
        r#"
version = 1
[servers.corp]
type = "http"
url = "https://mcp.corp.example/mcp"
headers_command = "corp-auth print-mcp-headers"
[servers.found]
type = "http"
url = "https://ok.example/mcp"
headers_command = ["present"]
"#,
    );
    let exists = |cmd: &str| cmd == "present";
    assert!(
        check_server(None, "found", &config.servers["found"], &exists).is_empty(),
        "a command on PATH earns no finding"
    );

    let findings = check_server(None, "corp", &config.servers["corp"], &exists);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].severity, Severity::Error);
    let message = &findings[0].message;
    assert!(
        message.contains(mcpgw_core::doctor::HEADERS_FROM_COMMAND),
        "{message}"
    );
    assert!(message.contains("corp-auth print-mcp-headers"), "{message}");
    assert!(message.contains("absolute path"), "{message}");
}

#[test]
fn a_tool_rule_matching_nothing_is_a_warning_naming_the_list() {
    let server = Server {
        enabled: true,
        tags: Vec::new(),
        tools: Some(mcpgw_core::ToolRules {
            allow: vec!["echo".to_owned(), "gone".to_owned()],
            deny: vec!["dead_*".to_owned()],
            ..mcpgw_core::ToolRules::default()
        }),
        transport: Transport::Stdio {
            command: "x".to_owned(),
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
        },
    };
    let offered = vec!["echo".to_owned(), "reverse".to_owned()];
    let findings = unmatched_tool_rules("fx", &server, &offered);
    let messages: Vec<&str> = findings.iter().map(|f| f.message.as_str()).collect();
    assert_eq!(
        messages,
        [
            "[servers.fx.tools] allow entry \"gone\" matches no tool fx offers",
            "[servers.fx.tools] deny entry \"dead_*\" matches no tool fx offers"
        ]
    );
    assert!(findings.iter().all(|f| f.severity == Severity::Warning));

    // A server with no table has nothing to check.
    let mut plain = server.clone();
    plain.tools = None;
    assert!(unmatched_tool_rules("fx", &plain, &offered).is_empty());
}
