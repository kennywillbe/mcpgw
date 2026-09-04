//! `mcpgw eject`: put every client back the way it was before mcpgw.
//!
//! The escape hatch that makes gateway-only mode acceptable. It writes the
//! canonical config's *original* transports back into every client mcpgw
//! wrote to — same names, so the gateway entries land as plain updates —
//! drops the leftover single `mcpgw` entry a 0.3.x `sync --aggregate` wrote,
//! and offers to take the daemon with it.
//!
//! The desired entry set is built here, out of the canonical servers, rather
//! than by asking `sync` for its direct mode: `sync` has no direct mode any
//! more, and the restore must not have gone with it. Everything *under* the
//! desired map is `sync`'s and unchanged — the codec, the plan, the backup,
//! the state bookkeeping — so an ejected client is byte-identical to a
//! directly synced one, and `mcpgw sync --rollback` undoes this run exactly
//! as it undoes any other.
//!
//! Repo-local files come along for the ride: anything `sync --project`
//! wrote is in mcpgw's record with its own path, so eject restores those
//! files too — including the ones in a repo the user is not standing in,
//! because a committed entry pointing at a gateway nobody runs any more is
//! exactly the leftover this command exists to remove. No flag: a restore
//! that skipped half of what was written would be a worse promise than the
//! one it makes.
//!
//! What eject deliberately does not do is delete anything of the user's. The
//! canonical config, the state directory and the binary stay; the closing
//! screen names all three and leaves the decision where it belongs.

use std::io::IsTerminal as _;
use std::path::Path;

use anyhow::{Context as _, bail};
use mcpgw_core::daemon::{DaemonError, ServiceManager as _};
use mcpgw_core::state::{ManagedState, Scope};
use mcpgw_core::sync::SyncPlan;
use mcpgw_core::{ClientKind, Config, Error, paths};
use owo_colors::OwoColorize as _;

use super::sync::{Applied, Planned, PlannedClient, apply_client, plan_client, plan_file};
use crate::ui;
use crate::update::{self, InstallMethod};

#[derive(clap::Args)]
pub struct EjectArgs {
    /// Restore without asking. The full plan is still printed
    #[arg(long)]
    pub yes: bool,
}

