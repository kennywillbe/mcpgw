use std::path::{Path, PathBuf};
use std::process::Output;

use assert_cmd::Command;

mod util;
use util::fixture_binary;

/// A command that really resolves on this machine, JSON-quoted so it can be
/// dropped straight into a client file. Import now brings in anything it
/// cannot find on PATH switched off, so every test about naming, dedupe or
/// adoption has to name a command that is actually there — `npx` is on some
/// runners and not others, which is a coin flip, not a fixture.
fn real_command() -> String {
    serde_json::to_string(&fixture_binary().to_string_lossy()).unwrap()
}

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
            // Hermetic: no test may phone home for a version notice.
            .env("MCPGW_NO_UPDATE_CHECK", "1")
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

    /// The same run from a working directory of the test's choosing —
    /// `--project` reads the repo the process is standing in.
    fn ok_in(&self, cwd: &Path, args: &[&str]) -> String {
        let out = Command::cargo_bin("mcpgw")
            .unwrap()
            .current_dir(cwd)
            .env("MCPGW_NO_UPDATE_CHECK", "1")
            .args(args)
            .env("MCPGW_CONFIG", self.home.join("config.toml"))
            .env("MCPGW_STATE_DIR", self.home.join("state"))
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("APPDATA", self.home.join("AppData"))
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// A repo with a committed `.mcp.json` and a hand-written
    /// `.cursor/mcp.json`, the two files a team most often has.
    fn fake_repo(&self, claude: &str, cursor: &str) -> PathBuf {
        let repo = self.home.join("work/api");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join(".cursor")).unwrap();
        std::fs::write(repo.join(".mcp.json"), claude).unwrap();
        std::fs::write(repo.join(".cursor/mcp.json"), cursor).unwrap();
        repo
    }
}

#[test]
fn imports_dedups_and_renames_across_clients() {
    let sb = Sandbox::new();
    let cmd = real_command();
    sb.write_client(
        ".cursor/mcp.json",
        &format!(
            r#"{{"mcpServers": {{
            "github": {{"command": {cmd}, "args": ["server-github"]}},
            "My Notes": {{"command": {cmd}}}
        }}}}"#
        ),
    );
    sb.write_client(
        ".claude.json",
        &format!(
            r#"{{"mcpServers": {{"github": {{"command": {cmd}, "args": ["server-github"]}}}}}}"#
        ),
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
        &format!(
            r#"{{"mcpServers": {{"github": {{"command": {}, "args": ["server-github"]}}}}}}"#,
            real_command()
        ),
    );
    sb.ok(&["import", "--from", "cursor"]);

    // Adoption must make sync own the entry: it is re-pointed at the gateway
    // under its own name rather than refused as somebody else's.
    let out = sb.ok(&["sync", "--client", "cursor"]);
    assert!(!out.contains("! github"), "{out}");
    assert!(out.contains("~ github"), "{out}");
    let text = std::fs::read_to_string(sb.home.join(".cursor/mcp.json")).unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        json["mcpServers"]["github"]["url"],
        "http://127.0.0.1:8137/s/github"
    );

    let again = sb.ok(&["sync", "--client", "cursor"]);
    assert!(again.contains("no changes"), "{again}");
}

#[test]
fn renamed_import_is_renamed_in_client_by_next_sync() {
    let sb = Sandbox::new();
    sb.write_client(
        ".cursor/mcp.json",
        &format!(
            r#"{{"mcpServers": {{"My Notes": {{"command": {}}}}}}}"#,
            real_command()
        ),
    );
    sb.ok(&["import", "--from", "cursor"]);
    let out = sb.ok(&["sync", "--client", "cursor"]);
    // The adopted original name is replaced by the canonical slug.
    assert!(out.contains("+ my-notes"), "{out}");
    assert!(out.contains("- My Notes"), "{out}");

    let text = std::fs::read_to_string(sb.home.join(".cursor/mcp.json")).unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(json["mcpServers"].get("My Notes").is_none());
    assert_eq!(
        json["mcpServers"]["my-notes"]["url"],
        "http://127.0.0.1:8137/s/my-notes"
    );
}

