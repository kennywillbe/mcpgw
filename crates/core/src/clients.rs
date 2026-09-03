//! Read-side adapters for the MCP client configs mcpgw manages
//! (Claude Desktop, Claude Code, Cursor, VS Code, Gemini CLI, Codex CLI,
//! opencode, Windsurf, Zed, Cline, the Cline CLI, Amp and Zoo Code).
//!
//! Reads are deliberately lenient: one broken entry becomes a [`Problem`],
//! never a file-level failure — `doctor` reports problems, so the reader
//! must survive them. Only an unparseable file fails as a whole.
//!
//! What each client's file looks like lives in [`codec`]; this module owns
//! only what is the same everywhere — detection, paths, and the lenient
//! read loop.

pub mod codec;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::config::Server;
use crate::error::Error;
use codec::{Codec, EntrySchema, Format, RootPath};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientKind {
    ClaudeDesktop,
    ClaudeCode,
    Cursor,
    VsCode,
    Gemini,
    Codex,
    Opencode,
    Windsurf,
    Zed,
    /// Cline's VS Code extension, which keeps its servers in the
    /// extension's own globalStorage directory.
    Cline,
    /// Cline's standalone CLI. A separate kind rather than a second path
    /// candidate on [`ClientKind::Cline`] because the two installs are
    /// genuinely independent: neither reads the other's file and nothing
    /// syncs them (cline/cline#11671). One kind would report whichever file
    /// won the candidate race and hide the other.
    ClineCli,
    Amp,
    /// Zoo Code's VS Code extension, the community successor to Roo Code
    /// (archived May 2026) and so a second-generation fork of Cline's
    /// config lineage: same `mcpServers` map, same `disabled` switch, its
    /// own storage dir and its own remote-type spelling.
    ZooCode,
}

/// Three-state detection: "installed but unconfigured" and "not present"
/// produce different doctor/sync advice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detection {
    NotInstalled,
    Installed,
    Configured(PathBuf),
}

/// A lenient read result: whatever converted cleanly, plus every reported loss.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientRead {
    /// Keys are the client's server names verbatim — they may be invalid as
    /// canonical names; `import` (M7) owns renaming, not the reader.
    pub servers: BTreeMap<String, Server>,
    pub problems: Vec<Problem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// `None` marks a file-level problem.
    pub server: Option<String>,
    pub message: String,
}

impl ClientKind {
    pub const ALL: [Self; 13] = [
        Self::ClaudeDesktop,
        Self::ClaudeCode,
        Self::Cursor,
        Self::VsCode,
        Self::Gemini,
        Self::Codex,
        Self::Opencode,
        Self::Windsurf,
        Self::Zed,
        Self::Cline,
        Self::ClineCli,
        Self::Amp,
        Self::ZooCode,
    ];

