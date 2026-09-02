//! End-to-end tests for `mcpgw eject`, driven through the real binary.
//!
//! The headline assertion in most of them is a byte comparison against the
//! file a pre-gateway mcpgw wrote: eject claims to put a client back the way
//! it was, and the only honest check of that is that the file it leaves
//! behind is indistinguishable from one mcpgw never pointed at a gateway.

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

    fn install_cursor(&self, config: Option<&str>) {
        let dir = self.home.join(".cursor");
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(text) = config {
            std::fs::write(dir.join("mcp.json"), text).unwrap();
        }
    }

    fn cursor_path(&self) -> PathBuf {
        self.home.join(".cursor/mcp.json")
    }

    fn cursor_text(&self) -> String {
        std::fs::read_to_string(self.cursor_path()).unwrap()
    }

    fn cursor_json(&self) -> serde_json::Value {
        serde_json::from_str(&self.cursor_text()).unwrap()
    }

    /// The platform-native app-data dir under the sandbox environment.
    fn app_data(&self) -> PathBuf {
        if cfg!(target_os = "macos") {
            self.home.join("Library/Application Support")
        } else if cfg!(windows) {
            self.home.join("AppData")
        } else {
            self.home.join(".config")
        }
    }

    fn claude_desktop_path(&self) -> PathBuf {
        self.app_data().join("Claude/claude_desktop_config.json")
    }

    fn install_claude_desktop(&self) {
        std::fs::create_dir_all(self.claude_desktop_path().parent().unwrap()).unwrap();
    }

    fn claude_desktop_text(&self) -> String {
        std::fs::read_to_string(self.claude_desktop_path()).unwrap()
    }

    fn managed(&self) -> serde_json::Value {
        let text = std::fs::read_to_string(self.state.join("managed.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    /// The two servers every test below configures: one stdio with env, one
    /// remote — the two shapes a restore has to spell differently.
    fn add_servers(&self) {
        self.ok(&[
            "add",
            "github",
            "--env",
            "TOKEN=t",
            "--",
            "npx",
            "-y",
            "server-github",
        ]);
        self.ok(&["add", "linear", "--url", "https://mcp.linear.app/mcp"]);
    }

    /// A Cursor config as 0.3.x `sync --aggregate` left it — one `mcpgw` entry
    /// for the whole gateway, claimed in the state file.
    ///
    /// Written by hand because no mcpgw that writes this shape exists any
    /// more, and eject still has to clear it out of the configs that hold it.
    fn install_legacy_aggregate_cursor(&self) {
        self.install_cursor(Some(
            r#"{"mcpServers": {"mcpgw": {"type": "http", "url": "http://127.0.0.1:8137/mcp"}}}"#,
        ));
        std::fs::create_dir_all(&self.state).unwrap();
        std::fs::write(
            self.state.join("managed.json"),
            r#"{"clients": {"cursor": ["mcpgw"]}}"#,
        )
        .unwrap();
    }
}

/// The Cursor file eject has to reproduce, byte for byte: the two servers of
/// `add_servers` written as the client's own, beside a foreign entry.
///
/// A literal, and captured from the last mcpgw that could write it: sync no
/// longer has a direct mode to generate a reference sandbox with. Pinning the
/// bytes is the point — this is the file a user gets back.
const CURSOR_DIRECT: &str = r#"{
  "mcpServers": {
    "mine": {
      "command": "deno"
    },
    "github": {
      "command": "npx",
      "args": [
        "-y",
        "server-github"
      ],
      "env": {
        "TOKEN": "t"
      }
    },
    "linear": {
      "type": "http",
      "url": "https://mcp.linear.app/mcp"
    }
  }
}
"#;

/// The same for Claude Desktop, which starts empty and has no foreign entry.
const CLAUDE_DESKTOP_DIRECT: &str = r#"{
  "mcpServers": {
    "github": {
      "command": "npx",
      "args": [
        "-y",
        "server-github"
      ],
      "env": {
        "TOKEN": "t"
      }
    },
    "linear": {
      "type": "http",
      "url": "https://mcp.linear.app/mcp"
    }
  }
}
"#;