#[test]
fn piped_conflict_is_skipped_and_canonical_untouched() {
    let sb = Sandbox::new();
    sb.ok(&[
        "add",
        "--no-sync",
        "github",
        "--",
        "npx",
        "canonical-version",
    ]);
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
    sb.ok(&["add", "--no-sync", "github", "--", "npx", "server-github"]);
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
        &format!(
            r#"{{"mcpServers": {{"github": {{"command": {}, "args": ["server-github"]}}}}}}"#,
            real_command()
        ),
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
        &format!(
            r#"{{
            "theme": "Default",
            "mcp": {{ "excluded": ["notes"] }},
            "mcpServers": {{
                "github": {{"command": {}, "args": ["server-github"], "trust": true}},
                "linear": {{"httpUrl": "https://mcp.linear.app/mcp"}},
                "notes": {{"command": "notes-mcp"}}
            }}
        }}"#,
            real_command()
        ),
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
fn import_from_codex_reads_toml_and_flags_managed_auth() {
    let sb = Sandbox::new();
    sb.write_client(
        ".codex/config.toml",
        &format!(
            r#"model = "gpt-5-codex"

[mcp_servers.github]
command = '{}'
args = ["server-github"]
startup_timeout_sec = 20

[mcp_servers.linear]
url = "https://mcp.linear.app/mcp"
http_headers = {{ Authorization = "Bearer t" }}

[mcp_servers.figma]
url = "https://mcp.figma.com/mcp"
auth = "oauth"

[mcp_servers.notes]
command = "notes-mcp"
enabled = false
"#,
            fixture_binary().display()
        ),
    );
    let out = sb.ok(&["import", "--from", "codex"]);
    for name in ["+ github", "+ linear", "+ figma", "+ notes"] {
        assert!(out.contains(name), "{out}");
    }
    // The credential Codex mints for this server cannot come along, so the
    // import has to say so rather than hand over a URL that will 401.
    assert!(out.contains("codex-managed auth not carried over"), "{out}");

    let json: serde_json::Value = serde_json::from_str(&sb.ok(&["list", "--json"])).unwrap();
    assert_eq!(
        json["servers"]["linear"]["url"],
        "https://mcp.linear.app/mcp"
    );
    assert_eq!(json["servers"]["notes"]["enabled"], false);
    assert_ne!(json["servers"]["github"]["enabled"], false);

    // Adoption means the next sync owns the entries rather than conflicting.
    let sync = sb.ok(&["sync", "--client", "codex"]);
    assert!(!sync.contains("! "), "{sync}");
}

#[test]
fn import_from_opencode_reads_jsonc_and_flags_managed_oauth() {
    let sb = Sandbox::new();
    sb.write_client(
        ".config/opencode/opencode.jsonc",
        r#"// My opencode config.
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "context7": {
      "type": "remote",
      "url": "https://mcp.context7.com/mcp",
      "headers": { "Authorization": "Bearer {env:CONTEXT7_KEY}" },
    },
    "github": {
      "type": "local",
      "command": ["npx", "-y", "server-github"],
      "environment": { "TOKEN": "t" },
    },
    "figma": { "type": "remote", "url": "https://mcp.figma.com/mcp", "oauth": {} },
    "notes": { "type": "local", "command": ["notes-mcp"], "enabled": false },
  },
}
"#,
    );
    let out = sb.ok(&["import", "--from", "opencode"]);
    for name in ["+ context7", "+ github", "+ figma", "+ notes"] {
        assert!(out.contains(name), "{out}");
    }
    // The tokens opencode holds for this server cannot come along, so the
    // import has to say so rather than hand over a URL that will 401.
    assert!(
        out.contains("opencode-managed oauth not carried over"),
        "{out}"
    );

    let json: serde_json::Value = serde_json::from_str(&sb.ok(&["list", "--json"])).unwrap();
    assert_eq!(
        json["servers"]["context7"]["url"],
        "https://mcp.context7.com/mcp"
    );
    // opencode's own interpolation survives unexpanded — `list` masks header
    // values, so the canonical file is where that is visible.
    let canonical = std::fs::read_to_string(sb.home.join("config.toml")).unwrap();
    assert!(
        canonical.contains("Bearer {env:CONTEXT7_KEY}"),
        "{canonical}"
    );
    assert_eq!(json["servers"]["github"]["command"], "npx");
    assert_eq!(json["servers"]["github"]["args"][1], "server-github");
    assert_eq!(json["servers"]["notes"]["enabled"], false);

    // Adoption means the next sync owns the entries rather than conflicting.
    let sync = sb.ok(&["sync", "--client", "opencode"]);
    assert!(!sync.contains("! "), "{sync}");
}

