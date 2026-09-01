//! The per-client codec: how a client config is stored (JSON, JSONC, TOML),
//! where inside it the server map lives, and how one entry maps to and from
//! the canonical [`Server`].
//!
//! Every client mcpgw supports differs on those three axes and agrees on
//! nothing else, so they are the seam: adding a client is picking a
//! [`Format`], a [`RootPath`] and an [`EntrySchema`], never touching sync.
//!
//! Writes go through [`ClientDocument`], which keeps whatever the format can
//! keep — comments and formatting for JSONC and TOML — and edits only the
//! entries a plan owns.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::config::{Server, Transport};

/// How a client's config file is stored on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Strict JSON. Rewritten wholesale by `serde_json`, so comments and
    /// hand formatting are not a concern — a file carrying them is not
    /// strict JSON and is refused before it reaches a write.
    Json,
    /// JSON with comments and trailing commas, edited through a CST so both
    /// survive a write.
    Jsonc,
    /// TOML, edited through `toml_edit` for the same reason.
    Toml,
}

/// Where a client keeps its server map, as a path of literal object keys.
///
/// One segment is the common case (`mcpServers`, `servers`, `mcp`,
/// `context_servers`, or a TOML `[mcp_servers]` table); several segments walk
/// nested tables. Segments are *literal*: a client whose key contains a dot
/// (Amp's `amp.mcpServers`) spells it as one segment, so the two shapes never
/// collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootPath(&'static [&'static str]);

impl RootPath {
    /// # Panics
    ///
    /// Panics on an empty path — a client without a server map has nothing
    /// for mcpgw to sync, so this is a wiring mistake, not a runtime state.
    #[must_use]
    pub const fn new(segments: &'static [&'static str]) -> Self {
        assert!(!segments.is_empty(), "a root path needs at least one key");
        Self(segments)
    }

    #[must_use]
    pub fn segments(self) -> &'static [&'static str] {
        self.0
    }

    /// The path as it is written in messages: dotted, which for the single
    /// segment case is just the key itself.
    #[must_use]
    pub fn display(self) -> String {
        self.0.join(".")
    }

    /// Resolves the server map inside a JSON document.
    ///
    /// `Ok(None)` is the normal "no MCP servers configured yet" state — an
    /// absent key anywhere along the path. `Err` carries the key that exists
    /// but is not an object, which the reader reports as a problem.
    ///
    /// # Errors
    ///
    /// Returns the dotted path of the offending key.
    pub fn locate_in(self, root: &Value) -> Result<Option<&Map<String, Value>>, String> {
        let mut node = root;
        for (depth, segment) in self.0.iter().enumerate() {
            let Some(object) = node.as_object() else {
                return Err(self.0[..depth].join("."));
            };
            match object.get(*segment) {
                None => return Ok(None),
                Some(next) => node = next,
            }
        }
        match node.as_object() {
            Some(entries) => Ok(Some(entries)),
            None => Err(self.display()),
        }
    }
}

/// How one server entry is spelled inside a client's server map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntrySchema {
    /// The `mcpServers` shape shared by Claude Desktop, Claude Code and
    /// Cursor: stdio is implied by `command`, so `type` is written only
    /// where it carries information.
    McpServers,
    /// VS Code's `servers` shape, whose schema wants an explicit `type` on
    /// every entry.
    VsCode,
    /// Gemini CLI's shape: no `type` at all, and two distinct remote fields
    /// — `httpUrl` is streamable HTTP, plain `url` is the legacy SSE
    /// transport. Writing the wrong one silently picks the wrong protocol.
    Gemini,
    /// Codex CLI's TOML shape: no `type`, headers spelled `http_headers`,
    /// a per-entry `enabled` flag, and a long tail of optional fields whose
    /// set keeps growing release to release.
    Codex,
    /// opencode's shape: `type` is `local` or `remote`, a local entry holds
    /// its program and arguments in one `command` array, and its variables
    /// are `environment` rather than `env`.
    Opencode,
}