    /// Stable machine id used in `--client` filters and the state file.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::ClaudeDesktop => "claude-desktop",
            Self::ClaudeCode => "claude-code",
            Self::Cursor => "cursor",
            Self::VsCode => "vscode",
            Self::Gemini => "gemini",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
            Self::Windsurf => "windsurf",
            Self::Zed => "zed",
            Self::Cline => "cline",
            Self::ClineCli => "cline-cli",
            Self::Amp => "amp",
            Self::ZooCode => "zoo",
        }
    }

    /// Reverse of [`ClientKind::id`].
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.id() == id)
    }

    #[must_use]
    pub fn display_name(self) -> &'static str {
        match self {
            Self::ClaudeDesktop => "Claude Desktop",
            Self::ClaudeCode => "Claude Code",
            Self::Cursor => "Cursor",
            Self::VsCode => "VS Code",
            Self::Gemini => "Gemini CLI",
            Self::Codex => "Codex CLI",
            // Lowercase is the project's own spelling of its name.
            Self::Opencode => "opencode",
            Self::Windsurf => "Windsurf",
            Self::Zed => "Zed",
            Self::Cline => "Cline",
            Self::ClineCli => "Cline CLI",
            Self::Amp => "Amp",
            Self::ZooCode => "Zoo Code",
        }
    }

    /// Whether the client accepts remote-URL entries in its MCP config.
    #[must_use]
    pub fn supports_http_entries(self) -> bool {
        // Claude Desktop only launches local stdio servers — it has no remote
        // URL entry shape at all, so it needs the `mcpgw connect` bridge.
        !matches!(self, Self::ClaudeDesktop)
    }

    /// How this client's config is stored, addressed and shaped.
    ///
    /// Most clients here are plain-JSON `mcpServers` maps. VS Code renames
    /// the map and wants an explicit entry `type`, Gemini CLI keeps the map
    /// name but spells entries its own way, Codex CLI is TOML end to end —
    /// a `[mcp_servers]` table of `snake_case` entries — opencode is
    /// JSONC under a plain `mcp` key, Windsurf keeps the `mcpServers`
    /// rules but spells the remote URL `serverUrl`, Zed keeps its
    /// servers under `context_servers` inside its whole-editor settings,
    /// both Cline surfaces are `mcpServers` with a `disabled` flag and a
    /// camelCase remote `type`, Amp namespaces its map under a single
    /// dotted key, and Zoo Code is Cline's shape with the remote `type`
    /// hyphenated.
    #[must_use]
    pub fn codec(self) -> Codec {
        match self {
            Self::VsCode => Codec {
                format: Format::Json,
                root: RootPath::new(&["servers"]),
                entries: EntrySchema::VsCode,
            },
            Self::Gemini => Codec {
                format: Format::Json,
                root: RootPath::new(&["mcpServers"]),
                entries: EntrySchema::Gemini,
            },
            Self::Codex => Codec {
                format: Format::Toml,
                root: RootPath::new(&["mcp_servers"]),
                entries: EntrySchema::Codex,
            },
            // Comments in an opencode config are ordinary, so this is the
            // first client read and written through the JSONC path.
            Self::Opencode => Codec {
                format: Format::Jsonc,
                root: RootPath::new(&["mcp"]),
                entries: EntrySchema::Opencode,
            },
            Self::Windsurf => Codec {
                format: Format::Json,
                root: RootPath::new(&["mcpServers"]),
                entries: EntrySchema::Windsurf,
            },
            // Zed's settings file is hand-edited and its own parser accepts
            // comments, so it is read and written as JSONC.
            Self::Zed => Codec {
                format: Format::Jsonc,
                root: RootPath::new(&["context_servers"]),
                entries: EntrySchema::Zed,
            },
            // Both Cline surfaces write the same file format under the same
            // name; only the directory differs. JSONC rather than strict
            // JSON: the file is VS Code-shaped, so a `//` comment in it is
            // plausible and under the strict reader would have skipped the
            // client outright.
            Self::Cline | Self::ClineCli => Codec {
                format: Format::Jsonc,
                root: RootPath::new(&["mcpServers"]),
                entries: EntrySchema::Cline,
            },
            // The dot is part of Amp's key: its settings file holds one
            // `"amp.mcpServers"` property, not an `amp` object with a
            // `mcpServers` inside it. One literal segment says exactly that.
            //
            // JSONC for the same reason Zed is: this file is the whole of
            // Amp's VS Code-style settings, hand-edited and commentable, and
            // the strict reader both refused a commented one and reformatted
            // the rest of it on every successful write.
            Self::Amp => Codec {
                format: Format::Jsonc,
                root: RootPath::new(&["amp.mcpServers"]),
                entries: EntrySchema::Amp,
            },
            Self::ZooCode => Codec {
                format: Format::Json,
                root: RootPath::new(&["mcpServers"]),
                entries: EntrySchema::ZooCode,
            },
            // Exhaustive rather than a catch-all: a new client that forgot
            // its codec would otherwise inherit this one silently and write
            // the wrong shape, where a missing arm is a compile error.
            Self::ClaudeDesktop | Self::ClaudeCode | Self::Cursor => Codec {
                format: Format::Json,
                root: RootPath::new(&["mcpServers"]),
                entries: EntrySchema::McpServers,
            },
        }
    }

    /// Where the client keeps a list of server names it refuses to start,
    /// as a path of literal object keys.
    ///
    /// Gemini CLI is the only one: it has no per-entry enabled flag, so a
    /// server is switched off by naming it in `mcp.excluded`. That makes the
    /// list part of a server's state, and a write that ignored it would
    /// report `+ name` for a server Gemini will not run — with the next plan
    /// seeing the entry already correct and nothing left to do.
    #[must_use]
    pub fn exclusion_list(self) -> Option<&'static [&'static str]> {
        match self {
            Self::Gemini => Some(&["mcp", "excluded"]),
            Self::ClaudeDesktop
            | Self::ClaudeCode
            | Self::Cursor
            | Self::VsCode
            | Self::Codex
            | Self::Opencode
            | Self::Windsurf
            | Self::Zed
            | Self::Cline
            | Self::ClineCli
            | Self::Amp
            | Self::ZooCode => None,
        }
    }

    /// Resolves the client's MCP config path from the process environment.
    #[must_use]
    pub fn config_path(self) -> Option<PathBuf> {
        self.config_path_with(|key| std::env::var_os(key))
    }

    /// Same as [`ClientKind::config_path`] with an injectable environment.
    ///
    /// Where a client accepts several filenames this is whichever one exists,
    /// in the order [`ClientKind::config_path_candidates_with`] lists them,
    /// and the first candidate when none does — so the answer is the file a
    /// read would open and the file a write would create.
    #[must_use]
    pub fn config_path_with(self, get: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
        let candidates = self.config_path_candidates_with(&get);
        candidates
            .iter()
            .find(|path| path.is_file())
            .or_else(|| candidates.first())
            .cloned()
    }

    /// Every path this client may keep its MCP config at, most preferred
    /// first.
    ///
    /// One path is the rule. opencode is the exception: `opencode.json` and
    /// `opencode.jsonc` are both first-class there, so a machine may have
    /// either — and one with neither gets the `.json` spelling, which is what
    /// its own docs lead with.
    #[must_use]
    pub fn config_path_candidates_with(
        self,
        get: impl Fn(&str) -> Option<OsString>,
    ) -> Vec<PathBuf> {
        let Some(dir) = (match self {
            Self::ClaudeDesktop => app_data_dir(&get).map(|dir| dir.join("Claude")),
            Self::ClaudeCode => home_dir(&get),
            Self::Cursor => home_dir(&get).map(|dir| dir.join(".cursor")),
            Self::VsCode => app_data_dir(&get).map(|dir| dir.join("Code/User")),
            // Gemini and Codex are CLIs, so their settings live in the
            // home dir on every platform rather than in the app-data dir.
            Self::Gemini => home_dir(&get).map(|dir| dir.join(".gemini")),
            Self::Codex => home_dir(&get).map(|dir| dir.join(".codex")),
            Self::Opencode => xdg_config_dir(&get).map(|dir| dir.join("opencode")),
            // Windsurf is mid-rebrand to Devin, so this path is worth
            // re-checking: it is still `.codeium` today, and a future one
            // becomes another entry in the candidate list above.
            Self::Windsurf => home_dir(&get).map(|dir| dir.join(".codeium/windsurf")),
            Self::Zed => xdg_or_windows_app_data_dir(&get, "zed", "Zed"),
            Self::Cline => cline_extension_dir(&get).map(|dir| dir.join("settings")),
            // Cline's own docs say `~/.cline/mcp.json`; the CLI does not
            // read that path and never has (cline/cline#11671). This is the
            // file it actually loads.
            Self::ClineCli => home_dir(&get).map(|dir| dir.join(".cline/data/settings")),
            Self::Amp => xdg_or_windows_app_data_dir(&get, "amp", "amp"),
            Self::ZooCode => zoo_extension_dir(&get).map(|dir| dir.join("settings")),
        }) else {
            return Vec::new();
        };
        let names: &[&str] = match self {
            Self::ClaudeDesktop => &["claude_desktop_config.json"],
            Self::ClaudeCode => &[".claude.json"],
            Self::Cursor | Self::VsCode => &["mcp.json"],
            // None of these is an MCP file: each is the whole of its
            // tool's settings, so everything outside the server map is
            // foreign state a write has to leave exactly as it found it.
            Self::Gemini | Self::Zed | Self::Amp => &["settings.json"],
            Self::Codex => &["config.toml"],
            Self::Opencode => &["opencode.json", "opencode.jsonc"],
            Self::Windsurf => &["mcp_config.json"],
            Self::Cline | Self::ClineCli => &["cline_mcp_settings.json"],
            Self::ZooCode => &["mcp_settings.json"],
        };
        names.iter().map(|name| dir.join(name)).collect()
    }

    /// Where this client looks for a repo-local MCP config, relative to a
    /// project directory and always spelled with `/`.
    ///
    /// Empty for a client with no documented project-level file — most of
    /// them. A file listed here is read with the same [`ClientKind::codec`]
    /// as the home-dir one, which is what makes the whole feature a path
    /// list rather than a second set of adapters: every client that has both
    /// spells them identically, VS Code's `servers` and Claude Code's
    /// `mcpServers` included.
    ///
    /// Discovery only — [`crate::projects`] reads these and nothing writes
    /// them. `sync` still owns the home-dir file alone.
    #[must_use]
    pub fn project_config_names(self) -> &'static [&'static str] {
        match self {
            Self::ClaudeCode => &[".mcp.json"],
            Self::Cursor => &[".cursor/mcp.json"],
            Self::VsCode => &[".vscode/mcp.json"],
            Self::Gemini => &[".gemini/settings.json"],
            // Both spellings are first-class in a project the same way they
            // are in the config dir.
            Self::Opencode => &["opencode.json", "opencode.jsonc"],
            // Codex loads a project file only for a project the user has
            // marked trusted. Reading one it would ignore is still worth
            // reporting: the file is there, and it is live the moment that
            // trust is given.
            Self::Codex => &[".codex/config.toml"],
            Self::Amp => &[".amp/settings.json"],
            // Zoo Code kept the `.roo` directory of the project it forked
            // rather than renaming it, so this is not a typo.
            Self::ZooCode => &[".roo/mcp.json"],
            // Windsurf, both Cline surfaces and Claude Desktop read MCP
            // servers from one per-user file only; a repo can carry
            // instructions for them, not servers. Zed is here because its
            // project settings are documented without saying whether
            // `context_servers` is one of the keys they may carry — an
            // unverified path would report a file mcpgw cannot vouch for.
            Self::ClaudeDesktop | Self::Windsurf | Self::Zed | Self::Cline | Self::ClineCli => &[],
        }
    }

    /// A path whose existence indicates the client is installed at all,
    /// independent of MCP configuration.
    #[must_use]
    pub fn install_trace_with(self, get: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
        let path = match self {
            Self::ClaudeDesktop => app_data_dir(&get)?.join("Claude"),
            // Claude Code keeps its state dir separate from ~/.claude.json.
            Self::ClaudeCode => home_dir(&get)?.join(".claude"),
            Self::Cursor => home_dir(&get)?.join(".cursor"),
            Self::VsCode => app_data_dir(&get)?.join("Code"),
            Self::Gemini => home_dir(&get)?.join(".gemini"),
            Self::Codex => home_dir(&get)?.join(".codex"),
            Self::Opencode => xdg_config_dir(&get)?.join("opencode"),
            Self::Windsurf => home_dir(&get)?.join(".codeium/windsurf"),
            Self::Zed => xdg_or_windows_app_data_dir(&get, "zed", "Zed")?,
            // The extension's storage dir exists from its first run, which
            // is what tells "Cline installed" apart from "VS Code installed".
            Self::Cline => cline_extension_dir(&get)?,
            Self::ClineCli => home_dir(&get)?.join(".cline"),
            // Amp's own dir, which its CLI and its editor extensions share.
            Self::Amp => xdg_or_windows_app_data_dir(&get, "amp", "amp")?,
            // Same reasoning as Cline: the extension's storage dir exists
            // from its first run, so it tells "Zoo Code installed" apart
            // from "VS Code installed".
            Self::ZooCode => zoo_extension_dir(&get)?,
        };
        Some(path)
    }

    /// Detects the client via the real filesystem and process environment.
    #[must_use]
    pub fn detect(self) -> Detection {
        self.detect_with(|key| std::env::var_os(key))
    }

    /// Same as [`ClientKind::detect`] with an injectable environment
    /// (filesystem checks stay real; tests point the env at a temp dir).
    #[must_use]
    pub fn detect_with(self, get: impl Fn(&str) -> Option<OsString>) -> Detection {
        if let Some(config) = self.config_path_with(&get)
            && config.is_file()
        {
            return Detection::Configured(config);
        }
        if let Some(trace) = self.install_trace_with(&get)
            && trace.exists()
        {
            return Detection::Installed;
        }
        Detection::NotInstalled
    }

    /// Loads and leniently parses the client config file at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotFound`] / [`Error::Io`] for filesystem failures
    /// and [`Error::ClientParse`] when the file is not valid JSON.
    pub fn load(self, path: &Path) -> Result<ClientRead, Error> {
        let text = std::fs::read_to_string(path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                Error::NotFound {
                    path: path.to_owned(),
                }
            } else {
                Error::Io {
                    path: path.to_owned(),
                    source,
                }
            }
        })?;
        self.read_text(&text, path)
    }

    /// Parses client config text. `path` is only used in error messages.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ClientParse`] when the text does not parse in the
    /// client's format or its root is not an object. Broken entries are
    /// collected as problems.
    pub fn read_text(self, text: &str, path: &Path) -> Result<ClientRead, Error> {
        let parse_err = |source| Error::ClientParse {
            client: self.display_name(),
            path: path.to_owned(),
            source: Box::new(source),
        };
        let codec = self.codec();
        let root = codec.parse_value(text).map_err(parse_err)?;
        if !root.is_object() {
            return Err(parse_err(serde::de::Error::custom("root is not an object")));
        }

        let mut read = ClientRead {
            servers: BTreeMap::new(),
            problems: Vec::new(),
        };
        let entries = match codec.root.locate_in(&root) {
            // Absent root key is the normal "no MCP servers yet" state.
            Ok(None) => return Ok(read),
            Ok(Some(entries)) => entries,
            Err(key) => {
                read.problems.push(Problem {
                    server: None,
                    message: format!("`{key}` is not an object"),
                });
                return Ok(read);
            }
        };

        for (name, entry) in entries {
            match codec.entries.parse(entry) {
                Ok((server, note)) => {
                    if let Some(note) = note {
                        read.problems.push(Problem {
                            server: Some(name.clone()),
                            message: note,
                        });
                    }
                    read.servers.insert(name.clone(), server);
                }
                Err(reason) => read.problems.push(Problem {
                    server: Some(name.clone()),
                    message: reason,
                }),
            }
        }
        self.apply_document_context(&root, &mut read);
        Ok(read)
    }

    /// Adjusts a finished read with facts that live outside the entry map.
    ///
    /// Almost everything about an entry is in the entry, which is why the
    /// codec works one entry at a time. A client that keeps part of a
    /// server's state elsewhere in the file needs the whole document, so it
    /// gets this hook rather than a wider codec that every other client
    /// would have to ignore.
    fn apply_document_context(self, root: &serde_json::Value, read: &mut ClientRead) {
        let Some(path) = self.exclusion_list() else {
            return;
        };
        // A server named in the list is off however its entry reads. mcpgw
        // never adds a name to the list — disabling a server canonically
        // removes its entry instead — and only ever takes out names it
        // manages, so the user's own choices there survive a sync.
        let Some(excluded) = value_at(root, path) else {
            return;
        };
        let Some(names) = excluded.as_array() else {
            read.problems.push(Problem {
                server: None,
                message: format!("`{}` is not an array", path.join(".")),
            });
            return;
        };
        for name in names.iter().filter_map(serde_json::Value::as_str) {
            if let Some(server) = read.servers.get_mut(name) {
                server.enabled = false;
            }
        }
    }
}

