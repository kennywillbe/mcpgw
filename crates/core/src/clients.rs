//! Read-side adapters for the MCP client configs mcpgw manages
//! (Claude Desktop, Claude Code, Cursor, VS Code, Gemini CLI, Codex CLI).
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
    pub const ALL: [Self; 6] = [
        Self::ClaudeDesktop,
        Self::ClaudeCode,
        Self::Cursor,
        Self::VsCode,
        Self::Gemini,
        Self::Codex,
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
    /// name but spells entries its own way, and Codex CLI is TOML end to
    /// end — a `[mcp_servers]` table of `snake_case` entries.
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
            _ => Codec {
                format: Format::Json,
                root: RootPath::new(&["mcpServers"]),
                entries: EntrySchema::McpServers,
            },
        }
    }

    /// Resolves the client's MCP config path from the process environment.
    #[must_use]
    pub fn config_path(self) -> Option<PathBuf> {
        self.config_path_with(|key| std::env::var_os(key))
    }

    /// Same as [`ClientKind::config_path`] with an injectable environment.
    #[must_use]
    pub fn config_path_with(self, get: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
        let path = match self {
            Self::ClaudeDesktop => app_data_dir(&get)?.join("Claude/claude_desktop_config.json"),
            Self::ClaudeCode => home_dir(&get)?.join(".claude.json"),
            Self::Cursor => home_dir(&get)?.join(".cursor/mcp.json"),
            Self::VsCode => app_data_dir(&get)?.join("Code/User/mcp.json"),
            // Gemini and Codex are CLIs, so their settings live in the
            // home dir on every platform rather than in the app-data dir.
            Self::Gemini => home_dir(&get)?.join(".gemini/settings.json"),
            Self::Codex => home_dir(&get)?.join(".codex/config.toml"),
        };
        Some(path)
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
        if self != Self::Gemini {
            return;
        }
        // Gemini CLI has no per-server enabled flag: a server is switched
        // off by naming it in the sibling `mcp.excluded` list. The list (and
        // its `mcp.allowed` counterpart) is foreign state mcpgw only reads —
        // disabling a server canonically removes the entry instead.
        let Some(excluded) = root.get("mcp").and_then(|mcp| mcp.get("excluded")) else {
            return;
        };
        let Some(names) = excluded.as_array() else {
            read.problems.push(Problem {
                server: None,
                message: "`mcp.excluded` is not an array".to_owned(),
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
        match get("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            Some(xdg) => Some(PathBuf::from(xdg)),
            None => Some(home_dir(get)?.join(".config")),
        }
    }
}
