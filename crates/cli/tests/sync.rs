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

    /// The platform-native app-data dir under the sandbox environment, which
    /// is where the GUI clients keep their config.
    fn app_data(&self) -> PathBuf {
        if cfg!(target_os = "macos") {
            self.home.join("Library/Application Support")
        } else if cfg!(windows) {
            self.home.join("AppData")
        } else {
            self.home.join(".config")
        }
    }

    /// Mirrors `ClientKind::config_path` for Claude Desktop under the sandbox
    /// environment.
    fn claude_desktop_path(&self) -> PathBuf {
        self.app_data().join("Claude/claude_desktop_config.json")
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

    /// `None` installs the directory alone (Windsurf present, unconfigured).
    fn install_windsurf(&self, config: Option<&str>) {
        let dir = self.home.join(".codeium/windsurf");
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(text) = config {
            std::fs::write(dir.join("mcp_config.json"), text).unwrap();
        }
    }

    /// Zed follows the XDG layout on macOS as well as Linux, so its
    /// directory is under the sandbox home rather than the app-data dir.
    fn zed_dir(&self) -> PathBuf {
        if cfg!(windows) {
            self.home.join("AppData/Zed")
        } else {
            self.home.join(".config/zed")
        }
    }

    /// `None` installs the directory alone (Zed present, unconfigured).
    fn install_zed(&self, settings: Option<&str>) {
        std::fs::create_dir_all(self.zed_dir()).unwrap();
        if let Some(text) = settings {
            std::fs::write(self.zed_dir().join("settings.json"), text).unwrap();
        }
    }

    fn zed_text(&self) -> String {
        std::fs::read_to_string(self.zed_dir().join("settings.json")).unwrap()
    }

    /// The written settings as canonical JSON — the comments are asserted on
    /// the text itself, the values here.
    fn zed_json(&self) -> serde_json::Value {
        mcpgw_core::ClientKind::Zed
            .codec()
            .parse_value(&self.zed_text())
            .unwrap()
    }

    /// Amp follows the XDG layout on macOS as well as Linux, and only uses
    /// the app-data dir on Windows.
    fn amp_dir(&self) -> PathBuf {
        if cfg!(windows) {
            self.home.join("AppData/amp")
        } else {
            self.home.join(".config/amp")
        }
    }

    /// `None` installs the directory alone (Amp present, unconfigured).
    fn install_amp(&self, settings: Option<&str>) {
        std::fs::create_dir_all(self.amp_dir()).unwrap();
        if let Some(text) = settings {
            std::fs::write(self.amp_dir().join("settings.json"), text).unwrap();
        }
    }

    fn amp_text(&self) -> String {
        std::fs::read_to_string(self.amp_dir().join("settings.json")).unwrap()
    }

    fn amp_json(&self) -> serde_json::Value {
        serde_json::from_str(&self.amp_text()).unwrap()
    }

    /// Cline's two surfaces read different files that nothing keeps in
    /// step, so each gets its own directory here.
    fn cline_dir(&self, kind: mcpgw_core::ClientKind) -> PathBuf {
        match kind {
            mcpgw_core::ClientKind::Cline => self
                .app_data()
                .join("Code/User/globalStorage/saoudrizwan.claude-dev/settings"),
            _ => self.home.join(".cline/data/settings"),
        }
    }

    /// `None` installs the directory alone (Cline present, unconfigured).
    fn install_cline(&self, kind: mcpgw_core::ClientKind, settings: Option<&str>) {
        std::fs::create_dir_all(self.cline_dir(kind)).unwrap();
        if let Some(text) = settings {
            std::fs::write(self.cline_dir(kind).join("cline_mcp_settings.json"), text).unwrap();
        }
    }

    fn cline_text(&self, kind: mcpgw_core::ClientKind) -> String {
        std::fs::read_to_string(self.cline_dir(kind).join("cline_mcp_settings.json")).unwrap()
    }

    fn cline_json(&self, kind: mcpgw_core::ClientKind) -> serde_json::Value {
        serde_json::from_str(&self.cline_text(kind)).unwrap()
    }

    /// Zoo Code's globalStorage dir, a sibling of Cline's under VS Code's.
    fn zoo_dir(&self) -> PathBuf {
        self.app_data()
            .join("Code/User/globalStorage/zoocodeorganization.zoo-code/settings")
    }

    /// `None` installs the directory alone (Zoo Code present, unconfigured).
    fn install_zoo(&self, settings: Option<&str>) {
        std::fs::create_dir_all(self.zoo_dir()).unwrap();
        if let Some(text) = settings {
            std::fs::write(self.zoo_dir().join("mcp_settings.json"), text).unwrap();
        }
    }

    fn zoo_text(&self) -> String {
        std::fs::read_to_string(self.zoo_dir().join("mcp_settings.json")).unwrap()
    }

    fn zoo_json(&self) -> serde_json::Value {
        serde_json::from_str(&self.zoo_text()).unwrap()
    }

    fn windsurf_json(&self) -> serde_json::Value {
        let text =
            std::fs::read_to_string(self.home.join(".codeium/windsurf/mcp_config.json")).unwrap();
        serde_json::from_str(&text).unwrap()
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

/// Windsurf's file is MCP-only, but a foreign entry in it still has to
/// survive a sync — including the `${env:VAR}` interpolation it may carry.
const WINDSURF_CONFIG: &str = r#"{
  "mcpServers": {
    "notes": {
      "command": "notes-mcp",
      "env": { "NOTES_KEY": "${env:NOTES_KEY}" }
    }
  }
}"#;

/// Zed's file is the whole editor's settings, comments and all, so a sync
/// has to leave every one of those bytes — and the entry an extension owns —
/// exactly where it found them.
const ZED_SETTINGS: &str = r#"// My Zed settings — do not reformat.
{
  "theme": "One Dark",
  "vim_mode": true,
  "context_servers": {
    // Installed by an extension, not by me.
    "postgres": {
      "source": "extension",
      "command": "postgres-context-server",
      "settings": { "database_url": "postgres://localhost/dev" }
    }
  }
}
"#;

/// A Cline file with an entry mcpgw does not own: switched off in place, and
/// carrying the `autoApprove` list Cline maintains itself. A sync has to
/// leave both exactly as it found them.
const CLINE_SETTINGS: &str = r#"{
  "mcpServers": {
    "notes": {
      "command": "notes-mcp",
      "disabled": true,
      "autoApprove": ["list_notes", "read_note"]
    }
  }
}"#;

