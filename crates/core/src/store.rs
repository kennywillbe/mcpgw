use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use fs4::fs_std::FileExt as _;
use toml_edit::{DocumentMut, Item, Table, value};

use crate::config::{Config, Server, Transport, validate_name};
use crate::error::Error;

const TEMPLATE: &str = "\
# mcpgw canonical config — the single source of truth for your MCP servers.
# `mcpgw sync` pushes these entries into every client config.
# Hand-edits (comments, ordering) are preserved by mcpgw commands.
version = 1
";

/// Read-modify-write handle for the canonical config file.
///
/// Construction takes an exclusive advisory lock that covers the whole
/// read-modify-write cycle (released on drop), so concurrent writers cannot
/// lose each other's updates. Mutations edit the `toml_edit` AST — user
/// comments and ordering survive — and are validated through serde before
/// they replace in-memory state, so an invalid config can never reach disk.
#[derive(Debug)]
pub struct ConfigStore {
    path: PathBuf,
    doc: DocumentMut,
    config: Config,
    // Sidecar lock file, never the config itself: the atomic rename in
    // `save` swaps the inode, which would strand any lock held on it.
    _lock: File,
}

impl ConfigStore {
    /// Opens the config at `path` for editing.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotFound`] when the file does not exist, plus
    /// everything [`Config::parse`] and lock acquisition can return.
    pub fn edit(path: &Path) -> Result<Self, Error> {
        Self::open(path, false)
    }

    /// Like [`ConfigStore::edit`], but starts from a commented template when
    /// the file does not exist yet (the first-`add` path).
    ///
    /// # Errors
    ///
    /// Same as [`ConfigStore::edit`] except a missing file is not an error.
    pub fn edit_or_create(path: &Path) -> Result<Self, Error> {
        Self::open(path, true)
    }

