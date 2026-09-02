use std::collections::BTreeMap;
use std::path::Path;

use mcpgw_core::import::{plan_import, slugify};
use mcpgw_core::{ClientKind, ClientRead, Config};

fn read(kind: ClientKind, json: &str) -> ClientRead {
    kind.read_text(json, Path::new("x.json")).unwrap()
}

/// Every command resolves. These cases are about naming, dedupe and
/// classification, not about what happens to be installed on the machine
/// running the suite.
fn resolves(_command: &str) -> bool {
    true
}

fn canonical(toml: &str) -> BTreeMap<String, mcpgw_core::Server> {
    Config::parse(toml, Path::new("c.toml")).unwrap().servers
}

#[test]
fn slugify_cases() {
    assert_eq!(slugify("My Server"), "my-server");
    assert_eq!(slugify("GitHub_Tools"), "github_tools");
    assert_eq!(slugify("çok--Güzel!!"), "ok-g-zel");
    assert_eq!(slugify("!!!"), "imported-server");
}

#[test]
fn identical_definitions_dedup_across_clients() {
    let cursor = read(
        ClientKind::Cursor,
        r#"{"mcpServers": {"github": {"command": "npx", "args": ["server-github"]}}}"#,
    );
    let vscode = read(
        ClientKind::VsCode,
        r#"{"servers": {"github": {"type": "stdio", "command": "npx", "args": ["server-github"]}}}"#,
    );
    let plan = plan_import(
        &[("cursor".into(), cursor), ("vscode".into(), vscode)],
        &BTreeMap::new(),
        &resolves,
    );
    assert_eq!(plan.new.len(), 1);
    assert_eq!(plan.new[0].origins.len(), 2);
    assert!(!plan.new[0].renamed);
}

#[test]
fn dedup_merges_metadata_instead_of_keeping_only_the_first_source() {
    // Same transport in both clients, but VS Code has it disabled and reads
    // it through the lossy sse mapping.
    let cursor = read(
        ClientKind::Cursor,
        r#"{"mcpServers": {"linear": {"type": "http", "url": "https://mcp.linear.app/mcp"}}}"#,
    );
    let vscode = read(
        ClientKind::VsCode,
        r#"{"servers": {"linear": {"type": "sse", "url": "https://mcp.linear.app/mcp",
            "disabled": true}}}"#,
    );
    let plan = plan_import(
        &[("cursor".into(), cursor), ("vscode".into(), vscode)],
        &BTreeMap::new(),
        &resolves,
    );

    assert_eq!(plan.new.len(), 1);
    let candidate = &plan.new[0];
    assert_eq!(candidate.origins.len(), 2);
    // Disabled anywhere wins, and the second source's note is not lost.
    assert!(!candidate.server.enabled);
    assert_eq!(candidate.notes.len(), 1);
    assert!(candidate.notes[0].contains("sse"), "{:?}", candidate.notes);
}

#[test]
fn cross_client_name_clash_suffixes_the_later_one() {
    let cursor = read(
        ClientKind::Cursor,
        r#"{"mcpServers": {"tools": {"command": "npx"}}}"#,
    );
    let vscode = read(
        ClientKind::VsCode,
        r#"{"servers": {"tools": {"type": "http", "url": "https://x/mcp"}}}"#,
    );
    let plan = plan_import(
        &[("cursor".into(), cursor), ("vscode".into(), vscode)],
        &BTreeMap::new(),
        &resolves,
    );
    let names: Vec<&str> = plan.new.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["tools", "tools-2"]);
    assert!(plan.new[1].renamed);
}

#[test]
fn invalid_names_are_slugified_and_avoid_canonical_collisions() {
    let cursor = read(
        ClientKind::Cursor,
        r#"{"mcpServers": {"My Server": {"command": "npx"}}}"#,
    );
    let canonical = canonical(
        r#"
version = 1
[servers.my-server]
type = "http"
url = "https://occupied/mcp"
"#,
    );
    let plan = plan_import(&[("cursor".into(), cursor)], &canonical, &resolves);
    assert_eq!(plan.new.len(), 1);
    assert_eq!(plan.new[0].name, "my-server-2");
    assert!(plan.new[0].renamed);
}

