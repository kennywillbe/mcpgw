use std::error::Error as _;
use std::path::Path;

use mcpgw_core::{Config, Error, SUPPORTED_VERSION, Transport, config::validate_name};

fn parse_fixture(name: &str) -> Result<Config, Error> {
    let full = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let text = std::fs::read_to_string(full).unwrap();
    // The logical path keeps error snapshots machine-independent.
    Config::parse(&text, Path::new(name))
}

#[test]
fn full_fixture_parses() {
    insta::assert_debug_snapshot!(parse_fixture("full.toml").unwrap());
}

#[test]
fn defaults_are_applied() {
    let config = parse_fixture("full.toml").unwrap();
    let fs = &config.servers["fs"];
    assert!(fs.enabled);
    assert!(fs.tags.is_empty());
    let Transport::Stdio { args, env, .. } = &fs.transport else {
        panic!("fs should be stdio");
    };
    assert!(args.is_empty());
    assert!(env.is_empty());
}

#[test]
fn serialization_round_trips() {
    let config = parse_fixture("full.toml").unwrap();
    let text = config.to_toml_string().unwrap();
    let reparsed = Config::parse(&text, Path::new("roundtrip.toml")).unwrap();
    assert_eq!(config, reparsed);
}

#[test]
fn serialized_form_is_stable() {
    let config = parse_fixture("full.toml").unwrap();
    insta::assert_snapshot!(config.to_toml_string().unwrap());
}

#[test]
fn missing_version_is_a_parse_error() {
    let err = Config::parse(
        "[servers.a]\ntype = \"stdio\"\ncommand = \"x\"\n",
        Path::new("c.toml"),
    )
    .unwrap_err();
    assert!(matches!(err, Error::Parse { .. }));
    insta::assert_snapshot!(err.to_string());
}

#[test]
fn future_version_is_rejected_before_field_errors() {
    // `mode` does not exist in version 1; the probe must report the version
    // mismatch instead of an unknown-field error.
    let text = "version = 2\n\n[servers.a]\nmode = \"pooled\"\n";
    let err = Config::parse(text, Path::new("c.toml")).unwrap_err();
    let Error::UnsupportedVersion { found } = err else {
        panic!("expected UnsupportedVersion, got: {err}");
    };
    assert_eq!(found, 2);
}

#[test]
fn invalid_server_name_is_rejected() {
    let text = "version = 1\n\n[servers.\"Bad Name\"]\ntype = \"stdio\"\ncommand = \"x\"\n";
    let err = Config::parse(text, Path::new("c.toml")).unwrap_err();
    insta::assert_snapshot!(err.to_string());
}

#[test]
fn unknown_transport_type_is_rejected() {
    let text = "version = 1\n\n[servers.a]\ntype = \"websocket\"\nurl = \"ws://x\"\n";
    let err = Config::parse(text, Path::new("c.toml")).unwrap_err();
    assert!(matches!(err, Error::Parse { .. }));
}

#[test]
fn name_validation_cases() {
    for good in ["a", "github", "my-server_2"] {
        assert!(validate_name(good).is_ok(), "{good:?} should be valid");
    }
    for bad in ["", "Server", "has space", "ünicode", "dot.dot"] {
        assert!(validate_name(bad).is_err(), "{bad:?} should be invalid");
    }
}

#[test]
fn double_underscore_is_reserved_for_the_gateway_separator() {
    for bad in ["a__b", "__x", "x__", "a__b__c"] {
        let err = validate_name(bad).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("__"), "{bad:?}: {text}");
        assert!(text.contains("reserved"), "{bad:?}: {text}");
    }
    // A single underscore stays legal.
    assert!(validate_name("a_b").is_ok());
}

#[test]
fn config_rejects_a_server_named_with_the_separator() {
    let text = "version = 1\n\n[servers.a__b]\ntype = \"stdio\"\ncommand = \"x\"\n";
    let err = Config::parse(text, Path::new("c.toml")).unwrap_err();
    assert!(matches!(err, Error::InvalidName { .. }), "{err}");
}

#[test]
fn load_distinguishes_missing_file() {
    let err = Config::load(Path::new("/nonexistent/mcpgw/config.toml")).unwrap_err();
    assert!(matches!(err, Error::NotFound { .. }));
}

#[test]
fn empty_config_is_current_version() {
    assert_eq!(Config::empty().version, SUPPORTED_VERSION);
    assert!(Config::empty().servers.is_empty());
}

#[test]
fn the_capture_table_is_optional_and_stays_out_of_a_config_without_it() {
    let config = Config::parse("version = 1\n", Path::new("c.toml")).unwrap();
    assert!(config.capture.redact.is_empty());
    // Nobody's file grows a table they never asked for.
    assert_eq!(
        config.to_toml_string().unwrap(),
        "version = 1\n\n[servers]\n"
    );
}