/// Walks a path of literal object keys, the way [`codec::RootPath`] does for
/// the server map — the key spelling is the client's, so no dot is ever read
/// as a separator.
pub(crate) fn value_at<'a>(
    root: &'a serde_json::Value,
    path: &[&str],
) -> Option<&'a serde_json::Value> {
    path.iter().try_fold(root, |node, key| node.get(*key))
}

fn home_dir(get: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    #[cfg(windows)]
    const HOME: &str = "USERPROFILE";
    #[cfg(not(windows))]
    const HOME: &str = "HOME";
    get(HOME).filter(|v| !v.is_empty()).map(PathBuf::from)
}

// GUI clients keep their config in the platform-native app-data dir (unlike
// mcpgw's own ~/.config choice, these paths are the clients' decision).
fn app_data_dir(get: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        Some(home_dir(get)?.join("Library/Application Support"))
    } else if cfg!(windows) {
        get("APPDATA").filter(|v| !v.is_empty()).map(PathBuf::from)
    } else {
        xdg_config_dir(get)
    }
}

// Zed and Amp are XDG on macOS as well as Linux — their settings are in
// ~/.config/zed and ~/.config/amp on both, *not* in ~/Library/Application
// Support like the GUI clients that go through `app_data_dir`. Windows is the
// one platform where they use the native app-data dir, and Zed capitalizes
// its directory there (`%APPDATA%\Zed`), so the caller passes both spellings
// rather than one to be case-mangled.
fn xdg_or_windows_app_data_dir(
    get: impl Fn(&str) -> Option<OsString>,
    xdg_name: &str,
    windows_name: &str,
) -> Option<PathBuf> {
    if cfg!(windows) {
        get("APPDATA")
            .filter(|v| !v.is_empty())
            .map(|appdata| PathBuf::from(appdata).join(windows_name))
    } else {
        Some(xdg_config_dir(get)?.join(xdg_name))
    }
}

