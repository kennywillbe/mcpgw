//! Repo-local config discovery: what it finds, how it classifies it, and —
//! just as much the point — where it refuses to look.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use mcpgw_core::projects::{Standing, discover, repo_root};
use mcpgw_core::state::{ManagedState, Scope};
use mcpgw_core::{ClientKind, Server, Transport};

const SHARED_URL: &str = "https://mcp.linear.app/mcp";

/// `.mcp.json` at the root: the shared entry plus one only this file has.
const CLAUDE_CODE: &str = r#"{
  "mcpServers": {
    "linear": { "type": "http", "url": "https://mcp.linear.app/mcp" },
    "scratch": { "command": "cargo", "args": ["run"] }
  }
}"#;

/// `.cursor/mcp.json`: the same shared entry, and a different unique one.
const CURSOR: &str = r#"{
  "mcpServers": {
    "linear": { "type": "http", "url": "https://mcp.linear.app/mcp" },
    "notes": { "command": "cargo" }
  }
}"#;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A repo with both project files in it, under a parent that also holds a
/// `.mcp.json` nothing may reach.
fn fake_repo(root: &Path) -> std::path::PathBuf {
    write(
        &root.join(".mcp.json"),
        r#"{"mcpServers": {"outsider": {}}}"#,
    );
    let repo = root.join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    write(&repo.join(".mcp.json"), CLAUDE_CODE);
    write(&repo.join(".cursor/mcp.json"), CURSOR);
    repo
}

/// The canonical config against which entries are mirrored or not: it holds
/// the shared server and neither of the unique ones.
fn canonical() -> BTreeMap<String, Server> {
    [(
        "linear".to_owned(),
        Server {
            enabled: true,
            tags: Vec::new(),
            transport: Transport::Http {
                url: SHARED_URL.to_owned(),
                headers_command: Vec::new(),
                headers: BTreeMap::new(),
            },
        },
    )]
    .into_iter()
    .collect()
}

#[test]
fn both_project_files_are_found_and_classified() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fake_repo(dir.path());

    let found = discover(&repo);
    let kinds: Vec<ClientKind> = found.iter().map(|config| config.kind).collect();
    assert_eq!(kinds, vec![ClientKind::ClaudeCode, ClientKind::Cursor]);

    let canonical = canonical();
    let claude = &found[0];
    assert_eq!(claude.path, repo.join(".mcp.json"));
    assert_eq!(
        claude.standings(&canonical),
        vec![
            ("linear", Standing::Mirrors),
            ("scratch", Standing::Unmanaged),
        ]
    );
    assert_eq!(claude.unmanaged(&canonical), 1);

    let cursor = &found[1];
    assert_eq!(cursor.path, repo.join(".cursor").join("mcp.json"));
    assert_eq!(
        cursor.standings(&canonical),
        vec![
            ("linear", Standing::Mirrors),
            ("notes", Standing::Unmanaged)
        ]
    );
}

/// An entry whose name matches but whose transport does not is nobody's
/// mirror: syncing the canonical one leaves this file pointing somewhere
/// else entirely, which is the case the report exists for.
#[test]
fn a_matching_name_with_a_different_target_is_unmanaged() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fake_repo(dir.path());
    let mut canonical = canonical();
    canonical.get_mut("linear").unwrap().transport = Transport::Http {
        url: "https://mcp.example.com/mcp".to_owned(),
        headers_command: Vec::new(),
        headers: BTreeMap::new(),
    };

    let found = discover(&repo);
    assert_eq!(found[0].unmanaged(&canonical), 2);
}

/// The repo root is the boundary. A `.mcp.json` in the directory above a
/// checkout belongs to whatever that directory is, and discovery from
/// anywhere inside the repo must never open it.
#[test]
fn nothing_above_the_repo_root_is_read() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fake_repo(dir.path());
    let deep = repo.join("crates").join("core");
    std::fs::create_dir_all(&deep).unwrap();

    let found = discover(&deep);
    assert_eq!(repo_root(&deep).as_deref(), Some(repo.as_path()));
    for config in &found {
        assert!(
            config.path.starts_with(&repo),
            "{} is outside {}",
            config.path.display(),
            repo.display()
        );
        assert!(!config.read.servers.contains_key("outsider"));
    }
    assert_eq!(found.len(), 2);
}

/// No `.git` anywhere is not "no project": a directory can hold a `.mcp.json`
/// before it is a repo, or without ever being one.
#[test]
fn a_directory_that_is_not_a_repo_still_checks_itself() {
    let dir = tempfile::tempdir().unwrap();
    let loose = dir.path().join("loose");
    write(&loose.join(".mcp.json"), CLAUDE_CODE);

    assert_eq!(repo_root(&loose), None);
    let found = discover(&loose);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].path, loose.join(".mcp.json"));
    assert_eq!(found[0].read.servers.len(), 2);
}