#[test]
fn redaction_patterns_round_trip_through_the_capture_table() {
    let text = "version = 1\n\n[capture]\nredact = [\"ACME-[0-9]{4}\"]\n";
    let config = Config::parse(text, Path::new("c.toml")).unwrap();
    assert_eq!(config.capture.redact, ["ACME-[0-9]{4}"]);
    let reparsed = Config::parse(&config.to_toml_string().unwrap(), Path::new("c.toml")).unwrap();
    assert_eq!(config, reparsed);
}

#[test]
fn an_unusable_redaction_pattern_is_a_config_error_that_names_it() {
    let err = Config::parse(
        "version = 1\n\n[capture]\nredact = [\"(unclosed\"]\n",
        Path::new("c.toml"),
    )
    .unwrap_err();
    assert!(matches!(err, Error::InvalidRedaction { .. }), "{err:?}");
    insta::assert_snapshot!(err.to_string());
}

/// Both spellings land on the same argv: the array mcpgw writes, and the
/// single line Claude Code and Codex spell their helper with, which is what
/// a config pasted out of either looks like.
#[test]
fn a_headers_command_reads_as_argv_from_either_spelling() {
    let argv = |text: &str| {
        let config = Config::parse(text, Path::new("c.toml")).unwrap();
        let Transport::Http {
            headers_command, ..
        } = &config.servers["corp"].transport
        else {
            panic!("corp should be http");
        };
        headers_command.clone()
    };
    let want = ["corp-auth".to_owned(), "print-mcp-headers".to_owned()];

    assert_eq!(
        argv(
            "version = 1\n[servers.corp]\ntype = \"http\"\nurl = \"https://c.example/mcp\"\n\
             headers_command = [\"corp-auth\", \"print-mcp-headers\"]\n"
        ),
        want
    );
    assert_eq!(
        argv(
            "version = 1\n[servers.corp]\ntype = \"http\"\nurl = \"https://c.example/mcp\"\n\
             headers_command = \"corp-auth print-mcp-headers\"\n"
        ),
        want
    );
}

/// A command with nothing to run is a config error, not something the gateway
/// discovers at connect time on the one morning the token expires.
#[test]
fn an_empty_headers_command_is_rejected() {
    for spelling in ["[]", "\"\"", "[\"corp-auth\", \"\"]"] {
        let text = format!(
            "version = 1\n[servers.corp]\ntype = \"http\"\nurl = \"https://c.example/mcp\"\n\
             headers_command = {spelling}\n"
        );
        let err = Config::parse(&text, Path::new("c.toml")).unwrap_err();
        assert!(matches!(err, Error::Parse { .. }), "{spelling}: {err}");
        assert!(
            err.source()
                .unwrap()
                .to_string()
                .contains("headers_command")
        );
    }
}

/// Serialized as an array whatever it was written as, and absent entirely
/// from an entry that has none.
#[test]
fn a_headers_command_round_trips_as_an_array() {
    let config = Config::parse(
        "version = 1\n[servers.corp]\ntype = \"http\"\nurl = \"https://c.example/mcp\"\n\
         headers_command = \"corp-auth print-mcp-headers\"\n\
         [servers.plain]\ntype = \"http\"\nurl = \"https://p.example/mcp\"\n",
        Path::new("c.toml"),
    )
    .unwrap();
    let text = config.to_toml_string().unwrap();
    assert!(text.contains("headers_command = ["), "{text}");
    assert!(text.contains("\"print-mcp-headers\""), "{text}");
    assert_eq!(text.matches("headers_command").count(), 1, "{text}");
    assert_eq!(
        Config::parse(&text, Path::new("roundtrip.toml")).unwrap(),
        config
    );
}

const WITH_TOOLS: &str = r#"
version = 1

[servers.github]
type = "stdio"
command = "npx"

[servers.github.tools]
allow = ["search_repositories", "get_*"]
deny = ["get_secret"]

[servers.linear]
type = "http"
url = "https://mcp.linear.app/mcp"
"#;

#[test]
fn a_tools_table_parses_and_only_where_it_is_written() {
    let config = Config::parse(WITH_TOOLS, Path::new("tools.toml")).unwrap();
    let rules = config.servers["github"].tools.as_ref().unwrap();
    assert_eq!(rules.allow, ["search_repositories", "get_*"]);
    assert_eq!(rules.deny, ["get_secret"]);
    // The promise the wizard makes to everyone who upgrades: a server with
    // no table has no rules, not empty ones.
    assert!(config.servers["linear"].tools.is_none());
    assert!(config.servers["linear"].allows_tool("anything"));
    assert!(config.servers["github"].allows_tool("get_file_contents"));
    assert!(!config.servers["github"].allows_tool("get_secret"));
    assert!(!config.servers["github"].allows_tool("delete_repository"));
}