#[test]
fn classification_against_canonical() {
    let cursor = read(
        ClientKind::Cursor,
        r#"{"mcpServers": {
            "same": {"command": "npx", "args": ["a"]},
            "differs": {"command": "npx", "args": ["client-version"]},
            "fresh": {"url": "https://fresh/mcp"}
        }}"#,
    );
    let canonical = canonical(
        r#"
version = 1
[servers.same]
type = "stdio"
command = "npx"
args = ["a"]
[servers.differs]
type = "stdio"
command = "npx"
args = ["canonical-version"]
"#,
    );
    let plan = plan_import(&[("cursor".into(), cursor)], &canonical, &resolves);
    assert_eq!(plan.already.len(), 1);
    assert_eq!(plan.already[0].name, "same");
    assert_eq!(plan.conflicts.len(), 1);
    assert_eq!(plan.conflicts[0].name, "differs");
    assert_eq!(plan.new.len(), 1);
    assert_eq!(plan.new[0].name, "fresh");
}

#[test]
fn sse_note_travels_with_the_candidate() {
    let cursor = read(
        ClientKind::Cursor,
        r#"{"mcpServers": {"linear": {"type": "sse", "url": "https://l/sse"}}}"#,
    );
    let plan = plan_import(&[("cursor".into(), cursor)], &BTreeMap::new(), &resolves);
    assert_eq!(plan.new[0].notes.len(), 1);
    assert!(plan.new[0].notes[0].contains("sse"));
}

/// Case in the scheme and host and one trailing slash are the same endpoint
/// spelled two ways, and two clients routinely spell it both ways. Merging
/// them is the difference between one server in the config and two.
#[test]
fn http_urls_that_differ_only_in_case_or_a_trailing_slash_dedup() {
    let cursor = read(
        ClientKind::Cursor,
        r#"{"mcpServers": {"ctx7": {"type": "http", "url": "HTTPS://MCP.Context7.com/mcp/"}}}"#,
    );
    let vscode = read(
        ClientKind::VsCode,
        r#"{"servers": {"context7": {"type": "http", "url": "https://mcp.context7.com/mcp"}}}"#,
    );
    let plan = plan_import(
        &[("cursor".into(), cursor), ("vscode".into(), vscode)],
        &BTreeMap::new(),
        &resolves,
    );

    assert_eq!(plan.new.len(), 1);
    assert_eq!(plan.new[0].origins.len(), 2);
}

/// A host is case-insensitive; a path, a query and a port are not. Nothing
/// past the authority is normalized, because everything past it can change
/// which server answers.
#[test]
fn urls_that_differ_past_the_host_stay_separate() {
    for (a, b) in [
        ("https://h/mcp", "https://h/MCP"),
        ("https://h/mcp", "https://h/mcp/v2"),
        ("https://h/mcp?x=1", "https://h/mcp?x=2"),
        ("https://h/mcp", "https://h:8443/mcp"),
        ("https://h/mcp", "http://h/mcp"),
        ("https://h/mcp//", "https://h/mcp"),
    ] {
        let cursor = read(
            ClientKind::Cursor,
            &format!(r#"{{"mcpServers": {{"one": {{"type": "http", "url": "{a}"}}}}}}"#),
        );
        let vscode = read(
            ClientKind::VsCode,
            &format!(r#"{{"servers": {{"two": {{"type": "http", "url": "{b}"}}}}}}"#),
        );
        let plan = plan_import(
            &[("cursor".into(), cursor), ("vscode".into(), vscode)],
            &BTreeMap::new(),
            &resolves,
        );
        assert_eq!(plan.new.len(), 2, "{a} and {b} were merged");
    }
}

/// The same headers under two spellings of one URL still merge; different
/// headers are a different server however the URL is spelled, because the
/// surviving copy's definition is the one that runs.
#[test]
fn header_differences_survive_url_canonicalization() {
    let cursor = read(
        ClientKind::Cursor,
        r#"{"mcpServers": {"a": {"type": "http", "url": "https://H/mcp/",
            "headers": {"X-Key": "one"}}}}"#,
    );
    let vscode = read(
        ClientKind::VsCode,
        r#"{"servers": {"b": {"type": "http", "url": "https://h/mcp",
            "headers": {"X-Key": "two"}}}}"#,
    );
    let plan = plan_import(
        &[("cursor".into(), cursor), ("vscode".into(), vscode)],
        &BTreeMap::new(),
        &resolves,
    );
    assert_eq!(plan.new.len(), 2);
}

/// Stdio is compared byte for byte, argument order included: two commands
/// with the same arguments in a different order can behave differently, and
/// a merge picks one of them for the user without saying so.
#[test]
fn stdio_is_untouched_by_url_canonicalization() {
    let cursor = read(
        ClientKind::Cursor,
        r#"{"mcpServers": {"one": {"command": "NPX", "args": ["--a", "--b"]}}}"#,
    );
    let vscode = read(
        ClientKind::VsCode,
        r#"{"servers": {"two": {"type": "stdio", "command": "npx", "args": ["--b", "--a"]}}}"#,
    );
    let plan = plan_import(
        &[("cursor".into(), cursor), ("vscode".into(), vscode)],
        &BTreeMap::new(),
        &resolves,
    );
    assert_eq!(plan.new.len(), 2);
}

