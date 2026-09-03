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