// Cline's VS Code extension stores everything under VS Code's own
// globalStorage, keyed by the extension id — `saoudrizwan.claude-dev`, the id
// it shipped under before the rename, which the marketplace still uses.
fn cline_extension_dir(get: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    Some(app_data_dir(get)?.join("Code/User/globalStorage/saoudrizwan.claude-dev"))
}

// Zoo Code stores everything under VS Code's globalStorage the way Cline
// does, keyed by its marketplace id `ZooCodeOrganization.zoo-code` — folded
// to lower case, because that is how VS Code names a globalStorage directory.
//
// Roo Code, the archived product Zoo Code forked, kept the same layout under
// its own sibling id (`RooVeterinaryInc.roo-cline`). mcpgw does not detect it:
// the product is dead, and offering to sync a client nobody can install would
// be noise. A future read-only legacy import is a second dir here, not a
// change to this one.
fn zoo_extension_dir(get: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    Some(app_data_dir(get)?.join("Code/User/globalStorage/zoocodeorganization.zoo-code"))
}

// The XDG config dir on *every* platform, which is what a client following
// the XDG layout uses even on Windows (%USERPROFILE%\.config\, not %APPDATA%).
fn xdg_config_dir(get: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    match get("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(xdg) => Some(PathBuf::from(xdg)),
        None => Some(home_dir(get)?.join(".config")),
    }
}
