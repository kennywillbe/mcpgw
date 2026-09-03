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

use std::collections::{BTreeMap, BTreeSet};

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
    /// Windsurf's shape: the `mcpServers` rules with the remote URL spelled
    /// `serverUrl`.
    Windsurf,
    /// Zed's `context_servers` shape: the `mcpServers` fields, no `type`,
    /// and a mandatory `source` on anything written.
    Zed,
    /// Cline's shape, shared by its VS Code extension and its standalone
    /// CLI: the `mcpServers` fields with `disabled` as the off switch, a
    /// remote `type` spelled `streamableHttp`, and an `autoApprove` list of
    /// tool names mcpgw has no counterpart for.
    Cline,
    /// Amp's shape: the `mcpServers` fields — `disabled` included — with no
    /// `type` at all, so a remote entry is a bare `url` and the transport is
    /// whichever target field is present.
    Amp,
    /// Zoo Code's shape: Cline's fields, read by exactly Cline's rules, with
    /// the remote `type` spelled `streamable-http`. The two cannot share one
    /// variant because Zoo Code validates the entry against a `z.enum` that
    /// lists the hyphenated spelling alone — Cline's `streamableHttp` is a
    /// schema error there, not a tolerated alias.
    ZooCode,
}

/// A write that refused rather than replace what it found.
///
/// The read side reports a root key holding something that is not an object
/// as a [`Problem`](crate::clients::Problem) and returns no servers. Before
/// this existed the write side walked the same shape and overwrote it, so the
/// two halves disagreed about whether the file was usable and a hand-written
/// value was destroyed without a word. Refusing is the symmetric answer:
/// nothing on that path can be a server map, and only the user knows what
/// they meant by it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotAnObject {
    /// Dotted path of the offending key; empty for the document root.
    pub path: String,
}

impl std::fmt::Display for NotAnObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.path.is_empty() {
            // The same wording `ClientKind::read_text` uses for this shape.
            f.write_str("root is not an object")
        } else {
            write!(f, "`{}` is not an object", self.path)
        }
    }
}

