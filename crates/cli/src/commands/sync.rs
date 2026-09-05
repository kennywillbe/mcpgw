use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use mcpgw_core::clients::codec::ClientDocument;
use mcpgw_core::projects::ProjectConfig;
use mcpgw_core::state::{ManagedState, Scope};
use mcpgw_core::sync::{
    SyncPlan, apply_plan_to, per_server_gateway_servers, plan_client_context, plan_sync,
};
use mcpgw_core::{ClientKind, Config, Detection, Error, Server, Transport, backup, paths};
use owo_colors::OwoColorize as _;

#[derive(clap::Args)]
pub struct SyncArgs {
    /// Only sync these clients (repeatable)
    #[arg(
        long = "client",
        value_name = "ID",
        long_help = super::client_ids_help("Only sync these clients")
    )]
    pub clients: Vec<String>,
    /// Also write the repo-local MCP configs found from this directory
    #[arg(long)]
    pub project: bool,
    /// Show what would change without writing anything
    #[arg(long)]
    pub dry_run: bool,
    /// Restore each selected client's config from its most recent backup
    #[arg(long, conflicts_with = "dry_run")]
    pub rollback: bool,
    /// URL of the gateway the entries point at
    #[arg(long, default_value = super::connect::DEFAULT_URL, value_name = "URL")]
    pub gateway_url: String,
}

pub fn run(args: &SyncArgs, color: bool) -> anyhow::Result<()> {
    let targets = super::select_clients(&args.clients)?;
    let state_dir =
        paths::state_dir().context("cannot determine a home directory for the state dir")?;

    if args.rollback {
        return rollback(&targets, &state_dir, args.project);
    }

    // The entries mirror the canonical servers by name, so the canonical
    // config is what this run is a function of.
    let config_path = super::canonical_config_path()?;
    let (canonical, scopes) = match Config::load(&config_path) {
        Ok(config) => (config.servers, config.clients),
        // An absent canonical config means "manage nothing": previously
        // managed entries get removed, everything else is untouched.
        Err(Error::NotFound { .. }) => (BTreeMap::new(), BTreeMap::new()),
        Err(err) => return Err(err.into()),
    };

    // Read once for the run: every client's entries carry the same token,
    // and a rotate that landed halfway through would be worse than one that
    // lands on the next run.
    let token = super::token::current();
    let bridge = announce(args, token.as_ref(), color, &scopes)?;

    let state_path = state_dir.join("managed.json");
    // Held across the whole load→modify→save window: a second `mcpgw sync
    // --client other` running at the same time would otherwise write back a
    // state it read before this run's changes and drop them.
    let _state_lock = ManagedState::lock(&state_path)?;
    let mut state = ManagedState::load(&state_path)?;

    let run = Run {
        args,
        color,
        state_dir: &state_dir,
        state_path: &state_path,
    };
    // Set at the end of the run rather than after the first client: two
    // clients flipped by one command both deserve the explanation, and the
    // flag is what stops the *next* run from repeating it.
    let mut notified = false;
    for kind in targets.iter().copied() {
        let scope = Scope::Home(kind);
        let desired = gateway_entries(
            kind,
            args,
            &canonical,
            &bridge,
            token.as_ref(),
            scopes.get(kind.id()),
            &scope.resolved(&state),
        )?;
        let planned = plan_client(kind, &desired.desired, &scope.managed(&state))?;
        run.one(&scope, planned, &desired, &mut state, &mut notified)?;
    }

    if args.project {
        let found = project_targets(&targets);
        if found.is_empty() {
            println!(
                "no repo-local MCP config here — --project had nothing to write \
                 (`mcpgw doctor` lists the files it looks for)"
            );
        }
        for config in found {
            let kind = config.kind;
            let scope = config.scope();
            let desired = gateway_entries(
                kind,
                args,
                &canonical,
                &bridge,
                token.as_ref(),
                scopes.get(kind.id()),
                &scope.resolved(&state),
            )?;
            let planned = plan_project(&config, &desired.desired, &scope.managed(&state))?;
            run.one(&scope, planned, &desired, &mut state, &mut notified)?;
        }
    }

    if notified {
        state.migrated = true;
        state.save(&state_path)?;
    }
    Ok(())
}

