use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;

use anyhow::{Context as _, bail};
use mcpgw_core::sync::{
    GATEWAY_NAME, apply_plan_to, gateway_server, plan_client_context, plan_sync,
};
use mcpgw_core::{ClientKind, Config, Detection, Error, backup, paths, state::ManagedState};
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
    /// Show what would change without writing anything
    #[arg(long)]
    pub dry_run: bool,
    /// Restore each selected client's config from its most recent backup
    #[arg(long, conflicts_with = "dry_run")]
    pub rollback: bool,
    /// Write a single entry pointing at the gateway instead of one entry per server
    #[arg(long, conflicts_with = "rollback")]
    pub gateway: bool,
    /// URL of the gateway written by `--gateway`
    #[arg(long, default_value = super::connect::DEFAULT_URL, value_name = "URL")]
    pub gateway_url: String,
}

pub fn run(args: &SyncArgs, color: bool) -> anyhow::Result<()> {
    let targets = super::select_clients(&args.clients)?;
    let state_dir =
        paths::state_dir().context("cannot determine a home directory for the state dir")?;

    if args.rollback {
        return rollback(&targets, &state_dir);
    }

    // Gateway mode writes one synthetic entry per client, so the canonical
    // servers are irrelevant there — an unreadable config must not block it.
    let canonical = if args.gateway {
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

    let bridge = if args.gateway {
        println!(
            "gateway mode — every client gets a single `{GATEWAY_NAME}` entry pointing at {}",
            args.gateway_url
        );
        // Resolved once: probing PATH per client would give the same answer.
        Some(bridge_command())
    } else {
        println!("direct mode — every enabled server gets its own client entry");
        None
    };

    let state_path = state_dir.join("managed.json");
    // Held across the whole load→modify→save window: a second `mcpgw sync
    // --client other` running at the same time would otherwise write back a
    // state it read before this run's changes and drop them.
    let _state_lock = ManagedState::lock(&state_path)?;
    let mut state = ManagedState::load(&state_path)?;

    for kind in targets {
        let heading = |text: &str| {
            if color {
                println!("{}", format!("{} — {text}", kind.display_name()).bold());
            } else {
                println!("{} — {text}", kind.display_name());
            }
        };
        let (path, exists) = match resolve_target(kind) {
            Ok(target) => target,
            Err(reason) => {
                heading(reason);
                continue;
            }
        };

        let codec = kind.codec();
        let mut doc = if exists {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("cannot read {}", path.display()))?;
            match codec.parse_document(&text) {
                Ok(doc) => doc,
                // Hand-broken, or JSONC in a client whose format is strict
                // JSON: refuse to rewrite what we cannot faithfully parse.
                Err(err) => {
                    heading(&format!(
                        "skipped: {} is not {} ({err}); fix or sync it manually",
                        path.display(),
                        codec.format_name()
                    ));
                    continue;
                }
            }
        } else {
            codec.empty_document()
        };

        let current = doc.entries(codec.root);
        let managed = state.clients.get(kind.id()).cloned().unwrap_or_default();
        // In gateway mode the desired set is the synthetic entry alone, so
        // entries managed by an earlier direct sync fall out as removes.
        let gateway_desired = bridge.as_ref().map(|command| {
            BTreeMap::from([(
                GATEWAY_NAME.to_owned(),
                gateway_server(kind, &args.gateway_url, command),
            )])
        });
        let desired = gateway_desired.as_ref().unwrap_or(&canonical);
        let mut plan = plan_sync(kind, &current, desired, &managed);
        plan_client_context(kind, &doc.to_value(), &mut plan);

        heading(&describe(&plan));
        print_plan_lines(&plan, color);

        if !plan.has_changes() {
            continue;
        }
        if args.dry_run {
            continue;
        }

        // In memory and before anything on disk moves, so a refusal costs
        // neither a backup nor a state entry claiming what was never written.
        if let Err(err) = apply_plan_to(kind, &mut doc, &plan) {
            println!(
                "  refused: {err} in {} — nothing written; `mcpgw doctor` reports the same problem",
                path.display()
            );
            continue;
        }
        if exists {
            backup::backup_file(&state_dir, kind.id(), &path)?;
        }
        // Intent first: the state file records what this run is about to
        // write *before* the client file is touched. A crash in between then
        // leaves entries claimed but absent, which the next sync sees as
        // plain adds and repairs. The reverse order fails the other way —
        // entries in the client file that mcpgw never claimed are foreign
        // forever, and sync refuses to touch them.
        state
            .clients
            .insert(kind.id().to_owned(), plan.managed_after());
        state.save(&state_path)?;
        write_text(&path, &doc.to_text()?)?;
    }
    Ok(())
}

/// The file this client's sync reads and writes, and whether it is there
/// already — or the reason there is nothing to sync.
fn resolve_target(kind: ClientKind) -> Result<(std::path::PathBuf, bool), &'static str> {
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
fn bridge_command() -> String {
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
