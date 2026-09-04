//! `mcpgw tools <server>`: which of a server's tools reach a client, what
//! their definitions are pinned as, and the edits to
//! `[servers.NAME.tools]` that change either.
//!
//! The listing connects to the server directly, the way `inspect` does, so
//! it shows the tools the server offers right now rather than the names
//! somebody wrote in the config months ago — which is the whole question
//! when a rule stops matching, and the only way to say whether a definition
//! still matches its pin.

use std::fmt::Write as _;
use std::time::Duration;

use anyhow::Context as _;
use mcpgw_core::pins::{PinFile, PinStore, ToolFingerprint};
use mcpgw_core::probe::fingerprint_tools;
use mcpgw_core::{Config, ConfigStore, Error, Server, ToolRules};
use owo_colors::OwoColorize as _;

#[derive(clap::Args)]
pub struct ToolsArgs {
    /// Server name from the canonical config
    pub name: String,
    #[command(subcommand)]
    pub command: Option<ToolsCommand>,
    /// Connection timeout in seconds, for the listing
    #[arg(long, default_value_t = 10, value_name = "SECS")]
    pub timeout: u64,
}

#[derive(clap::Subcommand)]
pub enum ToolsCommand {
    /// Add names (or `prefix*`) to the server's allow list
    Allow {
        /// Tool names or `prefix*` patterns
        #[arg(required = true, value_name = "TOOL")]
        tools: Vec<String>,
    },
    /// Add names (or `prefix*`) to the server's deny list
    Deny {
        /// Tool names or `prefix*` patterns
        #[arg(required = true, value_name = "TOOL")]
        tools: Vec<String>,
    },
    /// Remove the server's lists entirely, allowing every tool again
    Clear,
    /// Accept the server's current tool definitions as the pinned ones
    Pin {
        /// Print the pinned definitions and the drift since, changing nothing
        #[arg(long)]
        show: bool,
    },
    /// Forget the pinned definitions; the next list pins afresh
    Unpin,
}

pub fn run(args: &ToolsArgs, color: bool) -> anyhow::Result<()> {
    match &args.command {
        Some(ToolsCommand::Allow { tools }) => edit(&args.name, tools, Which::Allow),
        Some(ToolsCommand::Deny { tools }) => edit(&args.name, tools, Which::Deny),
        Some(ToolsCommand::Clear) => clear(&args.name),
        Some(ToolsCommand::Pin { show: true }) => show_pins(&args.name),
        Some(ToolsCommand::Pin { show: false }) => pin(args),
        Some(ToolsCommand::Unpin) => unpin(&args.name),
        None => list(args, color),
    }
}

/// Which list an edit adds to. The other one is the list the same name is
/// taken out of — see [`edit`].
#[derive(Clone, Copy)]
enum Which {
    Allow,
    Deny,
}

impl Which {
    fn word(self) -> &'static str {
        match self {
            Which::Allow => "allowed",
            Which::Deny => "denied",
        }
    }
}

/// Adds `tools` to one list and removes the same entries from the other.
///
/// Both halves, because the lists are read allow-then-deny: `allow foo` on a
/// server whose `deny` still names `foo` would print a confirmation and
/// change nothing a client can see.
fn edit(name: &str, tools: &[String], which: Which) -> anyhow::Result<()> {
    let path = super::canonical_config_path()?;
    let mut store = ConfigStore::edit(&path)?;
    let mut rules = rules_of(store.config(), name)?;
    let (into, out_of) = match which {
        Which::Allow => (&mut rules.allow, &mut rules.deny),
        Which::Deny => (&mut rules.deny, &mut rules.allow),
    };
    for tool in tools {
        out_of.retain(|existing| existing != tool);
        if !into.contains(tool) {
            into.push(tool.clone());
        }
    }
    store.set_tool_rules(name, &rules)?;
    store.save()?;
    println!("{} on {name:?}: {}", which.word(), tools.join(", "));
    Ok(())
}

fn clear(name: &str) -> anyhow::Result<()> {
    let path = super::canonical_config_path()?;
    let mut store = ConfigStore::edit(&path)?;
    let mut rules = rules_of(store.config(), name)?;
    // The lists and not the whole table: `drift = "off"` is not a list, and
    // "allow every tool again" is not a request to start watching the
    // definitions of a server somebody deliberately stopped watching.
    rules.allow.clear();
    rules.deny.clear();
    store.set_tool_rules(name, &rules)?;
    store.save()?;
    println!("cleared the tool lists on {name:?}: every tool is allowed again");
    Ok(())
}