/// The sync a canonical edit runs on its way out.
///
/// `add`, `remove`, `enable` and `disable` all change what the clients are
/// supposed to hold, and until their files are rewritten the edit has landed
/// in exactly one place the user cannot see it. So each of them finishes with
/// the run a bare `mcpgw sync` is: every client, the default gateway URL, and
/// no repo-local files — none of those commands takes a `--project`, so there
/// is no project scope for this run to inherit.
///
/// # Errors
///
/// Whatever `mcpgw sync` itself fails with.
pub fn after_edit(no_sync: bool, color: bool) -> anyhow::Result<()> {
    if no_sync {
        println!("run `mcpgw sync` to bring the clients up to date.");
        return Ok(());
    }
    println!();
    // Said once instead of one "not found, skipped" per client: on a machine
    // with no client at all that list is the whole output, and none of its
    // lines is the answer ("there is nothing here to sync").
    if ClientKind::ALL
        .iter()
        .all(|kind| matches!(kind.detect(), Detection::NotInstalled))
    {
        println!(
            "no MCP client found on this machine — nothing to sync \
             (`mcpgw clients` lists the ones mcpgw knows)"
        );
        return Ok(());
    }
    run(
        &SyncArgs {
            clients: Vec::new(),
            project: false,
            dry_run: false,
            rollback: false,
            gateway_url: super::connect::DEFAULT_URL.to_owned(),
        },
        color,
    )
}

/// The repo-local files this run may write: the ones discovery found, minus
/// any client `--client` left out.
fn project_targets(targets: &[ClientKind]) -> Vec<ProjectConfig> {
    mcpgw_core::projects::discover_cwd()
        .into_iter()
        .filter(|config| targets.contains(&config.kind))
        .collect()
}

/// What one sync run is, apart from the file it is pointed at.
///
/// Every file goes through exactly the same show-then-apply, whether it is a
/// client's per-user config or one committed in a repo — which is the point
/// of the whole change, and the reason this is one function taking a
/// [`Scope`] rather than two loops that could drift.
struct Run<'a> {
    args: &'a SyncArgs,
    color: bool,
    state_dir: &'a Path,
    state_path: &'a Path,
}

impl Run<'_> {
    fn one(
        &self,
        scope: &Scope,
        planned: Planned,
        desired: &mcpgw_core::sync::ClientNames,
        state: &mut ManagedState,
        notified: &mut bool,
    ) -> anyhow::Result<()> {
        let heading = |text: &str| {
            let line = format!("{} — {text}", scope.label());
            if self.color {
                println!("{}", line.bold());
            } else {
                println!("{line}");
            }
        };
        let mut planned = match planned {
            Planned::Ready(planned) => planned,
            Planned::Skipped(reason) => {
                heading(&reason);
                return Ok(());
            }
        };

        heading(&describe(&planned.plan));
        print_plan_lines(&planned.plan, &desired.desired, self.color);
        for name in &desired.displaced {
            println!(
                "  {}",
                crate::ui::dim(
                    &format!(
                        "! {name} not written here — this client's {name:?} entry is your own \
                         server, kept when you said the two were different"
                    ),
                    self.color,
                )
            );
        }

        if !planned.plan.has_changes() || self.args.dry_run {
            return Ok(());
        }

        // Asked before the plan is applied: afterwards the document holds the
        // gateway entries and every answer is "already there".
        let migrating = !state.migrated && planned.migrates_to_gateway(&self.args.gateway_url);

        match apply_client(&mut planned, state, self.state_dir, self.state_path)? {
            Applied::Refused(reason) => println!("  {reason}"),
            Applied::Written if migrating => {
                print_migration_notice(self.color);
                *notified = true;
            }
            Applied::Written => {}
        }
        Ok(())
    }
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
    /// Which file this is, and so which bookkeeping and which backup stack
    /// it belongs to.
    pub scope: Scope,
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
    plan_file(Scope::Home(kind), path, exists, desired, managed)
}

