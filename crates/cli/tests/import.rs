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
fn import_from_codex_reads_toml_and_flags_managed_auth() {
    let sb = Sandbox::new();
    sb.write_client(
        ".codex/config.toml",
        r#"model = "gpt-5-codex"

[mcp_servers.github]
command = "npx"
args = ["server-github"]
startup_timeout_sec = 20

[mcp_servers.linear]
url = "https://mcp.linear.app/mcp"
http_headers = { Authorization = "Bearer t" }

[mcp_servers.figma]
url = "https://mcp.figma.com/mcp"
auth = "oauth"

[mcp_servers.notes]
command = "notes-mcp"
enabled = false
"#,
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
    let entry = r#"{"mcpServers": {
        "github": {"command": "npx", "args": ["server-github"]}
    }}"#;
    sb.write_client(cline_extension_rel(), entry);
    sb.write_client(".cline/data/settings/cline_mcp_settings.json", entry);

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