/// A stdio entry whose command is not on this machine comes in switched off.
/// Importing it enabled publishes an endpoint that can never answer and then
/// spreads it to every client on the next sync, which is how one pre-existing
/// broken entry turns into one failure per client.
#[test]
fn a_command_that_does_not_resolve_is_imported_disabled() {
    let cursor = read(
        ClientKind::Cursor,
        r#"{"mcpServers": {
            "node_repl": {"command": "/Applications/Gone.app/bin/node_repl"},
            "notes": {"command": "notes-mcp"}
        }}"#,
    );
    let plan = plan_import(&[("cursor".into(), cursor)], &BTreeMap::new(), &|command| {
        command == "notes-mcp"
    });

    let by_name = |name: &str| {
        plan.new
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("{name} not planned"))
    };
    assert!(by_name("node_repl").command_missing);
    assert!(!by_name("node_repl").server.enabled);
    // The entry that can run is untouched: this is a check, not a policy of
    // importing everything off.
    assert!(!by_name("notes").command_missing);
    assert!(by_name("notes").server.enabled);
}

/// Resolvability is asked about stdio only — an http entry has no command to
/// look for, and a lookup that returns false for everything must not switch
/// off every remote server on the machine.
#[test]
fn an_http_entry_is_never_disabled_for_a_missing_command() {
    let cursor = read(
        ClientKind::Cursor,
        r#"{"mcpServers": {"linear": {"type": "http", "url": "https://mcp.linear.app/mcp"}}}"#,
    );
    let plan = plan_import(&[("cursor".into(), cursor)], &BTreeMap::new(), &|_| false);

    assert!(!plan.new[0].command_missing);
    assert!(plan.new[0].server.enabled);
}

/// The same remote server with a second token is not two servers, and the
/// plan says so rather than inventing `context7-2` in silence.
#[test]
fn the_same_url_with_a_different_token_is_flagged_against_the_canonical_entry() {
    let cursor = read(
        ClientKind::Cursor,
        r#"{"mcpServers": {"context7": {"type": "http", "url": "https://mcp.context7.com/mcp",
            "headers": {"Authorization": "Bearer incoming-secret"}}}}"#,
    );
    let canonical = canonical(
        "version = 1\n\n[servers.ctx]\ntype = \"http\"\n\
         url = \"https://mcp.context7.com/mcp\"\n\
         headers = { Authorization = \"Bearer canonical-secret\" }\n",
    );
    let plan = plan_import(&[("cursor".into(), cursor)], &canonical, &resolves);

    let same = plan.new[0].same_address.as_ref().expect("not flagged");
    assert_eq!(same.name, "ctx");
    assert!(same.canonical);
    // The flag exists so the caller can talk about the difference without
    // printing it: neither token may travel with it.
    let described = format!("{same:?}");
    assert!(!described.contains("secret"), "{described}");
}

/// Two clients holding one server under two tokens is the case that started
/// this: the second one matches the first candidate, not the config.
#[test]
fn the_same_url_with_a_different_token_is_flagged_against_an_earlier_candidate() {
    let codex = read(
        ClientKind::Cursor,
        r#"{"mcpServers": {"context7": {"type": "http", "url": "https://mcp.context7.com/mcp",
            "headers": {"Authorization": "Bearer one"}}}}"#,
    );
    let opencode = read(
        ClientKind::VsCode,
        r#"{"servers": {"context7": {"type": "http", "url": "https://mcp.context7.com/mcp",
            "headers": {"Authorization": "Bearer two"}}}}"#,
    );
    let plan = plan_import(
        &[("cursor".into(), codex), ("vscode".into(), opencode)],
        &BTreeMap::new(),
        &resolves,
    );

    assert_eq!(plan.new.len(), 2, "the two definitions still both survive");
    assert_eq!(plan.new[0].same_address, None);
    let same = plan.new[1].same_address.as_ref().expect("not flagged");
    assert_eq!(same.name, "context7");
    assert!(!same.canonical);
    for token in ["one", "two"] {
        let described = format!("{same:?}");
        assert!(
            !described.contains(&format!("Bearer {token}")),
            "{described}"
        );
    }
}

