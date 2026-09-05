use std::path::{Path, PathBuf};
use std::process::Output;

use assert_cmd::Command;

/// The repo's own Claude Code config, as it stands before mcpgw sees it.
const PROJECT_CLAUDE: &str = r#"{
  "mcpServers": {
    "github": {
      "command": "cargo",
      "args": ["run"]
    }
  }
}
"#;

/// The same file with an entry that arrived after the import — nobody
/// adopted it, so nothing mcpgw does may touch it.
const PROJECT_CLAUDE_WITH_A_STRANGER: &str = r#"{
  "mcpServers": {
    "github": {
      "command": "cargo",
      "args": ["run"]
    },
    "housekeeping": {
      "command": "make"
    }
  }
}
"#;

/// The repo's Cursor config, hand-written the way one really is.
const PROJECT_CURSOR: &str = r#"{
  // the server we all share
  "mcpServers": {
    "github": { "command": "cargo", "args": ["run"] },
  }
}
"#;

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
            // Hermetic: no test may phone home for a version notice.
            .env("MCPGW_NO_UPDATE_CHECK", "1")
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

    /// The same run from a working directory of the test's choosing —
    /// `--project` reports on the repo the process is standing in, so a test
    /// about it has to say where that is.
    fn mcpgw_in(&self, cwd: &Path, args: &[&str]) -> Output {
        Command::cargo_bin("mcpgw")
            .unwrap()
            .current_dir(cwd)
            .env("MCPGW_NO_UPDATE_CHECK", "1")
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

    fn ok_in(&self, cwd: &Path, args: &[&str]) -> String {
        let out = self.mcpgw_in(cwd, args);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// A repo with the two project files a team really commits: Claude
    /// Code's plain `.mcp.json` and a `.cursor/mcp.json` with a comment and
    /// a trailing comma in it.
    fn fake_repo(&self) -> PathBuf {
        let repo = self.home.join("work/api");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join(".cursor")).unwrap();
        std::fs::write(repo.join(".mcp.json"), PROJECT_CLAUDE).unwrap();
        std::fs::write(repo.join(".cursor/mcp.json"), PROJECT_CURSOR).unwrap();
        repo
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

    /// Stamps mcpgw's own state with the entries a client is claimed to hold.
    ///
    /// The upgrade-path tests need a client a *previous* mcpgw synced, and
    /// what that mcpgw wrote is a shape this binary can no longer produce. An
    /// entry nothing claims reads as foreign and sync leaves it alone, so the
    /// claim is what makes such a fixture a migration rather than a stranger.
    fn claim(&self, client: &str, names: &[&str]) {
        self.claim_clients(&[(client, names)]);
    }

    /// [`claim`](Self::claim) for a fixture where a previous mcpgw synced
    /// more than one client.
    fn claim_clients(&self, clients: &[(&str, &[&str])]) {
        std::fs::create_dir_all(&self.state).unwrap();
        let clients: serde_json::Map<String, serde_json::Value> = clients
            .iter()
            .map(|(client, names)| ((*client).to_owned(), serde_json::json!(names)))
            .collect();
        // Deliberately without `migrated`: this is a state file an older
        // mcpgw wrote, and the field did not exist then.
        let state = serde_json::json!({ "clients": clients });
        std::fs::write(
            self.state.join("managed.json"),
            serde_json::to_string(&state).unwrap(),
        )
        .unwrap();
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

/// The same file after Cline has been at it: mcpgw's own entry, switched off
/// from inside Cline and carrying the auto-approved tool list Cline keeps.
const CLINE_AFTER_THE_USER: &str = r#"{
  // Switched off from inside Cline, on purpose.
  "mcpServers": {
    "github": {
      "command": "npx",
      "args": ["server-github"],
      "disabled": true,
      "autoApprove": ["list_issues"],
    },
  },
}
"#;

/// The same file once the entry points at the gateway: mcpgw's own entry in
/// the shape a sync writes it, switched off from inside Cline and carrying
/// the auto-approved tool list Cline keeps.
const CLINE_GATEWAY_AFTER_THE_USER: &str = r#"{
  // Switched off from inside Cline, on purpose.
  "mcpServers": {
    "github": {
      "url": "http://127.0.0.1:8137/s/github",
      "type": "streamableHttp",
      "disabled": true,
      "autoApprove": ["list_issues"],
    },
  },
}
"#;

/// An Amp settings file the way Amp's own docs show one: VS Code-shaped
/// settings with a comment in them, which the strict-JSON reader refused.
const AMP_COMMENTED_SETTINGS: &str = r#"{
  // My Amp settings — do not reformat.
  "amp.notifications.enabled": true,
  "amp.mcpServers": {
    "notes": { "command": "notes-mcp", "disabled": true }
  }
}
"#;

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
        "--no-sync",
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
    let entry = sb.cursor_json()["mcpServers"]["github"].clone();
    assert_eq!(entry["type"], "http");
    assert_eq!(entry["url"], "http://127.0.0.1:8137/s/github");
    // The server's own transport stays behind the gateway: the client is
    // handed an endpoint, never the command or the environment to run it.
    assert!(entry.get("command").is_none());
    assert!(entry.get("env").is_none());
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
    sb.ok(&["add", "--no-sync", "github", "--", "npx", "server-github"]);
    let out = sb.ok(&["sync", "--client", "cursor"]);
    assert!(out.contains("? mine"), "{out}");

    let json = sb.cursor_json();
    assert_eq!(json["telemetry"], false);
    assert_eq!(json["mcpServers"]["mine"]["command"], "deno");
    assert_eq!(
        json["mcpServers"]["github"]["url"],
        "http://127.0.0.1:8137/s/github"
    );
}

