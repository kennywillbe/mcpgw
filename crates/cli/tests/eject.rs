//! End-to-end tests for `mcpgw eject`, driven through the real binary.
//!
//! The headline assertion in most of them is a byte comparison against the
//! file a pre-gateway mcpgw wrote: eject claims to put a client back the way
//! it was, and the only honest check of that is that the file it leaves
//! behind is indistinguishable from one mcpgw never pointed at a gateway.

use std::path::{Path, PathBuf};
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

    /// A `mcpgw` pointed at the sandbox and nothing of the real machine.
    ///
    /// The plain `std` command rather than `assert_cmd`'s wrapper: the
    /// prompt test spawns one and drives it while it runs, which needs the
    /// `Child` a wrapper does not hand back.
    fn command(&self) -> std::process::Command {
        let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin("mcpgw"));
        command
            // Hermetic: no test may phone home for a version notice.
            .env("MCPGW_NO_UPDATE_CHECK", "1")
            .env("MCPGW_CONFIG", &self.config)
            .env("MCPGW_STATE_DIR", &self.state)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("APPDATA", self.home.join("AppData"))
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME");
        command
    }

    fn mcpgw(&self, args: &[&str]) -> Output {
        let mut command = self.command();
        command.args(args);
        Command::from_std(command).output().unwrap()
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

    /// A run from a working directory of the test's choosing — the project
    /// steps read the repo the process is standing in.
    fn ok_in(&self, cwd: &Path, args: &[&str]) -> String {
        let mut command = self.command();
        command.current_dir(cwd).args(args);
        let out = Command::from_std(command).output().unwrap();
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

/// A repo carrying one committed server, with the `.git` that makes it one.
fn fake_repo(home: &Path) -> PathBuf {
    let repo = home.join("work/api");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::write(
        repo.join(".mcp.json"),
        "{\n  // ours\n  \"mcpServers\": {\n    \"github\": {\"command\": \"cargo\", \
         \"args\": [\"run\"]}\n  }\n}\n",
    )
    .unwrap();
    repo
}

/// Eject covers a repo-local file without being asked to: whatever `sync
/// --project` wrote is in mcpgw's record, and a committed entry pointing at
/// a gateway nobody runs any more is the leftover eject exists to remove.
#[test]
fn eject_restores_the_repo_files_sync_wrote() {
    let sb = Sandbox::new();
    let repo = fake_repo(&sb.home);
    let path = repo.join(".mcp.json");
    std::fs::write(
        &sb.config,
        "version = 1\n[servers.github]\ntype = \"stdio\"\ncommand = \"cargo\"\nargs = [\"run\"]\n",
    )
    .unwrap();

    sb.ok_in(&repo, &["import", "--project", "--yes"]);
    sb.ok_in(&repo, &["sync", "--project"]);
    let synced = std::fs::read_to_string(&path).unwrap();
    assert!(synced.contains("/s/github"), "{synced}");

    let out = sb.ok_in(&repo, &["eject", "--yes"]);
    assert!(out.contains(".mcp.json"), "{out}");
    let restored = std::fs::read_to_string(&path).unwrap();
    // The definition is the one the repo committed, and the comment around
    // it never moved.
    assert!(restored.contains("// ours"), "{restored}");
    assert!(!restored.contains("/s/github"), "{restored}");
    // Parsed with the comment line dropped: the assertion is about the entry
    // the repo gets back, and `serde_json` is not the reader that has to
    // tolerate JSONC.
    let bare: String = restored
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let value: serde_json::Value = serde_json::from_str(&bare).unwrap();
    assert_eq!(
        value["mcpServers"]["github"],
        serde_json::json!({"command": "cargo", "args": ["run"]})
    );

    // Ejecting again has nothing to do, so it writes nothing at all.
    sb.ok_in(&repo, &["eject", "--yes"]);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), restored);

    // And syncing again lands on exactly the bytes the first sync wrote:
    // eject is a round trip, not a reformat.
    sb.ok_in(&repo, &["sync", "--project"]);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), synced);
}