pub fn run(args: &EjectArgs, color: bool) -> anyhow::Result<u8> {
    let config_path = super::canonical_config_path()?;
    let canonical = match Config::load(&config_path) {
        Ok(config) => config.servers,
        // Eject restores what the canonical config holds, so without it
        // there is nothing to restore *to* — and treating that as "manage
        // nothing", the way sync does, would strip every managed entry
        // instead of putting the originals back.
        Err(Error::NotFound { .. }) => bail!(
            "eject needs your canonical config for the original server definitions, \
             and there is none at {}\n\
             `mcpgw import` puts what a client still holds back into the config \
             first, and `mcpgw sync --rollback` restores each client from its most \
             recent backup.",
            config_path.display()
        ),
        Err(err) => return Err(err.into()),
    };

    let state_dir =
        paths::state_dir().context("cannot determine a home directory for the state dir")?;
    let state_path = state_dir.join("managed.json");
    // Held across the plan, like sync: the plan shown to the user and the
    // state it was read from must be one a concurrent `mcpgw sync` cannot
    // have moved underneath. Where it stops being held is below, at the
    // prompt.
    let state_lock = ManagedState::lock(&state_path)?;
    let state = ManagedState::load(&state_path)?;

    let (plans, notes) = plan(&canonical, &state)?;

    println!("mcpgw eject — putting every client back the way it was.");
    println!();
    for client in &plans {
        heading(&client.scope, &summary(&client.plan), color);
        print_restore(&client.plan, color);
    }
    for note in &notes {
        ui::already_done(&format!("· {note}"), color);
    }

    if plans.is_empty() {
        println!();
        println!("nothing to eject — no client holds an entry mcpgw wrote.");
        if let Ok(status) = mcpgw_core::daemon::platform_service().query()
            && status.installed
        {
            println!("a gateway service is still installed — `mcpgw daemon uninstall` removes it.");
        }
        return Ok(0);
    }

    println!();
    println!("Every file is backed up before it is written, and `mcpgw sync --rollback`");
    println!("undoes this run like any other.");
    if !args.yes && !std::io::stdin().is_terminal() {
        bail!("refusing to rewrite client configs without confirmation (pass --yes)");
    }
    // A human at a terminal is arbitrary think-time, so the lock is dropped
    // before the question and taken again after it: holding the state lock
    // across the prompt stalls every concurrent `mcpgw sync` and `mcpgw
    // import` for as long as the terminal goes unanswered — the argument
    // `import` already makes about its own prompt.
    //
    // Taking it again means planning again. The client files were unlocked
    // while the question sat there, so what gets written is read back rather
    // than carried over from before the answer, and an edit made under the
    // prompt survives instead of being overwritten by a document read before
    // it. A plan that no longer matches the printed one is a different
    // question than the one that was answered, so it stops the run rather
    // than writing something nobody was shown.
    //
    // `--yes` never reaches the prompt, so it never releases anything: its
    // plan and its write are the same read, exactly as sync's are.
    let (mut plans, mut state, state_lock) = if args.yes {
        // The echo `--yes` prints in place of the question, so the
        // transcript still shows what was agreed to.
        ask(true, "restore these clients?")?;
        (plans, state, state_lock)
    } else {
        drop(state_lock);
        if !ask(false, "restore these clients?")? {
            println!();
            ui::already_done("Stopped. Nothing was written.", color);
            return Ok(0);
        }
        let state_lock = ManagedState::lock(&state_path)?;
        let state = ManagedState::load(&state_path)?;
        let (fresh, fresh_notes) = plan(&canonical, &state)?;
        if shown(&fresh, &fresh_notes) != shown(&plans, &notes) {
            bail!(
                "your clients changed while that question was open, so the plan above is not \
                 what would be written any more — nothing was touched.\n\
                 Run `mcpgw eject` again to see the plan as it stands now."
            );
        }
        (fresh, state, state_lock)
    };

    for client in &mut plans {
        if let Applied::Refused(reason) = apply_client(client, &mut state, &state_dir, &state_path)?
        {
            println!("  {reason}");
        }
    }
    // The lock is only needed while the state file is being rewritten, and
    // the daemon question below can sit on a terminal for a while.
    drop(state_lock);

    println!();
    eject_daemon(args.yes, color)?;
    println!();
    closing(&config_path, &state_dir, color);
    Ok(0)
}

/// Plans the restore for every client mcpgw has written to.
///
/// The desired set is the canonical servers themselves — no gateway
/// translation — which is what makes a gateway entry a plain update over the
/// name it already occupies. Clients mcpgw never wrote to are not in the
/// result at all: their config is none of eject's business.
///
/// The second half is the lines about clients with nothing to restore, kept
/// separate so they print under the plans rather than between them.
fn plan(
    canonical: &std::collections::BTreeMap<String, mcpgw_core::Server>,
    state: &ManagedState,
) -> anyhow::Result<(Vec<PlannedClient>, Vec<String>)> {
    let mut plans = Vec::new();
    let mut notes = Vec::new();
    let scopes = ClientKind::ALL
        .into_iter()
        .map(Scope::Home)
        // Every repo-local file mcpgw wrote to, wherever it is: the record
        // is the only thing that knows about them, and a cwd is not it.
        .chain(state.project_scopes());
    for scope in scopes {
        let managed = scope.managed(state);
        if managed.is_empty() {
            continue;
        }
        // Under the client's own names, so an entry that has stood for a
        // second canonical copy since a keep-both is restored to *that*
        // definition rather than to the one it happens to be named after.
        let resolved = scope.resolved(state);
        let desired = mcpgw_core::sync::under_client_names(canonical.clone(), &resolved).desired;
        let name = scope.label();
        let planned = match scope.path() {
            None => plan_client(scope.kind(), &desired, &managed)?,
            Some(path) => plan_file(
                scope.clone(),
                path.to_path_buf(),
                path.is_file(),
                &desired,
                &managed,
            )?,
        };
        match planned {
            Planned::Skipped(reason) => notes.push(format!("{name} — {reason}")),
            // Sync would create the file and write the entries into it.
            // Eject must not: a client config that is gone was deleted by
            // someone, and handing it back is not a restore.
            Planned::Ready(planned) if !planned.exists => notes.push(format!(
                "{name} — {} is gone; nothing to restore there",
                planned.path.display()
            )),
            Planned::Ready(planned) if !planned.plan.has_changes() => {
                notes.push(format!(
                    "{name} — already pointing straight at your servers"
                ));
            }
            Planned::Ready(planned) => plans.push(*planned),
        }
    }
    Ok((plans, notes))
}