#[test]
fn import_from_windsurf_reads_both_remote_spellings() {
    let sb = Sandbox::new();
    sb.write_client(
        ".codeium/windsurf/mcp_config.json",
        r#"{"mcpServers": {
            "github": {"command": "npx", "args": ["server-github"],
                       "env": {"TOKEN": "${env:GITHUB_TOKEN}"}},
            "linear": {"serverUrl": "https://mcp.linear.app/mcp"},
            "figma": {"url": "https://mcp.figma.com/mcp"}
        }}"#,
    );
    let out = sb.ok(&["import", "--from", "windsurf"]);
    for name in ["+ github", "+ linear", "+ figma"] {
        assert!(out.contains(name), "{out}");
    }

    let json: serde_json::Value = serde_json::from_str(&sb.ok(&["list", "--json"])).unwrap();
    assert_eq!(
        json["servers"]["linear"]["url"],
        "https://mcp.linear.app/mcp"
    );
    assert_eq!(json["servers"]["figma"]["url"], "https://mcp.figma.com/mcp");
    // Windsurf's own interpolation survives unexpanded.
    let canonical = std::fs::read_to_string(sb.home.join("config.toml")).unwrap();
    assert!(canonical.contains("${env:GITHUB_TOKEN}"), "{canonical}");

    // Adoption means the next sync owns the entries rather than conflicting.
    let sync = sb.ok(&["sync", "--client", "windsurf"]);
    assert!(!sync.contains("! "), "{sync}");
}

#[test]
fn import_from_zed_reads_every_source_out_of_the_editor_settings() {
    let sb = Sandbox::new();
    // Zed is XDG on macOS as well as Linux; Windows is the exception.
    let rel = if cfg!(windows) {
        "AppData/Zed/settings.json"
    } else {
        ".config/zed/settings.json"
    };
    sb.write_client(
        rel,
        r#"// mine
        {
            "theme": "One Dark",
            "context_servers": {
                "github": {"source": "custom", "command": "npx",
                           "args": ["server-github"]},
                "postgres": {"source": "extension",
                             "command": "postgres-context-server"},
                "linear": {"source": "custom", "url": "https://mcp.linear.app/mcp"}
            }
        }"#,
    );
    let out = sb.ok(&["import", "--from", "zed"]);
    for name in ["+ github", "+ postgres", "+ linear"] {
        assert!(out.contains(name), "{out}");
    }

    let json: serde_json::Value = serde_json::from_str(&sb.ok(&["list", "--json"])).unwrap();
    assert_eq!(
        json["servers"]["linear"]["url"],
        "https://mcp.linear.app/mcp"
    );
    // `source` is Zed's own bookkeeping and has no canonical counterpart.
    let canonical = std::fs::read_to_string(sb.home.join("config.toml")).unwrap();
    assert!(!canonical.contains("source ="), "{canonical}");

    // Adoption means the next sync owns the entries rather than conflicting.
    let sync = sb.ok(&["sync", "--client", "zed"]);
    assert!(!sync.contains("! "), "{sync}");
}

/// The extension's own path, which is inside VS Code's globalStorage.
fn cline_extension_rel() -> &'static str {
    if cfg!(target_os = "macos") {
        "Library/Application Support/Code/User/globalStorage/saoudrizwan.claude-dev/settings/cline_mcp_settings.json"
    } else if cfg!(windows) {
        "AppData/Code/User/globalStorage/saoudrizwan.claude-dev/settings/cline_mcp_settings.json"
    } else {
        ".config/Code/User/globalStorage/saoudrizwan.claude-dev/settings/cline_mcp_settings.json"
    }
}