impl std::error::Error for NotAnObject {}

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
            Self::Gemini => parse_gemini(obj),
            Self::Codex => parse_codex(obj),
            Self::Opencode => parse_opencode(obj),
            Self::Windsurf => parse_windsurf(obj),
            // Zoo Code inherited Cline's read rules through Roo Code,
            // untyped remote entry included, so it reads by them too.
            Self::Cline | Self::ZooCode => parse_cline(obj),
            // Zed adds `source` to the shared fields and nothing else, and
            // that one is read as absent: an entry an extension installed
            // carries a source of its own, and refusing to read it would
            // hide a server the user does have.
            //
            // Amp only ever writes a subset of the shared fields, so reading
            // it by the shared rules costs nothing and gains leniency: a
            // hand-written `type` is understood rather than rejected.
            Self::McpServers | Self::VsCode | Self::Zed | Self::Amp => parse_mcp_servers(obj),
        }
        .map(|(server, note)| {
            // Only a remote entry can have a credential the client holds:
            // a local one is a process mcpgw starts, and its environment
            // comes across whole.
            let auth = match server.transport {
                Transport::Http { .. } => client_managed_auth(self, obj),
                Transport::Stdio { .. } => None,
            };
            let note = match (note, auth) {
                (Some(note), Some(auth)) => Some(format!("{note}; {auth}")),
                (note, auth) => note.or(auth),
            };
            (server, note)
        })
    }

    /// Fields on an entry that belong to the client, not to mcpgw.
    ///
    /// [`EntrySchema::emit`] builds an entry from the canonical server alone,
    /// and sync writes that value over whatever was there. For most clients
    /// that is right — every field they define has a canonical counterpart.
    /// These do not: the user sets them from inside the client and mcpgw has
    /// nothing to say about them, so a rewrite carries them over instead of
    /// resetting them. The line is switches the user flips (the off switch,
    /// the per-tool permission lists), not server configuration a canonical
    /// field could one day express.
    #[must_use]
    pub fn preserved_fields(self) -> &'static [&'static str] {
        match self {
            // `disabled` is Cline's off switch and `autoApprove` the list of
            // tools it runs without asking. Emitting neither turned a server
            // the user had switched off back on, every single sync.
            Self::Cline => &["disabled", "autoApprove"],
            // Zoo Code inherited Cline's two through Roo Code and kept Roo's
            // spelling of the allow list beside them, plus its deny list; a
            // file carried over from either fork may use any of them.
            Self::ZooCode => &["disabled", "autoApprove", "alwaysAllow", "disabledTools"],
            // Amp documents the off switch and no permission list.
            Self::Amp => &["disabled"],
            // Cursor spells its off switch `disabled` too, but this stays
            // empty until that is asked for: the shared schema is four
            // clients wide and only one of them defines the field.
            Self::McpServers
            | Self::VsCode
            | Self::Gemini
            | Self::Codex
            | Self::Opencode
            | Self::Windsurf
            | Self::Zed => &[],
        }
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
                    Self::Windsurf => {
                        // Windsurf reads the remote URL from `serverUrl`
                        // alone, and infers the transport from its presence.
                        obj.insert("serverUrl".to_owned(), url.as_str().into());
                        "headers"
                    }
                    Self::Zed | Self::Amp => {
                        // A remote entry is a bare `url` in both: neither has
                        // a `type` field, and both infer the transport from
                        // which target field is present. Writing the `type`
                        // the other JSON clients take would be a field their
                        // schemas do not define.
                        obj.insert("url".to_owned(), url.as_str().into());
                        "headers"
                    }
                    Self::Cline => {
                        // Cline's own spelling of streamable HTTP. An entry
                        // left untyped would read back as the legacy SSE
                        // transport, which is a different protocol.
                        obj.insert("type".to_owned(), "streamableHttp".into());
                        obj.insert("url".to_owned(), url.as_str().into());
                        "headers"
                    }
                    Self::ZooCode => {
                        // Zoo Code's own spelling of the same transport
                        // Cline calls `streamableHttp`. Its schema accepts
                        // this one alone, and an untyped entry would read
                        // back as legacy SSE.
                        obj.insert("type".to_owned(), "streamable-http".into());
                        obj.insert("url".to_owned(), url.as_str().into());
                        "headers"
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
        // Stdio only, and deliberately not on a remote entry.
        //
        // Zed's settings enum was tagged on `source`, and an entry without it
        // was dropped without a word — the commonest reason a hand-added
        // server never showed up. `custom` is the value for one the user
        // configured, and it is the only one mcpgw ever writes: any other
        // source claims the entry came from an extension Zed would then load
        // code from (GHSA-cv6g-cmxc-vw8j).
        //
        // The enum is untagged now, with a remote variant whose whole shape
        // is `{url, headers}`. `source` is not one of its fields, and a
        // discriminator naming the variant that carries `command` is at best
        // ignored on a `url` entry and at worst the reason it does not
        // deserialize at all — a synced server Zed silently never loads. So
        // it stays on the stdio shape it was always about, where a Zed old
        // enough to need it still gets it and a current one ignores it.
        if matches!(self, Self::Zed) && matches!(server.transport, Transport::Stdio { .. }) {
            obj.insert("source".to_owned(), "custom".into());
        }
        Value::Object(obj)
    }
}

/// The tail every client-managed-auth note ends with, and the whole of what
/// [`is_client_managed_auth`] recognises.
const NOT_CARRIED_OVER: &str = "not carried over";

/// Whether `note` is the one an entry gets because its client holds the
/// credential itself.
///
/// A predicate over the text rather than a flag on the read: a note travels
/// from a client read through [`crate::clients::Problem`] and an import
/// candidate as a string, and threading a second channel the whole way for
/// one bit would touch every one of those types. The text is generated in
/// exactly one place — [`client_managed_auth`] — which is what keeps the two
/// halves honest.
#[must_use]
pub fn is_client_managed_auth(note: &str) -> bool {
    note.ends_with(NOT_CARRIED_OVER)
}