/// Everything below drives an `eject` that reaches its confirmation
/// question, which it only asks when stdin is a terminal — so the tests hand
/// it one. Unix only: a pty is `openpty` here and a whole other API on
/// Windows, and what is under test (which locks are held while the question
/// waits) is not platform-specific.
#[cfg(unix)]
mod prompted {
    use std::io::{Read as _, Write as _};
    use std::process::{Child, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::{CURSOR_DIRECT, Sandbox};

    /// How long the prompt is waited for before the test calls the run hung.
    /// Generous: it bounds a cold process start on a loaded runner, and only
    /// a genuinely stuck eject still trips it.
    const DEADLINE: Duration = Duration::from_secs(60);

    /// A running `mcpgw eject` with a terminal on its stdin, its stdout
    /// drained into a string the test can read while it is still running.
    struct Run {
        child: Child,
        terminal: std::fs::File,
        printed: Arc<Mutex<String>>,
    }

    impl Run {
        /// Blocks until `text` has been printed, or the run ends or the
        /// deadline passes without it.
        fn wait_for(&mut self, text: &str) {
            let deadline = Instant::now() + DEADLINE;
            loop {
                if self.printed.lock().unwrap().contains(text) {
                    return;
                }
                let exited = self.child.try_wait().unwrap().is_some();
                let printed = self.printed.lock().unwrap().clone();
                assert!(!exited, "eject ended before printing {text:?}:\n{printed}");
                assert!(
                    Instant::now() < deadline,
                    "eject did not print {text:?} within {DEADLINE:?}:\n{printed}"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        /// Types an answer at the terminal, newline and all.
        fn answer(&mut self, line: &str) {
            self.terminal.write_all(line.as_bytes()).unwrap();
            self.terminal.flush().unwrap();
        }

        /// Waits the run out and returns how it ended, with everything it
        /// printed on the way.
        fn finish(self) -> (std::process::Output, String) {
            let output = self.child.wait_with_output().unwrap();
            let printed = self.printed.lock().unwrap().clone();
            (output, printed)
        }
    }

    /// A pty pair: the terminal end the test types at, and the device end a
    /// child gets as its stdin.
    fn openpty() -> (std::fs::File, std::os::fd::OwnedFd) {
        use std::os::fd::FromRawFd as _;

        let mut terminal = -1;
        let mut device = -1;
        // SAFETY: both descriptors are written through valid pointers, and
        // the three optional arguments are null — which `openpty` documents
        // as "take the defaults".
        let rc = unsafe {
            libc::openpty(
                &raw mut terminal,
                &raw mut device,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, 0, "openpty: {}", std::io::Error::last_os_error());
        // SAFETY: `openpty` just returned both, and nothing else owns them.
        unsafe {
            (
                std::fs::File::from_raw_fd(terminal),
                std::os::fd::OwnedFd::from_raw_fd(device),
            )
        }
    }

    /// Starts `mcpgw eject` — no `--yes`, so it asks — on a terminal of the
    /// test's own.
    fn eject_on_a_terminal(sb: &Sandbox) -> Run {
        let (terminal, device) = openpty();
        let mut command = sb.command();
        let mut child = command
            .arg("eject")
            .stdin(Stdio::from(device))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        // Drained on a thread of its own: the test has to read the prompt
        // *while* the run is waiting, and a pipe nobody empties would stall
        // the writer long before the question ever reached it.
        let printed = Arc::new(Mutex::new(String::new()));
        let mut stdout = child.stdout.take().unwrap();
        let collected = Arc::clone(&printed);
        std::thread::spawn(move || {
            let mut buffer = [0u8; 512];
            while let Ok(read) = stdout.read(&mut buffer)
                && read > 0
            {
                collected
                    .lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buffer[..read]));
            }
        });
        Run {
            child,
            terminal,
            printed,
        }
    }

    /// Whether the state lock can be taken right now — what a concurrent
    /// `mcpgw sync` would be doing, except that it would block and a test
    /// that blocked here would hang instead of failing.
    fn state_lock_is_free(sb: &Sandbox) -> bool {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(sb.state.join("managed.json.lock"))
            .unwrap();
        file.try_lock().is_ok()
    }

    /// The prompt is a human's think-time, which is unbounded. Nothing of
    /// mcpgw's may be locked across it: a `mcpgw sync` in the next window
    /// would otherwise block until somebody gets back to the terminal.
    #[test]
    fn the_state_lock_is_free_while_eject_waits_for_an_answer() {
        let sb = Sandbox::new();
        sb.install_cursor(Some(r#"{"mcpServers": {"mine": {"command": "deno"}}}"#));
        sb.add_servers();
        sb.ok(&["sync"]);

        let mut run = eject_on_a_terminal(&sb);
        run.wait_for("restore these clients?");
        assert!(
            state_lock_is_free(&sb),
            "eject held the state lock while its prompt waited for an answer"
        );

        run.answer("y\n");
        let (output, printed) = run.finish();
        assert!(
            output.status.success(),
            "{}\n{printed}",
            String::from_utf8_lossy(&output.stderr)
        );
        // And the answer still did what it was given for.
        assert_eq!(sb.cursor_text(), CURSOR_DIRECT);
    }

    /// The window the released lock opens is the one the re-plan closes: an
    /// edit that lands while the question is open is not overwritten by a
    /// document read before it. The run stops instead, because the plan the
    /// user said yes to is no longer the plan that would be written.
    #[test]
    fn a_client_edited_under_the_prompt_stops_the_run() {
        let sb = Sandbox::new();
        sb.install_cursor(None);
        sb.add_servers();
        sb.ok(&["sync"]);

        let mut run = eject_on_a_terminal(&sb);
        run.wait_for("restore these clients?");
        // Somebody else's write into the file eject planned against.
        let mut entries = sb.cursor_json();
        entries["mcpServers"]["theirs"] = serde_json::json!({"command": "deno"});
        std::fs::write(
            sb.cursor_path(),
            serde_json::to_string_pretty(&entries).unwrap(),
        )
        .unwrap();
        let edited = sb.cursor_text();

        run.answer("y\n");
        let (output, printed) = run.finish();
        assert!(!output.status.success(), "{printed}");
        let err = String::from_utf8(output.stderr).unwrap();
        assert!(
            err.contains("changed while that question was open"),
            "{err}"
        );
        assert!(err.contains("mcpgw eject` again"), "{err}");
        // Untouched: their entry is still there, and the gateway entries the
        // stale plan would have replaced are still the ones on disk.
        assert_eq!(sb.cursor_text(), edited);
    }
}