#[test]
fn a_tools_table_round_trips_through_toml() {
    let config = Config::parse(WITH_TOOLS, Path::new("tools.toml")).unwrap();
    let text = config.to_toml_string().unwrap();
    let reparsed = Config::parse(&text, Path::new("roundtrip.toml")).unwrap();
    assert_eq!(config, reparsed);
    // A server that had no table must not grow an empty one on the way out:
    // `tools = {}` in a written config would be a rule nobody asked for.
    assert!(!text.contains("linear.tools"));
}

/// The `[auth]` table reads back as written and survives a round trip
/// through the plain serde form, which is what `list --json` and the reload
/// path both go through.
#[test]
fn an_auth_table_round_trips() {
    let config = Config::parse(
        "version = 1\n\n[servers.jira]\ntype = \"http\"\nurl = \"https://mcp.atlassian.com/mcp\"\n\
         auth = { client_id = \"abc123\", client_secret_env = \"JIRA_SECRET\", scopes = [\"read\"] }\n",
        Path::new("t.toml"),
    )
    .unwrap();
    let Transport::Http { auth, .. } = &config.servers["jira"].transport else {
        panic!("expected an http server");
    };
    let auth = auth.as_ref().expect("the table is read");
    assert_eq!(auth.client_id.as_deref(), Some("abc123"));
    assert_eq!(auth.client_secret_env.as_deref(), Some("JIRA_SECRET"));
    assert_eq!(auth.scopes, ["read"]);

    let text = config.to_toml_string().unwrap();
    assert_eq!(Config::parse(&text, Path::new("t.toml")).unwrap(), config);

    // An entry with no table has none, rather than an empty one: the two say
    // different things, and only the second would be written back out.
    let plain = Config::parse(
        "version = 1\n\n[servers.linear]\ntype = \"http\"\nurl = \"https://mcp.linear.app/mcp\"\n",
        Path::new("t.toml"),
    )
    .unwrap();
    let Transport::Http { auth, .. } = &plain.servers["linear"].transport else {
        panic!("expected an http server");
    };
    assert!(auth.is_none());
    assert!(!plain.to_toml_string().unwrap().contains("auth"));
}

/// Both fill the `Authorization` header, so an entry with both has no defined
/// behaviour — and it is refused where it is written, not at the next connect.
#[test]
fn a_server_cannot_have_both_headers_command_and_auth() {
    let err = Config::parse(
        "version = 1\n\n[servers.linear]\ntype = \"http\"\nurl = \"https://mcp.linear.app/mcp\"\n\
         headers_command = [\"corp-auth\"]\nauth = { client_id = \"abc\" }\n",
        Path::new("t.toml"),
    )
    .unwrap_err();
    assert!(
        matches!(&err, Error::AuthConflict { name } if name == "linear"),
        "{err}"
    );
    assert!(
        err.to_string()
            .contains("sets both headers_command and [auth]"),
        "{err}"
    );
}

const WITH_BUDGET: &str = r#"
version = 1

[servers.github]
type = "stdio"
command = "npx"
calls_per_minute = 120

[servers.linear]
type = "http"
url = "https://mcp.linear.app/mcp"
"#;

/// A config with both halves of the milestone in it: two clients given
/// different servers, one of them narrowing what it sees on top of a server's
/// own list.
const WITH_CLIENTS: &str = r#"
version = 1

[clients.cursor]
servers = ["github"]

[clients.cursor.tools]
deny = ["get_*"]

[clients.claude-desktop]
max_tools = 40

[servers.github]
type = "stdio"
command = "npx"

[servers.github.tools]
deny = ["delete_*"]

[servers.linear]
type = "http"
url = "https://mcp.linear.app/mcp"
"#;

#[test]
fn a_call_budget_parses_and_only_where_it_is_written() {
    let config = Config::parse(WITH_BUDGET, Path::new("budget.toml")).unwrap();
    assert_eq!(config.servers["github"].calls_per_minute, 120);
    // The promise the upgrade makes: a server that never mentions a budget
    // is unmetered, which is what it always was.
    assert_eq!(config.servers["linear"].calls_per_minute, 0);
}