/// A `.git` file rather than a directory is what a worktree and a submodule
/// have, and both are checkouts with a root worth honouring.
#[test]
fn a_git_file_marks_a_root_the_way_a_git_directory_does() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("worktree");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join(".git"), "gitdir: /elsewhere/.git/worktrees/x\n").unwrap();
    write(&repo.join(".vscode/mcp.json"), r#"{"servers": {}}"#);

    let inner = repo.join("src");
    std::fs::create_dir_all(&inner).unwrap();
    let found = discover(&inner);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].kind, ClientKind::VsCode);
}

/// A file that will not parse is one file-level problem, not a lost report:
/// the entries it holds are unknown, and saying so is the whole job.
#[test]
fn an_unparseable_project_file_becomes_a_problem() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    write(&repo.join(".mcp.json"), "{ this is not json");

    let found = discover(&repo);
    assert_eq!(found.len(), 1);
    assert!(found[0].read.servers.is_empty());
    assert_eq!(found[0].read.problems.len(), 1);
    assert!(found[0].read.problems[0].server.is_none());
}

/// Standing in your own home directory does not turn a client's per-user
/// config into a project file: `~/.cursor/mcp.json` is the same path under
/// both names, and reporting it as unmanaged would accuse mcpgw's own sync
/// target of being unmanaged.
#[test]
fn a_clients_own_per_user_file_is_not_a_project_file() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    write(&home.join(".cursor/mcp.json"), CURSOR);
    write(&home.join(".mcp.json"), CLAUDE_CODE);

    let home_env = home.clone();
    let found = mcpgw_core::projects::discover_with(&home, |key| match key {
        "HOME" | "USERPROFILE" => Some(home_env.clone().into_os_string()),
        _ => None,
    });

    // Cursor's is skipped; Claude Code's `.mcp.json` is a project file even
    // here, because its per-user config is `~/.claude.json`.
    let kinds: Vec<ClientKind> = found.iter().map(|config| config.kind).collect();
    assert_eq!(kinds, vec![ClientKind::ClaudeCode]);
}

/// `.cursor/mcp.json` as a team really keeps it: comments, a trailing comma
/// and one entry that is nobody's but the repo's.
const CURSOR_JSONC: &str = r#"{
  // the one we all share
  "mcpServers": {
    "linear": { "type": "http", "url": "https://mcp.linear.app/mcp" },
    "notes": { "command": "cargo" },
  }
}"#;

/// The names an `import --project` would have adopted out of one file.
fn adopted(
    servers: &BTreeMap<String, Server>,
    canonical: &BTreeMap<String, Server>,
) -> BTreeSet<String> {
    servers
        .keys()
        .filter(|name| canonical.contains_key(*name))
        .cloned()
        .collect()
}

/// The gateway entry `sync --project` writes for `name`.
fn gateway_entry(kind: ClientKind, name: &str, canonical: &BTreeMap<String, Server>) -> Server {
    mcpgw_core::sync::per_server_gateway_server(
        kind,
        name,
        &canonical[name],
        "http://127.0.0.1:8137/mcp",
        "mcpgw",
    )
    .unwrap()
}

/// One project file, planned and written the way `sync --project` does it:
/// the project codec, the plan, and the edit that only touches what the plan
/// owns.
fn sync_project(
    kind: ClientKind,
    path: &Path,
    desired: &BTreeMap<String, Server>,
    managed: &BTreeSet<String>,
) -> BTreeSet<String> {
    let codec = kind.project_codec();
    let text = std::fs::read_to_string(path).unwrap();
    let mut doc = codec.parse_document(&text).unwrap();
    let plan = mcpgw_core::sync::plan_sync(kind, &doc.entries(codec.root), desired, managed);
    if !plan.has_changes() {
        return plan.managed_after();
    }
    mcpgw_core::sync::apply_plan_to(kind, &mut doc, &plan).unwrap();
    std::fs::write(path, doc.to_text().unwrap()).unwrap();
    plan.managed_after()
}