/// A Zoo Code file whose foreign entry carries the whole Roo-era tail —
/// `cwd`, `timeout`, `watchPaths`, `alwaysAllow`, `disabledTools` — none of
/// which mcpgw models. A sync has to leave every one of them where it is.
const ZOO_SETTINGS: &str = r#"{
  "mcpServers": {
    "notes": {
      "command": "notes-mcp",
      "cwd": "/srv/notes",
      "timeout": 60,
      "watchPaths": ["/srv/notes/dist/index.js"],
      "disabled": true,
      "alwaysAllow": ["list_notes", "read_note"],
      "disabledTools": ["delete_note"]
    }
  }
}"#;

/// An Amp settings file with everything a sync has to leave alone: settings
/// that are not MCP at all, an entry mcpgw does not own, and — the one that
/// only Amp can get wrong — a genuinely nested `amp` object, which is a
/// different property from the `amp.mcpServers` key beside it.
const AMP_SETTINGS: &str = r#"{
  "amp.notifications.enabled": true,
  "amp.mcpServers": {
    "notes": {
      "command": "notes-mcp",
      "disabled": true
    }
  },
  "amp": {
    "mcpServers": {
      "decoy": { "command": "never-read-me" }
    }
  },
  "amp.tools.disable": ["edit_file"]
}"#;

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
fn windsurf_sync_writes_server_url_and_leaves_foreign_entries_alone() {
    let sb = Sandbox::new();
    sb.install_windsurf(Some(WINDSURF_CONFIG));
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

    let out = sb.ok(&["sync", "--client", "windsurf"]);
    assert!(out.contains("+ github"), "{out}");
    assert!(out.contains("+ linear"), "{out}");
    assert!(out.contains("? notes"), "{out}");

    let json = sb.windsurf_json();
    assert_eq!(json["mcpServers"]["notes"]["command"], "notes-mcp");
    assert_eq!(
        json["mcpServers"]["notes"]["env"]["NOTES_KEY"],
        "${env:NOTES_KEY}"
    );
    assert_eq!(json["mcpServers"]["github"]["command"], "npx");
    assert_eq!(json["mcpServers"]["github"]["env"]["TOKEN"], "t");
    // The remote field Windsurf reads is `serverUrl`; a plain `url` would
    // leave the entry unusable.
    assert_eq!(
        json["mcpServers"]["linear"]["serverUrl"],
        "https://mcp.linear.app/mcp"
    );
    assert!(json["mcpServers"]["linear"].get("url").is_none());
    assert!(json["mcpServers"]["linear"].get("type").is_none());

    let again = sb.ok(&["sync", "--client", "windsurf"]);
    assert!(again.contains("no changes"), "{again}");
}

