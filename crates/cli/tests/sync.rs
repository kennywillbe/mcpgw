use std::path::PathBuf;
use std::process::Output;

use assert_cmd::Command;

struct Sandbox {
    _dir: tempfile::TempDir,
    home: PathBuf,
    config: PathBuf,
    state: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_owned();
        Self {
            config: home.join("config.toml"),
            state: home.join("state"),
            home,
            _dir: dir,
        }
    }

    fn mcpgw(&self, args: &[&str]) -> Output {
        Command::cargo_bin("mcpgw")
            .unwrap()
            .args(args)
            .env("MCPGW_CONFIG", &self.config)
            .env("MCPGW_STATE_DIR", &self.state)
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

    fn install_cursor(&self, config: Option<&str>) -> PathBuf {
        let dir = self.home.join(".cursor");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mcp.json");
        if let Some(text) = config {
            std::fs::write(&path, text).unwrap();
        }
        path
    }

    fn cursor_json(&self) -> serde_json::Value {
        let text = std::fs::read_to_string(self.home.join(".cursor/mcp.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }
}

#[test]
fn sync_creates_config_for_installed_client_and_is_idempotent() {
    let sb = Sandbox::new();
    sb.install_cursor(None);
    sb.ok(&[
        "add",
        "github",
        "--env",
        "TOKEN=t",
        "--",
        "npx",
        "-y",
        "server-github",
    ]);

    let out = sb.ok(&["sync", "--client", "cursor"]);
    assert!(out.contains("+ github"), "{out}");
    assert_eq!(sb.cursor_json()["mcpServers"]["github"]["command"], "npx");
    // State recorded ownership.
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(sb.state.join("managed.json")).unwrap())
            .unwrap();
    assert_eq!(state["clients"]["cursor"][0], "github");

    let again = sb.ok(&["sync", "--client", "cursor"]);
    assert!(again.contains("no changes"), "{again}");
}

#[test]
fn foreign_entries_and_root_keys_survive() {
    let sb = Sandbox::new();
    sb.install_cursor(Some(
        r#"{"telemetry": false, "mcpServers": {"mine": {"command": "deno"}}}"#,
    ));
    sb.ok(&["add", "github", "--", "npx", "server-github"]);
    let out = sb.ok(&["sync", "--client", "cursor"]);
    assert!(out.contains("? mine"), "{out}");

    let json = sb.cursor_json();
    assert_eq!(json["telemetry"], false);
    assert_eq!(json["mcpServers"]["mine"]["command"], "deno");
    assert_eq!(json["mcpServers"]["github"]["command"], "npx");
}

#[test]
fn conflicting_unmanaged_name_is_never_overwritten() {
    let sb = Sandbox::new();
    sb.install_cursor(Some(r#"{"mcpServers": {"github": {"command": "my-own"}}}"#));
    sb.ok(&["add", "github", "--", "npx", "server-github"]);
    let out = sb.ok(&["sync", "--client", "cursor"]);
    assert!(out.contains("! github"), "{out}");
    assert_eq!(
        sb.cursor_json()["mcpServers"]["github"]["command"],
        "my-own"
    );
}

#[test]
fn dry_run_writes_nothing() {
    let sb = Sandbox::new();
    sb.install_cursor(None);
    sb.ok(&["add", "github", "--", "npx", "server-github"]);
    let out = sb.ok(&["sync", "--client", "cursor", "--dry-run"]);
    assert!(out.contains("+ github"), "{out}");
    assert!(!sb.home.join(".cursor/mcp.json").exists());
    assert!(!sb.state.join("managed.json").exists());
}

#[test]
fn disabling_a_server_removes_it_on_next_sync() {
    let sb = Sandbox::new();
    sb.install_cursor(None);
    sb.ok(&["add", "github", "--", "npx", "server-github"]);
    sb.ok(&["sync", "--client", "cursor"]);
    sb.ok(&["disable", "github"]);
    let out = sb.ok(&["sync", "--client", "cursor"]);
    assert!(out.contains("- github"), "{out}");
    assert!(sb.cursor_json()["mcpServers"].get("github").is_none());
}

#[test]
fn rollback_restores_previous_content() {
    let sb = Sandbox::new();
    sb.install_cursor(Some(r#"{"mcpServers": {"mine": {"command": "deno"}}}"#));
    sb.ok(&["add", "github", "--", "npx", "server-github"]);
    sb.ok(&["sync", "--client", "cursor"]);
    assert!(sb.cursor_json()["mcpServers"].get("github").is_some());

    let out = sb.ok(&["sync", "--client", "cursor", "--rollback"]);
    assert!(out.contains("restored"), "{out}");
    let json = sb.cursor_json();
    assert!(json["mcpServers"].get("github").is_none());
    assert_eq!(json["mcpServers"]["mine"]["command"], "deno");
}

#[test]
fn jsonc_file_is_skipped_untouched() {
    let sb = Sandbox::new();
    let path = sb.install_cursor(Some("// my comment\n{\"mcpServers\": {}}\n"));
    sb.ok(&["add", "github", "--", "npx", "server-github"]);
    let out = sb.ok(&["sync", "--client", "cursor"]);
    assert!(out.contains("not strict JSON"), "{out}");
    assert!(
        std::fs::read_to_string(path)
            .unwrap()
            .starts_with("// my comment")
    );
}

#[test]
fn unknown_client_id_is_an_error() {
    let sb = Sandbox::new();
    let out = sb.mcpgw(&["sync", "--client", "emacs"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("valid: claude-desktop"), "{stderr}");
}

#[test]
fn rollback_without_backups_fails() {
    let sb = Sandbox::new();
    let out = sb.mcpgw(&["sync", "--rollback"]);
    assert!(!out.status.success());
}