/// The whole of `sync --project` over a two-file repo: both files get the
/// gateway entries, the entry only one of them has is written only there,
/// and the file nobody edited by hand keeps everything about it that is not
/// a server entry.
#[test]
fn a_project_sync_writes_both_files_and_leaves_the_rest_alone() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fake_repo(dir.path());
    write(&repo.join(".cursor/mcp.json"), CURSOR_JSONC);

    let mut canonical = canonical();
    canonical.insert(
        "scratch".to_owned(),
        Server {
            enabled: true,
            tags: Vec::new(),
            transport: Transport::Stdio {
                command: "cargo".to_owned(),
                args: vec!["run".to_owned()],
                env: BTreeMap::new(),
            },
        },
    );

    for config in discover(&repo) {
        let desired: BTreeMap<String, Server> = canonical
            .keys()
            .map(|name| (name.clone(), gateway_entry(config.kind, name, &canonical)))
            .collect();
        // What an `import --project` would have adopted: the entries this
        // file holds that the canonical config now knows about. Anything
        // else in the file is the repo's own and stays foreign.
        let managed: BTreeSet<String> = adopted(&config.read.servers, &canonical);
        sync_project(config.kind, &config.path, &desired, &managed);
    }

    let claude = std::fs::read_to_string(repo.join(".mcp.json")).unwrap();
    assert!(claude.contains("/s/linear"), "{claude}");
    assert!(claude.contains("/s/scratch"), "{claude}");

    let cursor = std::fs::read_to_string(repo.join(".cursor/mcp.json")).unwrap();
    assert!(cursor.contains("/s/linear"), "{cursor}");
    // The comment is the point of the project codec: this file is reviewed.
    assert!(cursor.contains("// the one we all share"), "{cursor}");
    // `notes` is the repo's own, and nothing claimed it.
    assert!(cursor.contains(r#""command": "cargo""#), "{cursor}");
}

/// A file that is already what the plan wants is not rewritten at all, so a
/// second `sync --project` leaves the repo with nothing to commit.
#[test]
fn a_second_project_sync_changes_no_byte() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fake_repo(dir.path());
    write(&repo.join(".cursor/mcp.json"), CURSOR_JSONC);
    let canonical = canonical();

    let mut once: Vec<(ClientKind, std::path::PathBuf, BTreeSet<String>, String)> = Vec::new();
    for config in discover(&repo) {
        let desired: BTreeMap<String, Server> = canonical
            .keys()
            .map(|name| (name.clone(), gateway_entry(config.kind, name, &canonical)))
            .collect();
        let managed: BTreeSet<String> = adopted(&config.read.servers, &canonical);
        let after = sync_project(config.kind, &config.path, &desired, &managed);
        let text = std::fs::read_to_string(&config.path).unwrap();
        once.push((config.kind, config.path, after, text));
    }

    for (kind, path, managed, first) in once {
        let desired: BTreeMap<String, Server> = canonical
            .keys()
            .map(|name| (name.clone(), gateway_entry(kind, name, &canonical)))
            .collect();
        sync_project(kind, &path, &desired, &managed);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            first,
            "{} was rewritten by a sync with nothing to do",
            path.display()
        );
    }
}

/// An entry mcpgw never wrote is reported and left exactly as it is — the
/// same promise a per-user file gets, and the one a committed file needs
/// most.
#[test]
fn a_project_entry_mcpgw_never_wrote_is_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fake_repo(dir.path());
    let path = repo.join(".mcp.json");
    let canonical = canonical();
    let desired: BTreeMap<String, Server> = canonical
        .keys()
        .map(|name| {
            (
                name.clone(),
                gateway_entry(ClientKind::ClaudeCode, name, &canonical),
            )
        })
        .collect();

    // Nothing is claimed, so `linear` is a conflict and `scratch` is foreign.
    let codec = ClientKind::ClaudeCode.project_codec();
    let text = std::fs::read_to_string(&path).unwrap();
    let doc = codec.parse_document(&text).unwrap();
    let plan = mcpgw_core::sync::plan_sync(
        ClientKind::ClaudeCode,
        &doc.entries(codec.root),
        &desired,
        &BTreeSet::new(),
    );
    assert_eq!(plan.conflicts, vec!["linear".to_owned()]);
    assert_eq!(plan.foreign, vec!["scratch".to_owned()]);
    assert!(!plan.has_changes());

    sync_project(ClientKind::ClaudeCode, &path, &desired, &BTreeSet::new());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
}