#[test]
fn windsurf_gateway_entry_uses_server_url() {
    let sb = Sandbox::new();
    sb.install_windsurf(None);
    let out = sb.ok(&["sync", "--client", "windsurf", "--gateway"]);
    assert!(out.contains("+ mcpgw"), "{out}");

    let entry = sb.windsurf_json()["mcpServers"]["mcpgw"].clone();
    assert_eq!(entry["serverUrl"], "http://127.0.0.1:8137/mcp");
    assert!(entry.get("url").is_none());
    assert!(entry.get("command").is_none());
}

#[test]
fn zed_sync_marks_entries_custom_and_leaves_the_rest_of_the_settings_alone() {
    let sb = Sandbox::new();
    sb.install_zed(Some(ZED_SETTINGS));
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

    let out = sb.ok(&["sync", "--client", "zed"]);
    assert!(out.contains("+ github"), "{out}");
    assert!(out.contains("+ linear"), "{out}");
    assert!(out.contains("? postgres"), "{out}");

    let text = sb.zed_text();
    for comment in [
        "// My Zed settings — do not reformat.",
        "// Installed by an extension, not by me.",
    ] {
        assert!(text.contains(comment), "lost {comment:?} in:\n{text}");
    }

    let json = sb.zed_json();
    // Settings that have nothing to do with MCP are untouched.
    assert_eq!(json["theme"], "One Dark");
    assert_eq!(json["vim_mode"], true);
    // So is the entry mcpgw does not own, its foreign source included.
    assert_eq!(json["context_servers"]["postgres"]["source"], "extension");
    assert_eq!(
        json["context_servers"]["postgres"]["settings"]["database_url"],
        "postgres://localhost/dev"
    );

    // Without `source: custom` Zed ignores an entry without a word, so both
    // transports carry it.
    assert_eq!(json["context_servers"]["github"]["source"], "custom");
    assert_eq!(json["context_servers"]["github"]["command"], "npx");
    assert_eq!(json["context_servers"]["github"]["env"]["TOKEN"], "t");
    assert_eq!(json["context_servers"]["linear"]["source"], "custom");
    assert_eq!(
        json["context_servers"]["linear"]["url"],
        "https://mcp.linear.app/mcp"
    );
    assert!(json["context_servers"]["linear"].get("type").is_none());

    let again = sb.ok(&["sync", "--client", "zed"]);
    assert!(again.contains("no changes"), "{again}");
    // A no-op run leaves the file byte for byte as the first one wrote it.
    assert_eq!(sb.zed_text(), text);
}

#[test]
fn zed_gateway_entry_is_a_custom_url() {
    let sb = Sandbox::new();
    sb.install_zed(None);
    let out = sb.ok(&["sync", "--client", "zed", "--gateway"]);
    assert!(out.contains("+ mcpgw"), "{out}");

    let entry = sb.zed_json()["context_servers"]["mcpgw"].clone();
    assert_eq!(entry["url"], "http://127.0.0.1:8137/mcp");
    assert_eq!(entry["source"], "custom");
    assert!(entry.get("command").is_none());
}

