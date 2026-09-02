use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use mcpgw_core::clients::codec::ClientDocument;
use mcpgw_core::sync::{
    GATEWAY_NAME, SyncPlan, apply_plan_to, gateway_server, per_server_gateway_servers,
    plan_client_context, plan_sync,
};
use mcpgw_core::{
    ClientKind, Config, Detection, Error, Server, backup, paths, state::ManagedState,
};
use owo_colors::OwoColorize as _;

// The flags are a flat description of a command line, not state to model: how
// they combine is already spelled out by clap's own `conflicts_with` and
// `requires`, and hiding them behind an enum would only move that away from
// the place `--help` is generated from.
#[allow(clippy::struct_excessive_bools)]
#[derive(clap::Args)]
pub struct SyncArgs {
    /// Only sync these clients (repeatable)
    #[arg(
        long = "client",
        value_name = "ID",
        long_help = super::client_ids_help("Only sync these clients")
    )]
    pub clients: Vec<String>,
    /// Show what would change without writing anything
    #[arg(long)]
    pub dry_run: bool,
    /// Restore each selected client's config from its most recent backup
    #[arg(long, conflicts_with = "dry_run")]
    pub rollback: bool,
    /// Accepted and ignored: syncing through the gateway is the only mode.
    /// Kept for one release so scripts and docs that spell it keep working.
    #[arg(long, hide = true)]
    pub gateway: bool,
    /// Write one `mcpgw` entry for the whole gateway instead of one entry per
    /// server
    #[arg(long, conflicts_with = "rollback")]
    pub aggregate: bool,
    /// URL of the gateway the entries point at
    #[arg(long, default_value = super::connect::DEFAULT_URL, value_name = "URL")]
    pub gateway_url: String,
}

pub fn run(args: &SyncArgs, color: bool) -> anyhow::Result<()> {
    let targets = super::select_clients(&args.clients)?;
    let state_dir =
        paths::state_dir().context("cannot determine a home directory for the state dir")?;

    if args.gateway {
        println!(
            "{}",
            crate::ui::dim(
                "note: --gateway is the only mode now; the flag does nothing and will be removed",
                color
            )
        );
    }

    if args.rollback {
        return rollback(&targets, &state_dir);
    }

    // Aggregate mode writes one synthetic entry per client, so the canonical
    // servers are irrelevant there — an unreadable config must not block it.
    // Per-server mode mirrors those servers by name and needs them.
    let canonical = if args.aggregate {
        BTreeMap::new()
    } else {
        let config_path = super::canonical_config_path()?;
        match Config::load(&config_path) {
            Ok(config) => config.servers,
            // An absent canonical config means "manage nothing": previously
            // managed entries get removed, everything else is untouched.
            Err(Error::NotFound { .. }) => BTreeMap::new(),
            Err(err) => return Err(err.into()),
        }
    };

    let bridge = announce_mode(args)?;

    let state_path = state_dir.join("managed.json");
    // Held across the whole load→modify→save window: a second `mcpgw sync
    // --client other` running at the same time would otherwise write back a
    // state it read before this run's changes and drop them.
    let _state_lock = ManagedState::lock(&state_path)?;
    let mut state = ManagedState::load(&state_path)?;

    // Set at the end of the run rather than after the first client: two
    // clients flipped by one command both deserve the explanation, and the
    // flag is what stops the *next* run from repeating it.
    let mut notified = false;
    for kind in targets {
        let heading = |text: &str| {
            if color {
                println!("{}", format!("{} — {text}", kind.display_name()).bold());
            } else {
                println!("{} — {text}", kind.display_name());
            }
        };
        let managed = state.clients.get(kind.id()).cloned().unwrap_or_default();
        let desired = gateway_entries(kind, args, &canonical, &bridge)?;
        let mut planned = match plan_client(kind, &desired, &managed)? {
            Planned::Ready(planned) => planned,
            Planned::Skipped(reason) => {
                heading(&reason);
                continue;
            }
        };

        heading(&describe(&planned.plan));
        print_plan_lines(&planned.plan, color);

        if !planned.plan.has_changes() {
            continue;
        }
        if args.dry_run {
            continue;
        }

        // Asked before the plan is applied: afterwards the document holds the
        // gateway entries and every answer is "already there".
        let migrating = !state.migrated && planned.migrates_to_gateway(&args.gateway_url);

        match apply_client(&mut planned, &mut state, &state_dir, &state_path)? {
            Applied::Refused(reason) => println!("  {reason}"),
            Applied::Written if migrating => {
                print_migration_notice(color);
                notified = true;
            }
            Applied::Written => {}
        }
    }

    if notified {
        state.migrated = true;
        state.save(&state_path)?;
    }
    Ok(())
}