#[test]
fn import_from_cline_adopts_disabled_and_auto_approved_entries() {
    let sb = Sandbox::new();
    sb.write_client(
        cline_extension_rel(),
        r#"{"mcpServers": {
            "github": {"command": "npx", "args": ["server-github"],
                       "autoApprove": ["search_repositories"]},
            "browser": {"command": "browser-mcp", "disabled": true},
            "linear": {"url": "https://mcp.linear.app/mcp", "type": "streamableHttp"}
        }}"#,
    );
    let out = sb.ok(&["import", "--from", "cline"]);
    for name in ["+ github", "+ browser", "+ linear"] {
        assert!(out.contains(name), "{out}");
    }

    let json: serde_json::Value = serde_json::from_str(&sb.ok(&["list", "--json"])).unwrap();
    assert_eq!(
        json["servers"]["linear"]["url"],
        "https://mcp.linear.app/mcp"
    );
    // `disabled: true` is the inverse of the canonical flag.
    assert_eq!(json["servers"]["browser"]["enabled"], false);
    // `autoApprove` is Cline's own and has no canonical counterpart.
    let canonical = std::fs::read_to_string(sb.home.join("config.toml")).unwrap();
    assert!(!canonical.contains("autoApprove"), "{canonical}");

    // Adoption means the next sync owns the entries rather than conflicting.
    let sync = sb.ok(&["sync", "--client", "cline"]);
    assert!(!sync.contains("! "), "{sync}");
}

/// Cline's extension and its CLI read different files that nothing keeps in
/// step, which is why they are two clients — and why importing from a machine
/// with both has to land one canonical server carrying both origins rather
/// than two copies of it.
#[test]
fn import_dedupes_a_server_the_user_has_on_both_cline_surfaces() {
    let sb = Sandbox::new();
    let entry = format!(
        r#"{{"mcpServers": {{
        "github": {{"command": {}, "args": ["server-github"]}}
    }}}}"#,
        real_command()
    );
    sb.write_client(cline_extension_rel(), &entry);
    sb.write_client(".cline/data/settings/cline_mcp_settings.json", &entry);

    let out = sb.ok(&["import"]);
    assert!(
        out.contains("+ github (from cline, cline-cli)")
            || out.contains("+ github (from cline-cli, cline)"),
        "{out}"
    );

    let json: serde_json::Value = serde_json::from_str(&sb.ok(&["list", "--json"])).unwrap();
    assert_eq!(json["servers"].as_object().unwrap().len(), 1);
    // One import adopts the entry on both surfaces, so neither conflicts.
    for id in ["cline", "cline-cli"] {
        let sync = sb.ok(&["sync", "--client", id]);
        assert!(!sync.contains("! "), "{id}: {sync}");
    }
}

/// Zoo Code's own path, a sibling of Cline's inside VS Code's globalStorage.
fn zoo_extension_rel() -> &'static str {
    if cfg!(target_os = "macos") {
        "Library/Application Support/Code/User/globalStorage/zoocodeorganization.zoo-code/settings/mcp_settings.json"
    } else if cfg!(windows) {
        "AppData/Code/User/globalStorage/zoocodeorganization.zoo-code/settings/mcp_settings.json"
    } else {
        ".config/Code/User/globalStorage/zoocodeorganization.zoo-code/settings/mcp_settings.json"
    }
}

#[test]
fn import_from_zoo_adopts_entries_carrying_the_roo_extras() {
    let sb = Sandbox::new();
    sb.write_client(
        zoo_extension_rel(),
        r#"{"mcpServers": {
            "github": {"command": "npx", "args": ["server-github"],
                       "cwd": "/srv", "timeout": 60,
                       "watchPaths": ["/srv/dist/index.js"],
                       "alwaysAllow": ["search_repositories"],
                       "disabledTools": ["delete_repository"]},
            "browser": {"command": "browser-mcp", "disabled": true},
            "linear": {"url": "https://mcp.linear.app/mcp", "type": "streamable-http"},
            "inherited": {"url": "https://inherited.example/mcp", "type": "streamableHttp"}
        }}"#,
    );
    let out = sb.ok(&["import", "--from", "zoo"]);
    for name in ["+ github", "+ browser", "+ linear", "+ inherited"] {
        assert!(out.contains(name), "{out}");
    }

    let json: serde_json::Value = serde_json::from_str(&sb.ok(&["list", "--json"])).unwrap();
    assert_eq!(
        json["servers"]["linear"]["url"],
        "https://mcp.linear.app/mcp"
    );
    // The camelCase spelling Zoo Code inherited from Cline is the same
    // transport, so an entry carried over from an older install imports too.
    assert_eq!(
        json["servers"]["inherited"]["url"],
        "https://inherited.example/mcp"
    );
    // `disabled: true` is the inverse of the canonical flag.
    assert_eq!(json["servers"]["browser"]["enabled"], false);
    // None of Zoo Code's own bookkeeping has a canonical counterpart.
    let canonical = std::fs::read_to_string(sb.home.join("config.toml")).unwrap();
    for extra in ["alwaysAllow", "disabledTools", "watchPaths"] {
        assert!(!canonical.contains(extra), "{canonical}");
    }

    // Adoption means the next sync owns the entries rather than conflicting.
    let sync = sb.ok(&["sync", "--client", "zoo"]);
    assert!(!sync.contains("! "), "{sync}");
}