#[test]
fn cline_sync_types_remote_entries_and_leaves_foreign_ones_intact() {
    for kind in [
        mcpgw_core::ClientKind::Cline,
        mcpgw_core::ClientKind::ClineCli,
    ] {
        let sb = Sandbox::new();
        sb.install_cline(kind, Some(CLINE_SETTINGS));
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

        let out = sb.ok(&["sync", "--client", kind.id()]);
        assert!(out.contains("+ github"), "{out}");
        assert!(out.contains("+ linear"), "{out}");
        assert!(out.contains("? notes"), "{out}");

        let json = sb.cline_json(kind);
        // The entry mcpgw does not own keeps its off switch and the
        // auto-approved tool list Cline maintains for it.
        assert_eq!(json["mcpServers"]["notes"]["disabled"], true);
        assert_eq!(
            json["mcpServers"]["notes"]["autoApprove"],
            serde_json::json!(["list_notes", "read_note"])
        );

        assert_eq!(json["mcpServers"]["github"]["command"], "npx");
        assert_eq!(json["mcpServers"]["github"]["env"]["TOKEN"], "t");
        // Without the type Cline would treat the URL as an SSE endpoint.
        assert_eq!(json["mcpServers"]["linear"]["type"], "streamableHttp");
        assert_eq!(
            json["mcpServers"]["linear"]["url"],
            "https://mcp.linear.app/mcp"
        );

        let text = sb.cline_text(kind);
        let again = sb.ok(&["sync", "--client", kind.id()]);
        assert!(again.contains("no changes"), "{again}");
        assert_eq!(sb.cline_text(kind), text);
    }
}

#[test]
fn cline_gateway_entry_carries_the_streamable_type() {
    let sb = Sandbox::new();
    sb.install_cline(mcpgw_core::ClientKind::Cline, None);
    let out = sb.ok(&["sync", "--client", "cline", "--gateway"]);
    assert!(out.contains("+ mcpgw"), "{out}");

    let entry = sb.cline_json(mcpgw_core::ClientKind::Cline)["mcpServers"]["mcpgw"].clone();
    assert_eq!(entry["url"], "http://127.0.0.1:8137/mcp");
    assert_eq!(entry["type"], "streamableHttp");
    assert!(entry.get("command").is_none());
}

/// Syncing one surface must not touch the other: they are separate installs,
/// and a user who has both expects both to end up with the server.
#[test]
fn the_two_cline_surfaces_are_synced_independently() {
    let sb = Sandbox::new();
    sb.install_cline(mcpgw_core::ClientKind::Cline, None);
    sb.install_cline(mcpgw_core::ClientKind::ClineCli, None);
    sb.ok(&["add", "linear", "--url", "https://mcp.linear.app/mcp"]);

    sb.ok(&["sync", "--client", "cline"]);
    assert!(
        !sb.cline_dir(mcpgw_core::ClientKind::ClineCli)
            .join("cline_mcp_settings.json")
            .exists()
    );

    sb.ok(&["sync", "--client", "cline-cli"]);
    for kind in [
        mcpgw_core::ClientKind::Cline,
        mcpgw_core::ClientKind::ClineCli,
    ] {
        assert_eq!(
            sb.cline_json(kind)["mcpServers"]["linear"]["url"],
            "https://mcp.linear.app/mcp"
        );
    }
}

#[test]
fn zoo_sync_types_remote_entries_and_keeps_the_roo_extras() {
    let sb = Sandbox::new();
    sb.install_zoo(Some(ZOO_SETTINGS));
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

    let out = sb.ok(&["sync", "--client", "zoo"]);
    assert!(out.contains("+ github"), "{out}");
    assert!(out.contains("+ linear"), "{out}");
    assert!(out.contains("? notes"), "{out}");

    let json = sb.zoo_json();
    // Every field of the entry mcpgw does not own survives untouched, extras
    // that have no canonical counterpart included.
    let notes = &json["mcpServers"]["notes"];
    assert_eq!(notes["disabled"], true);
    assert_eq!(notes["cwd"], "/srv/notes");
    assert_eq!(notes["timeout"], 60);
    assert_eq!(
        notes["watchPaths"],
        serde_json::json!(["/srv/notes/dist/index.js"])
    );
    assert_eq!(
        notes["alwaysAllow"],
        serde_json::json!(["list_notes", "read_note"])
    );
    assert_eq!(notes["disabledTools"], serde_json::json!(["delete_note"]));

    assert_eq!(json["mcpServers"]["github"]["command"], "npx");
    assert_eq!(json["mcpServers"]["github"]["env"]["TOKEN"], "t");
    // The hyphenated spelling is the only one Zoo Code's schema accepts, and
    // without a type at all it would treat the URL as an SSE endpoint.
    assert_eq!(json["mcpServers"]["linear"]["type"], "streamable-http");
    assert_eq!(
        json["mcpServers"]["linear"]["url"],
        "https://mcp.linear.app/mcp"
    );

    let text = sb.zoo_text();
    let again = sb.ok(&["sync", "--client", "zoo"]);
    assert!(again.contains("no changes"), "{again}");
    assert_eq!(sb.zoo_text(), text);
}