impl EntrySchema {
    /// Converts one client entry into a canonical server.
    ///
    /// `Err` is a problem reason; the optional note reports a conversion
    /// that succeeded but lost something. Reads are deliberately lenient —
    /// see the module docs on [`crate::clients`].
    ///
    /// # Errors
    ///
    /// Returns the reason the entry could not be converted at all.
    pub fn parse(self, entry: &Value) -> Result<(Server, Option<String>), String> {
        let Some(obj) = entry.as_object() else {
            return Err("entry is not an object".to_owned());
        };
        match self {
            Self::Gemini => return parse_gemini(obj),
            Self::Codex => return parse_codex(obj),
            Self::Opencode => return parse_opencode(obj),
            Self::McpServers | Self::VsCode => {}
        }

        let explicit = match obj.get("type") {
            None => None,
            Some(Value::String(t)) => Some(t.as_str()),
            Some(_) => return Err("`type` is not a string".to_owned()),
        };
        let has_command = obj.contains_key("command");
        let has_url = obj.contains_key("url");

        let mut note = None;
        let stdio = match explicit {
            Some("stdio") => true,
            Some("http" | "streamable-http" | "streamable_http") => false,
            Some("sse") => {
                // Legacy transport we don't model; the URL still identifies
                // the server, so read it as http and say so.
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

        // Some clients (e.g. Cline-style configs) mark entries disabled in
        // place.
        let enabled = !matches!(obj.get("disabled"), Some(Value::Bool(true)));

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

    /// The client-shaped value for one canonical server.
    #[must_use]
    pub fn emit(self, server: &Server) -> Value {
        // opencode shares no field spelling with the others — not even the
        // type of `command` — so it is written whole rather than patched
        // into the shape below.
        if matches!(self, Self::Opencode) {
            return emit_opencode(server);
        }
        let mut obj = Map::new();
        match &server.transport {
            Transport::Stdio { command, args, env } => {
                if matches!(self, Self::VsCode) {
                    obj.insert("type".to_owned(), "stdio".into());
                }
                obj.insert("command".to_owned(), command.as_str().into());
                if !args.is_empty() {
                    obj.insert("args".to_owned(), args.clone().into());
                }
                if !env.is_empty() {
                    obj.insert("env".to_owned(), string_map(env));
                }
            }
            Transport::Http { url, headers } => {
                let headers_key = match self {
                    Self::Gemini => {
                        // `url` here would mean SSE to Gemini; streamable
                        // HTTP is spelled `httpUrl`, and there is no `type`.
                        obj.insert("httpUrl".to_owned(), url.as_str().into());
                        "headers"
                    }
                    Self::Codex => {
                        obj.insert("url".to_owned(), url.as_str().into());
                        "http_headers"
                    }
                    Self::McpServers | Self::VsCode => {
                        obj.insert("type".to_owned(), "http".into());
                        obj.insert("url".to_owned(), url.as_str().into());
                        "headers"
                    }
                    Self::Opencode => unreachable!("emitted whole above"),
                };
                if !headers.is_empty() {
                    obj.insert(headers_key.to_owned(), string_map(headers));
                }
            }
        }
        Value::Object(obj)
    }
}

/// Gemini CLI's entry shape, which shares no discriminator with the others:
/// there is no `type`, the transport is whichever target field is present,
/// and the two remote fields are different protocols.
///
/// Everything else an entry may carry (`cwd`, `timeout`, `trust`,
/// `includeTools`, `authProviderType`, …) is ignored on read — those fields
/// have no canonical counterpart, and an entry mcpgw does not manage keeps
/// them verbatim because sync never rewrites it.
fn parse_gemini(obj: &Map<String, Value>) -> Result<(Server, Option<String>), String> {
    let http_url = string_field(obj, "httpUrl")?;
    let sse_url = string_field(obj, "url")?;
    let mut note = None;

    let transport = match (http_url, sse_url) {
        (Some(http_url), sse) => {
            if sse.is_some() {
                // Gemini itself prefers httpUrl; saying so keeps a reader
                // from assuming the SSE endpoint is the live one.
                note = Some("`url` ignored: `httpUrl` takes precedence".to_owned());
            }
            Transport::Http {
                url: http_url,
                headers: string_object(obj, "headers")?,
            }
        }
        (None, Some(sse_url)) => {
            // Legacy transport we don't model; the URL still identifies the
            // server, so read it as http and say so.
            note = Some("legacy `sse` transport read as http".to_owned());
            Transport::Http {
                url: sse_url,
                headers: string_object(obj, "headers")?,
            }
        }
        (None, None) => Transport::Stdio {
            command: string_field(obj, "command")?
                .ok_or("has none of `command`, `httpUrl` or `url`")?,
            args: string_list(obj, "args")?,
            env: string_object(obj, "env")?,
        },
    };

    Ok((
        Server {
            // Gemini has no per-entry enabled flag; exclusion lives in a
            // sibling root key and is applied after the whole file is read.
            enabled: true,
            tags: Vec::new(),
            transport,
        },
        note,
    ))
}

/// Codex CLI's entry shape. Transport is whichever target field is present
/// — `command` for stdio, `url` for remote — with no `type` discriminator,
/// and remote headers are `http_headers` rather than `headers`.
///
/// Codex's entry schema grows every few releases (`env_vars`, `cwd`,
/// `startup_timeout_sec`, `tool_timeout_sec`, `required`, `enabled_tools`,
/// per-tool sub-tables, …), so unknown fields are read as if absent instead
/// of rejected: an entry mcpgw does not manage keeps every one of them
/// verbatim because sync never rewrites it.
fn parse_codex(obj: &Map<String, Value>) -> Result<(Server, Option<String>), String> {
    let command = string_field(obj, "command")?;
    let url = string_field(obj, "url")?;
    let mut note = None;

    let transport = match (command, url) {
        (Some(command), None) => Transport::Stdio {
            command,
            args: string_list(obj, "args")?,
            env: string_object(obj, "env")?,
        },
        (None, Some(url)) => {
            if obj.contains_key("auth") || obj.contains_key("bearer_token_env_var") {
                // Codex mints or forwards the credential itself; the
                // canonical config has no field for that, so importing this
                // entry yields a URL that will not authenticate on its own.
                note = Some("codex-managed auth not carried over".to_owned());
            }
            Transport::Http {
                url,
                headers: string_object(obj, "http_headers")?,
            }
        }
        (Some(_), Some(_)) => return Err("has both `command` and `url`".to_owned()),
        (None, None) => return Err("has neither `command` nor `url`".to_owned()),
    };

    Ok((
        Server {
            enabled: !matches!(obj.get("enabled"), Some(Value::Bool(false))),
            tags: Vec::new(),
            transport,
        },
        note,
    ))
}

/// opencode's entry shape. `type` is the discriminator (`local` or
/// `remote`), a local entry's `command` is a single array holding the program
/// *and* its arguments, and its variables are `environment`.
///
/// The type is optional here only because a hand-written file may omit it;
/// opencode's own schema requires it, so the inference below is leniency for
/// reads, never something the writer relies on. Unknown fields (`cwd`, the
/// rest of the schema) are read as absent — an entry mcpgw does not manage
/// keeps them verbatim because sync never rewrites it.
fn parse_opencode(obj: &Map<String, Value>) -> Result<(Server, Option<String>), String> {
    let declared = match obj.get("type") {
        None => None,
        Some(Value::String(t)) => Some(t.clone()),
        Some(_) => return Err("`type` is not a string".to_owned()),
    };
    let has_command = obj.contains_key("command");
    let local = match declared.as_deref() {
        Some("local") => true,
        Some("remote") => false,
        Some(other) => return Err(format!("unknown type {other:?}")),
        None => match (has_command, obj.contains_key("url")) {
            (true, false) => true,
            (false, true) => false,
            (true, true) => return Err("has both `command` and `url`".to_owned()),
            (false, false) => return Err("has neither `command` nor `url`".to_owned()),
        },
    };

    let mut note = None;
    let transport = if local {
        let mut argv = string_list(obj, "command")?.into_iter();
        let command = argv.next().ok_or_else(|| {
            if has_command {
                "`command` is empty".to_owned()
            } else {
                "missing `command`".to_owned()
            }
        })?;
        Transport::Stdio {
            command,
            args: argv.collect(),
            env: string_object(obj, "environment")?,
        }
    } else {
        if !matches!(obj.get("oauth"), None | Some(Value::Bool(false))) {
            // opencode holds the OAuth tokens itself; the canonical config
            // has no field for them, so the imported URL will not
            // authenticate on its own.
            note = Some("opencode-managed oauth not carried over".to_owned());
        }
        Transport::Http {
            // Header values may carry opencode's own `{env:VAR}`
            // interpolation, which is kept verbatim: expanding it here would
            // bake a secret into the canonical config.
            url: string_field(obj, "url")?.ok_or("missing `url`")?,
            headers: string_object(obj, "headers")?,
        }
    };

    Ok((
        Server {
            enabled: !matches!(obj.get("enabled"), Some(Value::Bool(false))),
            tags: Vec::new(),
            transport,
        },
        note,
    ))
}

fn emit_opencode(server: &Server) -> Value {
    let mut obj = Map::new();
    match &server.transport {
        Transport::Stdio { command, args, env } => {
            obj.insert("type".to_owned(), "local".into());
            let mut argv = Vec::with_capacity(args.len() + 1);
            argv.push(Value::from(command.as_str()));
            argv.extend(args.iter().map(|arg| Value::from(arg.as_str())));
            obj.insert("command".to_owned(), argv.into());
            if !env.is_empty() {
                obj.insert("environment".to_owned(), string_map(env));
            }
        }
        Transport::Http { url, headers } => {
            obj.insert("type".to_owned(), "remote".into());
            obj.insert("url".to_owned(), url.as_str().into());
            if !headers.is_empty() {
                obj.insert("headers".to_owned(), string_map(headers));
            }
        }
    }
    Value::Object(obj)
}

/// One client's read/write rules, the whole of what makes clients differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Codec {
    pub format: Format,
    pub root: RootPath,
    pub entries: EntrySchema,
}

impl Codec {
    /// Parses client text into the canonical JSON view used by reads and by
    /// plan comparison.
    ///
    /// # Errors
    ///
    /// Returns the format parser's own failure. Every format funnels into a
    /// `serde_json::Error` because [`crate::Error::ClientParse`] carries that
    /// type: JSON keeps its real error (line and column intact), JSONC and
    /// TOML get their message re-wrapped, and no caller needs a second error
    /// type per format.
    pub fn parse_value(self, text: &str) -> Result<Value, serde_json::Error> {
        match self.format {
            Format::Json => serde_json::from_str(text),
            // Via the CST rather than the crate's value parser so reads and
            // writes accept exactly the same dialect.
            Format::Jsonc => jsonc_parser::cst::CstRootNode::parse(text, &jsonc_options())
                .map(|root| root.to_serde_value().unwrap_or_default())
                .map_err(foreign_error),
            Format::Toml => text
                .parse::<toml_edit::DocumentMut>()
                .map(|doc| toml_to_json(doc.as_item()))
                .map_err(foreign_error),
        }
    }

    /// Parses client text into an editable document.
    ///
    /// # Errors
    ///
    /// Same failures, and the same funnelling, as [`Codec::parse_value`].
    pub fn parse_document(self, text: &str) -> Result<ClientDocument, serde_json::Error> {
        match self.format {
            Format::Json => serde_json::from_str(text).map(ClientDocument::Json),
            Format::Jsonc => jsonc_parser::cst::CstRootNode::parse(text, &jsonc_options())
                .map(ClientDocument::Jsonc)
                .map_err(foreign_error),
            Format::Toml => text
                .parse::<toml_edit::DocumentMut>()
                .map(ClientDocument::Toml)
                .map_err(foreign_error),
        }
    }

    /// The document to edit when the client has no config file yet.
    ///
    /// # Panics
    ///
    /// Panics if the empty JSONC document fails to parse, which would mean
    /// the parser cannot read `{}`.
    #[must_use]
    pub fn empty_document(self) -> ClientDocument {
        match self.format {
            Format::Json => ClientDocument::Json(Value::Object(Map::new())),
            // Starting from a parsed `{}` rather than an empty CST keeps the
            // first write on the same code path as every later one.
            Format::Jsonc => ClientDocument::Jsonc(
                jsonc_parser::cst::CstRootNode::parse("{}\n", &jsonc_options())
                    .expect("`{}` parses as JSONC"),
            ),
            Format::Toml => ClientDocument::Toml(toml_edit::DocumentMut::new()),
        }
    }

    /// What the file is called in the CLI's "cannot parse, skipped" message.
    #[must_use]
    pub fn format_name(self) -> &'static str {
        match self.format {
            Format::Json => "strict JSON",
            Format::Jsonc => "JSONC",
            Format::Toml => "TOML",
        }
    }
}

/// A parsed client config, editable in place without losing what the format
/// can preserve.
#[derive(Debug)]
pub enum ClientDocument {
    Json(Value),
    Jsonc(jsonc_parser::cst::CstRootNode),
    Toml(toml_edit::DocumentMut),
}

impl ClientDocument {
    /// The client's server map as canonical JSON.
    ///
    /// An absent or wrongly typed map reads as empty: sync only ever adds to
    /// it, so "no map" and "not a map" lead to the same plan.
    #[must_use]
    pub fn entries(&self, root: RootPath) -> Map<String, Value> {
        let value = match self {
            Self::Json(value) => return json_entries(root, value),
            Self::Jsonc(node) => node.to_serde_value().unwrap_or_default(),
            Self::Toml(doc) => toml_to_json(doc.as_item()),
        };
        json_entries(root, &value)
    }