#[test]
fn conflicting_unmanaged_name_is_never_overwritten() {
    let sb = Sandbox::new();
    sb.install_cursor(Some(r#"{"mcpServers": {"github": {"command": "my-own"}}}"#));
    sb.ok(&["add", "--no-sync", "github", "--", "npx", "server-github"]);
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
    sb.ok(&["add", "--no-sync", "github", "--", "npx", "server-github"]);
    let out = sb.ok(&["sync", "--client", "cursor", "--dry-run"]);
    assert!(out.contains("+ github"), "{out}");
    assert!(!sb.home.join(".cursor/mcp.json").exists());
    assert!(!sb.state.join("managed.json").exists());
}

#[test]
fn disabling_a_server_removes_it_on_next_sync() {
    let sb = Sandbox::new();
    sb.install_cursor(None);
    sb.ok(&["add", "--no-sync", "github", "--", "npx", "server-github"]);
    sb.ok(&[
        "add",
        "--no-sync",
        "linear",
        "--url",
        "https://mcp.linear.app/mcp",
    ]);
    sb.ok(&["sync", "--client", "cursor"]);
    sb.ok(&["disable", "--no-sync", "linear"]);
    let out = sb.ok(&["sync", "--client", "cursor"]);
    assert!(out.contains("- linear"), "{out}");
    let entries = sb.cursor_json()["mcpServers"].clone();
    assert!(entries.get("linear").is_none());
    // The servers that stayed enabled are left exactly where they were.
    assert_eq!(entries["github"]["url"], "http://127.0.0.1:8137/s/github");
}

#[test]
fn rollback_restores_previous_content() {
    let sb = Sandbox::new();
    sb.install_cursor(Some(r#"{"mcpServers": {"mine": {"command": "deno"}}}"#));
    sb.ok(&["add", "--no-sync", "github", "--", "npx", "server-github"]);
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
    sb.ok(&["add", "--no-sync", "github", "--", "npx", "server-github"]);
    sb.ok(&["sync", "--client", "cursor"]);

    // Back to the pre-sync file...
    sb.ok(&["sync", "--client", "cursor", "--rollback"]);
    assert!(sb.cursor_json()["mcpServers"].get("github").is_none());

    // ...and back again: the rollback stacked a backup of the synced file,
    // so a rollback fired by mistake is not the end of the road.
    sb.ok(&["sync", "--client", "cursor", "--rollback"]);
    let json = sb.cursor_json();
    assert_eq!(
        json["mcpServers"]["github"]["url"],
        "http://127.0.0.1:8137/s/github"
    );
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
    sb.ok(&["add", "--no-sync", "github", "--", "npx", "server-github"]);

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
    assert_eq!(
        sb.cursor_json()["mcpServers"]["github"]["url"],
        "http://127.0.0.1:8137/s/github"
    );
}

#[test]
fn jsonc_file_is_skipped_untouched() {
    let sb = Sandbox::new();
    let path = sb.install_cursor(Some("// my comment\n{\"mcpServers\": {}}\n"));
    sb.ok(&["add", "--no-sync", "github", "--", "npx", "server-github"]);
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

/// A Cursor config as a pre-gateway mcpgw left it: the servers spawned by the
/// client itself, under the canonical names. A literal rather than the output
/// of a sync, because no mcpgw that writes this shape exists any more — which
/// is what makes it worth pinning.
const CURSOR_DIRECT: &str = r#"{
  "mcpServers": {
    "mine": { "command": "deno" },
    "github": { "command": "npx", "args": ["server-github"] },
    "linear": { "type": "http", "url": "https://mcp.linear.app/mcp" }
  }
}"#;

/// The upgrade path, and the headline of what sync does: the entry names do
/// not move, so a client an older mcpgw synced directly comes over as a set of
/// updates — no removes, no adds, and nothing for the user to re-approve
/// under a new name.
#[test]
fn a_directly_synced_client_comes_over_as_plain_updates() {
    let sb = Sandbox::new();
    sb.install_cursor(Some(CURSOR_DIRECT));
    sb.claim("cursor", &["github", "linear"]);
    sb.ok(&["add", "--no-sync", "github", "--", "npx", "server-github"]);
    sb.ok(&[
        "add",
        "--no-sync",
        "linear",
        "--url",
        "https://mcp.linear.app/mcp",
    ]);

    let out = sb.ok(&["sync", "--client", "cursor"]);
    assert!(out.contains("its own endpoint on the gateway"), "{out}");
    assert!(out.contains("~ github"), "{out}");
    assert!(out.contains("~ linear"), "{out}");
    assert!(!out.contains("- github"), "{out}");
    assert!(!out.contains("mcpgw\n"), "{out}");

    let entries = sb.cursor_json()["mcpServers"].clone();
    let names: Vec<&str> = entries
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(names, ["mine", "github", "linear"]);
    assert_eq!(entries["github"]["type"], "http");
    assert_eq!(entries["github"]["url"], "http://127.0.0.1:8137/s/github");
    assert!(entries["github"].get("command").is_none());
    assert_eq!(entries["linear"]["url"], "http://127.0.0.1:8137/s/linear");
    assert_eq!(entries["mine"]["command"], "deno");

    let again = sb.ok(&["sync", "--client", "cursor"]);
    assert!(again.contains("no changes"), "{again}");
}

/// The line a user who never asked for a gateway needs to read: their entries
/// moved, nothing about the servers or the tools did, and here is the way
/// back. It fires from `sync` itself because that — not the wizard — is how
/// most existing installs will meet the flip.
#[test]
fn moving_direct_entries_onto_the_gateway_explains_itself_once() {
    let sb = Sandbox::new();
    sb.install_cursor(Some(CURSOR_DIRECT));
    sb.install_claude_desktop();
    std::fs::write(
        sb.claude_desktop_path(),
        r#"{"mcpServers": {"github": {"command": "npx", "args": ["server-github"]}}}"#,
    )
    .unwrap();
    sb.claim_clients(&[
        ("cursor", &["github", "linear"]),
        ("claude-desktop", &["github"]),
    ]);
    sb.ok(&["add", "--no-sync", "github", "--", "npx", "server-github"]);
    sb.ok(&[
        "add",
        "--no-sync",
        "linear",
        "--url",
        "https://mcp.linear.app/mcp",
    ]);

    let out = sb.ok(&["sync", "--client", "cursor"]);
    assert!(
        out.contains("used to point straight at the servers"),
        "{out}"
    );
    assert!(out.contains("same names, same tools"), "{out}");
    assert!(out.contains("mcpgw daemon status"), "{out}");
    assert!(out.contains("mcpgw sync --rollback"), "{out}");

    // Once ever, not once per run: the second client is flipped by a later
    // run and gets the entries without the speech.
    let again = sb.ok(&["sync", "--client", "claude-desktop"]);
    assert!(
        !again.contains("used to point straight at the servers"),
        "{again}"
    );
    assert!(again.contains("~ github"), "{again}");
}

/// Nothing to explain when nothing moved: a client mcpgw sets up from scratch
/// only ever had gateway entries, so the notice would be describing a past it
/// does not have.
#[test]
fn a_fresh_client_never_sees_the_migration_notice() {
    let sb = Sandbox::new();
    sb.install_cursor(None);
    sb.ok(&["add", "--no-sync", "github", "--", "npx", "server-github"]);

    let out = sb.ok(&["sync", "--client", "cursor"]);
    assert!(out.contains("+ github"), "{out}");
    assert!(
        !out.contains("used to point straight at the servers"),
        "{out}"
    );

    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(sb.state.join("managed.json")).unwrap())
            .unwrap();
    // Still unspent: a later run that does move direct entries must be able
    // to say so.
    assert_eq!(state["migrated"], serde_json::json!(false));
}

/// A Cursor config as 0.3.x `sync --aggregate` left it: one `mcpgw` entry
/// pointing at the gateway's `/mcp`, beside an entry of the user's own. A
/// literal, because no mcpgw that writes this shape exists any more — and the
/// bytes are what an install that took the old mode still holds on disk.
const CURSOR_AGGREGATE: &str = r#"{
  "mcpServers": {
    "mine": { "command": "deno" },
    "mcpgw": { "type": "http", "url": "http://127.0.0.1:8137/mcp" }
  }
}"#;

/// The same for Claude Desktop, which took the aggregate over the stdio
/// bridge: `connect` with no `--server`, the shape that meant "the whole
/// gateway".
const CLAUDE_DESKTOP_AGGREGATE: &str = r#"{
  "mcpServers": {
    "mcpgw": {
      "command": "mcpgw",
      "args": ["connect", "--url", "http://127.0.0.1:8137/mcp"]
    }
  }
}"#;

/// The upgrade path off the mode that is gone: a plain `mcpgw sync` — no flag,
/// nothing for the user to know about — converts a config the old aggregate
/// mode wrote into per-server entries, over both entry shapes it could have
/// written. `mcpgw` was managed, so it falls out of the plan as a remove and
/// the server names arrive beside it in the same run.
#[test]
fn a_config_synced_in_the_old_aggregate_mode_converts_to_per_server_entries() {
    let sb = Sandbox::new();
    sb.install_cursor(Some(CURSOR_AGGREGATE));
    sb.install_claude_desktop();
    std::fs::write(sb.claude_desktop_path(), CLAUDE_DESKTOP_AGGREGATE).unwrap();
    sb.claim_clients(&[("cursor", &["mcpgw"]), ("claude-desktop", &["mcpgw"])]);
    sb.ok(&["add", "--no-sync", "github", "--", "npx", "server-github"]);
    sb.ok(&[
        "add",
        "--no-sync",
        "linear",
        "--url",
        "https://mcp.linear.app/mcp",
    ]);

    let out = sb.ok(&["sync", "--client", "cursor", "--client", "claude-desktop"]);
    assert!(out.contains("- mcpgw"), "{out}");
    assert!(out.contains("+ github"), "{out}");
    assert!(out.contains("+ linear"), "{out}");

    let entries = sb.cursor_json()["mcpServers"].clone();
    assert!(entries.get("mcpgw").is_none(), "{entries}");
    assert_eq!(entries["github"]["url"], "http://127.0.0.1:8137/s/github");
    assert_eq!(entries["linear"]["url"], "http://127.0.0.1:8137/s/linear");
    // The entry the user wrote themselves was never mcpgw's to move.
    assert_eq!(entries["mine"]["command"], "deno");

    let bridged = sb.claude_desktop_json()["mcpServers"].clone();
    assert!(bridged.get("mcpgw").is_none(), "{bridged}");
    assert_eq!(
        bridged["github"]["args"],
        serde_json::json!([
            "connect",
            "--server",
            "github",
            "--url",
            "http://127.0.0.1:8137/mcp"
        ])
    );

    // Converted for good: the state file stopped claiming the old name, so
    // the next run has nothing left to say about it.
    let again = sb.ok(&["sync", "--client", "cursor", "--client", "claude-desktop"]);
    assert!(again.contains("no changes"), "{again}");
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(sb.state.join("managed.json")).unwrap())
            .unwrap();
    assert_eq!(
        state["clients"]["cursor"],
        serde_json::json!(["github", "linear"])
    );

    // And there is no way back, because there is no other mode: `--aggregate`
    // is not a flag mcpgw has any more.
    let refused = sb.mcpgw(&["sync", "--client", "cursor", "--aggregate"]);
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert!(stderr.contains("--aggregate"), "{stderr}");
}