/// The plan in comparable form: what was printed, and so what the answer to
/// the prompt was an answer about — the per-client diffs under the labels
/// they were printed with, and the notes below them.
fn shown(plans: &[PlannedClient], notes: &[String]) -> (Vec<(String, SyncPlan)>, Vec<String>) {
    (
        plans
            .iter()
            .map(|client| (client.scope.label(), client.plan.clone()))
            .collect(),
        notes.to_vec(),
    )
}

fn heading(scope: &Scope, text: &str, color: bool) {
    let line = format!("{} — {text}", scope.label());
    if color {
        println!("{}", line.bold());
    } else {
        println!("{line}");
    }
}

fn summary(plan: &SyncPlan) -> String {
    let restored = plan.adds.len() + plan.updates.len();
    let mut text = format!(
        "{restored} entr{} restored, {} removed",
        if restored == 1 { "y" } else { "ies" },
        plan.removes.len()
    );
    if !plan.unexclude.is_empty() {
        use std::fmt::Write as _;
        let _ = write!(text, ", {} un-excluded", plan.unexclude.len());
    }
    text
}

/// The per-client diff, in eject's language rather than sync's: the same
/// plan, read as "what your file goes back to" instead of "what mcpgw is
/// pushing".
fn print_restore(plan: &SyncPlan, color: bool) {
    let line = |mark: &str, name: &str, note: &str, colored: fn(&str) -> String| {
        if color {
            println!("  {} {name}{note}", colored(mark));
        } else {
            println!("  {mark} {name}{note}");
        }
    };
    for name in &plan.updates {
        line("~", name, " back to your own definition", |m| {
            m.yellow().to_string()
        });
    }
    for name in &plan.adds {
        line("+", name, " restored (it was not in the file)", |m| {
            m.green().to_string()
        });
    }
    for name in &plan.removes {
        line("-", name, " removed (mcpgw put it there)", |m| {
            m.red().to_string()
        });
    }
    for name in &plan.unexclude {
        line(
            "~",
            name,
            " taken back out of the client's exclusion list",
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
        line("?", name, " (not mine — left untouched)", |m| {
            m.dimmed().to_string()
        });
    }
}

/// Takes the daemon out of the path too, since a service still starting a
/// gateway nobody points at is the one leftover a user would find later and
/// not understand.
fn eject_daemon(assume_yes: bool, color: bool) -> anyhow::Result<()> {
    let service = mcpgw_core::daemon::platform_service();
    match service.query() {
        Ok(status) if status.installed => {
            let unit = status
                .unit_path
                .as_ref()
                .map(|path| format!(" ({})", path.display()))
                .unwrap_or_default();
            println!(
                "A gateway service is installed under {}{unit}.",
                service.name()
            );
            if ask(assume_yes, "remove it as well?")? {
                service.uninstall()?;
                println!("  removed it — your config and captured traffic are untouched");
            } else {
                println!("  left installed — `mcpgw daemon uninstall` removes it whenever");
            }
        }
        Ok(_) => ui::already_done(
            &format!("· no gateway service is installed under {}", service.name()),
            color,
        ),
        // A platform whose installer has not shipped cannot have installed
        // anything, so there is nothing here to report as a failure.
        Err(DaemonError::NotSupportedYet(_)) => {
            ui::already_done("· no gateway service to remove on this platform", color);
        }
        Err(err) => ui::already_done(
            &format!(
                "· the gateway service cannot be queried ({err}) — `mcpgw daemon uninstall` removes it by hand"
            ),
            color,
        ),
    }
    Ok(())
}

/// Where things stand, and what is left to delete for a full uninstall.
fn closing(config_path: &Path, state_dir: &Path, color: bool) {
    let done =
        "Done — your clients talk to their servers directly again, and mcpgw is out of the path.";
    if color {
        println!("{}", done.bold());
    } else {
        println!("{done}");
    }
    println!();
    println!("Nothing of yours was deleted. To remove mcpgw entirely, delete these yourself:");
    // The config file rather than the directory holding it: MCPGW_CONFIG can
    // point mcpgw at a file that shares a directory with things that are not
    // ours, and "delete this directory" would then be bad advice.
    println!("  config   {}", config_path.display());
    println!(
        "  state    {}   {}",
        state_dir.display(),
        ui::dim("(backups, logs, captured traffic)", color)
    );
    println!("  binary   {}", binary_hint());
    println!();
    ui::already_done(
        "Changed your mind? Run `mcpgw` again and the wizard sets it all back up.",
        color,
    );
}

/// How to get rid of the binary, in the words of whatever put it there.
fn binary_hint() -> String {
    let Ok(exe) = std::env::current_exe() else {
        return "wherever you put the mcpgw binary".to_owned();
    };
    match update::install_method(&exe) {
        InstallMethod::Cargo => "cargo uninstall mcpgw".to_owned(),
        InstallMethod::Homebrew => "brew uninstall kennywillbe/tap/mcpgw".to_owned(),
        InstallMethod::Standalone => format!("delete {}", exe.display()),
    }
}

/// A question whose recommended answer is yes, echoed rather than skipped
/// under `--yes` so the transcript still shows what was agreed to.
fn ask(assume_yes: bool, question: &str) -> anyhow::Result<bool> {
    if assume_yes {
        println!("{question} [Y/n] y");
        return Ok(true);
    }
    ui::confirm_default_yes(question)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_with(adds: &[&str], updates: &[&str], removes: &[&str]) -> SyncPlan {
        let mut plan = mcpgw_core::sync::plan_sync(
            ClientKind::Cursor,
            &serde_json::Map::new(),
            &std::collections::BTreeMap::new(),
            &std::collections::BTreeSet::new(),
        );
        plan.adds = adds.iter().map(|s| (*s).to_owned()).collect();
        plan.updates = updates.iter().map(|s| (*s).to_owned()).collect();
        plan.removes = removes.iter().map(|s| (*s).to_owned()).collect();
        plan
    }

    #[test]
    fn the_summary_counts_restores_and_removals_apart() {
        assert_eq!(
            summary(&plan_with(&["a"], &["b"], &["mcpgw"])),
            "2 entries restored, 1 removed"
        );
        // A single restore reads as one entry, not one entries.
        assert_eq!(
            summary(&plan_with(&[], &["b"], &[])),
            "1 entry restored, 0 removed"
        );
    }

    #[test]
    fn the_uninstall_hint_names_the_package_manager_that_owns_the_binary() {
        assert_eq!(
            super::update::install_method(Path::new("/home/u/.cargo/bin/mcpgw")),
            InstallMethod::Cargo
        );
        assert!(binary_hint().contains("mcpgw"));
    }
}