/// A header the other side does not send at all is a genuinely different
/// definition — a proxy header, a version pin — not one server wearing a
/// second credential. Those keep the old silent behaviour.
#[test]
fn a_different_set_of_header_keys_is_not_a_credentials_difference() {
    let cursor = read(
        ClientKind::Cursor,
        r#"{"mcpServers": {"a": {"type": "http", "url": "https://h/mcp",
            "headers": {"Authorization": "Bearer one", "X-Tenant": "acme"}}}}"#,
    );
    let vscode = read(
        ClientKind::VsCode,
        r#"{"servers": {"b": {"type": "http", "url": "https://h/mcp",
            "headers": {"Authorization": "Bearer two"}}}}"#,
    );
    let plan = plan_import(
        &[("cursor".into(), cursor), ("vscode".into(), vscode)],
        &BTreeMap::new(),
        &resolves,
    );

    assert_eq!(plan.new.len(), 2);
    assert!(plan.new.iter().all(|c| c.same_address.is_none()));
}

/// Identical headers are the dedupe case and must not be dressed up as a
/// question: there is nothing for the user to decide.
#[test]
fn identical_headers_dedupe_rather_than_asking() {
    let cursor = read(
        ClientKind::Cursor,
        r#"{"mcpServers": {"a": {"type": "http", "url": "https://h/mcp",
            "headers": {"Authorization": "Bearer one"}}}}"#,
    );
    let vscode = read(
        ClientKind::VsCode,
        r#"{"servers": {"b": {"type": "http", "url": "https://h/mcp",
            "headers": {"Authorization": "Bearer one"}}}}"#,
    );
    let plan = plan_import(
        &[("cursor".into(), cursor), ("vscode".into(), vscode)],
        &BTreeMap::new(),
        &resolves,
    );

    assert_eq!(plan.new.len(), 1);
    assert_eq!(plan.new[0].same_address, None);
}

/// The third outcome a conflict can have: keep the canonical entry and adopt
/// the client's differing one beside it. The planner is what names it, so
/// the name has to be there on every conflict.
#[test]
fn a_conflict_offers_a_second_name_to_adopt_it_under() {
    let cursor = read(
        ClientKind::Cursor,
        r#"{"mcpServers": {"context7": {"url": "https://client/mcp"}}}"#,
    );
    let canonical = canonical(
        r#"
version = 1
[servers.context7]
type = "stdio"
command = "npx"
args = ["context7"]
"#,
    );
    let plan = plan_import(&[("cursor".into(), cursor)], &canonical, &resolves);
    assert_eq!(plan.conflicts.len(), 1);
    assert_eq!(plan.conflicts[0].adopt_as.as_deref(), Some("context7-2"));
    // Nothing else needs a second name, and offering one would read as a
    // rename that is about to happen.
    assert!(plan.new.iter().all(|c| c.adopt_as.is_none()));
    assert!(plan.already.iter().all(|c| c.adopt_as.is_none()));
}

/// The second name is the first *free* one: a canonical `-2` that already
/// exists cannot be the name the run offers to write.
#[test]
fn the_second_name_steps_over_what_is_already_taken() {
    let cursor = read(
        ClientKind::Cursor,
        r#"{"mcpServers": {"context7": {"url": "https://client/mcp"}}}"#,
    );
    let canonical = canonical(
        r#"
version = 1
[servers.context7]
type = "stdio"
command = "npx"
args = ["context7"]
[servers."context7-2"]
type = "stdio"
command = "npx"
args = ["something-else"]
"#,
    );
    let plan = plan_import(&[("cursor".into(), cursor)], &canonical, &resolves);
    assert_eq!(plan.conflicts[0].adopt_as.as_deref(), Some("context7-3"));
}

/// The second name has to dodge what this same run is about to write, not
/// just what the config already holds: a client entry named `context7-2` is
/// a new import, and handing its name to a conflict would be two writes to
/// one name.
#[test]
fn the_second_name_dodges_the_rest_of_the_plan() {
    let cursor = read(
        ClientKind::Cursor,
        r#"{"mcpServers": {
            "context7": {"url": "https://client/mcp"},
            "context7-2": {"url": "https://other/mcp"}
        }}"#,
    );
    let canonical = canonical(
        r#"
version = 1
[servers.context7]
type = "stdio"
command = "npx"
args = ["context7"]
"#,
    );
    let plan = plan_import(&[("cursor".into(), cursor)], &canonical, &resolves);
    assert_eq!(plan.new.len(), 1);
    assert_eq!(plan.new[0].name, "context7-2");
    assert_eq!(plan.conflicts.len(), 1);
    assert_eq!(plan.conflicts[0].adopt_as.as_deref(), Some("context7-3"));
}