/// The same for one repo-local file, which is always already there —
/// discovery is what found it, and `sync --project` never creates a project
/// config the repo does not have.
///
/// # Errors
///
/// Same failure as [`plan_client`].
pub fn plan_project(
    config: &ProjectConfig,
    desired: &BTreeMap<String, Server>,
    managed: &BTreeSet<String>,
) -> anyhow::Result<Planned> {
    plan_file(config.scope(), config.path.clone(), true, desired, managed)
}

/// Reads one file and plans `desired` over what it holds.
///
/// # Errors
///
/// Returns a failure only if a file that is there cannot be read; a file that
/// cannot be *parsed* is a skip rather than an error, because the remaining
/// files are still worth syncing.
pub fn plan_file(
    scope: Scope,
    path: PathBuf,
    exists: bool,
    desired: &BTreeMap<String, Server>,
    managed: &BTreeSet<String>,
) -> anyhow::Result<Planned> {
    let kind = scope.kind();
    let codec = scope.codec();
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
        scope,
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
        backup::backup_file(state_dir, &planned.scope.backup_key(), &planned.path)?;
    }
    // Intent first: the state file records what this run is about to write
    // *before* the client file is touched. A crash in between then leaves
    // entries claimed but absent, which the next sync sees as plain adds and
    // repairs. The reverse order fails the other way — entries in the client
    // file that mcpgw never claimed are foreign forever, and sync refuses to
    // touch them.
    planned.scope.claim(state, planned.plan.managed_after());
    state.save(state_path)?;
    write_text(&planned.path, &planned.doc.to_text()?)?;
    Ok(Applied::Written)
}

/// Prints what this run is about to do — the token included, masked — and
/// resolves the stdio bridge command.
///
/// The bridge is resolved once for the whole run: probing PATH per client
/// would give the same answer.
fn announce(
    args: &SyncArgs,
    token: Option<&mcpgw_core::gateway_token::GatewayToken>,
    color: bool,
    scopes: &BTreeMap<String, mcpgw_core::config::ClientScope>,
) -> anyhow::Result<String> {
    // Checked before a single client is touched: a base URL that cannot take
    // an endpoint path is wrong for all of them, and failing halfway would
    // leave some clients rewritten and some not.
    mcpgw_core::endpoints::per_server_url(&args.gateway_url, "probe")
        .with_context(|| format!("--gateway-url {} is not a URL", args.gateway_url))?;
    // No mode to name: this is the one thing sync does, so the line describes
    // it rather than announcing which of several shapes was picked.
    println!(
        "every enabled server keeps its name and points at its own endpoint \
         on the gateway at {} (serve it with `mcpgw serve`)",
        args.gateway_url
    );
    // Masked, always. A dry run is the output people paste into an issue.
    match token {
        Some(token) => println!(
            "  {}",
            crate::ui::dim(
                &format!(
                    "each entry carries Authorization: Bearer {} — the gateway's install \
                     token (`mcpgw token show`)",
                    token.masked()
                ),
                color,
            )
        ),
        None => println!(
            "  {}",
            crate::ui::dim(
                "no gateway token on this machine yet, so the entries carry none — \
                 `mcpgw serve` or `mcpgw daemon install` issues one",
                color,
            )
        ),
    }
    // Named up front because it is the one reason two clients come out of
    // one run holding different entries, and a user looking at a shorter
    // file than they expected should not have to guess why.
    let narrowed: Vec<&str> = scopes
        .iter()
        .filter(|(_, scope)| scope.restricts())
        .map(|(id, _)| id.as_str())
        .collect();
    if !narrowed.is_empty() {
        println!(
            "scoped by [clients]: {} — each gets only the servers its table names, \
             at an endpoint tagged with its own name",
            narrowed.join(", ")
        );
    }
    if args.project {
        println!("--project: the repo-local configs found from here are written too");
    }
    Ok(bridge_command())
}