/// The one-time explanation of what just happened to a client's entries.
///
/// A user who never opted into a gateway sees their harness config rewritten
/// to point at a process they have not heard of; this is the paragraph that
/// makes that legible, names the one way it can now fail, and gives the way
/// back. It is deliberately printed by `sync` and not only by the wizard —
/// most existing installs will meet the flip through a plain `mcpgw sync`.
fn print_migration_notice(color: bool) {
    println!();
    for line in [
        "These entries used to point straight at the servers. They now point at mcpgw,",
        "which forwards to the same servers — same names, same tools.",
        "",
        "One thing changed: if the gateway isn't running, they won't answer.",
        "`mcpgw daemon status` tells you, `mcpgw daemon install` keeps it running.",
        "",
        "Undo everything this run did: mcpgw sync --rollback",
    ] {
        if line.is_empty() {
            println!();
        } else {
            println!("  {}", crate::ui::dim(line, color));
        }
    }
}

/// One client's sync, read and planned but not yet applied.
///
/// It exists so the wizard's sync step can show a plan for every client, ask
/// once for the whole set, and then apply exactly what `mcpgw sync` applies —
/// backups, state bookkeeping and all — rather than growing a second copy of
/// any of it.
pub struct PlannedClient {
    pub kind: ClientKind,
    pub path: PathBuf,
    /// Whether the file was already there. One that was not is created, and
    /// there is nothing to back up. Public because `eject` reads it to make
    /// the opposite decision: it never creates a client config.
    pub exists: bool,
    doc: ClientDocument,
    pub plan: SyncPlan,
}

/// The outcome of reading and planning one client.
pub enum Planned {
    Ready(Box<PlannedClient>),
    /// Why this client has nothing to plan, in the words `sync` prints.
    Skipped(String),
}

/// Reads one client and plans `desired` over what it holds.
///
/// # Errors
///
/// Returns a failure only if a client file that is there cannot be read; a
/// file that cannot be *parsed* is a skip rather than an error, because the
/// remaining clients are still worth syncing.
pub fn plan_client(
    kind: ClientKind,
    desired: &BTreeMap<String, Server>,
    managed: &BTreeSet<String>,
) -> anyhow::Result<Planned> {
    let (path, exists) = match resolve_target(kind) {
        Ok(target) => target,
        Err(reason) => return Ok(Planned::Skipped(reason.to_owned())),
    };

    let codec = kind.codec();
    let doc = if exists {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        match codec.parse_document(&text) {
            Ok(doc) => doc,
            // Hand-broken, or JSONC in a client whose format is strict
            // JSON: refuse to rewrite what we cannot faithfully parse.
            Err(err) => {
                return Ok(Planned::Skipped(format!(
                    "skipped: {} is not {} ({err}); fix or sync it manually",
                    path.display(),
                    codec.format_name()
                )));
            }
        }
    } else {
        codec.empty_document()
    };

    let current = doc.entries(codec.root);
    let mut plan = plan_sync(kind, &current, desired, managed);
    plan_client_context(kind, &doc.to_value(), &mut plan);

    Ok(Planned::Ready(Box::new(PlannedClient {
        kind,
        path,
        exists,
        doc,
        plan,
    })))
}

impl PlannedClient {
    /// Whether applying this plan moves entries that dial their servers
    /// directly onto the gateway.
    ///
    /// Only updates can: an add writes an entry that was not there (a fresh
    /// install, nothing to explain) and a remove takes one away. Must be
    /// asked before [`apply_client`] — afterwards the document holds the new
    /// entries and every one of them aims at the gateway.
    #[must_use]
    pub fn migrates_to_gateway(&self, gateway_url: &str) -> bool {
        let codec = self.kind.codec();
        let current = self.doc.entries(codec.root);
        self.plan.updates.iter().any(|name| {
            current.get(name).is_some_and(|entry| {
                // An entry mcpgw cannot read back is left out rather than
                // guessed at: the notice describes a move we can see.
                codec.entries.parse(entry).is_ok_and(|(server, _)| {
                    !mcpgw_core::doctor::aims_at_gateway(&server, gateway_url)
                })
            })
        })
    }
}