/// Amp's settings file under the sandbox environment; XDG on macOS and
/// Linux, the app-data dir only on Windows.
fn amp_settings_rel() -> &'static str {
    if cfg!(windows) {
        "AppData/amp/settings.json"
    } else {
        ".config/amp/settings.json"
    }
}

#[test]
fn import_from_amp_reads_the_namespaced_key_only() {
    let sb = Sandbox::new();
    sb.write_client(
        amp_settings_rel(),
        r#"{
            "amp.notifications.enabled": true,
            "amp.mcpServers": {
                "playwright": {"command": "npx", "args": ["-y", "@playwright/mcp@latest"]},
                "browser": {"command": "browser-mcp", "disabled": true},
                "linear": {"url": "https://mcp.linear.app/sse"}
            },
            "amp": {"mcpServers": {"decoy": {"command": "never-read-me"}}}
        }"#,
    );
    let out = sb.ok(&["import", "--from", "amp"]);
    for name in ["+ playwright", "+ browser", "+ linear"] {
        assert!(out.contains(name), "{out}");
    }
    // The dot belongs to the key, so the nested object is another property.
    assert!(!out.contains("decoy"), "{out}");

    let json: serde_json::Value = serde_json::from_str(&sb.ok(&["list", "--json"])).unwrap();
    assert_eq!(json["servers"].as_object().unwrap().len(), 3);
    assert_eq!(
        json["servers"]["linear"]["url"],
        "https://mcp.linear.app/sse"
    );
    // `disabled: true` is the inverse of the canonical flag.
    assert_eq!(json["servers"]["browser"]["enabled"], false);

    // Adoption means the next sync owns the entries rather than conflicting.
    let sync = sb.ok(&["sync", "--client", "amp"]);
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
fn yes_skips_the_conflict_and_keeps_canonical() {
    let sb = Sandbox::new();
    sb.ok(&[
        "add",
        "--no-sync",
        "github",
        "--",
        "npx",
        "canonical-version",
    ]);
    sb.write_client(
        ".cursor/mcp.json",
        r#"{"mcpServers": {
            "github": {"command": "npx", "args": ["client-version"]},
            "linear": {"command": "linear-mcp"}
        }}"#,
    );

    let out = sb.ok(&["import", "--from", "cursor", "--yes"]);
    assert!(
        out.contains("! github differs from the canonical entry (skipped — --yes keeps canonical)"),
        "{out}"
    );
    // The rest of the run must still land: --yes resolves the conflict, it
    // does not abandon the import.
    assert!(out.contains("+ linear"), "{out}");
    assert!(
        out.contains("imported 1, already present 0, skipped 1"),
        "{out}"
    );

    let list = sb.ok(&["list", "--json"]);
    let json: serde_json::Value = serde_json::from_str(&list).unwrap();
    assert_eq!(json["servers"]["github"]["args"][0], "canonical-version");
    assert_eq!(json["servers"]["linear"]["command"], "linear-mcp");
}

