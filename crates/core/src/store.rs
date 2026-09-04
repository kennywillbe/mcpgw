use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, Table, value};

use crate::config::{ClientScope, Config, Server, ToolRules, Transport, validate_name};
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
            source: Box::new(source),
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
        let mut table = server_table(server);
        // An overwrite redefines the transport, not the limits around it: a
        // re-import or `add --force` that dropped `[tools]` would silently
        // widen what every client can reach through that server, and one
        // that dropped `calls_per_minute` would silently take the brake off
        // it. Both are the edit nobody would think to check for afterwards.
        // An incoming entry that carries a value of its own still wins.
        let existing = self
            .doc
            .get("servers")
            .and_then(Item::as_table_like)
            .and_then(|servers| servers.get(name))
            .and_then(Item::as_table_like);
        for (key, incoming) in [
            ("tools", server.tools.is_some()),
            ("calls_per_minute", server.calls_per_minute > 0),
        ] {
            if incoming {
                continue;
            }
            if let Some(kept) = existing.and_then(|entry| entry.get(key)) {
                table.insert(key, kept.clone());
            }
        }
        ensure_servers_table(&mut doc).insert(name, Item::Table(table));
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

    /// Replaces `name`'s `[tools]` table, or removes it when `rules` says
    /// nothing at all — `mcpgw tools <server> clear`, and the shape every
    /// other edit reduces to.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnknownServer`] when no such entry exists.
    pub fn set_tool_rules(&mut self, name: &str, rules: &ToolRules) -> Result<(), Error> {
        self.ensure_known(name)?;
        let mut doc = self.doc.clone();
        let entry = &mut doc["servers"][name];
        // A hand-written inline entry cannot hold a standard sub-table;
        // normalized the same way `ensure_servers_table` normalizes its
        // parent, and for the same reason.
        if let Item::Value(toml_edit::Value::InlineTable(inline)) = entry {
            *entry = Item::Table(std::mem::take(inline).into_table());
        }
        if let Some(entry) = entry.as_table_like_mut() {
            if rules.is_empty() {
                entry.remove("tools");
            } else {
                entry.insert("tools", Item::Table(tools_table(rules)));
            }
        }
        self.commit(doc)
    }

    /// Records the identity `mcpgw auth login` was told to use for `name`,
    /// so the next login and every refresh present the same one.
    ///
    /// Persisted rather than asked for again because a client id issued out
    /// of band is a property of the *server*, not of the one command that
    /// first mentioned it: a refresh runs in the daemon, where nobody can
    /// pass a flag.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnknownServer`] when no such entry exists,
    /// [`Error::AuthConflict`] when the entry gets its headers from a
    /// command, and [`Error::Parse`] if the edit somehow does not round-trip.
    pub fn set_auth(&mut self, name: &str, auth: &crate::config::ServerAuth) -> Result<(), Error> {
        self.ensure_known(name)?;
        let mut doc = self.doc.clone();
        doc["servers"][name]["auth"] = value(auth_table(auth));
        self.commit(doc)
    }

    /// Sets `name`'s `calls_per_minute`, or removes the key when `calls` is
    /// 0 — `mcpgw tools <server> budget off`.
    ///
    /// Removed rather than written as `calls_per_minute = 0`, because a zero
    /// in the file is a config error: see
    /// [`Server::calls_per_minute`](crate::Server::calls_per_minute).
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnknownServer`] when no such entry exists.
    pub fn set_call_budget(&mut self, name: &str, calls: u32) -> Result<(), Error> {
        self.ensure_known(name)?;
        let mut doc = self.doc.clone();
        let entry = &mut doc["servers"][name];
        if calls == 0 {
            if let Some(entry) = entry.as_table_like_mut() {
                entry.remove("calls_per_minute");
            }
        } else {
            entry["calls_per_minute"] = value(i64::from(calls));
        }
        self.commit(doc)
    }

    /// Replaces `[clients.ID]`, or removes it when `scope` says nothing at
    /// all — `mcpgw clients ID servers all` on a client with no tool rules,
    /// and the shape every other edit reduces to.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnknownClient`] for an id no adapter answers to; the
    /// table would otherwise sit in the file doing nothing.
    pub fn set_client_scope(&mut self, id: &str, scope: &ClientScope) -> Result<(), Error> {
        if crate::clients::ClientKind::from_id(id).is_none() {
            return Err(Error::UnknownClient {
                id: id.to_owned(),
                available: crate::clients::ClientKind::ALL
                    .iter()
                    .map(|kind| (*kind).id().to_owned())
                    .collect(),
            });
        }
        let mut doc = self.doc.clone();
        if scope.is_empty() {
            if let Some(clients) = doc.get_mut("clients").and_then(Item::as_table_like_mut) {
                clients.remove(id);
            }
            return self.commit(doc);
        }
        ensure_table(&mut doc, "clients").insert(id, Item::Table(client_table(scope)));
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
        crate::private::sync_dir(parent).map_err(io_err(parent))?;
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

/// Takes the exclusive advisory lock guarding `config`, blocking until any
/// other mcpgw process releases it. The lock lives for as long as the
/// returned handle. Shared with the state file, which needs the same
/// read-modify-write protection around a different path.
pub(crate) fn acquire_lock(config: &Path) -> Result<File, Error> {
    let io_err = |path: &Path| {
        let path = path.to_owned();
        move |source| Error::Io { path, source }
    };
    let parent = config.parent().unwrap_or_else(|| Path::new("."));
    crate::private::create_dir_all(parent).map_err(io_err(parent))?;
    let path = lock_path(config);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(io_err(&path))?;
    // Blocks until any other mcpgw process releases the config. This used
    // to go through the `fs4` crate, but `std::fs::File::lock` now covers
    // the same flock/LockFileEx-backed blocking exclusive lock directly, so
    // the extra dependency was dropped rather than chasing its 1.x rename.
    file.lock().map_err(io_err(&path))?;
    Ok(file)
}

fn ensure_servers_table(doc: &mut DocumentMut) -> &mut dyn toml_edit::TableLike {
    ensure_table(doc, "servers")
}

/// A top-level table of tables (`servers`, `clients`), created implicit so it
/// renders only once it holds an entry.
fn ensure_table<'d>(doc: &'d mut DocumentMut, key: &str) -> &'d mut dyn toml_edit::TableLike {
    let item = doc.entry(key).or_insert_with(|| {
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
        .expect("the table is a table after normalization")
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
        Transport::Http {
            url,
            headers_command,
            headers,
            auth,
        } => {
            table["type"] = value("http");
            table["url"] = value(url);
            // The one generated field that is omitted when empty: `[]`
            // beside a `headers` table reads as an entry that uses a helper
            // and got nothing back, which is a different claim from an entry
            // that has no helper.
            if !headers_command.is_empty() {
                table["headers_command"] = value(string_array(headers_command));
            }
            table["headers"] = value(string_map(headers));
            // Omitted when absent, like `headers_command` and for the same
            // reason: an empty `auth = {}` beside a working entry reads as a
            // server whose login produced nothing, which is a different claim
            // from a server that needs no login at all.
            if let Some(auth) = auth {
                table["auth"] = value(auth_table(auth));
            }
        }
    }
    // Omitted when there is no budget: a generated `calls_per_minute = 0`
    // would be a file this build refuses to load again.
    if server.calls_per_minute > 0 {
        table["calls_per_minute"] = value(i64::from(server.calls_per_minute));
    }
    // Last, because it is a sub-table: TOML puts every value of a section
    // ahead of the sections nested in it.
    if let Some(rules) = &server.tools
        && !rules.is_empty()
    {
        table["tools"] = Item::Table(tools_table(rules));
    }
    table
}

