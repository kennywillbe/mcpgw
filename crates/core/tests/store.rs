use std::path::PathBuf;

use mcpgw_core::{Config, ConfigStore, Error, Server, ToolRules, Transport};

const COMMENTED: &str = r#"# my precious header comment
version = 1 # schema version

# github powers the code review flow — do not disable
[servers.github]
type = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
"#;

fn temp_config(text: Option<&str>) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    if let Some(text) = text {
        std::fs::write(&path, text).unwrap();
    }
    (dir, path)
}

fn http_server(url: &str) -> Server {
    Server {
        enabled: true,
        tags: vec!["work".to_owned()],
        tools: None,
        transport: Transport::Http {
            url: url.to_owned(),
            headers_command: Vec::new(),
            headers: std::collections::BTreeMap::new(),
            auth: None,
        },
    }
}

#[test]
fn upsert_preserves_user_comments() {
    let (_dir, path) = temp_config(Some(COMMENTED));
    let mut store = ConfigStore::edit(&path).unwrap();
    store
        .upsert_server("linear", &http_server("https://mcp.linear.app/mcp"), false)
        .unwrap();
    store.save().unwrap();
    insta::assert_snapshot!(std::fs::read_to_string(&path).unwrap());
}

#[test]
fn first_add_creates_file_from_template() {
    let (_dir, path) = temp_config(None);
    let mut store = ConfigStore::edit_or_create(&path).unwrap();
    store
        .upsert_server("linear", &http_server("https://mcp.linear.app/mcp"), false)
        .unwrap();
    store.save().unwrap();
    insta::assert_snapshot!(std::fs::read_to_string(&path).unwrap());
}

#[test]
fn edit_without_create_requires_existing_file() {
    let (_dir, path) = temp_config(None);
    assert!(matches!(
        ConfigStore::edit(&path).unwrap_err(),
        Error::NotFound { .. }
    ));
}

#[test]
fn duplicate_needs_overwrite() {
    let (_dir, path) = temp_config(Some(COMMENTED));
    let mut store = ConfigStore::edit(&path).unwrap();
    let err = store
        .upsert_server("github", &http_server("https://x"), false)
        .unwrap_err();
    assert!(matches!(err, Error::DuplicateName { .. }));

    let replaced = store
        .upsert_server("github", &http_server("https://x"), true)
        .unwrap();
    assert!(replaced);
    store.save().unwrap();
    insta::assert_snapshot!(std::fs::read_to_string(&path).unwrap());
}

#[test]
fn set_enabled_touches_only_that_key() {
    let (_dir, path) = temp_config(Some(COMMENTED));
    let mut store = ConfigStore::edit(&path).unwrap();
    store.set_enabled("github", false).unwrap();
    store.save().unwrap();
    insta::assert_snapshot!(std::fs::read_to_string(&path).unwrap());
}

#[test]
fn unknown_server_error_lists_known_names() {
    let (_dir, path) = temp_config(Some(COMMENTED));
    let mut store = ConfigStore::edit(&path).unwrap();
    insta::assert_snapshot!(store.set_enabled("nope", true).unwrap_err().to_string());
    insta::assert_snapshot!(store.remove_server("nope").unwrap_err().to_string());
}

#[test]
fn remove_deletes_entry() {
    let (_dir, path) = temp_config(Some(COMMENTED));
    let mut store = ConfigStore::edit(&path).unwrap();
    store.remove_server("github").unwrap();
    store.save().unwrap();
    let config = Config::load(&path).unwrap();
    assert!(config.servers.is_empty());
}

#[test]
fn invalid_name_rejected_before_any_edit() {
    let (_dir, path) = temp_config(Some(COMMENTED));
    let mut store = ConfigStore::edit(&path).unwrap();
    assert!(matches!(
        store.upsert_server("Bad Name", &http_server("https://x"), false),
        Err(Error::InvalidName { .. })
    ));
}

#[test]
fn lock_prevents_lost_updates() {
    let (_dir, path) = temp_config(None);

    // Writer A opens (and thereby locks) the config before B starts.
    let mut a = ConfigStore::edit_or_create(&path).unwrap();
    a.upsert_server("first", &http_server("https://a"), false)
        .unwrap();

    let b_path = path.clone();
    let b = std::thread::spawn(move || {
        // Blocks on the advisory lock until A saves and drops. Without the
        // lock B would read the missing file, start from the template, and
        // its save would erase A's server.
        let mut store = ConfigStore::edit_or_create(&b_path).unwrap();
        let saw_first = store.config().servers.contains_key("first");
        store
            .upsert_server("second", &http_server("https://b"), false)
            .unwrap();
        store.save().unwrap();
        saw_first
    });

    std::thread::sleep(std::time::Duration::from_millis(150));
    a.save().unwrap();
    drop(a);

    assert!(b.join().unwrap(), "B must observe A's saved state");
    let config = Config::load(&path).unwrap();
    assert!(config.servers.contains_key("first"));
    assert!(config.servers.contains_key("second"));
}

/// A hand-written `headers_command` is somebody's authentication, and an
/// unrelated `enable`/`disable` must not touch it — array spelling, string
/// spelling or the comment above either.
#[test]
fn an_unrelated_edit_leaves_a_hand_written_headers_command_alone() {
    const HELPER: &str = r#"version = 1

# the token lasts an hour, so it is minted per connect
[servers.corp]
type = "http"
url = "https://mcp.corp.example/mcp"
headers_command = "corp-auth print-mcp-headers"
"#;
    let (_dir, path) = temp_config(Some(HELPER));
    let mut store = ConfigStore::edit(&path).unwrap();
    store.set_enabled("corp", false).unwrap();
    store.save().unwrap();

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("headers_command = \"corp-auth print-mcp-headers\""),
        "{text}"
    );
    assert!(text.contains("# the token lasts an hour"), "{text}");
}

