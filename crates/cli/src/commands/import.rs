use std::collections::BTreeSet;
use std::io::IsTerminal as _;

use anyhow::Context as _;
use mcpgw_core::import::{ImportCandidate, plan_import};
use mcpgw_core::state::ManagedState;
use mcpgw_core::{ConfigStore, Detection, paths};

#[derive(clap::Args)]
pub struct ImportArgs {
    /// Only import from these clients (repeatable; ids as in sync --client)
    #[arg(long = "from", value_name = "ID")]
    pub from: Vec<String>,
    /// Show what would be imported without writing anything
    #[arg(long)]
    pub dry_run: bool,
}

pub fn run(args: &ImportArgs) -> anyhow::Result<()> {
    let targets = super::select_clients(&args.from)?;

    let mut sources: Vec<(String, mcpgw_core::ClientRead)> = Vec::new();
    for kind in targets {
        if let Detection::Configured(path) = kind.detect() {
            match kind.load(&path) {
                Ok(read) => sources.push((kind.id().to_owned(), read)),
                Err(err) => println!("skipping {}: {err}", kind.display_name()),
            }
        }
    }

    let config_path = super::canonical_config_path()?;
    // Locked before the plan is built, not after it is printed: reading the
    // canonical config unlocked left a window in which a concurrent `mcpgw
    // add` could claim one of the planned names, turning the first write
    // into a DuplicateName abort halfway through the reported progress. The
    // store now supplies both the planning input and the write handle, so
    // they cannot describe different files.
    let store = ConfigStore::edit_or_create(&config_path)?;
    let plan = plan_import(&sources, &store.config().servers);

    if plan.new.is_empty() && plan.already.is_empty() && plan.conflicts.is_empty() {
        println!("nothing to import");
        return Ok(());
    }
    if args.dry_run {
        report_dry(&plan);
        return Ok(());
    }

    // Conflicts need a human, and a human is arbitrary think-time. Locks are
    // therefore dropped before asking and retaken afterwards: holding the
    // config lock across the prompt stalls every concurrent `mcpgw add`, and
    // holding the state lock — which this command only started taking with
    // the adoption fix — stalls every concurrent `mcpgw sync` as well, for as
    // long as the terminal goes unanswered.
    //
    // Reacquiring means re-planning: the answers are keyed by name and only
    // honoured for a name that is *still* a conflict in the fresh plan, so
    // the guarantee the plan-under-lock fix bought (plan and write describe
    // the same file) survives, and a name that changed underneath us falls
    // back to being skipped.
    let (mut store, plan, overwrite) =
        if plan.conflicts.is_empty() || !std::io::stdin().is_terminal() {
            // Nothing to ask, so nothing to release: the plan already under
            // the lock is the one that gets applied.
            (store, plan, BTreeSet::new())
        } else {
            let questions: Vec<String> = plan.conflicts.iter().map(|c| c.name.clone()).collect();
            drop(store);
            let overwrite = ask(&questions)?;
            let store = ConfigStore::edit_or_create(&config_path)?;
            let plan = plan_import(&sources, &store.config().servers);
            (store, plan, overwrite)
        };

    let state_dir =
        paths::state_dir().context("cannot determine a home directory for the state dir")?;
    let state_path = state_dir.join("managed.json");
    // Held until the run ends, like sync: adoption is a read-modify-write of
    // the same file and must not race another mcpgw process.
    let _state_lock = ManagedState::lock(&state_path)?;
    let mut state = ManagedState::load(&state_path)?;
    let mut imported = 0;
    let mut skipped = 0;

    for candidate in &plan.new {
        store.upsert_server(&candidate.name, &candidate.server, false)?;
        adopt(&mut state, candidate);
        imported += 1;
        println!("+ {}{}", candidate.name, describe(candidate));
    }
    for candidate in &plan.already {
        // Nothing to write, but adopting the identical client entries frees
        // future syncs from reporting them as perpetual conflicts.
        adopt(&mut state, candidate);
        println!("= {} already present (adopted)", candidate.name);
    }
    for candidate in &plan.conflicts {
        if overwrite.contains(&candidate.name) {
            store.upsert_server(&candidate.name, &candidate.server, true)?;
            adopt(&mut state, candidate);
            imported += 1;
            println!("~ {} overwritten{}", candidate.name, describe(candidate));
        } else {
            skipped += 1;
            println!(
                "! {} differs from the canonical entry (skipped — run interactively to decide)",
                candidate.name
            );
        }
    }

    store.save()?;
    state.save(&state_path)?;
    println!(
        "imported {imported}, already present {}, skipped {skipped}",
        plan.already.len()
    );
    Ok(())
}

/// Asks about each conflicting name and returns the ones to overwrite.
///
/// Takes names rather than the plan so the caller cannot accidentally keep a
/// lock alive across the prompt: this runs with none held.
fn ask(names: &[String]) -> anyhow::Result<BTreeSet<String>> {
    let mut overwrite = BTreeSet::new();
    for name in names {
        if super::confirm(&format!(
            "{name:?} differs from the canonical entry — overwrite canonical?"
        ))? {
            overwrite.insert(name.clone());
        }
    }
    Ok(overwrite)
}

fn adopt(state: &mut ManagedState, candidate: &ImportCandidate) {
    for (client_id, original) in &candidate.origins {
        state
            .clients
            .entry(client_id.clone())
            .or_default()
            .insert(original.clone());
    }
}

fn describe(candidate: &ImportCandidate) -> String {
    let mut parts: Vec<String> = Vec::new();
    let clients: Vec<&str> = candidate
        .origins
        .iter()
        .map(|(id, _)| id.as_str())
        .collect();
    parts.push(format!("from {}", clients.join(", ")));
    if candidate.renamed {
        parts.push(format!("renamed from {:?}", candidate.origins[0].1));
    }
    for note in &candidate.notes {
        parts.push(format!("note: {note}"));
    }
    format!(" ({})", parts.join("; "))
}

fn report_dry(plan: &mcpgw_core::import::ImportPlan) {
    for candidate in &plan.new {
        println!("+ {}{}", candidate.name, describe(candidate));
    }
    for candidate in &plan.already {
        println!("= {} already present (would adopt)", candidate.name);
    }
    for candidate in &plan.conflicts {
        println!("! {} differs from the canonical entry", candidate.name);
    }
}