#[test]
fn claude_desktop_gets_the_per_server_bridge() {
    let sb = Sandbox::new();
    sb.install_cursor(None);
    sb.install_claude_desktop();
    sb.ok(&["add", "--no-sync", "github", "--", "npx", "server-github"]);
    sb.ok(&["sync", "--gateway-url", "http://127.0.0.1:9000/mcp"]);

    let entry = sb.claude_desktop_json()["mcpServers"]["github"].clone();
    assert_bridge_command(&entry["command"]);
    // The gateway's own URL plus the server's name: the bridge resolves the
    // endpoint, so the file names the server rather than a path shape.
    assert_eq!(
        entry["args"],
        serde_json::json!([
            "connect",
            "--server",
            "github",
            "--url",
            "http://127.0.0.1:9000/mcp"
        ])
    );
    assert!(entry.get("url").is_none());

    assert_eq!(
        sb.cursor_json()["mcpServers"]["github"]["url"],
        "http://127.0.0.1:9000/s/github"
    );
}

/// The user's switch survives the flip: `disabled` is their decision about
/// that server in that client, and mcpgw changing how the server is reached
/// is not a reason to turn it back on.
#[test]
fn per_server_gateway_mode_keeps_the_switch_the_client_owns() {
    let sb = Sandbox::new();
    sb.install_cline(mcpgw_core::ClientKind::Cline, None);
    sb.ok(&["add", "--no-sync", "github", "--", "npx", "server-github"]);
    sb.ok(&["sync", "--client", "cline"]);
    std::fs::write(
        sb.cline_dir(mcpgw_core::ClientKind::Cline)
            .join("cline_mcp_settings.json"),
        CLINE_AFTER_THE_USER,
    )
    .unwrap();

    let out = sb.ok(&["sync", "--client", "cline"]);
    assert!(out.contains("~ github"), "{out}");
    // Read through the codec: the file the user left behind is JSONC, which
    // is exactly why the comment in it has to survive this run.
    let written = mcpgw_core::ClientKind::Cline
        .codec()
        .parse_value(&sb.cline_text(mcpgw_core::ClientKind::Cline))
        .unwrap();
    let entry = written["mcpServers"]["github"].clone();
    assert_eq!(entry["url"], "http://127.0.0.1:8137/s/github");
    assert_eq!(entry["type"], "streamableHttp");
    assert!(entry.get("command").is_none());
    assert_eq!(entry["disabled"], true);
    assert_eq!(entry["autoApprove"], serde_json::json!(["list_issues"]));
    let text = sb.cline_text(mcpgw_core::ClientKind::Cline);
    assert!(
        text.contains("// Switched off from inside Cline, on purpose."),
        "{text}"
    );
}

