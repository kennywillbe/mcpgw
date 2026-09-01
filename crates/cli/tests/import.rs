use std::path::PathBuf;
use std::process::Output;

use assert_cmd::Command;

struct Sandbox {
    _dir: tempfile::TempDir,
    home: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        Self {
            home: dir.path().to_owned(),
            _dir: dir,
        }
    }

    fn mcpgw(&self, args: &[&str]) -> Output {
        Command::cargo_bin("mcpgw")
            .unwrap()
            .args(args)
            .env("MCPGW_CONFIG", self.home.join("config.toml"))
            .env("MCPGW_STATE_DIR", self.home.join("state"))
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("APPDATA", self.home.join("AppData"))
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.mcpgw(args);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn write_client(&self, rel: &str, json: &str) {
        let path = self.home.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, json).unwrap();
    }
}

#[test]
fn imports_dedups_and_renames_across_clients() {
    let sb = Sandbox::new();
    sb.write_client(
        ".cursor/mcp.json",
        r#"{"mcpServers": {
            "github": {"command": "npx", "args": ["server-github"]},
            "My Notes": {"command": "notes-mcp"}
        }}"#,
    );
    sb.write_client(
        ".claude.json",
        r#"{"mcpServers": {"github": {"command": "npx", "args": ["server-github"]}}}"#,
    );

    let out = sb.ok(&["import"]);
    assert!(
        out.contains("+ github (from claude-code, cursor)")
            || out.contains("+ github (from cursor, claude-code)"),
        "{out}"
    );
    assert!(out.contains("renamed from \"My Notes\""), "{out}");

    let list = sb.ok(&["list", "--json"]);
    let json: serde_json::Value = serde_json::from_str(&list).unwrap();
    assert!(json["servers"]["github"].is_object());
    assert!(json["servers"]["my-notes"].is_object());
}

#[test]
fn import_then_sync_produces_no_conflicts() {
    let sb = Sandbox::new();
    sb.write_client(
        ".cursor/mcp.json",
        r#"{"mcpServers": {"github": {"command": "npx", "args": ["server-github"]}}}"#,
    );
    sb.ok(&["import", "--from", "cursor"]);

    // Adoption must make sync own the entry: no `!` conflict, no changes.
    let out = sb.ok(&["sync", "--client", "cursor"]);
    assert!(!out.contains("! github"), "{out}");
    assert!(out.contains("no changes"), "{out}");
}

#[test]
fn renamed_import_is_renamed_in_client_by_next_sync() {
    let sb = Sandbox::new();
    sb.write_client(
        ".cursor/mcp.json",
        r#"{"mcpServers": {"My Notes": {"command": "notes-mcp"}}}"#,
    );
    sb.ok(&["import", "--from", "cursor"]);
    let out = sb.ok(&["sync", "--client", "cursor"]);
    // The adopted original name is replaced by the canonical slug.
    assert!(out.contains("+ my-notes"), "{out}");
    assert!(out.contains("- My Notes"), "{out}");

    let text = std::fs::read_to_string(sb.home.join(".cursor/mcp.json")).unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(json["mcpServers"].get("My Notes").is_none());
    assert_eq!(json["mcpServers"]["my-notes"]["command"], "notes-mcp");
}

#[test]
fn piped_conflict_is_skipped_and_canonical_untouched() {
    let sb = Sandbox::new();
    sb.ok(&["add", "github", "--", "npx", "canonical-version"]);
    sb.write_client(
        ".cursor/mcp.json",
        r#"{"mcpServers": {"github": {"command": "npx", "args": ["client-version"]}}}"#,
    );
    let out = sb.ok(&["import", "--from", "cursor"]);
    assert!(out.contains("! github"), "{out}");

    let list = sb.ok(&["list", "--json"]);
    let json: serde_json::Value = serde_json::from_str(&list).unwrap();
    assert_eq!(json["servers"]["github"]["args"][0], "canonical-version");
}