/// The note for a remote entry whose client mints or stores the credential
/// itself, or `None` when this client has no such field.
///
/// Three of the thirteen clients do. The other ten spell a remote server's
/// credential as a header, which is a value the canonical config has a field
/// for and imports verbatim; there is nothing to warn about there, and no
/// marker to invent. What these three carry cannot be copied out: an OAuth
/// token in the client's own store, or a credential it mints per request.
/// The imported URL is therefore real and the authentication is not, and the
/// entry says so rather than looking healthy until it is called.
fn client_managed_auth(schema: EntrySchema, obj: &Map<String, Value>) -> Option<String> {
    let note = |client: &str, kind: &str| format!("{client}-managed {kind} {NOT_CARRIED_OVER}");
    match schema {
        // `auth` is Codex's own OAuth block; `bearer_token_env_var` names
        // the variable it reads a token out of at call time.
        EntrySchema::Codex => (obj.contains_key("auth")
            || obj.contains_key("bearer_token_env_var"))
        .then(|| note("codex", "auth")),
        // opencode keeps the tokens in its own store, and this flag is what
        // switches that on — so a `false` here is not a marker.
        EntrySchema::Opencode => (!matches!(obj.get("oauth"), None | Some(Value::Bool(false))))
            .then(|| note("opencode", "oauth")),
        // Gemini CLI either brokers the flow itself (`oauth`) or signs the
        // request with a Google credential — ADC, or an impersonated service
        // account — which is what `authProviderType` selects.
        EntrySchema::Gemini => (obj.contains_key("oauth") || obj.contains_key("authProviderType"))
            .then(|| note("gemini", "auth")),
        EntrySchema::McpServers
        | EntrySchema::VsCode
        | EntrySchema::Windsurf
        | EntrySchema::Zed
        | EntrySchema::Cline
        | EntrySchema::Amp
        | EntrySchema::ZooCode => None,
    }
}