#[test]
fn yes_without_conflicts_matches_a_plain_import() {
    let plain = Sandbox::new();
    let yes = Sandbox::new();
    for sb in [&plain, &yes] {
        sb.write_client(
            ".cursor/mcp.json",
            r#"{"mcpServers": {
                "github": {"command": "npx", "args": ["server-github"]},
                "My Notes": {"command": "notes-mcp"}
            }}"#,
        );
    }

    let plain_out = plain.ok(&["import", "--from", "cursor"]);
    let yes_out = yes.ok(&["import", "--from", "cursor", "--yes"]);
    assert_eq!(plain_out, yes_out);
    assert_eq!(
        std::fs::read_to_string(plain.home.join("config.toml")).unwrap(),
        std::fs::read_to_string(yes.home.join("config.toml")).unwrap()
    );
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

/// A stdio entry pointing at something that is not on this machine — the
/// Codex CLI carries one for an app that may not be installed — is kept, but
/// switched off, so it reaches neither the gateway nor any client. The line
/// has to name the command that turns it back on.
#[test]
fn an_unresolvable_command_is_imported_disabled_and_says_so() {
    let sb = Sandbox::new();
    sb.write_client(
        ".cursor/mcp.json",
        r#"{"mcpServers": {
            "node_repl": {"command": "/Applications/Gone.app/cua_node/bin/node_repl"}
        }}"#,
    );

    let out = sb.ok(&["import", "--from", "cursor"]);
    assert!(
        out.contains(
            "command not found on this machine, importing disabled \
             (enable later: mcpgw toggle node_repl)"
        ),
        "{out}"
    );

    let json: serde_json::Value = serde_json::from_str(&sb.ok(&["list", "--json"])).unwrap();
    assert_eq!(json["servers"]["node_repl"]["enabled"], false);

    // The point of importing it off: sync has nothing to push, so the entry
    // cannot turn into a failure in every client.
    let sync = sb.ok(&["sync", "--client", "cursor"]);
    assert!(!sync.contains("+ node_repl"), "{sync}");
}

/// Off a terminal there is nobody to ask, so both copies are kept exactly as
/// before — but the observation is printed, because otherwise the user meets
/// `context7-2` with nothing to explain it.
#[test]
fn a_piped_run_explains_a_shared_address_and_keeps_both() {
    let sb = Sandbox::new();
    sb.ok(&[
        "add",
        "--no-sync",
        "ctx",
        "--url",
        "https://mcp.context7.com/mcp",
        "--header",
        "Authorization=Bearer canonical-secret",
    ]);
    sb.write_client(
        ".cursor/mcp.json",
        r#"{"mcpServers": {"context7": {"type": "http", "url": "https://mcp.context7.com/mcp",
            "headers": {"Authorization": "Bearer client-secret"}}}}"#,
    );

    let out = sb.ok(&["import", "--from", "cursor"]);
    assert!(
        out.contains(
            "context7 in Cursor points at the same address as your existing ctx, \
             with different credentials — probably the same server."
        ),
        "{out}"
    );
    assert!(out.contains("+ context7"), "{out}");
    // Never the values, on any surface: this transcript ends up in bug
    // reports.
    assert!(!out.contains("secret"), "{out}");

    let json: serde_json::Value = serde_json::from_str(&sb.ok(&["list", "--json"])).unwrap();
    assert!(json["servers"]["ctx"].is_object());
    assert!(json["servers"]["context7"].is_object());
}

/// The third outcome #82 added is offered, not taken: a run that cannot ask
/// still keeps the canonical entry, and must not invent the second copy on
/// the user's behalf.
#[test]
fn a_run_that_cannot_ask_never_writes_the_second_copy() {
    let sb = Sandbox::new();
    sb.ok(&[
        "add",
        "--no-sync",
        "github",
        "--",
        "npx",
        "canonical-version",
    ]);
    sb.write_client(
        ".cursor/mcp.json",
        r#"{"mcpServers": {"github": {"command": "npx", "args": ["client-version"]}}}"#,
    );

    let out = sb.ok(&["import", "--from", "cursor"]);
    assert!(
        out.contains("! github differs from the canonical entry (skipped — not a terminal, keeping canonical)"),
        "{out}"
    );

    let list = sb.ok(&["list", "--json"]);
    let json: serde_json::Value = serde_json::from_str(&list).unwrap();
    assert_eq!(json["servers"]["github"]["args"][0], "canonical-version");
    assert!(json["servers"].get("github-2").is_none(), "{list}");
}