#[test]
fn identical_entry_is_adopted_not_duplicated() {
    let sb = Sandbox::new();
    sb.ok(&["add", "github", "--", "npx", "server-github"]);
    sb.write_client(
        ".cursor/mcp.json",
        r#"{"mcpServers": {"github": {"command": "npx", "args": ["server-github"]}}}"#,
    );
    let out = sb.ok(&["import", "--from", "cursor"]);
    assert!(out.contains("= github already present (adopted)"), "{out}");
    let sync = sb.ok(&["sync", "--client", "cursor"]);
    assert!(!sync.contains("! github"), "{sync}");
}

/// Import writes two files and cannot make that atomic, so the order is
/// chosen for which half is safe to lose. Losing the adoption record (a
/// `state.save()` that fails after the canonical file landed) leaves the
/// entries unmanaged: sync reports them and leaves them alone, and a second
/// import adopts them. The other order would leave them claimed with no
/// canonical entry behind them, which sync reads as a removal.
#[test]
fn a_lost_adoption_record_never_costs_the_client_entry() {
    let sb = Sandbox::new();
    sb.write_client(
        ".cursor/mcp.json",
        r#"{"mcpServers": {"github": {"command": "npx", "args": ["server-github"]}}}"#,
    );
    sb.ok(&["import", "--from", "cursor"]);

    // The state exactly as a failed second save would have left it: the
    // canonical config has the server, nothing claims the client entry.
    std::fs::remove_file(sb.home.join("state/managed.json")).unwrap();

    let out = sb.ok(&["sync", "--client", "cursor"]);
    assert!(out.contains("! github"), "{out}");
    let text = std::fs::read_to_string(sb.home.join(".cursor/mcp.json")).unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(json["mcpServers"]["github"]["args"][0], "server-github");

    // Re-running import is the whole repair.
    let out = sb.ok(&["import", "--from", "cursor"]);
    assert!(out.contains("= github already present (adopted)"), "{out}");
    let out = sb.ok(&["sync", "--client", "cursor"]);
    assert!(!out.contains("! github"), "{out}");
}

#[test]
fn import_from_gemini_adopts_and_carries_the_excluded_flag() {
    let sb = Sandbox::new();
    sb.write_client(
        ".gemini/settings.json",
        r#"{
            "theme": "Default",
            "mcp": { "excluded": ["notes"] },
            "mcpServers": {
                "github": {"command": "npx", "args": ["server-github"], "trust": true},
                "linear": {"httpUrl": "https://mcp.linear.app/mcp"},
                "notes": {"command": "notes-mcp"}
            }
        }"#,
    );
    let out = sb.ok(&["import", "--from", "gemini"]);
    assert!(out.contains("+ github"), "{out}");
    assert!(out.contains("+ linear"), "{out}");
    assert!(out.contains("+ notes"), "{out}");

    let json: serde_json::Value = serde_json::from_str(&sb.ok(&["list", "--json"])).unwrap();
    assert_eq!(
        json["servers"]["linear"]["url"],
        "https://mcp.linear.app/mcp"
    );
    // `mcp.excluded` is Gemini's only off switch, so it has to survive the
    // trip into the canonical config as a disabled server.
    assert_eq!(json["servers"]["notes"]["enabled"], false);
    assert_ne!(json["servers"]["github"]["enabled"], false);

    // Adoption means the next sync owns the entries rather than conflicting.
    let sync = sb.ok(&["sync", "--client", "gemini"]);
    assert!(!sync.contains("! "), "{sync}");
}

#[test]
fn dry_run_writes_nothing() {
    let sb = Sandbox::new();
    sb.write_client(
        ".cursor/mcp.json",
        r#"{"mcpServers": {"github": {"command": "npx"}}}"#,
    );
    let out = sb.ok(&["import", "--dry-run"]);
    assert!(out.contains("+ github"), "{out}");
    assert!(!sb.home.join("config.toml").exists());
}

#[test]
fn unknown_from_id_errors() {
    let sb = Sandbox::new();
    let out = sb.mcpgw(&["import", "--from", "emacs"]);
    assert!(!out.status.success());
}

#[test]
fn nothing_to_import_reports_cleanly() {
    let sb = Sandbox::new();
    let out = sb.ok(&["import"]);
    assert!(out.contains("nothing to import"), "{out}");
}