#[test]
fn eject_restores_the_original_transports_byte_for_byte() {
    let sb = Sandbox::new();
    sb.install_cursor(Some(r#"{"mcpServers": {"mine": {"command": "deno"}}}"#));
    sb.add_servers();
    sb.ok(&["sync"]);
    assert!(sb.cursor_json()["mcpServers"]["github"]["url"].is_string());

    let out = sb.ok(&["eject", "--yes"]);
    assert!(
        out.contains("~ github back to your own definition"),
        "{out}"
    );
    // The foreign entry is named as such and never written.
    assert!(out.contains("? mine (not mine — left untouched)"), "{out}");
    assert!(out.contains("mcpgw is out of the path"), "{out}");

    assert_eq!(sb.cursor_text(), CURSOR_DIRECT);
    assert_eq!(sb.cursor_json()["mcpServers"]["mine"]["command"], "deno");
}

/// The `mcpgw` entry an older release wrote is still eject's to remove: an
/// install that never re-synced after the upgrade must still be able to get
/// out cleanly.
#[test]
fn eject_drops_the_legacy_aggregate_entry_and_puts_the_servers_back() {
    let sb = Sandbox::new();
    sb.install_legacy_aggregate_cursor();
    sb.add_servers();

    let out = sb.ok(&["eject", "--yes"]);
    assert!(out.contains("2 entries restored, 1 removed"), "{out}");
    assert!(
        out.contains("- mcpgw removed (mcpgw put it there)"),
        "{out}"
    );

    let entries = sb.cursor_json()["mcpServers"].clone();
    assert!(entries.get("mcpgw").is_none());
    assert_eq!(entries["github"]["command"], "npx");
    assert_eq!(entries["github"]["env"]["TOKEN"], "t");
    assert_eq!(entries["linear"]["url"], "https://mcp.linear.app/mcp");
}

/// The state file has to end the run describing what is actually in the
/// client, or a user who changes their mind is stuck: the restored entries
/// would read as foreign and sync would refuse to touch them ever again.
#[test]
fn a_later_sync_takes_the_restored_entries_back_over() {
    let sb = Sandbox::new();
    sb.install_legacy_aggregate_cursor();
    sb.add_servers();
    sb.ok(&["eject", "--yes"]);

    assert_eq!(
        sb.managed()["clients"]["cursor"],
        serde_json::json!(["github", "linear"])
    );
    let out = sb.ok(&["sync"]);
    // Updates under the same names: nothing refused as somebody else's, and
    // nothing left over from the aggregate entry eject removed.
    assert!(out.contains("~ github"), "{out}");
    assert!(out.contains("~ linear"), "{out}");
    assert!(!out.contains("! "), "{out}");
    assert!(!out.contains("+ mcpgw"), "{out}");
    assert_eq!(
        sb.cursor_json()["mcpServers"]["github"]["url"],
        "http://127.0.0.1:8137/s/github"
    );
}

/// Eject writes through the same machinery as sync, so it takes the same
/// backup — and the same rollback undoes it.
#[test]
fn rollback_after_eject_brings_the_gateway_entries_back() {
    let sb = Sandbox::new();
    sb.install_cursor(None);
    sb.add_servers();
    sb.ok(&["sync"]);
    let flipped = sb.cursor_text();
    sb.ok(&["eject", "--yes"]);
    assert_ne!(sb.cursor_text(), flipped);

    let out = sb.ok(&["sync", "--rollback"]);
    assert!(out.contains("restored"), "{out}");
    assert_eq!(sb.cursor_text(), flipped);
}

/// Claude Desktop cannot dial an HTTP server, and its original entry could
/// not either. A faithful restore writes the original back regardless — the
/// gap is the direct-mode gap it always was, not something eject invents a
/// fix for.
#[test]
fn a_stdio_only_client_gets_its_original_definitions_back_unchanged() {
    let sb = Sandbox::new();
    sb.install_claude_desktop();
    sb.add_servers();
    sb.ok(&["sync"]);
    sb.ok(&["eject", "--yes"]);

    assert_eq!(sb.claude_desktop_text(), CLAUDE_DESKTOP_DIRECT);
}

#[test]
fn nothing_to_eject_when_mcpgw_never_wrote_anywhere() {
    let sb = Sandbox::new();
    sb.install_cursor(Some(r#"{"mcpServers": {"mine": {"command": "deno"}}}"#));
    sb.add_servers();

    let before = sb.cursor_text();
    let out = sb.ok(&["eject", "--yes"]);
    assert!(out.contains("nothing to eject"), "{out}");
    assert_eq!(sb.cursor_text(), before);
}

/// A client whose config was deleted by hand is reported, not recreated.
#[test]
fn a_hand_deleted_client_config_is_skipped_with_a_note() {
    let sb = Sandbox::new();
    sb.install_cursor(None);
    sb.add_servers();
    sb.ok(&["sync"]);
    std::fs::remove_file(sb.cursor_path()).unwrap();

    let out = sb.ok(&["eject", "--yes"]);
    assert!(out.contains("nothing to restore there"), "{out}");
    assert!(out.contains("nothing to eject"), "{out}");
    assert!(!sb.cursor_path().exists());
}

/// Without the canonical config there are no original definitions to write,
/// and eject says so rather than stripping every managed entry.
#[test]
fn eject_without_a_canonical_config_is_an_actionable_error() {
    let sb = Sandbox::new();
    sb.install_cursor(None);
    sb.add_servers();
    sb.ok(&["sync"]);
    let before = sb.cursor_text();
    std::fs::remove_file(&sb.config).unwrap();

    let out = sb.mcpgw(&["eject", "--yes"]);
    assert!(!out.status.success());
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("eject needs your canonical config"), "{err}");
    assert!(err.contains("mcpgw sync --rollback"), "{err}");
    assert_eq!(sb.cursor_text(), before);
}

/// Off a terminal there is no one to confirm, so the plan prints and the run
/// stops before anything is written.
#[test]
fn eject_refuses_to_write_without_a_confirmation() {
    let sb = Sandbox::new();
    sb.install_cursor(None);
    sb.add_servers();
    sb.ok(&["sync"]);
    let before = sb.cursor_text();

    let out = sb.mcpgw(&["eject"]);
    assert!(!out.status.success());
    let printed = String::from_utf8(out.stdout).unwrap();
    assert!(printed.contains("~ github"), "{printed}");
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("pass --yes"), "{err}");
    assert_eq!(sb.cursor_text(), before);
}

/// The uninstall guidance is the whole point of the closing screen: a user
/// who ejected wants to know what is left and where.
#[test]
fn the_closing_screen_names_everything_eject_does_not_delete() {
    let sb = Sandbox::new();
    sb.install_cursor(None);
    sb.add_servers();
    sb.ok(&["sync"]);

    let out = sb.ok(&["eject", "--yes"]);
    assert!(out.contains(&sb.config.display().to_string()), "{out}");
    assert!(out.contains(&sb.state.display().to_string()), "{out}");
    assert!(out.contains("Nothing of yours was deleted"), "{out}");
    assert!(out.contains("Run `mcpgw` again"), "{out}");
    // No installer has run in the sandbox, so the daemon line is the quiet
    // one rather than a prompt.
    assert!(out.contains("gateway service"), "{out}");
    assert!(sb.config.exists() && sb.state.exists());
}