/// Zoo Code and Cline share a config lineage but not a file: each writes its
/// own globalStorage dir, and syncing one must not create the other's.
#[test]
fn zoo_gateway_entry_carries_the_hyphenated_streamable_type() {
    let sb = Sandbox::new();
    sb.install_zoo(None);
    let out = sb.ok(&["sync", "--client", "zoo", "--gateway"]);
    assert!(out.contains("+ mcpgw"), "{out}");

    let entry = sb.zoo_json()["mcpServers"]["mcpgw"].clone();
    assert_eq!(entry["url"], "http://127.0.0.1:8137/mcp");
    assert_eq!(entry["type"], "streamable-http");
    assert!(entry.get("command").is_none());

    // Cline's sibling dir stays untouched: a Zoo Code sync is not a Cline one.
    assert!(
        !sb.cline_dir(mcpgw_core::ClientKind::Cline)
            .join("cline_mcp_settings.json")
            .exists()
    );
}

#[test]
fn amp_sync_writes_the_namespaced_key_and_leaves_the_nested_one_alone() {
    let sb = Sandbox::new();
    sb.install_amp(Some(AMP_SETTINGS));
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

    let out = sb.ok(&["sync", "--client", "amp"]);
    assert!(out.contains("+ github"), "{out}");
    assert!(out.contains("+ linear"), "{out}");
    assert!(out.contains("? notes"), "{out}");

    let json = sb.amp_json();
    // Settings that have nothing to do with MCP are untouched, dotted
    // siblings of the server map included.
    assert_eq!(json["amp.notifications.enabled"], true);
    assert_eq!(json["amp.tools.disable"], serde_json::json!(["edit_file"]));
    // So is the entry mcpgw does not own, its off switch included.
    assert_eq!(json["amp.mcpServers"]["notes"]["disabled"], true);
    // The nested object is a different property and stays a bystander: it is
    // neither read as the server map nor written into.
    assert_eq!(
        json["amp"]["mcpServers"]["decoy"]["command"],
        "never-read-me"
    );
    assert!(json["amp"]["mcpServers"].get("github").is_none());

    assert_eq!(json["amp.mcpServers"]["github"]["command"], "npx");
    assert_eq!(json["amp.mcpServers"]["github"]["env"]["TOKEN"], "t");
    // Amp infers the transport from the URL; it has no `type` field.
    assert_eq!(
        json["amp.mcpServers"]["linear"]["url"],
        "https://mcp.linear.app/mcp"
    );
    assert!(json["amp.mcpServers"]["linear"].get("type").is_none());

    let text = sb.amp_text();
    let again = sb.ok(&["sync", "--client", "amp"]);
    assert!(again.contains("no changes"), "{again}");
    assert_eq!(sb.amp_text(), text);
}

#[test]
fn amp_gateway_entry_is_a_bare_url() {
    let sb = Sandbox::new();
    sb.install_amp(None);
    let out = sb.ok(&["sync", "--client", "amp", "--gateway"]);
    assert!(out.contains("+ mcpgw"), "{out}");

    let entry = sb.amp_json()["amp.mcpServers"]["mcpgw"].clone();
    assert_eq!(entry["url"], "http://127.0.0.1:8137/mcp");
    assert!(entry.get("type").is_none());
    assert!(entry.get("command").is_none());
}

/// The `--client` and `--from` help is generated from `ClientKind::ALL`, so
/// this fails the moment an adapter lands without it — which is what the
/// hand-written list it replaced never did.
#[test]
fn the_client_flags_list_every_shipped_id() {
    let sb = Sandbox::new();
    let sync = sb.ok(&["sync", "--help"]);
    let import = sb.ok(&["import", "--help"]);
    for kind in mcpgw_core::ClientKind::ALL {
        assert!(
            sync.contains(kind.id()),
            "sync help lost {}:\n{sync}",
            kind.id()
        );
        assert!(
            import.contains(kind.id()),
            "import help lost {}:\n{import}",
            kind.id()
        );
    }
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