#[test]
fn a_call_budget_round_trips_through_toml() {
    let config = Config::parse(WITH_BUDGET, Path::new("budget.toml")).unwrap();
    let text = config.to_toml_string().unwrap();
    let reparsed = Config::parse(&text, Path::new("roundtrip.toml")).unwrap();
    assert_eq!(config, reparsed);
    // A `calls_per_minute = 0` on the way out would be a file this build
    // refuses to load again.
    assert_eq!(text.matches("calls_per_minute").count(), 1, "{text}");
}

/// Zero, negative and fractional are all config errors rather than values
/// the gateway silently reinterprets — a budget nobody can read is worse
/// than no budget.
#[test]
fn an_unusable_call_budget_is_a_config_error_that_names_the_key() {
    for bad in ["0", "-1", "1.5", "\"120\""] {
        let text = format!(
            "version = 1\n[servers.fx]\ntype = \"stdio\"\ncommand = \"x\"\ncalls_per_minute = {bad}\n"
        );
        let err = Config::parse(&text, Path::new("bad.toml")).unwrap_err();
        let message = format!("{err}: {}", std::error::Error::source(&err).unwrap());
        assert!(message.contains("calls_per_minute"), "{bad}: {message}");
    }
    // And the wording for zero says what to do instead of guessing.
    let text =
        "version = 1\n[servers.fx]\ntype = \"stdio\"\ncommand = \"x\"\ncalls_per_minute = 0\n";
    let err = Config::parse(text, Path::new("bad.toml")).unwrap_err();
    let message = std::error::Error::source(&err).unwrap().to_string();
    assert!(message.contains("drop the key for no budget"), "{message}");
}

#[test]
fn a_clients_table_parses_and_only_where_it_is_written() {
    let config = Config::parse(WITH_CLIENTS, Path::new("clients.toml")).unwrap();
    let cursor = &config.clients["cursor"];
    assert_eq!(cursor.servers, ["github"]);
    assert!(cursor.has_server("github"));
    assert!(!cursor.has_server("linear"));
    assert!(cursor.restricts());
    // A table holding only a reporting threshold restricts nothing, which is
    // what decides whether `sync` writes that client a tagged endpoint.
    let desktop = &config.clients["claude-desktop"];
    assert_eq!(desktop.max_tools, Some(40));
    assert!(!desktop.restricts());
    assert!(!desktop.is_empty());
    assert!(desktop.has_server("linear"));
    // A client with no table at all is given everything, and every client
    // was one of those before this existed.
    assert!(!config.clients.contains_key("zed"));
}

/// The order the gateway reads them in: the server says what it offers
/// anybody, the client says which of that it gets.
#[test]
fn client_rules_compose_over_the_servers_own() {
    let config = Config::parse(WITH_CLIENTS, Path::new("clients.toml")).unwrap();
    let github = &config.servers["github"];
    let cursor = &config.clients["cursor"];
    let allowed = |tool: &str| github.allows_tool(tool) && cursor.allows_tool(tool);

    assert!(allowed("search_repositories"));
    // Denied by the client alone — the server offers it to everybody else.
    assert!(github.allows_tool("get_file_contents"));
    assert!(!allowed("get_file_contents"));
    // Denied by the server alone, which no client can widen.
    assert!(!allowed("delete_repository"));
    // A scope with no tool rules narrows nothing.
    assert!(config.clients["claude-desktop"].allows_tool("anything"));
}

#[test]
fn a_clients_table_round_trips_through_toml() {
    let config = Config::parse(WITH_CLIENTS, Path::new("clients.toml")).unwrap();
    let text = config.to_toml_string().unwrap();
    let reparsed = Config::parse(&text, Path::new("roundtrip.toml")).unwrap();
    assert_eq!(config, reparsed);
    // A client with no tool rules must not grow empty ones on the way out.
    assert!(!text.contains("claude-desktop.tools"), "{text}");
}

/// A `[clients.KIND]` nothing answers to is a scope that silently never
/// applies, so it fails at parse rather than at the request it would have
/// filtered.
#[test]
fn an_unknown_client_id_is_a_config_error_that_lists_the_real_ones() {
    let err = Config::parse(
        "version = 1\n[clients.cursorr]\nservers = []\n",
        Path::new("clients.toml"),
    )
    .unwrap_err();
    let message = err.to_string();
    assert!(message.contains("cursorr"), "{message}");
    assert!(message.contains("cursor"), "{message}");
}

/// The opposite call: a server name that has gone stale is left to `doctor`,
/// because the config still has to load for the commands that would fix it.
#[test]
fn a_scope_naming_a_missing_server_still_parses() {
    let config = Config::parse(
        "version = 1\n[clients.cursor]\nservers = [\"gone\"]\n",
        Path::new("clients.toml"),
    )
    .unwrap();
    assert_eq!(config.clients["cursor"].servers, ["gone"]);
}