    /// Upserts `upserts` and deletes `removes` inside the server map,
    /// creating the map (and any missing parent) if needed.
    ///
    /// Entries not named here are never rewritten — that is what keeps a
    /// foreign entry's own formatting, and in JSONC/TOML its comments.
    pub fn edit(&mut self, root: RootPath, removes: &[String], upserts: &[(&str, &Value)]) {
        match self {
            Self::Json(value) => edit_json(root, value, removes, upserts),
            Self::Jsonc(node) => edit_jsonc(root, node, removes, upserts),
            Self::Toml(doc) => edit_toml(root, doc, removes, upserts),
        }
    }

    /// Serializes the document back to file text, trailing newline included.
    ///
    /// # Errors
    ///
    /// Returns the JSON serializer's error; the JSONC and TOML documents
    /// print infallibly.
    pub fn to_text(&self) -> Result<String, serde_json::Error> {
        match self {
            Self::Json(value) => {
                let mut text = serde_json::to_string_pretty(value)?;
                text.push('\n');
                Ok(text)
            }
            Self::Jsonc(node) => Ok(node.to_string()),
            Self::Toml(doc) => Ok(doc.to_string()),
        }
    }
}

/// The JSONC dialect mcpgw reads: comments, trailing commas and the rest of
/// what VS Code-style files carry are all accepted, because the point of the
/// format is that a hand-edited file survives a sync.
fn jsonc_options() -> jsonc_parser::ParseOptions {
    jsonc_parser::ParseOptions::default()
}

/// Re-wraps a foreign parser's failure as a `serde_json::Error`; see
/// [`Codec::parse_value`] for why they all end up in one type.
fn foreign_error(source: impl std::fmt::Display) -> serde_json::Error {
    serde::de::Error::custom(source)
}

fn json_entries(root: RootPath, value: &Value) -> Map<String, Value> {
    root.locate_in(value)
        .ok()
        .flatten()
        .cloned()
        .unwrap_or_default()
}

/// Walks (creating as it goes) to the server map inside a JSON document.
/// Anything in the way that is not an object is replaced: it cannot be a
/// server map, and refusing to write would strand the client forever.
fn json_map_mut(root: RootPath, value: &mut Value) -> &mut Map<String, Value> {
    let mut node = value;
    for segment in root.segments() {
        if !node.is_object() {
            *node = Value::Object(Map::new());
        }
        node = node
            .as_object_mut()
            .expect("normalized to an object above")
            .entry((*segment).to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    if !node.is_object() {
        *node = Value::Object(Map::new());
    }
    node.as_object_mut().expect("normalized to an object above")
}

pub(crate) fn edit_json(
    root: RootPath,
    value: &mut Value,
    removes: &[String],
    upserts: &[(&str, &Value)],
) {
    let entries = json_map_mut(root, value);
    for name in removes {
        entries.remove(name);
    }
    for (name, entry) in upserts {
        entries.insert((*name).to_owned(), (*entry).clone());
    }
}

fn edit_jsonc(
    root: RootPath,
    node: &jsonc_parser::cst::CstRootNode,
    removes: &[String],
    upserts: &[(&str, &Value)],
) {
    let mut entries = node.object_value_or_set();
    for segment in root.segments() {
        entries = entries.object_value_or_set(segment);
    }
    for name in removes {
        if let Some(prop) = entries.get(name) {
            prop.remove();
        }
    }
    for (name, entry) in upserts {
        let input = json_to_cst(entry);
        match entries.get(name) {
            Some(prop) => prop.set_value(input),
            None => {
                entries.append(name, input);
            }
        }
    }
}

fn edit_toml(
    root: RootPath,
    doc: &mut toml_edit::DocumentMut,
    removes: &[String],
    upserts: &[(&str, &Value)],
) {
    let mut entries = doc.as_table_mut();
    for segment in root.segments() {
        let item = entries
            .entry(segment)
            .or_insert_with(|| toml_edit::Item::Table(implicit_table()));
        if !item.is_table() {
            *item = toml_edit::Item::Table(implicit_table());
        }
        entries = item.as_table_mut().expect("normalized to a table above");
    }
    for name in removes {
        entries.remove(name);
    }
    for (name, entry) in upserts {
        // Entries become section tables (`[mcp_servers.name]`) rather than
        // inline ones: that is how the TOML clients' own docs spell them,
        // and it keeps a long entry readable.
        let mut table = toml_edit::Table::new();
        if let Some(fields) = entry.as_object() {
            for (key, value) in fields {
                table.insert(key, toml_edit::Item::Value(json_to_toml(value)));
            }
        }
        entries.insert(name, toml_edit::Item::Table(table));
    }
}

/// A parent table that only prints its own header once it holds direct
/// values — without this an empty `[mcp_servers]` line precedes every
/// `[mcp_servers.name]` section.
fn implicit_table() -> toml_edit::Table {
    let mut table = toml_edit::Table::new();
    table.set_implicit(true);
    table
}

fn json_to_cst(value: &Value) -> jsonc_parser::cst::CstInputValue {
    use jsonc_parser::cst::CstInputValue;
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(b) => CstInputValue::Bool(*b),
        Value::Number(n) => CstInputValue::Number(n.to_string()),
        Value::String(s) => CstInputValue::String(s.clone()),
        Value::Array(items) => CstInputValue::Array(items.iter().map(json_to_cst).collect()),
        Value::Object(fields) => CstInputValue::Object(
            fields
                .iter()
                .map(|(key, value)| (key.clone(), json_to_cst(value)))
                .collect(),
        ),
    }
}

/// TOML has no null, so one becomes the empty string. No entry mcpgw emits
/// contains a null, and losing the key outright would read as "unset" to a
/// client that does distinguish the two.
fn json_to_toml(value: &Value) -> toml_edit::Value {
    match value {
        Value::Null => toml_edit::Value::from(""),
        Value::Bool(b) => toml_edit::Value::from(*b),
        Value::Number(n) => n.as_i64().map_or_else(
            || toml_edit::Value::from(n.as_f64().unwrap_or_default()),
            toml_edit::Value::from,
        ),
        Value::String(s) => toml_edit::Value::from(s.as_str()),
        Value::Array(items) => items
            .iter()
            .map(json_to_toml)
            .collect::<toml_edit::Array>()
            .into(),
        Value::Object(fields) => fields
            .iter()
            .map(|(key, value)| (key.clone(), json_to_toml(value)))
            .collect::<toml_edit::InlineTable>()
            .into(),
    }
}

fn toml_to_json(item: &toml_edit::Item) -> Value {
    match item {
        toml_edit::Item::None => Value::Null,
        toml_edit::Item::Value(value) => toml_value_to_json(value),
        toml_edit::Item::Table(table) => table
            .iter()
            .map(|(key, item)| (key.to_owned(), toml_to_json(item)))
            .collect::<Map<_, _>>()
            .into(),
        toml_edit::Item::ArrayOfTables(tables) => tables
            .iter()
            .map(|table| {
                table
                    .iter()
                    .map(|(key, item)| (key.to_owned(), toml_to_json(item)))
                    .collect::<Map<_, _>>()
                    .into()
            })
            .collect::<Vec<Value>>()
            .into(),
    }
}

fn toml_value_to_json(value: &toml_edit::Value) -> Value {
    match value {
        toml_edit::Value::String(s) => s.value().as_str().into(),
        toml_edit::Value::Integer(i) => (*i.value()).into(),
        toml_edit::Value::Float(f) => (*f.value()).into(),
        toml_edit::Value::Boolean(b) => (*b.value()).into(),
        // Dates have no JSON counterpart and no MCP entry field; their text
        // form keeps the value visible to a reader instead of dropping it.
        toml_edit::Value::Datetime(d) => d.value().to_string().into(),
        toml_edit::Value::Array(items) => items
            .iter()
            .map(toml_value_to_json)
            .collect::<Vec<Value>>()
            .into(),
        toml_edit::Value::InlineTable(table) => table
            .iter()
            .map(|(key, value)| (key.to_owned(), toml_value_to_json(value)))
            .collect::<Map<_, _>>()
            .into(),
    }
}

fn string_field(obj: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match obj.get(key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("`{key}` is not a string")),
    }
}

fn string_list(obj: &Map<String, Value>, key: &str) -> Result<Vec<String>, String> {
    match obj.get(key) {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::String(s) => Ok(s.clone()),
                _ => Err(format!("`{key}` contains a non-string element")),
            })
            .collect(),
        Some(_) => Err(format!("`{key}` is not an array")),
    }
}

fn string_object(obj: &Map<String, Value>, key: &str) -> Result<BTreeMap<String, String>, String> {
    match obj.get(key) {
        None => Ok(BTreeMap::new()),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(k, v)| match v {
                Value::String(s) => Ok((k.clone(), s.clone())),
                _ => Err(format!("`{key}.{k}` is not a string")),
            })
            .collect(),
        Some(_) => Err(format!("`{key}` is not an object")),
    }
}

fn string_map(map: &BTreeMap<String, String>) -> Value {
    map.iter()
        .map(|(k, v)| (k.clone(), Value::from(v.as_str())))
        .collect::<Map<_, _>>()
        .into()
}
