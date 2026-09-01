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

    /// Mirrors `ClientKind::config_path` for Claude Desktop under the sandbox
    /// environment, per platform app-data convention.
    fn claude_desktop_path(&self) -> PathBuf {
        let app_data = if cfg!(target_os = "macos") {
            self.home.join("Library/Application Support")
        } else if cfg!(windows) {
            self.home.join("AppData")
        } else {
            self.home.join(".config")
        };
        app_data.join("Claude/claude_desktop_config.json")
    }

    fn install_claude_desktop(&self) {
        std::fs::create_dir_all(self.claude_desktop_path().parent().unwrap()).unwrap();
    }

    fn claude_desktop_json(&self) -> serde_json::Value {
        let text = std::fs::read_to_string(self.claude_desktop_path()).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    fn install_gemini(&self, settings: Option<&str>) {
        let dir = self.home.join(".gemini");
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(text) = settings {
            std::fs::write(dir.join("settings.json"), text).unwrap();
        }
    }

    fn gemini_json(&self) -> serde_json::Value {
        let text = std::fs::read_to_string(self.home.join(".gemini/settings.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    fn install_codex(&self, config: Option<&str>) {
        let dir = self.home.join(".codex");
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(text) = config {
            std::fs::write(dir.join("config.toml"), text).unwrap();
        }
    }

    fn codex_text(&self) -> String {
        std::fs::read_to_string(self.home.join(".codex/config.toml")).unwrap()
    }

    /// The written TOML as canonical JSON, so the assertions below are about
    /// values rather than the spelling the writer happened to pick.
    fn codex_toml(&self) -> serde_json::Value {
        mcpgw_core::ClientKind::Codex
            .codec()
            .parse_value(&self.codex_text())
            .unwrap()
    }

    /// opencode follows the XDG layout on every platform, so its directory is
    /// under the sandbox home rather than the app-data dir.
    fn opencode_dir(&self) -> PathBuf {
        self.home.join(".config/opencode")
    }

    /// `name` picks which of the two accepted filenames the fixture uses;
    /// `None` installs the directory alone (opencode present, unconfigured).
    fn install_opencode(&self, name_and_config: Option<(&str, &str)>) {
        std::fs::create_dir_all(self.opencode_dir()).unwrap();
        if let Some((name, text)) = name_and_config {
            std::fs::write(self.opencode_dir().join(name), text).unwrap();
        }
    }

    fn opencode_text(&self, name: &str) -> String {
        std::fs::read_to_string(self.opencode_dir().join(name)).unwrap()
    }

    /// The written JSONC as canonical JSON — comments and all are asserted on
    /// the text itself, values here.
    fn opencode_json(&self, name: &str) -> serde_json::Value {
        mcpgw_core::ClientKind::Opencode
            .codec()
            .parse_value(&self.opencode_text(name))
            .unwrap()
    }
}

/// Gemini's settings file is the whole CLI's settings, not an MCP file: it
/// carries theme, auth and everything else next to `mcpServers`.
const GEMINI_SETTINGS: &str = r#"{
  "theme": "Default",
  "security": { "auth": { "selectedType": "oauth-personal" } },
  "mcp": { "excluded": ["notes"] },
  "mcpServers": {
    "notes": { "command": "notes-mcp" }
  }
}"#;

/// Codex's config.toml is the whole CLI's configuration, and TOML means a
/// sync has comments and hand formatting to lose as well as sibling keys.
const CODEX_CONFIG: &str = r#"# My codex setup — do not reformat.
model = "gpt-5-codex"
approval_policy = "on-request"

[sandbox_workspace_write]
network_access = false

# Added by hand, months ago.
[mcp_servers.notes]
command = "notes-mcp"
startup_timeout_sec = 20
required = true

[mcp_servers.notes.tools.search]
enabled = true
"#;

/// opencode's config is JSONC in practice: comments and a trailing comma in
/// a file that also holds the rest of the CLI's settings.
const OPENCODE_CONFIG: &str = r#"// My opencode setup — do not reformat.
{
  "$schema": "https://opencode.ai/config.json",
  "theme": "system",
  "mcp": {
    // Added by hand, months ago.
    "notes": {
      "type": "local",
      "command": ["notes-mcp"],
      "cwd": "./notes",
    },
  },
}
"#;

/// The bridge command is either the bare name (mcpgw on PATH) or the path of
/// the binary under test.
fn assert_bridge_command(value: &serde_json::Value) {
    let command = value.as_str().unwrap();
    assert!(
        command == "mcpgw" || command.contains("mcpgw"),
        "unexpected bridge command {command:?}"
    );
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
fn rollback_backs_up_what_it_overwrites_so_it_can_be_undone() {
    let sb = Sandbox::new();
    sb.install_cursor(Some(r#"{"mcpServers": {"mine": {"command": "deno"}}}"#));
    sb.ok(&["add", "github", "--", "npx", "server-github"]);
    sb.ok(&["sync", "--client", "cursor"]);

    // Back to the pre-sync file...
    sb.ok(&["sync", "--client", "cursor", "--rollback"]);
    assert!(sb.cursor_json()["mcpServers"].get("github").is_none());

    // ...and back again: the rollback stacked a backup of the synced file,
    // so a rollback fired by mistake is not the end of the road.
    sb.ok(&["sync", "--client", "cursor", "--rollback"]);
    let json = sb.cursor_json();
    assert_eq!(json["mcpServers"]["github"]["command"], "npx");
    assert_eq!(json["mcpServers"]["mine"]["command"], "deno");
}

/// A failed client write must leave the entries *claimed* rather than
/// orphaned: over-claiming reconciles on the next run, under-claiming makes
/// them foreign forever.
#[cfg(unix)]
#[test]
fn a_failed_client_write_leaves_state_the_next_sync_can_repair() {
    use std::os::unix::fs::PermissionsExt as _;

    let sb = Sandbox::new();
    sb.install_cursor(None);
    sb.ok(&["add", "github", "--", "npx", "server-github"]);

    // Read-only parent: the atomic write cannot create its temp file there.
    let dir = sb.home.join(".cursor");
    let original = std::fs::metadata(&dir).unwrap().permissions();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let failed = sb.mcpgw(&["sync", "--client", "cursor"]);
    std::fs::set_permissions(&dir, original).unwrap();
    assert!(!failed.status.success());

    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(sb.state.join("managed.json")).unwrap())
            .unwrap();
    assert_eq!(state["clients"]["cursor"][0], "github");

    // The claim without the entry reads as a plain add, not a conflict.
    let out = sb.ok(&["sync", "--client", "cursor"]);
    assert!(out.contains("+ github"), "{out}");
    assert!(!out.contains("! github"), "{out}");
    assert_eq!(sb.cursor_json()["mcpServers"]["github"]["command"], "npx");
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

#[test]
fn gateway_mode_replaces_directly_synced_entries() {
    let sb = Sandbox::new();
    sb.install_cursor(Some(r#"{"mcpServers": {"mine": {"command": "deno"}}}"#));
    sb.ok(&["add", "github", "--", "npx", "server-github"]);
    sb.ok(&["add", "linear", "--url", "https://mcp.linear.app/mcp"]);
    sb.ok(&["sync", "--client", "cursor"]);

    let out = sb.ok(&["sync", "--client", "cursor", "--gateway"]);
    assert!(out.contains("gateway mode"), "{out}");
    assert!(out.contains("+ mcpgw"), "{out}");
    assert!(out.contains("- github"), "{out}");
    assert!(out.contains("- linear"), "{out}");

    let entries = sb.cursor_json()["mcpServers"].clone();
    let names: Vec<&str> = entries
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(names, ["mine", "mcpgw"]);
    assert_eq!(entries["mine"]["command"], "deno");
    assert_eq!(entries["mcpgw"]["type"], "http");
    assert_eq!(entries["mcpgw"]["url"], "http://127.0.0.1:8137/mcp");
    assert!(entries["mcpgw"].get("command").is_none());

    let again = sb.ok(&["sync", "--client", "cursor", "--gateway"]);
    assert!(again.contains("no changes"), "{again}");
}

#[test]
fn claude_desktop_gets_the_stdio_bridge() {
    let sb = Sandbox::new();
    sb.install_cursor(None);
    sb.install_claude_desktop();
    sb.ok(&[
        "sync",
        "--client",
        "claude-desktop",
        "--client",
        "cursor",
        "--gateway",
    ]);

    let entry = sb.claude_desktop_json()["mcpServers"]["mcpgw"].clone();
    assert_bridge_command(&entry["command"]);
    assert_eq!(
        entry["args"],
        serde_json::json!(["connect", "--url", "http://127.0.0.1:8137/mcp"])
    );
    assert!(entry.get("url").is_none());
    assert!(entry.get("type").is_none());

    let cursor = sb.cursor_json()["mcpServers"]["mcpgw"].clone();
    assert_eq!(cursor["type"], "http");
    assert_eq!(cursor["url"], "http://127.0.0.1:8137/mcp");
    assert!(cursor.get("command").is_none());
}

#[test]
fn gateway_url_override_reaches_both_shapes() {
    let sb = Sandbox::new();
    sb.install_cursor(None);
    sb.install_claude_desktop();
    sb.ok(&[
        "sync",
        "--gateway",
        "--gateway-url",
        "http://127.0.0.1:9000/mcp",
    ]);

    assert_eq!(
        sb.cursor_json()["mcpServers"]["mcpgw"]["url"],
        "http://127.0.0.1:9000/mcp"
    );
    assert_eq!(
        sb.claude_desktop_json()["mcpServers"]["mcpgw"]["args"],
        serde_json::json!(["connect", "--url", "http://127.0.0.1:9000/mcp"])
    );
}

#[test]
fn gateway_mode_reverts_to_direct_entries() {
    let sb = Sandbox::new();
    sb.install_cursor(None);
    sb.ok(&["add", "github", "--", "npx", "server-github"]);
    sb.ok(&["sync", "--client", "cursor", "--gateway"]);
    assert!(sb.cursor_json()["mcpServers"]["mcpgw"]["url"].is_string());

    let out = sb.ok(&["sync", "--client", "cursor"]);
    assert!(out.contains("direct mode"), "{out}");
    assert!(out.contains("+ github"), "{out}");
    assert!(out.contains("- mcpgw"), "{out}");
    let entries = sb.cursor_json()["mcpServers"].clone();
    assert_eq!(entries["github"]["command"], "npx");
    assert!(entries.get("mcpgw").is_none());
}

#[test]
fn gemini_sync_preserves_the_rest_of_the_settings_file() {
    let sb = Sandbox::new();
    sb.install_gemini(Some(GEMINI_SETTINGS));
    sb.ok(&[
        "add",
        "github",
        "--env",
        "TOKEN=t",
        "--",
        "npx",
        "server-github",
    ]);
    sb.ok(&["add", "linear", "--url", "https://mcp.linear.app/mcp"]);

    let out = sb.ok(&["sync", "--client", "gemini"]);
    assert!(out.contains("+ github"), "{out}");
    assert!(out.contains("+ linear"), "{out}");
    // The excluded foreign entry is reported, never touched.
    assert!(out.contains("? notes"), "{out}");

    let json = sb.gemini_json();
    assert_eq!(json["theme"], "Default");
    assert_eq!(json["security"]["auth"]["selectedType"], "oauth-personal");
    assert_eq!(json["mcp"]["excluded"], serde_json::json!(["notes"]));
    assert_eq!(json["mcpServers"]["notes"]["command"], "notes-mcp");
    assert_eq!(json["mcpServers"]["github"]["command"], "npx");
    assert_eq!(json["mcpServers"]["github"]["env"]["TOKEN"], "t");
    // Streamable HTTP, not the legacy SSE `url` field.
    assert_eq!(
        json["mcpServers"]["linear"]["httpUrl"],
        "https://mcp.linear.app/mcp"
    );
    assert!(json["mcpServers"]["linear"].get("url").is_none());

    let again = sb.ok(&["sync", "--client", "gemini"]);
    assert!(again.contains("no changes"), "{again}");
}

#[test]
fn gemini_gateway_entry_uses_http_url() {
    let sb = Sandbox::new();
    sb.install_gemini(None);
    let out = sb.ok(&["sync", "--client", "gemini", "--gateway"]);
    assert!(out.contains("+ mcpgw"), "{out}");

    let entry = sb.gemini_json()["mcpServers"]["mcpgw"].clone();
    assert_eq!(entry["httpUrl"], "http://127.0.0.1:8137/mcp");
    assert!(entry.get("url").is_none());
    assert!(entry.get("type").is_none());
    assert!(entry.get("command").is_none());
}

#[test]
fn codex_sync_preserves_comments_siblings_and_foreign_entries() {
    let sb = Sandbox::new();
    sb.install_codex(Some(CODEX_CONFIG));
    sb.ok(&[
        "add",
        "github",
        "--env",
        "TOKEN=t",
        "--",
        "npx",
        "server-github",
    ]);
    sb.ok(&["add", "linear", "--url", "https://mcp.linear.app/mcp"]);

    let out = sb.ok(&["sync", "--client", "codex"]);
    assert!(out.contains("+ github"), "{out}");
    assert!(out.contains("+ linear"), "{out}");
    assert!(out.contains("? notes"), "{out}");

    // toml_edit rewrites only the entries the plan owns, so the hand-written
    // bytes around them survive verbatim.
    let text = sb.codex_text();
    for comment in [
        "# My codex setup — do not reformat.",
        "# Added by hand, months ago.",
    ] {
        assert!(text.contains(comment), "lost {comment:?} in:\n{text}");
    }
    assert!(text.contains("[mcp_servers.notes.tools.search]"), "{text}");

    let toml = sb.codex_toml();
    assert_eq!(toml["model"], "gpt-5-codex");
    assert_eq!(toml["approval_policy"], "on-request");
    assert_eq!(toml["sandbox_workspace_write"]["network_access"], false);
    // The foreign entry keeps every field Codex knows and mcpgw does not.
    assert_eq!(toml["mcp_servers"]["notes"]["startup_timeout_sec"], 20);
    assert_eq!(toml["mcp_servers"]["notes"]["required"], true);

    assert_eq!(toml["mcp_servers"]["github"]["command"], "npx");
    assert_eq!(toml["mcp_servers"]["github"]["env"]["TOKEN"], "t");
    assert_eq!(
        toml["mcp_servers"]["linear"]["url"],
        "https://mcp.linear.app/mcp"
    );
    assert!(toml["mcp_servers"]["linear"].get("type").is_none());

    let again = sb.ok(&["sync", "--client", "codex"]);
    assert!(again.contains("no changes"), "{again}");
}

#[test]
fn codex_gateway_entry_is_a_plain_url() {
    let sb = Sandbox::new();
    sb.install_codex(None);
    let out = sb.ok(&["sync", "--client", "codex", "--gateway"]);
    assert!(out.contains("+ mcpgw"), "{out}");

    let entry = sb.codex_toml()["mcp_servers"]["mcpgw"].clone();
    assert_eq!(entry["url"], "http://127.0.0.1:8137/mcp");
    assert!(entry.get("type").is_none());
    assert!(entry.get("command").is_none());
}

/// The headline of the JSONC write path: a hand-written opencode config goes
/// through a sync with its comments, its trailing commas and its foreign
/// entry's extra fields intact.
#[test]
fn opencode_sync_preserves_comments_siblings_and_foreign_entries() {
    let sb = Sandbox::new();
    sb.install_opencode(Some(("opencode.jsonc", OPENCODE_CONFIG)));
    sb.ok(&[
        "add",
        "github",
        "--env",
        "TOKEN=t",
        "--",
        "npx",
        "server-github",
    ]);
    sb.ok(&["add", "linear", "--url", "https://mcp.linear.app/mcp"]);

    let out = sb.ok(&["sync", "--client", "opencode"]);
    assert!(out.contains("+ github"), "{out}");
    assert!(out.contains("+ linear"), "{out}");
    assert!(out.contains("? notes"), "{out}");

    let text = sb.opencode_text("opencode.jsonc");
    for comment in [
        "// My opencode setup — do not reformat.",
        "// Added by hand, months ago.",
    ] {
        assert!(text.contains(comment), "lost {comment:?} in:\n{text}");
    }
    // The foreign entry keeps its own spelling, extra field included.
    assert!(text.contains(r#""cwd": "./notes""#), "{text}");

    let json = sb.opencode_json("opencode.jsonc");
    assert_eq!(json["$schema"], "https://opencode.ai/config.json");
    assert_eq!(json["theme"], "system");
    assert_eq!(
        json["mcp"]["notes"]["command"],
        serde_json::json!(["notes-mcp"])
    );

    // Program and arguments in one array, variables under `environment`.
    assert_eq!(json["mcp"]["github"]["type"], "local");
    assert_eq!(
        json["mcp"]["github"]["command"],
        serde_json::json!(["npx", "server-github"])
    );
    assert_eq!(json["mcp"]["github"]["environment"]["TOKEN"], "t");
    assert_eq!(json["mcp"]["linear"]["type"], "remote");
    assert_eq!(json["mcp"]["linear"]["url"], "https://mcp.linear.app/mcp");

    let again = sb.ok(&["sync", "--client", "opencode"]);
    assert!(again.contains("no changes"), "{again}");
    // A no-op run leaves the file byte for byte as the first one wrote it.
    assert_eq!(sb.opencode_text("opencode.jsonc"), text);
}

/// Both filenames are first-class, so which one a sync writes is decided by
/// what the machine already has — and by the `.json` default when it has
/// nothing.
#[test]
fn opencode_writes_the_extension_the_machine_already_uses() {
    let sb = Sandbox::new();
    sb.install_opencode(None);
    sb.ok(&["add", "github", "--", "npx", "server-github"]);
    sb.ok(&["sync", "--client", "opencode"]);
    assert!(sb.opencode_dir().join("opencode.json").is_file());
    assert!(!sb.opencode_dir().join("opencode.jsonc").exists());
    assert_eq!(
        sb.opencode_json("opencode.json")["mcp"]["github"]["command"],
        serde_json::json!(["npx", "server-github"])
    );

    let other = Sandbox::new();
    other.install_opencode(Some(("opencode.jsonc", "// mine\n{}\n")));
    other.ok(&["add", "github", "--", "npx", "server-github"]);
    other.ok(&["sync", "--client", "opencode"]);
    assert!(!other.opencode_dir().join("opencode.json").exists());
    let text = other.opencode_text("opencode.jsonc");
    assert!(text.contains("// mine"), "{text}");
    assert_eq!(
        other.opencode_json("opencode.jsonc")["mcp"]["github"]["type"],
        "local"
    );
}

#[test]
fn opencode_gateway_entry_is_a_remote_type() {
    let sb = Sandbox::new();
    sb.install_opencode(None);
    let out = sb.ok(&["sync", "--client", "opencode", "--gateway"]);
    assert!(out.contains("+ mcpgw"), "{out}");

    let entry = sb.opencode_json("opencode.json")["mcp"]["mcpgw"].clone();
    assert_eq!(entry["type"], "remote");
    assert_eq!(entry["url"], "http://127.0.0.1:8137/mcp");
    assert!(entry.get("command").is_none());
}

#[test]
fn gateway_and_rollback_conflict() {
    let sb = Sandbox::new();
    let out = sb.mcpgw(&["sync", "--gateway", "--rollback"]);
    assert!(!out.status.success());
}

#[test]
fn gateway_dry_run_writes_nothing() {
    let sb = Sandbox::new();
    sb.install_cursor(None);
    let out = sb.ok(&["sync", "--client", "cursor", "--gateway", "--dry-run"]);
    assert!(out.contains("+ mcpgw"), "{out}");
    assert!(!sb.home.join(".cursor/mcp.json").exists());
}
