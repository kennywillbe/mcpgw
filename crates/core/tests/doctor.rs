use std::path::Path;

use mcpgw_core::auth::TokenState;
use mcpgw_core::doctor::{
    GatewayFault, GatewayPlan, NEEDS_OAUTH, Severity, check_server, classify_gateway_failure,
    classify_problems, gateway_unreachable, missing_gateway_token, needs_oauth,
    unauthenticated_bind, unmatched_tool_rules, unserved_endpoint,
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
        calls_per_minute: 0,
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
        calls_per_minute: 0,
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
    // A scoped client's bridge dials the tagged endpoint, and that is the
    // path the probe has to take: the untagged one answers a different
    // question.
    assert!(plan.collect(
        "Claude Desktop",
        "linear",
        &bridge(
            "mcpgw",
            &[
                "connect",
                "--server",
                "linear",
                "--url",
                BASE,
                "--client",
                "claude-desktop"
            ],
        ),
    ));
    // Some other command called `connect` is not our bridge.
    assert!(!plan.collect("Zed", "x", &bridge("socat", &["connect"])));

    let targets = plan.into_targets();
    let urls: Vec<&str> = targets.iter().map(|t| t.url.as_str()).collect();
    assert_eq!(
        urls,
        [
            "http://127.0.0.1:8137/mcp",
            "http://127.0.0.1:8137/s/github",
            "http://127.0.0.1:8137/s/linear?client=claude-desktop"
        ]
    );
    // The base endpoint belongs to no single server, which is what marks
    // that entry as one to migrate rather than one that is served.
    assert_eq!(targets[0].server, None);
    assert_eq!(targets[1].server.as_deref(), Some("github"));
    assert_eq!(targets[2].server.as_deref(), Some("linear"));
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
        calls_per_minute: 0,
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

/// The gateway entry `sync` writes for `name`, with `headers` as given.
fn gateway_entry(headers: &[(&str, &str)]) -> Server {
    Server {
        enabled: true,
        tags: Vec::new(),
        calls_per_minute: 0,
        tools: None,
        transport: Transport::Http {
            url: "http://127.0.0.1:8137/s/github".to_owned(),
            headers_command: Vec::new(),
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            auth: None,
        },
    }
}

#[test]
fn a_managed_entry_without_the_token_is_a_warning_naming_sync() {
    let bare = missing_gateway_token("Cursor", "github", &gateway_entry(&[]), true).unwrap();
    assert_eq!(bare.severity, Severity::Warning);
    assert_eq!(bare.client.as_deref(), Some("Cursor"));
    assert_eq!(bare.server.as_deref(), Some("github"));
    assert!(bare.message.contains("mcpgw sync"), "{}", bare.message);

    // An entry that has it is not reported, whatever case the client's own
    // writer used for the header name.
    for name in ["Authorization", "authorization"] {
        assert!(
            missing_gateway_token(
                "Cursor",
                "github",
                &gateway_entry(&[(name, "Bearer t")]),
                true
            )
            .is_none(),
            "{name}"
        );
    }
    // Nor is one in a client that cannot carry the token at all: Zed and
    // Claude Desktop would get a permanent warning naming a fix that does
    // not exist for them.
    assert!(missing_gateway_token("Zed", "github", &gateway_entry(&[]), false).is_none());
}

#[test]
fn a_bind_past_loopback_with_no_token_required_is_an_error() {
    let finding = unauthenticated_bind("0.0.0.0", false).unwrap();
    assert_eq!(finding.severity, Severity::Error);
    assert!(finding.message.contains("require_token"), "{finding:?}");
    assert!(finding.message.contains("0.0.0.0"), "{finding:?}");

    // The same address with the token required is the whole point of the
    // knob, and loopback never needed it.
    assert!(unauthenticated_bind("0.0.0.0", true).is_none());
    for bind in ["127.0.0.1", "::1", "localhost"] {
        assert!(unauthenticated_bind(bind, false).is_none(), "{bind}");
    }
}

/// The budget is the two tables applied in the gateway's own order, over the
/// servers the client is given and nothing else.
#[test]
fn a_budget_counts_only_what_the_client_would_actually_be_offered() {
    use mcpgw_core::doctor::{WINDSURF_TOOL_CAP, client_budget, over_tool_cap, tool_cap};

    let config = parse(
        r#"
version = 1

[clients.cursor]
servers = ["github"]

[clients.cursor.tools]
deny = ["get_*"]

[servers.github]
type = "stdio"
command = "npx"

[servers.github.tools]
deny = ["delete_*"]

[servers.linear]
type = "http"
url = "https://mcp.linear.app/mcp"

[servers.parked]
type = "stdio"
command = "npx"
enabled = false
"#,
    );
    let listings = [
        (
            "github".to_owned(),
            [
                ("search".to_owned(), 100),
                ("get_file".to_owned(), 200),
                ("delete_repo".to_owned(), 400),
            ]
            .into_iter()
            .collect(),
        ),
        (
            "linear".to_owned(),
            [("issues".to_owned(), 800)].into_iter().collect(),
        ),
    ]
    .into_iter()
    .collect();

    let cursor = client_budget(
        ClientKind::Cursor,
        config.clients.get("cursor"),
        &config.servers,
        &listings,
    );
    assert_eq!(cursor.servers, 1);
    // `get_file` is denied by the client, `delete_repo` by the server, and
    // `linear` is not this client's at all.
    assert_eq!(cursor.tools, 1);
    assert_eq!(cursor.tokens, 100);
    assert!(cursor.unpriced.is_empty());
    assert_eq!(
        cursor.line(),
        "cursor sees 1 tool across 1 server (~100 tokens)"
    );

    // A client with no scope gets every enabled server, which is the number
    // nobody could see before.
    let zed = client_budget(ClientKind::Zed, None, &config.servers, &listings);
    assert_eq!(zed.servers, 2);
    assert_eq!(zed.tools, 3);
    assert_eq!(zed.tokens, 1100);
    assert_eq!(zed.line(), "zed sees 3 tools across 2 servers (~1k tokens)");

    // A server nothing priced still counts as one the client is offered, and
    // the line says the total is a floor.
    let unpriced = client_budget(
        ClientKind::Zed,
        None,
        &config.servers,
        &[("github".to_owned(), std::collections::BTreeMap::default())]
            .into_iter()
            .collect(),
    );
    assert_eq!(unpriced.unpriced, ["linear"]);
    assert!(unpriced.line().contains("at least: linear did not answer"));

    // Windsurf's own ceiling applies without anybody configuring it; every
    // other client is judged only against a threshold someone wrote.
    assert_eq!(
        tool_cap(ClientKind::Windsurf, None),
        Some((WINDSURF_TOOL_CAP, "Windsurf's own limit".to_owned()))
    );
    assert_eq!(tool_cap(ClientKind::Cursor, None), None);
    // A scope with no threshold in it does not invent one.
    assert_eq!(
        tool_cap(ClientKind::Cursor, config.clients.get("cursor")),
        None
    );
    assert!(over_tool_cap(&cursor, 1, "[clients.cursor] max_tools").is_none());
    let over = over_tool_cap(&zed, 1, "[clients.zed] max_tools").unwrap();
    assert_eq!(over.severity, Severity::Warning);
    assert!(over.message.contains("3 tools is over 1"), "{over:?}");
}

/// A scope naming a server that has been removed is a warning, not a broken
/// config: the commands that would fix it have to be able to load the file.
#[test]
fn a_scope_naming_a_missing_server_is_reported_not_refused() {
    use mcpgw_core::doctor::unknown_scoped_servers;

    let config = parse(
        r#"
version = 1
[clients.cursor]
servers = ["github", "gone"]
[servers.github]
type = "stdio"
command = "npx"
"#,
    );
    let findings = unknown_scoped_servers("cursor", &config.clients["cursor"], &config.servers);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].severity, Severity::Warning);
    assert!(findings[0].message.contains("gone"), "{findings:?}");
}