/// A `[servers.NAME.tools]` table, with each list written only when it has
/// entries: an `allow = []` beside a populated `deny` reads as an allowlist
/// that permits nothing, which is the opposite of what it means.
fn tools_table(rules: &ToolRules) -> Table {
    let mut table = Table::new();
    if !rules.allow.is_empty() {
        table["allow"] = value(string_array(&rules.allow));
    }
    if !rules.deny.is_empty() {
        table["deny"] = value(string_array(&rules.deny));
    }
    // Written back like the lists, or a `tools allow` on a server whose
    // drift is off would quietly turn it back on: this rewrites the whole
    // table from the parsed rules.
    if !rules.drift.is_default() {
        table["drift"] = value(rules.drift.as_str());
    }
    table
}

/// The `auth` table as an inline value.
///
/// Inline rather than a standard sub-table because [`server_table`] is
/// inserted whole into `[servers]`, and a nested standard table there would
/// have to be ordered after every value of every *sibling* entry rather than
/// after its own. `headers` is written inline for the same reason.
fn auth_table(auth: &crate::config::ServerAuth) -> toml_edit::Value {
    let mut table = toml_edit::InlineTable::new();
    if let Some(client_id) = &auth.client_id {
        table.insert("client_id", client_id.as_str().into());
    }
    if let Some(var) = &auth.client_secret_env {
        table.insert("client_secret_env", var.as_str().into());
    }
    if !auth.scopes.is_empty() {
        table.insert("scopes", string_array(&auth.scopes));
    }
    table.into()
}

/// A `[clients.ID]` table. Each key is written only when it says something,
/// for the same reason the tool lists are: `servers = []` reads as a client
/// that is given no servers, which is the opposite of what an absent list
/// means.
fn client_table(scope: &ClientScope) -> Table {
    let mut table = Table::new();
    if !scope.servers.is_empty() {
        table["servers"] = value(string_array(&scope.servers));
    }
    if let Some(max) = scope.max_tools {
        table["max_tools"] = value(i64::try_from(max).unwrap_or(i64::MAX));
    }
    // Last, because it is a sub-table.
    if let Some(rules) = &scope.tools
        && !rules.is_empty()
    {
        table["tools"] = Item::Table(tools_table(rules));
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
