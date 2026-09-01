//! Read-side adapters for the MCP client configs mcpgw manages
//! (Claude Desktop, Claude Code, Cursor, VS Code).
//!
//! Reads are deliberately lenient: one broken entry becomes a [`Problem`],
//! never a file-level failure — `doctor` reports problems, so the reader
//! must survive them. Only unparseable JSON fails the whole file.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::config::{Server, Transport};
use crate::error::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientKind {
    ClaudeDesktop,
    ClaudeCode,
    Cursor,
    VsCode,
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
    pub const ALL: [Self; 4] = [
        Self::ClaudeDesktop,
        Self::ClaudeCode,
        Self::Cursor,
        Self::VsCode,
    ];

    /// Stable machine id used in `--client` filters and the state file.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::ClaudeDesktop => "claude-desktop",
            Self::ClaudeCode => "claude-code",
            Self::Cursor => "cursor",
            Self::VsCode => "vscode",
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
        }
    }

    /// Whether the client accepts remote-URL entries in its MCP config.
    #[must_use]
    pub fn supports_http_entries(self) -> bool {
        // Claude Desktop only launches local stdio servers — it has no remote
        // URL entry shape at all, so it needs the `mcpgw connect` bridge.
        !matches!(self, Self::ClaudeDesktop)
    }

    /// The JSON key holding the server map (`servers` is the VS Code outlier).
    #[must_use]
    pub fn root_key(self) -> &'static str {
        match self {
            Self::VsCode => "servers",
            _ => "mcpServers",
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
    /// Returns [`Error::ClientParse`] when the text is not valid JSON or its
    /// root is not an object. Broken entries are collected as problems.
    pub fn read_text(self, text: &str, path: &Path) -> Result<ClientRead, Error> {
        let parse_err = |source| Error::ClientParse {
            client: self.display_name(),
            path: path.to_owned(),
            source,
        };
        let root: serde_json::Value = serde_json::from_str(text).map_err(parse_err)?;
        let Some(root) = root.as_object() else {
            return Err(parse_err(serde::de::Error::custom("root is not an object")));
        };

        let mut read = ClientRead {
            servers: BTreeMap::new(),
            problems: Vec::new(),
        };
        // Absent root key is the normal "no MCP servers yet" state.
        let Some(entries) = root.get(self.root_key()) else {
            return Ok(read);
        };
        let Some(entries) = entries.as_object() else {
            read.problems.push(Problem {
                server: None,
                message: format!("`{}` is not an object", self.root_key()),
            });
            return Ok(read);
        };

        for (name, entry) in entries {
            match parse_entry(entry) {
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
        Ok(read)
    }
}

/// Converts one client entry; `Err` is a problem reason, the optional note
/// reports a lossy-but-successful conversion.
fn parse_entry(entry: &serde_json::Value) -> Result<(Server, Option<String>), String> {
    let Some(obj) = entry.as_object() else {
        return Err("entry is not an object".to_owned());
    };

    let explicit = match obj.get("type") {
        None => None,
        Some(serde_json::Value::String(t)) => Some(t.as_str()),
        Some(_) => return Err("`type` is not a string".to_owned()),
    };
    let has_command = obj.contains_key("command");
    let has_url = obj.contains_key("url");

    let mut note = None;
    let stdio = match explicit {
        Some("stdio") => true,
        Some("http" | "streamable-http" | "streamable_http") => false,
        Some("sse") => {
            // Legacy transport we don't model; the URL still identifies the
            // server, so read it as http and say so.
            note = Some("legacy `sse` transport read as http".to_owned());
            false
        }
        Some(other) => return Err(format!("unknown transport type {other:?}")),
        // No explicit type: infer from which target field is present.
        None => match (has_command, has_url) {
            (true, false) => true,
            (false, true) => false,
            (true, true) => return Err("has both `command` and `url`".to_owned()),
            (false, false) => return Err("has neither `command` nor `url`".to_owned()),
        },
    };

    // Some clients (e.g. Cline-style configs) mark entries disabled in place.
    let enabled = !matches!(obj.get("disabled"), Some(serde_json::Value::Bool(true)));

    let transport = if stdio {
        Transport::Stdio {
            command: string_field(obj, "command")?.ok_or("missing `command`")?,
            args: string_list(obj, "args")?,
            env: string_object(obj, "env")?,
        }
    } else {
        Transport::Http {
            url: string_field(obj, "url")?.ok_or("missing `url`")?,
            headers: string_object(obj, "headers")?,
        }
    };
    Ok((
        Server {
            enabled,
            tags: Vec::new(),
            transport,
        },
        note,
    ))
}

fn string_field(
    obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<Option<String>, String> {
    match obj.get(key) {
        None => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("`{key}` is not a string")),
    }
}

fn string_list(
    obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<Vec<String>, String> {
    match obj.get(key) {
        None => Ok(Vec::new()),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                serde_json::Value::String(s) => Ok(s.clone()),
                _ => Err(format!("`{key}` contains a non-string element")),
            })
            .collect(),
        Some(_) => Err(format!("`{key}` is not an array")),
    }
}

fn string_object(
    obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<BTreeMap<String, String>, String> {
    match obj.get(key) {
        None => Ok(BTreeMap::new()),
        Some(serde_json::Value::Object(map)) => map
            .iter()
            .map(|(k, v)| match v {
                serde_json::Value::String(s) => Ok((k.clone(), s.clone())),
                _ => Err(format!("`{key}.{k}` is not a string")),
            })
            .collect(),
        Some(_) => Err(format!("`{key}` is not an object")),
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