/// A dry run is where someone finds out that a conflict has three answers and
/// what the third one would be called — the interactive run is the only place
/// they can be given, and they have to know it is worth starting one.
#[test]
fn dry_run_names_all_three_outcomes_and_the_second_name() {
    let sb = Sandbox::new();
    sb.ok(&[
        "add",
        "--no-sync",
        "github",
        "--",
        "npx",
        "canonical-version",
    ]);
    sb.write_client(
        ".cursor/mcp.json",
        r#"{"mcpServers": {"github": {"command": "npx", "args": ["client-version"]}}}"#,
    );

    let out = sb.ok(&["import", "--from", "cursor", "--dry-run"]);
    assert!(out.contains("keep both as github-2"), "{out}");
    assert!(out.contains("overwrite it"), "{out}");
}

/// The whole point of `--project`: the servers a team committed become
/// canonical servers, adopted per file so a later sync owns those entries.
#[test]
fn project_files_are_read_and_adopted_per_file() {
    let sb = Sandbox::new();
    let cmd = real_command();
    let repo = sb.fake_repo(
        &format!(r#"{{"mcpServers": {{"shared": {{"command": {cmd}}}}}}}"#),
        &format!(
            "{{\n  // ours\n  \"mcpServers\": {{\n    \"shared\": {{\"command\": {cmd}}},\n    \
             \"notes\": {{\"command\": {cmd}, \"args\": [\"notes\"]}},\n  }}\n}}"
        ),
    );

    let out = sb.ok_in(&repo, &["import", "--project", "--yes"]);
    // The origin names the file, because "from cursor" would be the same
    // words for the repo's file and the user's own.
    assert!(out.contains(".cursor"), "{out}");
    assert!(out.contains("+ shared"), "{out}");
    assert!(out.contains("+ notes"), "{out}");

    let list: serde_json::Value = serde_json::from_str(&sb.ok(&["list", "--json"])).unwrap();
    let names: Vec<&String> = list["servers"].as_object().unwrap().keys().collect();
    assert_eq!(names, vec!["notes", "shared"]);

    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(sb.home.join("state/managed.json")).unwrap())
            .unwrap();
    // One record per file, and none for either client's per-user config —
    // mcpgw has not written those and must not claim them.
    let files = state["files"].as_object().unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(state["clients"], serde_json::json!({}));
    let cursor = files
        .iter()
        .find(|(path, _)| path.contains(".cursor"))
        .unwrap()
        .1;
    assert_eq!(cursor["client"], "cursor");
    assert_eq!(cursor["managed"], serde_json::json!(["notes", "shared"]));
}

/// Without the flag the repo is not read at all, whatever the process is
/// standing in.
#[test]
fn a_plain_import_ignores_the_repo() {
    let sb = Sandbox::new();
    let cmd = real_command();
    let repo = sb.fake_repo(
        &format!(r#"{{"mcpServers": {{"shared": {{"command": {cmd}}}}}}}"#),
        r#"{"mcpServers": {}}"#,
    );

    let out = sb.ok_in(&repo, &["import"]);
    assert!(out.contains("nothing to import"), "{out}");
}

/// Claude Code's `headersHelper` is the one credential field a client has
/// that mcpgw can carry over whole, because it is a command rather than a
/// token. A server that used it before the gateway still works behind it.
#[test]
fn a_claude_code_headers_helper_becomes_a_headers_command() {
    let sb = Sandbox::new();
    sb.write_client(
        ".claude.json",
        r#"{"mcpServers": {
            "corp": {
                "type": "http",
                "url": "https://mcp.corp.example/mcp",
                "headersHelper": "corp-auth print-mcp-headers"
            }
        }}"#,
    );
    let out = sb.ok(&["import", "--from", "claude-code"]);
    assert!(out.contains("corp"), "{out}");
    // Nothing about this entry is lossy, so it must not be flagged the way a
    // client-held OAuth token is.
    assert!(!out.contains("not carried over"), "{out}");

    let text = std::fs::read_to_string(sb.home.join("config.toml")).unwrap();
    assert!(
        text.contains(r#"headers_command = ["corp-auth", "print-mcp-headers"]"#),
        "{text}"
    );
}
