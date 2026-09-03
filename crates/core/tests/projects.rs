//! Repo-local config discovery: what it finds, how it classifies it, and —
//! just as much the point — where it refuses to look.

use std::collections::BTreeMap;
use std::path::Path;

use mcpgw_core::projects::{Standing, discover, repo_root};
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