/// The `mcpServers` entry shape shared by Claude Desktop, Claude Code, Cursor
/// and VS Code: an optional `type`, and otherwise `command` for stdio or
/// `url` for remote.
fn parse_mcp_servers(obj: &Map<String, Value>) -> Result<(Server, Option<String>), String> {
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
        // One transport, four spellings in the wild: the MCP spec's own
        // `http`, the two hyphen/underscore forms clients copied from the
        // protocol name, and Cline's camelCase. Reading any of them as
        // "unknown transport" would drop a working server.
        Some("http" | "streamable-http" | "streamable_http" | "streamableHttp") => false,
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

    // Cursor and Cline both switch an entry off in place, and both spell it
    // `disabled` — the inverse of the canonical flag, so an absent field
    // means enabled.
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

/// Windsurf's entry shape: the `mcpServers` rules with one renamed field, so
/// it is read by renaming that field back and deferring to them.
///
/// `serverUrl` is what Windsurf's own docs and its UI write; a plain `url`
/// turns up in enough third-party examples to be worth accepting, and an
/// entry carrying both resolves the way Windsurf itself does.
///
/// Values may hold Windsurf's `${env:VAR}` / `${file:/path}` interpolation,
/// which is kept verbatim: expanding it here would bake a secret into the
/// canonical config.
fn parse_windsurf(obj: &Map<String, Value>) -> Result<(Server, Option<String>), String> {
    let Some(server_url) = obj.get("serverUrl") else {
        return parse_mcp_servers(obj);
    };
    let mut renamed = obj.clone();
    renamed.insert("url".to_owned(), server_url.clone());
    renamed.remove("serverUrl");

    let (server, shared) = parse_mcp_servers(&renamed)?;
    let precedence = obj
        .contains_key("url")
        .then(|| "`url` ignored: `serverUrl` takes precedence".to_owned());
    let note = match (precedence, shared) {
        (Some(precedence), Some(shared)) => Some(format!("{precedence}; {shared}")),
        (precedence, shared) => precedence.or(shared),
    };
    Ok((server, note))
}

/// Cline's entry shape, shared with the Zoo Code fork: the `mcpServers`
/// rules — `disabled` included — with
/// one transport difference, so it is read by deferring to them and then
/// correcting that one case.
///
/// A remote entry with no `type` is the legacy SSE transport, not streamable
/// HTTP: Cline shipped remote servers before the streamable transport
/// existed, and files written then still carry the bare `url`. The shared
/// rules read an untyped `url` as http without comment, which is right for
/// every other client and wrong here.
///
/// `autoApprove` is a list of tool names Cline runs without asking. It has no
/// canonical counterpart, so it is read as absent — an entry mcpgw does not
/// manage keeps it verbatim because sync never rewrites it. Zoo Code's own
/// extras (`alwaysAllow`, `disabledTools`, `watchPaths`, `cwd`, `timeout`)
/// are ignored on read for the same reason and survive for the same one.
fn parse_cline(obj: &Map<String, Value>) -> Result<(Server, Option<String>), String> {
    let (server, note) = parse_mcp_servers(obj)?;
    if !obj.contains_key("type") && matches!(server.transport, Transport::Http { .. }) {
        return Ok((
            server,
            Some("legacy `sse` transport read as http".to_owned()),
        ));
    }
    Ok((server, note))
}

/// Gemini CLI's entry shape, which shares no discriminator with the others:
/// there is no `type`, the transport is whichever target field is present,
/// and the two remote fields are different protocols.
///
/// Everything else an entry may carry (`cwd`, `timeout`, `trust`,
/// `includeTools`, …) is ignored on read — those fields have no canonical
/// counterpart, and an entry mcpgw does not manage keeps them verbatim
/// because sync never rewrites it. The two that say Gemini authenticates the
/// server itself are read for their note alone: see [`client_managed_auth`].
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

    let transport = match (command, url) {
        (Some(command), None) => Transport::Stdio {
            command,
            args: string_list(obj, "args")?,
            env: string_object(obj, "env")?,
        },
        (None, Some(url)) => Transport::Http {
            url,
            headers: string_object(obj, "http_headers")?,
        },
        (Some(_), Some(_)) => return Err("has both `command` and `url`".to_owned()),
        (None, None) => return Err("has neither `command` nor `url`".to_owned()),
    };

    Ok((
        Server {
            enabled: !matches!(obj.get("enabled"), Some(Value::Bool(false))),
            tags: Vec::new(),
            transport,
        },
        None,
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
        None,
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
    /// The whole document as canonical JSON.
    ///
    /// Almost everything sync needs is inside the server map, which is why
    /// [`ClientDocument::entries`] is the usual read. A client that keeps
    /// part of a server's state elsewhere in the file — Gemini's
    /// `mcp.excluded` — needs the rest of it too.
    #[must_use]
    pub fn to_value(&self) -> Value {
        match self {
            Self::Json(value) => value.clone(),
            Self::Jsonc(node) => node.to_serde_value().unwrap_or_default(),
            Self::Toml(doc) => toml_to_json(doc.as_item()),
        }
    }

    /// The client's server map as canonical JSON.
    ///
    /// An absent or wrongly typed map reads as empty: sync only ever adds to
    /// it, so "no map" and "not a map" lead to the same plan.
    #[must_use]
    pub fn entries(&self, root: RootPath) -> Map<String, Value> {
        match self {
            Self::Json(value) => json_entries(root, value),
            other => json_entries(root, &other.to_value()),
        }
    }

    /// Upserts `upserts` and deletes `removes` inside the server map,
    /// creating the map (and any missing parent) if needed.
    ///
    /// Entries not named here are never rewritten — that is what keeps a
    /// foreign entry's own formatting, and in JSONC/TOML its comments.
    ///
    /// # Errors
    ///
    /// Returns [`NotAnObject`] when a key on the way to the server map holds
    /// something that is not one. Nothing is written in that case: see the
    /// type's own docs for why refusing beats replacing.
    pub fn edit(
        &mut self,
        root: RootPath,
        removes: &[String],
        upserts: &[(&str, &Value)],
    ) -> Result<(), NotAnObject> {
        match self {
            Self::Json(value) => edit_json(root, value, removes, upserts),
            Self::Jsonc(node) => edit_jsonc(root, node, removes, upserts),
            Self::Toml(doc) => edit_toml(root, doc, removes, upserts),
        }
    }

    /// Drops every string in `names` from the array at `path`.
    ///
    /// A missing key, a missing array or a non-string element is left alone:
    /// the array is the client's own state, so this only ever takes names out
    /// of a list that is already there — it never creates one, never reorders
    /// what stays, and never touches a name the caller did not ask for.
    pub fn remove_from_string_array(&mut self, path: &[&str], names: &BTreeSet<String>) {
        if names.is_empty() {
            return;
        }
        match self {
            Self::Json(value) => {
                let Some(array) = path
                    .iter()
                    .try_fold(&mut *value, |node, key| node.get_mut(*key))
                    .and_then(Value::as_array_mut)
                else {
                    return;
                };
                array.retain(|item| !item.as_str().is_some_and(|s| names.contains(s)));
            }
            Self::Jsonc(node) => {
                let Some((last, parents)) = path.split_last() else {
                    return;
                };
                let Some(object) = node.object_value() else {
                    return;
                };
                let Some(object) = parents
                    .iter()
                    .try_fold(object, |object, key| object.object_value(key))
                else {
                    return;
                };
                let Some(array) = object.array_value(last) else {
                    return;
                };
                for element in array.elements() {
                    if element
                        .as_string_lit()
                        .and_then(|lit| lit.decoded_value().ok())
                        .is_some_and(|value| names.contains(&value))
                    {
                        element.remove();
                    }
                }
            }
            Self::Toml(doc) => {
                let Some(array) = path
                    .iter()
                    .try_fold(doc.as_item_mut(), |item, key| {
                        item.as_table_like_mut()?.get_mut(key)
                    })
                    .and_then(toml_edit::Item::as_array_mut)
                else {
                    return;
                };
                array.retain(|item| !item.as_str().is_some_and(|s| names.contains(s)));
            }
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
///
/// A *missing* key along the path is created — that is how a client with no
/// MCP config yet gets one. A key that exists but holds something other than
/// an object is refused, mirroring [`RootPath::locate_in`] exactly; see
/// [`NotAnObject`].
fn json_map_mut(root: RootPath, value: &mut Value) -> Result<&mut Map<String, Value>, NotAnObject> {
    let mut node = value;
    for (depth, segment) in root.segments().iter().enumerate() {
        let Some(object) = node.as_object_mut() else {
            return Err(NotAnObject {
                path: root.segments()[..depth].join("."),
            });
        };
        node = object
            .entry((*segment).to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    node.as_object_mut().ok_or_else(|| NotAnObject {
        path: root.display(),
    })
}

pub(crate) fn edit_json(
    root: RootPath,
    value: &mut Value,
    removes: &[String],
    upserts: &[(&str, &Value)],
) -> Result<(), NotAnObject> {
    let entries = json_map_mut(root, value)?;
    for name in removes {
        entries.remove(name);
    }
    for (name, entry) in upserts {
        entries.insert((*name).to_owned(), (*entry).clone());
    }
    Ok(())
}

fn edit_jsonc(
    root: RootPath,
    node: &jsonc_parser::cst::CstRootNode,
    removes: &[String],
    upserts: &[(&str, &Value)],
) -> Result<(), NotAnObject> {
    // `_or_create` rather than `_or_set`: the first creates a missing key and
    // returns `None` for one that is not an object, the second overwrites it.
    let mut entries = node.object_value_or_create().ok_or_else(|| NotAnObject {
        path: String::new(),
    })?;
    for (depth, segment) in root.segments().iter().enumerate() {
        entries = entries
            .object_value_or_create(segment)
            .ok_or_else(|| NotAnObject {
                path: root.segments()[..=depth].join("."),
            })?;
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
    Ok(())
}

fn edit_toml(
    root: RootPath,
    doc: &mut toml_edit::DocumentMut,
    removes: &[String],
    upserts: &[(&str, &Value)],
) -> Result<(), NotAnObject> {
    let mut entries = doc.as_table_mut();
    for (depth, segment) in root.segments().iter().enumerate() {
        // `mcp_servers = { linear = { url = "…" } }` is a legitimate server
        // map — valid TOML the reader accepts and reports entry by entry, so
        // replacing it deleted foreign entries sync had just called
        // untouched. An inline table cannot hold the section tables written
        // below, so normalize it into one instead, keeping every entry
        // (the canonical store does the same to its own `servers` key).
        let mut normalized = false;
        if let Some(item) = entries.get_mut(segment)
            && let toml_edit::Item::Value(toml_edit::Value::InlineTable(inline)) = item
        {
            *item = toml_edit::Item::Table(std::mem::take(inline).into_table());
            normalized = true;
        }
        // The key kept the spacing it had as an assignment, which reads
        // `[mcp_servers ]` once it is a section header.
        if normalized && let Some(mut key) = entries.key_mut(segment) {
            key.leaf_decor_mut().clear();
        }
        let item = entries
            .entry(segment)
            .or_insert_with(|| toml_edit::Item::Table(implicit_table()));
        let Some(table) = item.as_table_mut() else {
            return Err(NotAnObject {
                path: root.segments()[..=depth].join("."),
            });
        };
        entries = table;
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
    Ok(())
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