/// The entries a client should hold: every enabled server, reached through
/// the gateway.
///
/// They carry the canonical names, so a client that mcpgw synced before keeps
/// its entry names and the rewrite is plain updates — and anything else an
/// earlier sync managed under a name that is not a canonical server's, the
/// single `mcpgw` entry a 0.3.x `--aggregate` run wrote included, falls out
/// of the plan as a remove.
///
/// The exception is a name this client resolved to a different server: there
/// the entry keeps the client's name and points at the server that name has
/// stood for since the user kept both copies.
fn gateway_entries(
    kind: ClientKind,
    args: &SyncArgs,
    canonical: &BTreeMap<String, mcpgw_core::Server>,
    bridge: &str,
    token: Option<&mcpgw_core::gateway_token::GatewayToken>,
    scope: Option<&mcpgw_core::config::ClientScope>,
    resolved: &BTreeMap<String, String>,
) -> anyhow::Result<mcpgw_core::sync::ClientNames> {
    let desired =
        per_server_gateway_servers(kind, canonical, &args.gateway_url, bridge, token, scope)?;
    Ok(mcpgw_core::sync::under_client_names(desired, resolved))
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

/// Where one desired entry will point, spelled the way the client file will
/// spell it: a URL for the clients that take http entries, the bridge command
/// for the ones that only speak stdio.
///
/// No headers: the only one an entry carries is the gateway token, already
/// announced masked at the top of the run, and a dry run is output people
/// paste into an issue.
fn entry_target(server: &Server) -> String {
    match &server.transport {
        Transport::Http { url, .. } => url.clone(),
        Transport::Stdio { command, args, .. } => std::iter::once(command.as_str())
            .chain(args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// `desired` is this client's entry set, so an add or an update can say what
/// it will point at. Counts and names alone cannot be checked against
/// anything — the endpoint a scoped client gets is not derivable from the
/// server's name, and confirming it was the reason to run `--dry-run`.
fn print_plan_lines(
    plan: &mcpgw_core::sync::SyncPlan,
    desired: &BTreeMap<String, Server>,
    color: bool,
) {
    let line = |mark: &str, name: &str, note: &str, colored: fn(&str) -> String| {
        if color {
            println!("  {} {name}{note}", colored(mark));
        } else {
            println!("  {mark} {name}{note}");
        }
    };
    let target = |name: &String| {
        desired.get(name).map_or_else(String::new, |server| {
            format!(" → {}", crate::ui::dim(&entry_target(server), color))
        })
    };
    for name in &plan.adds {
        line("+", name, &target(name), |m| m.green().to_string());
    }
    for name in &plan.updates {
        line("~", name, &target(name), |m| m.yellow().to_string());
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

fn rollback(targets: &[ClientKind], state_dir: &Path, project: bool) -> anyhow::Result<()> {
    let mut scopes: Vec<(Scope, Option<PathBuf>)> = targets
        .iter()
        .map(|kind| (Scope::Home(*kind), kind.config_path()))
        .collect();
    if project {
        scopes.extend(
            project_targets(targets)
                .into_iter()
                .map(|config| (config.scope(), Some(config.path))),
        );
    }

    let mut restored = 0;
    for (scope, config_path) in scopes {
        let Some(backup_path) = backup::latest_backup(state_dir, &scope.backup_key())? else {
            continue;
        };
        let Some(config_path) = config_path else {
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
            backup::backup_file(state_dir, &scope.backup_key(), &config_path)?;
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