/// Gemini's exclusion list is reconciled for per-server names exactly as it is
/// in direct mode — same names, same pass, nothing special-cased.
#[test]
fn per_server_gateway_mode_unexcludes_in_gemini() {
    let sb = Sandbox::new();
    sb.install_gemini(Some(
        r#"{ "mcp": { "excluded": ["github", "theirs"] }, "mcpServers": {} }"#,
    ));
    sb.ok(&["add", "--no-sync", "github", "--", "npx", "server-github"]);

    let out = sb.ok(&["sync", "--client", "gemini"]);
    assert!(out.contains("to un-exclude"), "{out}");
    let json = sb.gemini_json();
    assert_eq!(
        json["mcpServers"]["github"]["httpUrl"],
        "http://127.0.0.1:8137/s/github"
    );
    // The user's own exclusion of a name mcpgw does not manage stays put.
    assert_eq!(json["mcp"]["excluded"], serde_json::json!(["theirs"]));
}

/// The other half of the upgrade path: rollback restores whatever the client
/// held before the run that replaced it, and for the first gateway sync of an
/// older install that is the direct entries. A user who wants their old setup
/// back has one command, not a mode.
#[test]
fn rollback_restores_the_direct_entries_the_first_gateway_sync_replaced() {
    let sb = Sandbox::new();
    sb.install_cursor(Some(CURSOR_DIRECT));
    sb.claim("cursor", &["github", "linear"]);
    sb.ok(&["add", "--no-sync", "github", "--", "npx", "server-github"]);
    sb.ok(&[
        "add",
        "--no-sync",
        "linear",
        "--url",
        "https://mcp.linear.app/mcp",
    ]);
    sb.ok(&["sync", "--client", "cursor"]);
    assert_eq!(
        sb.cursor_json()["mcpServers"]["github"]["url"],
        "http://127.0.0.1:8137/s/github"
    );

    sb.ok(&["sync", "--client", "cursor", "--rollback"]);
    let entries = sb.cursor_json()["mcpServers"].clone();
    assert_eq!(entries["github"]["command"], "npx");
    assert!(entries["github"].get("url").is_none());
    assert_eq!(entries["linear"]["url"], "https://mcp.linear.app/mcp");
}

/// A base URL that cannot take an endpoint path is wrong for every client, so
/// it fails the run outright — before the first client file is read, leaving
/// nothing half-written behind.
#[test]
fn a_bad_gateway_url_fails_before_anything_is_written() {
    let sb = Sandbox::new();
    sb.install_cursor(None);
    sb.ok(&["add", "--no-sync", "github", "--", "npx", "server-github"]);

    let out = sb.mcpgw(&["sync", "--gateway-url", "nonsense"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--gateway-url nonsense"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!sb.home.join(".cursor/mcp.json").exists());
    assert!(!sb.state.join("managed.json").exists());
}

#[test]
fn gemini_sync_preserves_the_rest_of_the_settings_file() {
    let sb = Sandbox::new();
    sb.install_gemini(Some(GEMINI_SETTINGS));
    sb.ok(&[
        "add",
        "--no-sync",
        "github",
        "--env",
        "TOKEN=t",
        "--",
        "npx",
        "server-github",
    ]);
    sb.ok(&[
        "add",
        "--no-sync",
        "linear",
        "--url",
        "https://mcp.linear.app/mcp",
    ]);

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
    // Streamable HTTP, not the legacy SSE `url` field — and the server's own
    // command and environment stay behind the gateway.
    assert_eq!(
        json["mcpServers"]["github"]["httpUrl"],
        "http://127.0.0.1:8137/s/github"
    );
    assert!(json["mcpServers"]["github"].get("command").is_none());
    assert!(json["mcpServers"]["github"].get("env").is_none());
    assert_eq!(
        json["mcpServers"]["linear"]["httpUrl"],
        "http://127.0.0.1:8137/s/linear"
    );
    assert!(json["mcpServers"]["linear"].get("url").is_none());

    let again = sb.ok(&["sync", "--client", "gemini"]);
    assert!(again.contains("no changes"), "{again}");
}

#[test]
fn codex_sync_preserves_comments_siblings_and_foreign_entries() {
    let sb = Sandbox::new();
    sb.install_codex(Some(CODEX_CONFIG));
    sb.ok(&[
        "add",
        "--no-sync",
        "github",
        "--env",
        "TOKEN=t",
        "--",
        "npx",
        "server-github",
    ]);
    sb.ok(&[
        "add",
        "--no-sync",
        "linear",
        "--url",
        "https://mcp.linear.app/mcp",
    ]);

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

    assert_eq!(
        toml["mcp_servers"]["github"]["url"],
        "http://127.0.0.1:8137/s/github"
    );
    assert!(toml["mcp_servers"]["github"].get("command").is_none());
    assert!(toml["mcp_servers"]["github"].get("env").is_none());
    assert_eq!(
        toml["mcp_servers"]["linear"]["url"],
        "http://127.0.0.1:8137/s/linear"
    );
    assert!(toml["mcp_servers"]["linear"].get("type").is_none());

    let again = sb.ok(&["sync", "--client", "codex"]);
    assert!(again.contains("no changes"), "{again}");
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
        "--no-sync",
        "github",
        "--env",
        "TOKEN=t",
        "--",
        "npx",
        "server-github",
    ]);
    sb.ok(&[
        "add",
        "--no-sync",
        "linear",
        "--url",
        "https://mcp.linear.app/mcp",
    ]);

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

    // Remote entries, so neither the `command` array nor the `environment`
    // map opencode would run appears at all.
    assert_eq!(json["mcp"]["github"]["type"], "remote");
    assert_eq!(
        json["mcp"]["github"]["url"],
        "http://127.0.0.1:8137/s/github"
    );
    assert!(json["mcp"]["github"].get("command").is_none());
    assert!(json["mcp"]["github"].get("environment").is_none());
    assert_eq!(json["mcp"]["linear"]["type"], "remote");
    assert_eq!(
        json["mcp"]["linear"]["url"],
        "http://127.0.0.1:8137/s/linear"
    );

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
    sb.ok(&["add", "--no-sync", "github", "--", "npx", "server-github"]);
    sb.ok(&["sync", "--client", "opencode"]);
    assert!(sb.opencode_dir().join("opencode.json").is_file());
    assert!(!sb.opencode_dir().join("opencode.jsonc").exists());
    assert_eq!(
        sb.opencode_json("opencode.json")["mcp"]["github"]["url"],
        "http://127.0.0.1:8137/s/github"
    );

    let other = Sandbox::new();
    other.install_opencode(Some(("opencode.jsonc", "// mine\n{}\n")));
    other.ok(&["add", "--no-sync", "github", "--", "npx", "server-github"]);
    other.ok(&["sync", "--client", "opencode"]);
    assert!(!other.opencode_dir().join("opencode.json").exists());
    let text = other.opencode_text("opencode.jsonc");
    assert!(text.contains("// mine"), "{text}");
    assert_eq!(
        other.opencode_json("opencode.jsonc")["mcp"]["github"]["type"],
        "remote"
    );
}