/// The server's current rules, or empty ones for a server that has no table.
fn rules_of(config: &Config, name: &str) -> Result<ToolRules, Error> {
    Ok(server_of(config, name)?.tools.clone().unwrap_or_default())
}

fn server_of<'c>(config: &'c Config, name: &str) -> Result<&'c Server, Error> {
    config
        .servers
        .get(name)
        .ok_or_else(|| Error::UnknownServer {
            name: name.to_owned(),
            available: config.servers.keys().cloned().collect(),
        })
}

fn config_and_server(name: &str) -> anyhow::Result<(Config, Server)> {
    let path = super::canonical_config_path()?;
    let config = Config::load(&path).with_context(|| format!("cannot load {}", path.display()))?;
    let server = server_of(&config, name)?.clone();
    Ok((config, server))
}

fn pin_store() -> anyhow::Result<PinStore> {
    let dir = mcpgw_core::paths::state_dir()
        .context("cannot determine a home directory to resolve the state directory")?;
    Ok(PinStore::under_state_dir(&dir))
}

/// The tools this server's endpoint offers, fingerprinted — which is exactly
/// the list the gateway pins, filter and all.
fn offered(server: &Server, timeout: u64, name: &str) -> anyhow::Result<Vec<ToolFingerprint>> {
    let runtime = tokio::runtime::Runtime::new()?;
    let mut tools = runtime
        .block_on(fingerprinted(name, server, timeout))
        .with_context(|| format!("cannot inspect server {name:?}"))?;
    tools.retain(|tool| server.allows_tool(&tool.name));
    Ok(tools)
}

/// The fingerprints, taken the way the gateway takes them.
///
/// The state directory goes in because a pin has to be read off the same
/// endpoint the gateway reaches, and for an OAuth server that means
/// connecting with the login the gateway holds.
async fn fingerprinted(
    name: &str,
    server: &Server,
    timeout: u64,
) -> Result<Vec<ToolFingerprint>, mcpgw_core::probe::ProbeError> {
    let state_dir = mcpgw_core::paths::state_dir();
    fingerprint_tools(
        name,
        server,
        state_dir.as_deref(),
        Duration::from_secs(timeout),
    )
    .await
}

fn pin(args: &ToolsArgs) -> anyhow::Result<()> {
    let name = &args.name;
    let (_, server) = config_and_server(name)?;
    let tools = offered(&server, args.timeout, name)?;
    let store = pin_store()?;
    let file = store.pin(name, &tools)?;
    println!(
        "pinned {} tool definition(s) for {name:?} — {}",
        file.tools.len(),
        store.path(name).display()
    );
    Ok(())
}

fn unpin(name: &str) -> anyhow::Result<()> {
    // The config is read first so an unknown name is refused before anything
    // on disk is touched.
    config_and_server(name)?;
    let store = pin_store()?;
    if store.remove(name)? {
        println!(
            "unpinned {name:?}: its next tools/list through the gateway pins afresh, \
             and nothing is compared until then"
        );
    } else {
        println!("{name:?} had no pinned tool definitions");
    }
    Ok(())
}

/// `pin --show`: what is on file, without dialing the server.
///
/// Deliberately offline. The question it answers — "what did this server say
/// when I trusted it, and what has it said since" — is answered by the pin
/// file alone, and a server that is down must not stop it being answered.
fn show_pins(name: &str) -> anyhow::Result<()> {
    config_and_server(name)?;
    let store = pin_store()?;
    let Some(file) = store.read(name)? else {
        println!(
            "{name:?} has no pinned tool definitions — the gateway pins them the first \
             time it lists this server, or `mcpgw tools {name} pin` does it now"
        );
        return Ok(());
    };
    print!(
        "{}",
        render_pins(&file, &store.path(name).display().to_string())
    );
    Ok(())
}