/// What applying a planned client did.
pub enum Applied {
    Written,
    /// The plan could not be applied to the parsed document, so nothing was
    /// written; the string says why, ready to print.
    Refused(String),
}

/// Writes one planned client: the document, then a backup of what is on
/// disk, then the state claim, then the file itself.
///
/// # Errors
///
/// Returns a failure if the backup, the state file or the client file cannot
/// be written.
pub fn apply_client(
    planned: &mut PlannedClient,
    state: &mut ManagedState,
    state_dir: &Path,
    state_path: &Path,
) -> anyhow::Result<Applied> {
    // In memory and before anything on disk moves, so a refusal costs
    // neither a backup nor a state entry claiming what was never written.
    if let Err(err) = apply_plan_to(planned.kind, &mut planned.doc, &planned.plan) {
        return Ok(Applied::Refused(format!(
            "refused: {err} in {} — nothing written; `mcpgw doctor` reports the same problem",
            planned.path.display()
        )));
    }
    if planned.exists {
        backup::backup_file(state_dir, planned.kind.id(), &planned.path)?;
    }
    // Intent first: the state file records what this run is about to write
    // *before* the client file is touched. A crash in between then leaves
    // entries claimed but absent, which the next sync sees as plain adds and
    // repairs. The reverse order fails the other way — entries in the client
    // file that mcpgw never claimed are foreign forever, and sync refuses to
    // touch them.
    state
        .clients
        .insert(planned.kind.id().to_owned(), planned.plan.managed_after());
    state.save(state_path)?;
    write_text(&planned.path, &planned.doc.to_text()?)?;
    Ok(Applied::Written)
}

/// Prints what this run is about to do and resolves the stdio bridge command.
///
/// The bridge is resolved once for the whole run: probing PATH per client
/// would give the same answer.
fn announce_mode(args: &SyncArgs) -> anyhow::Result<String> {
    // Checked before a single client is touched, in both modes: a base URL
    // that cannot take an endpoint path is wrong for all of them, and failing
    // halfway would leave some clients rewritten and some not.
    mcpgw_core::endpoints::per_server_url(&args.gateway_url, "probe")
        .with_context(|| format!("--gateway-url {} is not a URL", args.gateway_url))?;
    if args.aggregate {
        println!(
            "gateway mode — every client gets a single `{GATEWAY_NAME}` entry pointing at {}",
            args.gateway_url
        );
    } else {
        println!(
            "gateway mode — every enabled server keeps its name and points at \
             its own endpoint on the gateway at {} (serve it with `mcpgw \
             serve`)",
            args.gateway_url
        );
    }
    Ok(bridge_command())
}

/// The entries a client should hold: every enabled server, reached through
/// the gateway.
///
/// Per-server they carry the canonical names, so a client that mcpgw synced
/// before keeps its entry names and the rewrite is plain updates. Aggregate
/// it is the single synthetic entry, so everything an earlier sync managed —
/// per-server entries included — falls out as removes.
fn gateway_entries(
    kind: ClientKind,
    args: &SyncArgs,
    canonical: &BTreeMap<String, mcpgw_core::Server>,
    bridge: &str,
) -> anyhow::Result<BTreeMap<String, mcpgw_core::Server>> {
    if args.aggregate {
        return Ok(BTreeMap::from([(
            GATEWAY_NAME.to_owned(),
            gateway_server(kind, &args.gateway_url, bridge),
        )]));
    }
    Ok(per_server_gateway_servers(
        kind,
        canonical,
        &args.gateway_url,
        bridge,
    )?)
}

/// The file this client's sync reads and writes, and whether it is there
/// already — or the reason there is nothing to sync.
fn resolve_target(kind: ClientKind) -> Result<(PathBuf, bool), &'static str> {
    match kind.detect() {
        Detection::NotInstalled => Err("not found, skipped"),
        Detection::Installed => kind
            .config_path()
            .map(|path| (path, false))
            .ok_or("cannot resolve config path, skipped"),
        Detection::Configured(path) => Ok((path, true)),
    }
}

/// What the client should run to reach the gateway over stdio.
///
/// The bare name keeps working across upgrades and is what a user reading the
/// client file expects; the absolute path is the fallback for an mcpgw that
/// was never put on PATH (a downloaded binary run in place).
#[must_use]
pub fn bridge_command() -> String {
    if which::which("mcpgw").is_ok() {
        return "mcpgw".to_owned();
    }
    std::env::current_exe().map_or_else(|_| "mcpgw".to_owned(), |path| path.display().to_string())
}