#[test]
fn windsurf_sync_writes_server_url_and_leaves_foreign_entries_alone() {
    let sb = Sandbox::new();
    sb.install_windsurf(Some(WINDSURF_CONFIG));
    sb.ok(&[
        "add",
        "--no-sync",
        "github",
        "--env",
        "TOKEN=t",
        "--",
        "npx",
        "server-github",
    ]);
    sb.ok(&[
        "add",
        "--no-sync",
        "linear",
        "--url",
        "https://mcp.linear.app/mcp",
    ]);

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
    // The remote field Windsurf reads is `serverUrl`; a plain `url` would
    // leave the entry unusable.
    assert_eq!(
        json["mcpServers"]["github"]["serverUrl"],
        "http://127.0.0.1:8137/s/github"
    );
    assert!(json["mcpServers"]["github"].get("command").is_none());
    assert!(json["mcpServers"]["github"].get("env").is_none());
    assert_eq!(
        json["mcpServers"]["linear"]["serverUrl"],
        "http://127.0.0.1:8137/s/linear"
    );
    assert!(json["mcpServers"]["linear"].get("url").is_none());
    assert!(json["mcpServers"]["linear"].get("type").is_none());

    let again = sb.ok(&["sync", "--client", "windsurf"]);
    assert!(again.contains("no changes"), "{again}");
}

#[test]
fn zed_sync_marks_entries_custom_and_leaves_the_rest_of_the_settings_alone() {
    let sb = Sandbox::new();
    sb.install_zed(Some(ZED_SETTINGS));
    sb.ok(&[
        "add",
        "--no-sync",
        "github",
        "--env",
        "TOKEN=t",
        "--",
        "npx",
        "server-github",
    ]);
    sb.ok(&[
        "add",
        "--no-sync",
        "linear",
        "--url",
        "https://mcp.linear.app/mcp",
    ]);

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

    // Both entries point at the gateway, which makes both of them remote —
    // and Zed's remote shape is a bare `{url, headers}`. `source` belongs to
    // the stdio shape it discriminated, so neither carries it.
    assert_eq!(
        json["context_servers"]["github"]["url"],
        "http://127.0.0.1:8137/s/github"
    );
    assert!(json["context_servers"]["github"].get("source").is_none());
    assert!(json["context_servers"]["github"].get("command").is_none());
    assert!(json["context_servers"]["github"].get("env").is_none());
    assert_eq!(
        json["context_servers"]["linear"]["url"],
        "http://127.0.0.1:8137/s/linear"
    );
    assert!(json["context_servers"]["linear"].get("source").is_none());
    assert!(json["context_servers"]["linear"].get("type").is_none());

    let again = sb.ok(&["sync", "--client", "zed"]);
    assert!(again.contains("no changes"), "{again}");
    // A no-op run leaves the file byte for byte as the first one wrote it.
    assert_eq!(sb.zed_text(), text);
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
            "--no-sync",
            "github",
            "--env",
            "TOKEN=t",
            "--",
            "npx",
            "server-github",
        ]);
        sb.ok(&[
            "add",
            "--no-sync",
            "linear",
            "--url",
            "https://mcp.linear.app/mcp",
        ]);

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

        // Without the type Cline would treat the URL as an SSE endpoint.
        assert_eq!(json["mcpServers"]["github"]["type"], "streamableHttp");
        assert_eq!(
            json["mcpServers"]["github"]["url"],
            "http://127.0.0.1:8137/s/github"
        );
        assert!(json["mcpServers"]["github"].get("command").is_none());
        assert!(json["mcpServers"]["github"].get("env").is_none());
        assert_eq!(json["mcpServers"]["linear"]["type"], "streamableHttp");
        assert_eq!(
            json["mcpServers"]["linear"]["url"],
            "http://127.0.0.1:8137/s/linear"
        );

        let text = sb.cline_text(kind);
        let again = sb.ok(&["sync", "--client", kind.id()]);
        assert!(again.contains("no changes"), "{again}");
        assert_eq!(sb.cline_text(kind), text);
    }
}

/// Syncing one surface must not touch the other: they are separate installs,
/// and a user who has both expects both to end up with the server.
#[test]
fn the_two_cline_surfaces_are_synced_independently() {
    let sb = Sandbox::new();
    sb.install_cline(mcpgw_core::ClientKind::Cline, None);
    sb.install_cline(mcpgw_core::ClientKind::ClineCli, None);
    sb.ok(&[
        "add",
        "--no-sync",
        "linear",
        "--url",
        "https://mcp.linear.app/mcp",
    ]);

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
            "http://127.0.0.1:8137/s/linear"
        );
    }
}

#[test]
fn zoo_sync_types_remote_entries_and_keeps_the_roo_extras() {
    let sb = Sandbox::new();
    sb.install_zoo(Some(ZOO_SETTINGS));
    sb.ok(&[
        "add",
        "--no-sync",
        "github",
        "--env",
        "TOKEN=t",
        "--",
        "npx",
        "server-github",
    ]);
    sb.ok(&[
        "add",
        "--no-sync",
        "linear",
        "--url",
        "https://mcp.linear.app/mcp",
    ]);

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

    // The hyphenated spelling is the only one Zoo Code's schema accepts, and
    // without a type at all it would treat the URL as an SSE endpoint.
    assert_eq!(json["mcpServers"]["github"]["type"], "streamable-http");
    assert_eq!(
        json["mcpServers"]["github"]["url"],
        "http://127.0.0.1:8137/s/github"
    );
    assert!(json["mcpServers"]["github"].get("command").is_none());
    assert!(json["mcpServers"]["github"].get("env").is_none());
    assert_eq!(json["mcpServers"]["linear"]["type"], "streamable-http");
    assert_eq!(
        json["mcpServers"]["linear"]["url"],
        "http://127.0.0.1:8137/s/linear"
    );

    let text = sb.zoo_text();
    let again = sb.ok(&["sync", "--client", "zoo"]);
    assert!(again.contains("no changes"), "{again}");
    assert_eq!(sb.zoo_text(), text);
}

