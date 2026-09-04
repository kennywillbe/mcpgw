//! `mcpgw token`, and the token's way into a client file.

use std::path::Path;

use mcpgw_core::gateway_token::GatewayToken;

mod util;
use util::{mcpgw, stdout};

/// A sandbox holding a canonical config with one server and a Cursor install
/// to sync it into.
fn sandbox() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        "version = 1\n\n[servers.github]\ntype = \"stdio\"\ncommand = \"npx\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(home.path().join(".cursor")).unwrap();
    std::fs::write(
        home.path().join(".cursor/mcp.json"),
        "{\n  \"mcpServers\": {}\n}\n",
    )
    .unwrap();
    home
}

fn run(home: &Path, args: &[&str]) -> String {
    let out = util::output_retrying_while_busy(mcpgw(home).args(args));
    assert!(out.status.success(), "{}", util::stderr(&out));
    stdout(&out)
}

fn state(home: &Path) -> std::path::PathBuf {
    home.join("state")
}

#[test]
fn show_masks_the_token_until_asked_not_to() {
    let home = sandbox();
    // Nothing yet, and asking is not what creates one: a question must not
    // leave a secret on the disk as a side effect.
    let said = run(home.path(), &["token", "show"]);
    assert!(said.contains("no gateway token yet"), "{said}");
    assert!(!GatewayToken::path(&state(home.path())).exists());

    let token = GatewayToken::generate();
    token.save(&state(home.path())).unwrap();

    let masked = run(home.path(), &["token", "show"]);
    assert!(!masked.contains(token.secret()), "{masked}");
    assert!(masked.contains(&token.masked()), "{masked}");

    let full = run(home.path(), &["token", "show", "--show-secrets"]);
    assert!(full.contains(token.secret()), "{full}");
}

#[test]
fn rotate_issues_a_new_token_and_re_syncs_the_clients() {
    let home = sandbox();
    let old = GatewayToken::generate();
    old.save(&state(home.path())).unwrap();
    run(home.path(), &["sync"]);
    let cursor = home.path().join(".cursor/mcp.json");
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains(old.secret())
    );

    let said = run(home.path(), &["token", "rotate"]);
    let new = GatewayToken::load(&state(home.path())).unwrap().unwrap();
    assert_ne!(new.secret(), old.secret());
    // The output says the thing a reader has to know, and the sync it ran is
    // what makes it true again.
    assert!(
        said.contains("re-synced") || said.contains("until it is re-synced"),
        "{said}"
    );
    let written = std::fs::read_to_string(&cursor).unwrap();
    assert!(written.contains(new.secret()), "{written}");
    assert!(!written.contains(old.secret()), "{written}");
}

#[test]
fn rotate_no_sync_leaves_the_clients_holding_the_old_one() {
    let home = sandbox();
    let old = GatewayToken::generate();
    old.save(&state(home.path())).unwrap();
    run(home.path(), &["sync"]);

    let said = run(home.path(), &["token", "rotate", "--no-sync"]);
    assert!(said.contains("mcpgw sync"), "{said}");
    let written = std::fs::read_to_string(home.path().join(".cursor/mcp.json")).unwrap();
    assert!(written.contains(old.secret()), "{written}");
}

#[test]
fn a_dry_run_says_the_entries_carry_the_token_and_never_prints_it() {
    let home = sandbox();
    let token = GatewayToken::generate();
    token.save(&state(home.path())).unwrap();

    let said = run(home.path(), &["sync", "--dry-run"]);
    assert!(said.contains("Authorization: Bearer"), "{said}");
    assert!(said.contains(&token.masked()), "{said}");
    // A dry run is the output people paste into an issue.
    assert!(!said.contains(token.secret()), "{said}");
    // And it stayed a dry run.
    assert!(
        !std::fs::read_to_string(home.path().join(".cursor/mcp.json"))
            .unwrap()
            .contains("github")
    );
}

#[test]
fn without_a_token_sync_says_so_and_writes_the_entries_anyway() {
    let home = sandbox();
    let said = run(home.path(), &["sync"]);
    assert!(said.contains("no gateway token on this machine"), "{said}");
    let written = std::fs::read_to_string(home.path().join(".cursor/mcp.json")).unwrap();
    assert!(written.contains("/s/github"), "{written}");
    assert!(!written.contains("Authorization"), "{written}");
}

#[test]
fn doctor_reports_a_managed_entry_that_has_no_token() {
    let home = sandbox();
    run(home.path(), &["sync"]);
    let out = util::output_retrying_while_busy(mcpgw(home.path()).args(["doctor", "--json"]));
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    let findings = value["findings"].as_array().unwrap();
    assert!(
        findings.iter().any(|finding| {
            finding["code"] == "missing_gateway_token" && finding["server"] == "github"
        }),
        "{findings:#?}"
    );

    // Written with a token, the warning goes.
    GatewayToken::generate().save(&state(home.path())).unwrap();
    run(home.path(), &["sync"]);
    let out = util::output_retrying_while_busy(mcpgw(home.path()).args(["doctor", "--json"]));
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    let findings = value["findings"].as_array().unwrap();
    assert!(
        !findings
            .iter()
            .any(|finding| finding["code"] == "missing_gateway_token"),
        "{findings:#?}"
    );
}

#[test]
fn doctor_reports_a_gateway_bound_past_loopback_with_no_token_required() {
    let home = sandbox();
    util::record_installed_spec(
        home.path(),
        &std::env::current_exe().unwrap(),
        "0.0.0.0",
        8137,
    );

    let out = util::output_retrying_while_busy(mcpgw(home.path()).args(["doctor", "--json"]));
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    let finding = value["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| finding["code"] == "unauthenticated_bind")
        .unwrap_or_else(|| panic!("{}", value["findings"]));
    assert_eq!(finding["severity"], "error");
    assert!(!out.status.success());

    // Requiring the token is what makes that address defensible, and doctor
    // agrees with the preflight about it.
    std::fs::write(
        home.path().join("config.toml"),
        "version = 1\n\n[gateway]\nrequire_token = true\n\n\
         [servers.github]\ntype = \"stdio\"\ncommand = \"npx\"\n",
    )
    .unwrap();
    let out = util::output_retrying_while_busy(mcpgw(home.path()).args(["doctor", "--json"]));
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert!(
        !value["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["code"] == "unauthenticated_bind"),
        "{}",
        value["findings"]
    );
}