/// Backups are per file, not per client: a repo file and the client's own
/// per-user file must not share a five-deep stack, or a rollback restores
/// one from a snapshot of the other.
#[test]
fn each_project_file_gets_a_backup_stack_of_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fake_repo(dir.path());
    let state_dir = dir.path().join("state");

    let mut keys: Vec<String> = Vec::new();
    for config in discover(&repo) {
        let scope = config.scope();
        mcpgw_core::backup::backup_file(&state_dir, &scope.backup_key(), &config.path).unwrap();
        keys.push(scope.backup_key());
    }
    let home = Scope::Home(ClientKind::ClaudeCode).backup_key();
    assert_eq!(keys.len(), 2);
    assert_ne!(keys[0], keys[1]);
    assert!(!keys.contains(&home), "{keys:?}");

    // And the newest backup of a file restores that file.
    let scope = discover(&repo)[0].scope();
    let latest = mcpgw_core::backup::latest_backup(&state_dir, &scope.backup_key())
        .unwrap()
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(latest).unwrap(),
        std::fs::read_to_string(repo.join(".mcp.json")).unwrap()
    );
}

/// The two scopes are two records. A project file claiming `linear` must not
/// make the client's per-user file claim it too, or the next per-user sync
/// deletes an entry nobody wrote.
#[test]
fn a_project_file_and_a_per_user_file_keep_separate_records() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fake_repo(dir.path());
    let project = discover(&repo)[0].scope();
    let home = Scope::Home(ClientKind::ClaudeCode);

    let mut state = ManagedState::default();
    home.claim(&mut state, ["github".to_owned()].into_iter().collect());
    project.claim(&mut state, ["linear".to_owned()].into_iter().collect());
    project.resolve_to(&mut state, "linear", "linear-2");

    assert_eq!(home.managed(&state), ["github".to_owned()].into());
    assert_eq!(project.managed(&state), ["linear".to_owned()].into());
    assert!(home.resolved(&state).is_empty());
    assert_eq!(project.resolved(&state)["linear"], "linear-2");
    // And the record names its own file, so `eject` finds it without a cwd.
    assert_eq!(state.project_scopes(), vec![project]);
}

/// Two repos on one machine are two records, keyed by the file rather than
/// by the client that reads it.
#[test]
fn two_repos_do_not_share_one_record() {
    let dir = tempfile::tempdir().unwrap();
    let one = fake_repo(&dir.path().join("one"));
    let two = fake_repo(&dir.path().join("two"));
    let (one, two) = (discover(&one)[0].scope(), discover(&two)[0].scope());

    let mut state = ManagedState::default();
    one.claim(&mut state, ["linear".to_owned()].into_iter().collect());
    assert!(two.managed(&state).is_empty());
    assert_ne!(one.backup_key(), two.backup_key());
}

/// A state file from before project files could be written loads as exactly
/// what it says and nothing more: those per-user claims, and no repo.
#[test]
fn an_old_state_file_loads_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("managed.json");
    std::fs::write(
        &path,
        r#"{"clients":{"cursor":["github"]},"resolved":{"cursor":{"github":"github-2"}}}"#,
    )
    .unwrap();

    let state = ManagedState::load(&path).unwrap();
    let home = Scope::Home(ClientKind::Cursor);
    assert_eq!(home.managed(&state), ["github".to_owned()].into());
    assert_eq!(home.resolved(&state)["github"], "github-2");
    assert!(state.files.is_empty());
    assert!(state.project_scopes().is_empty());
    assert!(!state.migrated);

    // And saving it back does not invent a repo it never mentioned.
    state.save(&path).unwrap();
    assert_eq!(ManagedState::load(&path).unwrap(), state);
}

/// A committed file with a comment in it is read, not refused: the per-user
/// codec would call a `.mcp.json` carrying one broken and report the whole
/// file as a problem.
#[test]
fn a_commented_project_file_is_read_rather_than_refused() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    write(
        &repo.join(".mcp.json"),
        "{\n  // ours\n  \"mcpServers\": {}\n}",
    );

    let found = discover(&repo);
    assert_eq!(found.len(), 1);
    assert!(found[0].read.problems.is_empty());
}

/// An entry mcpgw writes reads as managed, which is the line `doctor` draws
/// between an entry that stays right by itself and one sync keeps right.
#[test]
fn a_managed_entry_stands_apart_from_one_that_merely_matches() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fake_repo(dir.path());
    let config = discover(&repo).remove(0);

    let mut state = ManagedState::default();
    assert_eq!(
        config.standings_in(&canonical(), &state),
        vec![
            ("linear", Standing::Mirrors),
            ("scratch", Standing::Unmanaged),
        ]
    );

    config
        .scope()
        .claim(&mut state, ["linear".to_owned()].into_iter().collect());
    assert_eq!(
        config.standings_in(&canonical(), &state),
        vec![
            ("linear", Standing::Managed),
            ("scratch", Standing::Unmanaged),
        ]
    );
    assert_eq!(config.unmanaged_in(&canonical(), &state), 1);
}