/// Zoo Code and Cline share a config lineage but not a file: each writes its
/// own globalStorage dir, and syncing one must not create the other's.
#[test]
fn a_zoo_sync_does_not_write_clines_file() {
    let sb = Sandbox::new();
    sb.install_zoo(None);
    sb.ok(&["add", "--no-sync", "github", "--", "npx", "server-github"]);
    let out = sb.ok(&["sync", "--client", "zoo"]);
    assert!(out.contains("+ github"), "{out}");

    let entry = sb.zoo_json()["mcpServers"]["github"].clone();
    assert_eq!(entry["url"], "http://127.0.0.1:8137/s/github");
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
        "--no-sync",
        "github",
        "--env",
        "TOKEN=t",
        "--",
        "npx",
        "server-github",
    ]);
    sb.ok(&[
        "add",
        "--no-sync",
        "linear",
        "--url",
        "https://mcp.linear.app/mcp",
    ]);

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

    // Amp infers the transport from the URL; it has no `type` field.
    assert_eq!(
        json["amp.mcpServers"]["github"]["url"],
        "http://127.0.0.1:8137/s/github"
    );
    assert!(json["amp.mcpServers"]["github"].get("type").is_none());
    assert!(json["amp.mcpServers"]["github"].get("command").is_none());
    assert!(json["amp.mcpServers"]["github"].get("env").is_none());
    assert_eq!(
        json["amp.mcpServers"]["linear"]["url"],
        "http://127.0.0.1:8137/s/linear"
    );
    assert!(json["amp.mcpServers"]["linear"].get("type").is_none());

    let text = sb.amp_text();
    let again = sb.ok(&["sync", "--client", "amp"]);
    assert!(again.contains("no changes"), "{again}");
    assert_eq!(sb.amp_text(), text);
}

/// Amp's settings file is the whole of the tool's VS Code-style settings, so
/// a comment in it is ordinary. Under the strict-JSON reader that comment
/// made the file unparseable and the client was skipped outright; a file
/// without one was reserialized whole on every write.
#[test]
fn amp_settings_survive_their_own_comments_and_formatting() {
    let sb = Sandbox::new();
    sb.install_amp(Some(AMP_COMMENTED_SETTINGS));
    sb.ok(&[
        "add",
        "--no-sync",
        "linear",
        "--url",
        "https://mcp.linear.app/mcp",
    ]);

    let out = sb.ok(&["sync", "--client", "amp"]);
    assert!(out.contains("+ linear"), "{out}");
    assert!(!out.contains("skipped"), "{out}");

    let text = sb.amp_text();
    assert!(
        text.contains("// My Amp settings — do not reformat."),
        "{text}"
    );
    // The untouched entry keeps its own single-line spelling, which a
    // reserialize-everything write would have expanded.
    assert!(
        text.contains(r#""notes": { "command": "notes-mcp", "disabled": true }"#),
        "{text}"
    );
    let json = mcpgw_core::ClientKind::Amp
        .codec()
        .parse_value(&text)
        .unwrap();
    assert_eq!(
        json["amp.mcpServers"]["linear"]["url"],
        "http://127.0.0.1:8137/s/linear"
    );

    let again = sb.ok(&["sync", "--client", "amp"]);
    assert!(again.contains("no changes"), "{again}");
    assert_eq!(sb.amp_text(), text);
}

/// An entry mcpgw manages is still the user's to switch off from inside
/// Cline. Sync used to write the emitted entry over the whole object, so the
/// server came back on and the auto-approved tool list was gone — and the
/// entry re-diffed on every run after that.
#[test]
fn a_managed_cline_entry_keeps_the_switch_the_user_flipped() {
    let sb = Sandbox::new();
    let kind = mcpgw_core::ClientKind::Cline;
    sb.install_cline(kind, None);
    sb.ok(&["add", "--no-sync", "github", "--", "npx", "server-github"]);
    // A first sync claims `github`, then the user switches it off inside
    // Cline and auto-approves a tool on it.
    sb.ok(&["sync", "--client", kind.id()]);
    std::fs::write(
        sb.cline_dir(kind).join("cline_mcp_settings.json"),
        CLINE_GATEWAY_AFTER_THE_USER,
    )
    .unwrap();

    // Nothing about the entry changed, so nothing is rewritten: the two
    // fields alone must not read as a diff, or the entry churns forever.
    let out = sb.ok(&["sync", "--client", kind.id()]);
    assert!(out.contains("no changes"), "{out}");
    assert_eq!(sb.cline_text(kind), CLINE_GATEWAY_AFTER_THE_USER);

    // Now the entry really does have to be rewritten — the gateway moved to
    // another port — and the rewrite has to carry both fields over.
    let out = sb.ok(&[
        "sync",
        "--client",
        kind.id(),
        "--gateway-url",
        "http://127.0.0.1:9000/mcp",
    ]);
    assert!(out.contains("~ github"), "{out}");

    let text = sb.cline_text(kind);
    assert!(
        text.contains("// Switched off from inside Cline, on purpose."),
        "{text}"
    );
    let json = mcpgw_core::ClientKind::Cline
        .codec()
        .parse_value(&text)
        .unwrap();
    let entry = &json["mcpServers"]["github"];
    // The transport fields win — those are mcpgw's — and Cline's own two
    // survive the rewrite that changed them.
    assert_eq!(entry["url"], "http://127.0.0.1:9000/s/github");
    assert_eq!(entry["disabled"], true);
    assert_eq!(entry["autoApprove"], serde_json::json!(["list_issues"]));

    let again = sb.ok(&[
        "sync",
        "--client",
        kind.id(),
        "--gateway-url",
        "http://127.0.0.1:9000/mcp",
    ]);
    assert!(again.contains("no changes"), "{again}");
    assert_eq!(sb.cline_text(kind), text);
}

/// Gemini refuses to start anything named in `mcp.excluded`, whatever its
/// entry says. Sync used to write the entry, report `+ name` and leave the
/// list alone — and the next run then saw the entry already correct, so the
/// wrong state was stable and invisible.
#[test]
fn gemini_sync_frees_the_servers_it_manages_from_the_excluded_list() {
    let sb = Sandbox::new();
    sb.install_gemini(Some(
        r#"{
  "theme": "Default",
  "mcp": { "excluded": ["github", "notes"] },
  "mcpServers": {
    "notes": { "command": "notes-mcp" }
  }
}"#,
    ));
    sb.ok(&["add", "--no-sync", "github", "--", "npx", "server-github"]);

    let out = sb.ok(&["sync", "--client", "gemini"]);
    assert!(out.contains("+ github"), "{out}");
    assert!(out.contains("to un-exclude"), "{out}");

    let json = sb.gemini_json();
    // Only the name mcpgw manages leaves the list; the user's exclusion of
    // an entry mcpgw does not own is their decision and stays.
    assert_eq!(json["mcp"]["excluded"], serde_json::json!(["notes"]));
    assert_eq!(json["theme"], "Default");
    assert_eq!(
        json["mcpServers"]["github"]["httpUrl"],
        "http://127.0.0.1:8137/s/github"
    );

    let again = sb.ok(&["sync", "--client", "gemini"]);
    assert!(again.contains("no changes"), "{again}");

    // Disabling the server removes its entry, and its name goes with it —
    // left behind it would silently switch off a server re-added by hand.
    sb.ok(&["disable", "--no-sync", "github"]);
    let out = sb.ok(&["sync", "--client", "gemini"]);
    assert!(out.contains("- github"), "{out}");
    assert_eq!(
        sb.gemini_json()["mcp"]["excluded"],
        serde_json::json!(["notes"])
    );
}