/// A generated entry writes the command as argv, and an entry without one
/// does not gain an empty array claiming it has a helper that printed
/// nothing.
#[test]
fn a_generated_entry_writes_the_command_as_argv() {
    let (_dir, path) = temp_config(None);
    let mut store = ConfigStore::edit_or_create(&path).unwrap();
    let mut corp = http_server("https://mcp.corp.example/mcp");
    let Transport::Http {
        headers_command, ..
    } = &mut corp.transport
    else {
        panic!("http");
    };
    *headers_command = vec!["corp-auth".to_owned(), "print-mcp-headers".to_owned()];
    store.upsert_server("corp", &corp, false).unwrap();
    store
        .upsert_server("plain", &http_server("https://p.example/mcp"), false)
        .unwrap();
    store.save().unwrap();

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains(r#"headers_command = ["corp-auth", "print-mcp-headers"]"#),
        "{text}"
    );
    assert_eq!(text.matches("headers_command").count(), 1, "{text}");
    // And it survives the read back, which is the invariant `commit` is for.
    assert_eq!(
        Config::load(&path).unwrap().servers["corp"].transport,
        corp.transport
    );
}

#[test]
fn tool_rules_are_written_as_a_table_and_keep_the_comments_around_them() {
    let (_dir, path) = temp_config(Some(COMMENTED));
    let mut store = ConfigStore::edit(&path).unwrap();
    store
        .set_tool_rules(
            "github",
            &ToolRules {
                allow: vec!["search_repositories".to_owned()],
                deny: vec!["delete_*".to_owned()],
            },
        )
        .unwrap();
    store.save().unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("# my precious header comment"), "{text}");
    assert!(
        text.contains("# github powers the code review flow"),
        "{text}"
    );
    insta::assert_snapshot!(text);

    // And read back as rules, not as text that happens to look like them.
    // The first handle goes first: the lock it holds is what a second
    // reader in this process would block on forever.
    drop(store);
    let store = ConfigStore::edit(&path).unwrap();
    let rules = store.config().servers["github"].tools.as_ref().unwrap();
    assert_eq!(rules.allow, ["search_repositories"]);
    assert_eq!(rules.deny, ["delete_*"]);
}

#[test]
fn clearing_the_rules_removes_the_table() {
    let (_dir, path) = temp_config(Some(COMMENTED));
    let mut store = ConfigStore::edit(&path).unwrap();
    store
        .set_tool_rules(
            "github",
            &ToolRules {
                allow: vec!["echo".to_owned()],
                deny: Vec::new(),
            },
        )
        .unwrap();
    store
        .set_tool_rules("github", &ToolRules::default())
        .unwrap();
    store.save().unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("tools"), "{text}");
    assert!(store.config().servers["github"].tools.is_none());
}

#[test]
fn rules_on_an_unknown_server_are_refused() {
    let (_dir, path) = temp_config(Some(COMMENTED));
    let mut store = ConfigStore::edit(&path).unwrap();
    let err = store
        .set_tool_rules("ghost", &ToolRules::default())
        .unwrap_err();
    assert!(matches!(err, Error::UnknownServer { .. }));
}

#[test]
fn overwriting_an_entry_keeps_its_tool_rules() {
    let (_dir, path) = temp_config(Some(COMMENTED));
    let mut store = ConfigStore::edit(&path).unwrap();
    store
        .set_tool_rules(
            "github",
            &ToolRules {
                allow: vec!["search_repositories".to_owned()],
                deny: Vec::new(),
            },
        )
        .unwrap();
    // What `add --force` and a re-import do. The transport is redefined; the
    // allowlist is not theirs to drop.
    store
        .upsert_server("github", &http_server("https://x"), true)
        .unwrap();
    store.save().unwrap();
    let rules = store.config().servers["github"].tools.as_ref().unwrap();
    assert_eq!(rules.allow, ["search_repositories"]);
}

/// `mcpgw auth login --client-id` has to leave the identity behind: a refresh
/// runs in the daemon, where nobody can pass a flag. The rest of the file —
/// comments, ordering, the entry's own hand-written shape — is untouched.
#[test]
fn set_auth_records_the_identity_without_rewriting_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "# hand-written\nversion = 1\n\n[servers.jira]\n# the corporate one\n\
         type = \"http\"\nurl = \"https://mcp.atlassian.com/mcp\"\n",
    )
    .unwrap();

    let mut store = mcpgw_core::ConfigStore::edit(&path).unwrap();
    store
        .set_auth(
            "jira",
            &mcpgw_core::config::ServerAuth {
                client_id: Some("abc123".to_owned()),
                client_secret_env: None,
                scopes: Vec::new(),
            },
        )
        .unwrap();
    store.save().unwrap();

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("# hand-written"), "{text}");
    assert!(text.contains("# the corporate one"), "{text}");
    assert!(text.contains("auth = { client_id = \"abc123\" }"), "{text}");

    let reread = mcpgw_core::Config::load(&path).unwrap();
    let mcpgw_core::Transport::Http { auth, .. } = &reread.servers["jira"].transport else {
        panic!("expected an http server");
    };
    assert_eq!(auth.as_ref().unwrap().client_id.as_deref(), Some("abc123"));

    // The first store still holds the file lock, and a second `edit` would
    // wait on it forever inside this one process.
    drop(store);

    // And it is refused on the entry where it would have no meaning.
    let err = mcpgw_core::ConfigStore::edit(&path)
        .unwrap()
        .set_auth("nope", &mcpgw_core::config::ServerAuth::default())
        .unwrap_err();
    assert!(
        matches!(err, mcpgw_core::Error::UnknownServer { .. }),
        "{err}"
    );
}