    fn open(path: &Path, create: bool) -> Result<Self, Error> {
        let lock = acquire_lock(path)?;
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if create {
                    TEMPLATE.to_owned()
                } else {
                    return Err(Error::NotFound {
                        path: path.to_owned(),
                    });
                }
            }
            Err(source) => {
                return Err(Error::Io {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        let config = Config::parse(&text, path)?;
        let doc: DocumentMut = text.parse().map_err(|source| Error::Edit {
            path: path.to_owned(),
            source,
        })?;
        Ok(Self {
            path: path.to_owned(),
            doc,
            config,
            _lock: lock,
        })
    }

    /// The validated view of the current (possibly unsaved) state.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Inserts `server` under `name`; returns whether an entry was replaced.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidName`] for a bad name and
    /// [`Error::DuplicateName`] when the name exists and `overwrite` is false.
    pub fn upsert_server(
        &mut self,
        name: &str,
        server: &Server,
        overwrite: bool,
    ) -> Result<bool, Error> {
        validate_name(name)?;
        let exists = self.config.servers.contains_key(name);
        if exists && !overwrite {
            return Err(Error::DuplicateName {
                name: name.to_owned(),
            });
        }
        let mut doc = self.doc.clone();
        ensure_servers_table(&mut doc).insert(name, Item::Table(server_table(server)));
        self.commit(doc)?;
        Ok(exists)
    }

    /// Deletes the entry for `name`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnknownServer`] when no such entry exists.
    pub fn remove_server(&mut self, name: &str) -> Result<(), Error> {
        self.ensure_known(name)?;
        let mut doc = self.doc.clone();
        if let Some(servers) = doc.get_mut("servers").and_then(Item::as_table_like_mut) {
            servers.remove(name);
        }
        self.commit(doc)
    }

    /// Flips only the `enabled` field of `name`, leaving the rest untouched.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnknownServer`] when no such entry exists.
    pub fn set_enabled(&mut self, name: &str, enabled: bool) -> Result<(), Error> {
        self.ensure_known(name)?;
        let mut doc = self.doc.clone();
        doc["servers"][name]["enabled"] = value(enabled);
        self.commit(doc)
    }

    /// Writes the current state to disk atomically (temp file + rename).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] for any filesystem failure.
    pub fn save(&self) -> Result<(), Error> {
        let io_err = |path: &Path| {
            let path = path.to_owned();
            move |source| Error::Io { path, source }
        };
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent).map_err(io_err(parent))?;
        let mut tmp = tempfile::Builder::new()
            .prefix(".config.toml.")
            .tempfile_in(parent)
            .map_err(io_err(parent))?;
        tmp.write_all(self.doc.to_string().as_bytes())
            .map_err(io_err(&self.path))?;
        // fsync before rename: a crash must yield the old file, never a
        // half-written new one.
        tmp.as_file().sync_all().map_err(io_err(&self.path))?;
        tmp.persist(&self.path).map_err(|err| Error::Io {
            path: self.path.clone(),
            source: err.error,
        })?;
        Ok(())
    }

    // Validates the edited document before it replaces in-memory state.
    fn commit(&mut self, doc: DocumentMut) -> Result<(), Error> {
        let config = Config::parse(&doc.to_string(), &self.path)?;
        self.doc = doc;
        self.config = config;
        Ok(())
    }

    fn ensure_known(&self, name: &str) -> Result<(), Error> {
        if self.config.servers.contains_key(name) {
            Ok(())
        } else {
            Err(Error::UnknownServer {
                name: name.to_owned(),
                available: self.config.servers.keys().cloned().collect(),
            })
        }
    }
}

/// The sidecar lock path for a given config path (`config.toml.lock`).
#[must_use]
pub fn lock_path(config: &Path) -> PathBuf {
    let mut os = config.as_os_str().to_owned();
    os.push(".lock");
    PathBuf::from(os)
}

fn acquire_lock(config: &Path) -> Result<File, Error> {
    let io_err = |path: &Path| {
        let path = path.to_owned();
        move |source| Error::Io { path, source }
    };
    let parent = config.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(io_err(parent))?;
    let path = lock_path(config);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(io_err(&path))?;
    // Blocks until any other mcpgw process releases the config.
    file.lock_exclusive().map_err(io_err(&path))?;
    Ok(file)
}

fn ensure_servers_table(doc: &mut DocumentMut) -> &mut dyn toml_edit::TableLike {
    let item = doc.entry("servers").or_insert_with(|| {
        let mut table = Table::new();
        // Implicit: renders only when it has entries, no bare [servers] header.
        table.set_implicit(true);
        Item::Table(table)
    });
    // A hand-written inline `servers = { ... }` cannot hold standard
    // sub-tables; normalize it (entry formatting inside is rebuilt, the
    // rest of the file is untouched).
    if let Item::Value(toml_edit::Value::InlineTable(inline)) = item {
        *item = Item::Table(std::mem::take(inline).into_table());
    }
    item.as_table_like_mut()
        .expect("servers is a table after normalization")
}

// Generated entries write every field explicitly (agreed in M2), so the file
// documents the full shape of an entry without consulting the docs.
fn server_table(server: &Server) -> Table {
    let mut table = Table::new();
    table["enabled"] = value(server.enabled);
    table["tags"] = value(string_array(&server.tags));
    match &server.transport {
        Transport::Stdio { command, args, env } => {
            table["type"] = value("stdio");
            table["command"] = value(command);
            table["args"] = value(string_array(args));
            table["env"] = value(string_map(env));
        }
        Transport::Http { url, headers } => {
            table["type"] = value("http");
            table["url"] = value(url);
            table["headers"] = value(string_map(headers));
        }
    }
    table
}

fn string_array(items: &[String]) -> toml_edit::Value {
    items
        .iter()
        .map(String::as_str)
        .collect::<toml_edit::Array>()
        .into()
}

fn string_map(map: &BTreeMap<String, String>) -> toml_edit::Value {
    let mut table = toml_edit::InlineTable::new();
    for (key, val) in map {
        table.insert(key, val.as_str().into());
    }
    table.into()
}