/// A root key holding something that is not a server map is what the reader
/// calls a problem. The writer used to replace it without a word, destroying
/// whatever the user had put there.
#[test]
fn a_non_map_root_key_is_refused_rather_than_overwritten() {
    let sb = Sandbox::new();
    sb.install_cursor(Some(r#"{"mcpServers": 5}"#));
    sb.ok(&[
        "add",
        "--no-sync",
        "linear",
        "--url",
        "https://mcp.linear.app/mcp",
    ]);

    let out = sb.ok(&["sync", "--client", "cursor"]);
    assert!(out.contains("refused"), "{out}");
    assert!(out.contains("`mcpServers` is not an object"), "{out}");
    assert_eq!(sb.cursor_json(), serde_json::json!({ "mcpServers": 5 }));
    // Nothing was claimed either, so a later fix syncs as a plain add.
    assert!(!sb.state.join("backups").exists());
}

/// A TOML server map spelled inline is one the reader accepts and sync
/// reports entry by entry — so a write that replaced it deleted foreign
/// entries the CLI had just promised to leave alone.
#[test]
fn an_inline_codex_server_map_keeps_its_foreign_entries() {
    let sb = Sandbox::new();
    sb.install_codex(Some(
        "model = \"gpt-5-codex\"\nmcp_servers = { notes = { command = \"notes-mcp\" } }\n",
    ));
    sb.ok(&["add", "--no-sync", "github", "--", "npx", "server-github"]);

    let out = sb.ok(&["sync", "--client", "codex"]);
    assert!(out.contains("+ github"), "{out}");
    assert!(out.contains("? notes"), "{out}");

    let json = sb.codex_toml();
    assert_eq!(json["model"], "gpt-5-codex");
    assert_eq!(
        json["mcp_servers"]["notes"]["command"],
        "notes-mcp",
        "the foreign entry the CLI called untouched was destroyed:\n{}",
        sb.codex_text()
    );
    assert_eq!(
        json["mcp_servers"]["github"]["url"],
        "http://127.0.0.1:8137/s/github"
    );

    let again = sb.ok(&["sync", "--client", "codex"]);
    assert!(again.contains("no changes"), "{again}");
}

/// The `--client` and `--from` help is generated from `ClientKind::ALL`, so
/// this fails the moment an adapter lands without it — which is what the
/// hand-written list it replaced never did.
#[test]
fn the_client_flags_list_every_shipped_id() {
    let sb = Sandbox::new();
    let sync_help = sb.ok(&["sync", "--help"]);
    let import = sb.ok(&["import", "--help"]);
    for kind in mcpgw_core::ClientKind::ALL {
        assert!(
            sync_help.contains(kind.id()),
            "sync help lost {}:\n{sync_help}",
            kind.id()
        );
        assert!(
            import.contains(kind.id()),
            "import help lost {}:\n{import}",
            kind.id()
        );
    }
}

/// `--gateway` selected a mode, then spent a release accepted and ignored.
/// Nothing selects a mode any more, so the flag is gone with them: a script
/// that still spells it gets told, rather than syncing under a name mcpgw no
/// longer understands.
#[test]
fn the_gateway_flag_is_gone() {
    let sb = Sandbox::new();
    sb.install_cursor(None);
    sb.ok(&["add", "--no-sync", "github", "--", "npx", "server-github"]);

    let refused = sb.mcpgw(&["sync", "--client", "cursor", "--gateway"]);
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert!(stderr.contains("--gateway"), "{stderr}");
    // The error came before any writing.
    assert!(!sb.home.join(".cursor/mcp.json").exists());

    // `--gateway-url` is a different flag and still a flag.
    let help = sb.ok(&["sync", "--help"]);
    assert!(help.contains("--gateway-url"), "{help}");
    assert!(!help.contains("--gateway "), "{help}");
    assert!(!help.contains("aggregate"), "{help}");
}

#[test]
fn a_dry_run_writes_nothing() {
    let sb = Sandbox::new();
    sb.install_cursor(None);
    sb.ok(&["add", "--no-sync", "github", "--", "npx", "server-github"]);
    let out = sb.ok(&["sync", "--client", "cursor", "--dry-run"]);
    assert!(out.contains("+ github"), "{out}");
    assert!(!sb.home.join(".cursor/mcp.json").exists());
}

/// The canonical config a repo-local sync pushes: one server the project
/// files already name, one they do not.
const PROJECT_CANONICAL: &str = r#"
version = 1
[servers.github]
type = "stdio"
command = "cargo"
args = ["run"]
[servers.linear]
type = "http"
url = "https://mcp.linear.app/mcp"
"#;

/// `import --project` first, so the entries the repo already had are mcpgw's
/// to rewrite rather than conflicts it must leave alone.
fn adopted_repo(sb: &Sandbox) -> PathBuf {
    let repo = sb.fake_repo();
    std::fs::write(&sb.config, PROJECT_CANONICAL).unwrap();
    sb.ok_in(&repo, &["import", "--project", "--yes"]);
    // Added after the adoption, so it is an entry mcpgw has never claimed —
    // which is the only kind a sync has to promise not to touch.
    std::fs::write(repo.join(".mcp.json"), PROJECT_CLAUDE_WITH_A_STRANGER).unwrap();
    repo
}

/// Without the flag nothing about a repo-local file changes — the promise
/// that this feature costs nothing to anyone who does not ask for it.
#[test]
fn a_plain_sync_does_not_touch_the_repo() {
    let sb = Sandbox::new();
    let repo = adopted_repo(&sb);
    let before = std::fs::read_to_string(repo.join(".mcp.json")).unwrap();

    let out = sb.ok_in(&repo, &["sync"]);
    assert!(!out.contains(".mcp.json"), "{out}");
    assert_eq!(
        std::fs::read_to_string(repo.join(".mcp.json")).unwrap(),
        before
    );
}

#[test]
fn a_project_dry_run_names_both_files_and_writes_nothing() {
    let sb = Sandbox::new();
    let repo = adopted_repo(&sb);
    let before = std::fs::read_to_string(repo.join(".cursor/mcp.json")).unwrap();

    let out = sb.ok_in(&repo, &["sync", "--project", "--dry-run"]);
    assert!(out.contains(".mcp.json"), "{out}");
    assert!(out.contains(".cursor"), "{out}");
    // `linear` is not in either file yet; `github` is, and moves.
    assert!(out.contains("+ linear"), "{out}");
    assert!(out.contains("~ github"), "{out}");
    assert_eq!(
        std::fs::read_to_string(repo.join(".cursor/mcp.json")).unwrap(),
        before
    );
}

#[test]
fn a_project_sync_writes_the_repo_files_and_stops_there() {
    let sb = Sandbox::new();
    let repo = adopted_repo(&sb);

    sb.ok_in(&repo, &["sync", "--project"]);
    let claude = std::fs::read_to_string(repo.join(".mcp.json")).unwrap();
    let cursor = std::fs::read_to_string(repo.join(".cursor/mcp.json")).unwrap();
    assert!(claude.contains("/s/github"), "{claude}");
    assert!(claude.contains("/s/linear"), "{claude}");
    // Never written by mcpgw, so never touched by it.
    assert!(claude.contains(r#""command": "make""#), "{claude}");
    // The comment is what a teammate wrote; a sync that ate it would be a
    // diff nobody would approve.
    assert!(cursor.contains("// the server we all share"), "{cursor}");

    // The record is keyed by the file, so the client's own per-user config
    // is not claimed along with it.
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(sb.state.join("managed.json")).unwrap())
            .unwrap();
    let files = state["files"].as_object().unwrap();
    assert_eq!(files.len(), 2);
    assert!(
        files
            .keys()
            .any(|path| path.ends_with(".mcp.json") && path.contains("work")),
        "{files:?}"
    );
    assert_eq!(state["clients"], serde_json::json!({}));

    // Nothing left to do, and so nothing written: the second run leaves the
    // repo with nothing to commit.
    let again = sb.ok_in(&repo, &["sync", "--project"]);
    assert!(again.contains("no changes"), "{again}");
    assert_eq!(
        std::fs::read_to_string(repo.join(".mcp.json")).unwrap(),
        claude
    );
    assert_eq!(
        std::fs::read_to_string(repo.join(".cursor/mcp.json")).unwrap(),
        cursor
    );
}

#[test]
fn a_project_rollback_puts_the_repo_files_back() {
    let sb = Sandbox::new();
    let repo = adopted_repo(&sb);
    let before = std::fs::read_to_string(repo.join(".cursor/mcp.json")).unwrap();

    sb.ok_in(&repo, &["sync", "--project"]);
    assert_ne!(
        std::fs::read_to_string(repo.join(".cursor/mcp.json")).unwrap(),
        before
    );

    let out = sb.ok_in(&repo, &["sync", "--project", "--rollback"]);
    assert!(out.contains("restored"), "{out}");
    assert_eq!(
        std::fs::read_to_string(repo.join(".cursor/mcp.json")).unwrap(),
        before
    );
}

/// A directory with nothing to find says so rather than reporting a clean
/// run over an empty set — `--project` was asked for, and silence would read
/// as "done".
#[test]
fn project_sync_outside_a_repo_says_it_found_nothing() {
    let sb = Sandbox::new();
    std::fs::write(&sb.config, PROJECT_CANONICAL).unwrap();
    let empty = sb.home.join("elsewhere");
    std::fs::create_dir_all(&empty).unwrap();

    let out = sb.ok_in(&empty, &["sync", "--project"]);
    assert!(out.contains("no repo-local MCP config here"), "{out}");
}

/// A scoped client's whole sync in one run: the announcement names it, the
/// plan holds only the servers its table gives it, and every entry written
/// carries the tag the gateway reads the scope back out of.
#[test]
fn a_scoped_client_is_written_only_its_own_servers_at_a_tagged_endpoint() {
    let sb = Sandbox::new();
    sb.install_cursor(None);
    sb.ok(&["add", "--no-sync", "github", "--", "npx", "server-github"]);
    sb.ok(&[
        "add",
        "--no-sync",
        "linear",
        "--url",
        "https://mcp.linear.app/mcp",
    ]);
    sb.ok(&["clients", "cursor", "servers", "github"]);

    let out = sb.ok(&["sync", "--client", "cursor", "--dry-run"]);
    assert!(out.contains("scoped by [clients]: cursor"), "{out}");
    // The endpoint, not just the name: the tag is the thing the run is about
    // and a name alone cannot be checked against anything.
    assert!(
        out.contains("+ github → http://127.0.0.1:8137/s/github?client=cursor"),
        "{out}"
    );
    assert!(!out.contains("+ linear"), "{out}");

    sb.ok(&["sync", "--client", "cursor"]);
    let entries = sb.cursor_json()["mcpServers"].clone();
    assert_eq!(
        entries["github"]["url"],
        "http://127.0.0.1:8137/s/github?client=cursor"
    );
    assert!(entries.get("linear").is_none(), "{entries}");

    // Widening the scope again takes the tag back off, so nothing about the
    // entry outlives the table that caused it.
    sb.ok(&["clients", "cursor", "servers", "all"]);
    sb.ok(&["sync", "--client", "cursor"]);
    let entries = sb.cursor_json()["mcpServers"].clone();
    assert_eq!(entries["github"]["url"], "http://127.0.0.1:8137/s/github");
    assert!(entries.get("linear").is_some(), "{entries}");
}

/// The other half of the promise: a client nobody scoped is written the same
/// bytes it was written before scopes existed.
#[test]
fn an_unscoped_client_is_untouched_by_another_clients_scope() {
    let sb = Sandbox::new();
    sb.install_cursor(None);
    sb.ok(&["add", "--no-sync", "github", "--", "npx", "server-github"]);
    sb.ok(&["sync", "--client", "cursor"]);
    let before = std::fs::read_to_string(sb.home.join(".cursor/mcp.json")).unwrap();

    sb.ok(&["clients", "windsurf", "servers", "github"]);
    let out = sb.ok(&["sync", "--client", "cursor"]);
    assert!(out.contains("no changes"), "{out}");
    assert_eq!(
        std::fs::read_to_string(sb.home.join(".cursor/mcp.json")).unwrap(),
        before
    );
}