fn describe(plan: &mcpgw_core::sync::SyncPlan) -> String {
    if !plan.has_changes() {
        return "no changes".to_owned();
    }
    let mut text = format!(
        "{} to add, {} to update, {} to remove",
        plan.adds.len(),
        plan.updates.len(),
        plan.removes.len()
    );
    // Only when there is one: every other client has no exclusion list, and
    // a permanent ", 0 to un-exclude" would be noise in all of them.
    if !plan.unexclude.is_empty() {
        let _ = write!(text, ", {} to un-exclude", plan.unexclude.len());
    }
    text
}

fn print_plan_lines(plan: &mcpgw_core::sync::SyncPlan, color: bool) {
    let line = |mark: &str, name: &str, note: &str, colored: fn(&str) -> String| {
        if color {
            println!("  {} {name}{note}", colored(mark));
        } else {
            println!("  {mark} {name}{note}");
        }
    };
    for name in &plan.adds {
        line("+", name, "", |m| m.green().to_string());
    }
    for name in &plan.updates {
        line("~", name, "", |m| m.yellow().to_string());
    }
    for name in &plan.removes {
        line("-", name, "", |m| m.red().to_string());
    }
    for name in &plan.unexclude {
        line(
            "~",
            name,
            " taken out of the client's exclusion list (it would refuse to start it)",
            |m| m.yellow().to_string(),
        );
    }
    for name in &plan.conflicts {
        line(
            "!",
            name,
            " exists in the client but is not managed by mcpgw (left untouched)",
            |m| m.red().to_string(),
        );
    }
    for name in &plan.foreign {
        line(
            "?",
            name,
            " (unmanaged, untouched — `mcpgw import` to adopt)",
            |m| m.dimmed().to_string(),
        );
    }
}

fn rollback(targets: &[ClientKind], state_dir: &Path) -> anyhow::Result<()> {
    let mut restored = 0;
    for kind in targets {
        let Some(backup_path) = backup::latest_backup(state_dir, kind.id())? else {
            continue;
        };
        let Some(config_path) = kind.config_path() else {
            continue;
        };
        let text = std::fs::read_to_string(&backup_path)
            .with_context(|| format!("cannot read backup {}", backup_path.display()))?;
        // Rollback is a client write like any other, so it takes a backup of
        // what it is about to destroy: a rollback fired by mistake is then
        // itself undoable instead of costing the live config outright. It
        // also re-stamps the stack, so repeated rollbacks alternate between
        // the last two states rather than silently re-applying one snapshot.
        if config_path.exists() {
            backup::backup_file(state_dir, kind.id(), &config_path)?;
        }
        write_text(&config_path, &text)?;
        println!(
            "restored {} from {}",
            config_path.display(),
            backup_path.display()
        );
        restored += 1;
    }
    if restored == 0 {
        bail!("no backups found for the selected clients");
    }
    Ok(())
}

// Atomic replace, same discipline as the canonical store.
//
// Two known side effects of replacing the file wholesale, both accepted in
// M6 ("client files are machine-written"): a plain-JSON document is
// re-serialized whole, so indentation is normalized even where nothing
// changed (for Claude Code that is all of ~/.claude.json), and the temp
// file's 0600 mode replaces whatever the client used. Comment-preserving
// formats keep their layout — only the edited entries move. The rename
// also wins any race
// against a client writing the same file concurrently — mcpgw's copy was
// read at the start of the run, so an edit made in between is lost. Backups
// are the recovery path; a lock the client does not take cannot prevent it.
fn write_text(path: &Path, text: &str) -> anyhow::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".mcpgw-sync.")
        .tempfile_in(parent)?;
    tmp.write_all(text.as_bytes())?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)
        .map_err(|err| anyhow::Error::from(err.error))
        .with_context(|| format!("cannot write {}", path.display()))?;
    // Syncing the bytes leaves the rename that publishes them undurable.
    sync_dir(parent);
    Ok(())
}

/// Best-effort directory fsync so the rename above survives a power loss.
/// Failure is not worth aborting a completed write over — and Windows has no
/// directory handle to sync at all.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(handle) = std::fs::File::open(dir) {
        let _ = handle.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}