fn render_pins(file: &PinFile, path: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} — {} pinned tool(s), pinned {}",
        file.server,
        file.tools.len(),
        stamp(file.pinned_at)
    );
    let _ = writeln!(out, "  {path}");
    out.push('\n');
    let width = file
        .tools
        .keys()
        .map(|tool| tool.chars().count())
        .max()
        .unwrap_or(0);
    for (tool, pin) in &file.tools {
        let pad = " ".repeat(width - tool.chars().count());
        // A short prefix: the whole digest is in the file for anyone
        // comparing two machines, and twelve hex characters is plenty to
        // read a difference off a screen.
        let _ = writeln!(out, "  {tool}{pad}  {}", &pin.hash[..12]);
    }
    out.push('\n');
    if file.drift.is_empty() {
        out.push_str("  no drift since\n");
        return out;
    }
    let _ = writeln!(out, "  drift since ({}):", file.drift.len());
    for event in &file.drift {
        let _ = writeln!(out, "    {} — {}", event.summary(), stamp(event.at));
    }
    out.push_str("\n  run `mcpgw tools ");
    out.push_str(&file.server);
    out.push_str(" pin` to accept the current definitions\n");
    out
}

/// Epoch millis as a coarse age. Relative rather than a local timestamp for
/// the same reason `watch` renders ages: no timezone rendering, and "three
/// days ago" is what the reader is actually asking.
fn stamp(ts_ms: u64) -> String {
    let now = mcpgw_core::capture::now_millis();
    let seconds = now.saturating_sub(ts_ms) / 1000;
    match seconds {
        s if s < 60 => "just now".to_owned(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

/// What the pins make of one tool the server is offering now.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pinned {
    /// The server has no pin file: nothing has been compared yet.
    Unpinned,
    /// Pinned, and still the definition that was pinned.
    Same,
    /// Pinned under this name, with a different definition.
    Changed,
    /// Offered now, absent from the pins.
    New,
    /// Not offered through this endpoint at all, so there is nothing pinned
    /// and nothing to compare.
    Filtered,
}

impl Pinned {
    fn as_str(self) -> &'static str {
        match self {
            Pinned::Unpinned => "unpinned",
            Pinned::Same => "pinned",
            Pinned::Changed => "changed",
            Pinned::New => "new",
            Pinned::Filtered => "-",
        }
    }
}

/// One row of the listing.
struct Row {
    tool: String,
    allowed: bool,
    pinned: Pinned,
}

fn list(args: &ToolsArgs, color: bool) -> anyhow::Result<()> {
    let name = &args.name;
    let (_, server) = config_and_server(name)?;

    let runtime = tokio::runtime::Runtime::new()?;
    let tools = runtime
        .block_on(fingerprinted(name, &server, args.timeout))
        .with_context(|| format!("cannot inspect server {name:?}"))?;
    let pins = pin_store()?.read(name)?;

    let rows: Vec<Row> = tools
        .iter()
        .map(|tool| {
            let allowed = server.allows_tool(&tool.name);
            Row {
                tool: tool.name.clone(),
                allowed,
                pinned: state(pins.as_ref(), tool, allowed, server.drift()),
            }
        })
        .collect();
    print!("{}", render(name, server.tools.as_ref(), &rows, color));
    Ok(())
}

fn state(
    pins: Option<&PinFile>,
    tool: &ToolFingerprint,
    allowed: bool,
    drift: mcpgw_core::Drift,
) -> Pinned {
    // A tool this endpoint does not offer was never pinned, and calling it
    // "new" would read as a change the server had made.
    if !allowed {
        return Pinned::Filtered;
    }
    if !drift.is_watched() {
        return Pinned::Unpinned;
    }
    match pins.and_then(|pins| pins.tools.get(&tool.name)) {
        Some(pin) if pin.hash == tool.hash => Pinned::Same,
        Some(_) => Pinned::Changed,
        None if pins.is_some() => Pinned::New,
        None => Pinned::Unpinned,
    }
}

/// The listing: every tool the server offers, with what the lists and the
/// pins make of it, under a header saying which of both are in force.
fn render(name: &str, rules: Option<&ToolRules>, rows: &[Row], color: bool) -> String {
    let mut out = String::new();
    let heading = if color {
        format!("{name} — {} tool(s)", rows.len())
            .bold()
            .to_string()
    } else {
        format!("{name} — {} tool(s)", rows.len())
    };
    out.push_str(&heading);
    out.push('\n');
    match rules.filter(|rules| !rules.is_empty()) {
        Some(rules) => {
            if !rules.allow.is_empty() {
                let _ = writeln!(out, "  allow = {:?}", rules.allow);
            }
            if !rules.deny.is_empty() {
                let _ = writeln!(out, "  deny  = {:?}", rules.deny);
            }
            if !rules.drift.is_default() {
                let _ = writeln!(out, "  drift = {:?}", rules.drift.as_str());
            }
        }
        // Said out loud rather than left to an all-`allowed` column: the two
        // states look identical in the table and mean different things the
        // moment a tool is added to the server.
        None => out.push_str("  no lists — every tool is allowed\n"),
    }
    out.push('\n');
    if rows.is_empty() {
        out.push_str("  (none)\n");
        return out;
    }
    let width = rows
        .iter()
        .map(|row| row.tool.chars().count())
        .max()
        .unwrap_or(0);
    let mut drifted = false;
    for row in rows {
        let pad = " ".repeat(width - row.tool.chars().count());
        let state = if row.allowed { "allowed" } else { "denied" };
        let state = if color && !row.allowed {
            state.red().to_string()
        } else {
            state.to_owned()
        };
        drifted |= matches!(row.pinned, Pinned::Changed | Pinned::New);
        let pinned = row.pinned.as_str();
        let pinned = if color && matches!(row.pinned, Pinned::Changed | Pinned::New) {
            pinned.yellow().to_string()
        } else {
            pinned.to_owned()
        };
        let _ = writeln!(out, "  {}{pad}  {state:<8}  {pinned}", row.tool);
    }
    if drifted {
        let _ = writeln!(
            out,
            "\n  definitions have moved since they were pinned — \
             run `mcpgw tools {name} pin` to accept them"
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use mcpgw_core::pins::{Change, DriftEvent, PinFile, ToolPin};

    use super::*;

    fn row(tool: &str, allowed: bool, pinned: Pinned) -> Row {
        Row {
            tool: tool.to_owned(),
            allowed,
            pinned,
        }
    }

    fn rows() -> Vec<Row> {
        vec![
            row("echo", true, Pinned::Same),
            row("reverse", false, Pinned::Filtered),
        ]
    }

    #[test]
    fn renders_the_lists_and_the_column() {
        let rules = ToolRules {
            allow: vec!["echo".to_owned()],
            ..ToolRules::default()
        };
        insta::assert_snapshot!(render("fx", Some(&rules), &rows(), false));
    }

    #[test]
    fn a_server_with_no_lists_says_so() {
        let rendered = render("fx", None, &[row("echo", true, Pinned::Unpinned)], false);
        assert!(rendered.contains("every tool is allowed"), "{rendered}");
    }

    #[test]
    fn a_changed_definition_is_marked_and_named_in_a_footer() {
        let rendered = render(
            "fx",
            None,
            &[
                row("echo", true, Pinned::Changed),
                row("exfiltrate", true, Pinned::New),
            ],
            false,
        );
        assert!(
            rendered.contains("echo        allowed   changed"),
            "{rendered}"
        );
        assert!(rendered.contains("exfiltrate  allowed   new"), "{rendered}");
        assert!(rendered.contains("mcpgw tools fx pin"), "{rendered}");
    }

    /// A server whose definitions all match must not be told to do anything.
    #[test]
    fn an_unchanged_server_gets_no_footer() {
        let rendered = render("fx", None, &rows(), false);
        assert!(!rendered.contains("pin`"), "{rendered}");
    }

    #[test]
    fn pin_show_prints_the_hashes_and_the_drift() {
        let file = PinFile {
            version: mcpgw_core::pins::VERSION,
            server: "fx".to_owned(),
            pinned_at: mcpgw_core::capture::now_millis(),
            tools: [(
                "echo".to_owned(),
                ToolPin {
                    hash: "0123456789abcdef0123".to_owned(),
                    desc_len: 12,
                },
            )]
            .into_iter()
            .collect(),
            drift: vec![DriftEvent {
                tool: "echo".to_owned(),
                change: Change::Changed,
                at: mcpgw_core::capture::now_millis(),
                desc_len_before: Some(12),
                desc_len_after: Some(384),
            }],
        };
        let rendered = render_pins(&file, "/state/pins/fx.json");
        assert!(rendered.contains("0123456789ab"), "{rendered}");
        assert!(
            rendered.contains("echo (changed, 12 → 384 bytes)"),
            "{rendered}"
        );
        assert!(rendered.contains("mcpgw tools fx pin"), "{rendered}");
        // The description itself never reaches this output, only its size.
        assert!(!rendered.contains("desc_len"), "{rendered}");
    }
}
