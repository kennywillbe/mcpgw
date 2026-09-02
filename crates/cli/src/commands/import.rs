use std::collections::{BTreeMap, BTreeSet};
use std::io::IsTerminal as _;

use anyhow::Context as _;
use mcpgw_core::import::{ImportCandidate, ImportPlan, SameAddress, plan_import};
use mcpgw_core::state::ManagedState;
use mcpgw_core::{ClientKind, ConfigStore, Detection, paths};

#[derive(clap::Args)]
pub struct ImportArgs {
    /// Only import from these clients (repeatable)
    #[arg(
        long = "from",
        value_name = "ID",
        long_help = super::client_ids_help("Only import from these clients")
    )]
    pub from: Vec<String>,
    /// Show what would be imported without writing anything
    #[arg(long)]
    pub dry_run: bool,
    /// Skip the conflict prompt and keep the canonical entry
    #[arg(long)]
    pub yes: bool,
}

/// What to do about a client entry whose name matches a canonical entry but
/// whose definition does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictChoice {
    /// Leave the canonical entry alone and leave the client's copy where it
    /// is. The answer every run that cannot ask takes, because it is the only
    /// one that writes nothing.
    KeepCanonical,
    /// Replace the canonical entry with the client's definition.
    Overwrite,
    /// Keep the canonical entry untouched *and* import the client's copy
    /// under a second name, so it stops being an unmanaged entry talking to
    /// its origin behind the gateway's back.
    KeepBoth,
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
    let plan = plan_import(&sources, &store.config().servers, &super::command_exists);

    if plan.new.is_empty() && plan.already.is_empty() && plan.conflicts.is_empty() {
        println!("nothing to import");
        return Ok(());
    }
    // Said before anything is asked, and said in every mode — dry run, piped,
    // `--yes`: a run that cannot ask still owes the reader the observation,
    // because "why do I have context7 and context7-2" is a question they will
    // otherwise ask themselves later with nothing in the transcript to
    // answer it.
    let addresses = same_address_questions(&plan);
    for shared in &addresses {
        println!("{}", same_address_line(shared));
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
    //
    // `--yes` states the answer up front — keep canonical — so the prompt is
    // never reached and the lock never has to be released for it. It says by
    // intent what the IsTerminal check can only guess: a script that pipes
    // input to import is indistinguishable from an interactive run, so the
    // fallback alone leaves scripted callers unable to promise they will not
    // block.
    let non_interactive = args.yes || !std::io::stdin().is_terminal();
    let conflicts: Vec<(String, String)> = plan
        .conflicts
        .iter()
        .map(|c| (c.name.clone(), adopt_as(c)))
        .collect();
    let (mut store, plan, answers, skip) =
        if (conflicts.is_empty() && addresses.is_empty()) || non_interactive {
            // Nothing to ask, so nothing to release: the plan already under
            // the lock is the one that gets applied. Under `--yes` that means
            // keeping both copies — the answer that loses nothing.
            (store, plan, BTreeMap::new(), BTreeSet::new())
        } else {
            drop(store);
            let answers = ask(&conflicts)?;
            let skip = ask_same_address(&addresses)?;
            let store = ConfigStore::edit_or_create(&config_path)?;
            let plan = plan_import(&sources, &store.config().servers, &super::command_exists);
            (store, plan, answers, skip)
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
        // Honoured only where the fresh plan still sees the same shared
        // address: a name that stopped matching under the prompt was answered
        // about something that no longer exists.
        if let Some(same) = &candidate.same_address
            && skip.contains(&candidate.name)
        {
            skipped += 1;
            println!(
                "- {} skipped — keeping {} instead (your client entry is untouched)",
                candidate.name, same.name
            );
            continue;
        }
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
    let why_kept = if args.yes {
        "--yes keeps canonical"
    } else if non_interactive {
        "not a terminal, keeping canonical"
    } else {
        "you kept canonical"
    };
    let (written, kept) = write_conflicts(&mut store, &mut state, &plan, &answers, why_kept)?;
    imported += written;
    skipped += kept;

    // Canonical file first, then the adoption record — deliberately the
    // reverse of the intent-first order sync uses, because the two ledgers
    // claim different things. Sync's state entry claims what that run is
    // about to *write* into the client, so an over-claim self-heals: the next
    // sync sees a managed name missing from the client and adds it. Import's
    // state entry claims client entries that only the canonical config
    // justifies. Claim them first and a failed `store.save()` leaves the
    // state saying "mcpgw manages cursor's github" with no canonical github
    // behind it — which the next sync reads as a removal and deletes the
    // user's entry. This order fails the harmless way instead: the entries
    // read as unmanaged and are left untouched, and re-running import adopts
    // them.
    store.save()?;
    state.save(&state_path).context(
        "the canonical config was written but the adoption record was not; re-run \
         `mcpgw import` to finish adopting — your client entries are untouched \
         until it succeeds",
    )?;
    println!(
        "imported {imported}, already present {}, skipped {skipped}",
        plan.already.len()
    );
    Ok(())
}

/// Writes the decided outcome for every conflict, returning how many entries
/// were written and how many were left to the canonical config.
///
/// `why_kept` is the parenthetical on a kept-canonical line: the same outcome
/// is reached by `--yes`, by a pipe and by a person answering, and which of
/// them it was is the only thing that makes the line actionable.
fn write_conflicts(
    store: &mut ConfigStore,
    state: &mut ManagedState,
    plan: &ImportPlan,
    answers: &BTreeMap<String, ConflictChoice>,
    why_kept: &str,
) -> anyhow::Result<(usize, usize)> {
    let (mut written, mut kept) = (0, 0);
    for candidate in &plan.conflicts {
        // Answers are keyed by name and only honoured where the fresh plan
        // still calls that name a conflict; anything else falls back to the
        // outcome that writes nothing.
        match answers
            .get(&candidate.name)
            .copied()
            .unwrap_or(ConflictChoice::KeepCanonical)
        {
            ConflictChoice::Overwrite => {
                store.upsert_server(&candidate.name, &candidate.server, true)?;
                adopt(state, candidate);
                written += 1;
                println!("~ {} overwritten{}", candidate.name, describe(candidate));
            }
            ConflictChoice::KeepBoth => {
                // The second name comes from the *fresh* plan, not from the
                // one the question was asked about: another process may have
                // claimed it while the terminal was being read.
                let second = adopt_as(candidate);
                store.upsert_server(&second, &candidate.server, false)?;
                adopt(state, candidate);
                written += 1;
                println!(
                    "+ {second} kept alongside the canonical {}{}",
                    candidate.name,
                    describe(candidate)
                );
            }
            ConflictChoice::KeepCanonical => {
                kept += 1;
                println!(
                    "! {} differs from the canonical entry (skipped — {why_kept})",
                    candidate.name
                );
            }
        }
    }
    Ok((written, kept))
}

/// The question a conflict raises, and the name it is about.
#[must_use]
pub fn conflict_prompt(name: &str) -> String {
    format!("{name:?} differs from the canonical entry — which would you like?")
}

/// The three outcomes, in the order [`conflict_choice`] decodes them.
///
/// Keeping canonical comes first because it is the recommended answer and the
/// only one that writes nothing. Overwriting is last of the two that change
/// the canonical entry: it is the single import outcome that loses data, so
/// it is the one that has to be picked deliberately.
#[must_use]
pub fn conflict_options(name: &str, adopt_as: &str) -> Vec<String> {
    vec![
        format!("Keep {name} as it is — your client's copy stays unmanaged"),
        format!("Keep both — bring your client's copy in as {adopt_as}"),
        format!("Overwrite {name} with your client's copy"),
    ]
}

/// Reads an index from [`conflict_options`] back as an outcome. Anything out
/// of range is the safe answer, which is also the default the prompt offers.
#[must_use]
pub fn conflict_choice(picked: usize) -> ConflictChoice {
    match picked {
        1 => ConflictChoice::KeepBoth,
        2 => ConflictChoice::Overwrite,
        _ => ConflictChoice::KeepCanonical,
    }
}

/// Asks about each conflict — `(canonical name, name to adopt it under)` —
/// and returns what was decided per name.
///
/// Takes names rather than the plan so the caller cannot accidentally keep a
/// lock alive across the prompt: this runs with none held.
fn ask(conflicts: &[(String, String)]) -> anyhow::Result<BTreeMap<String, ConflictChoice>> {
    let mut answers = BTreeMap::new();
    for (name, adopt_as) in conflicts {
        let picked =
            crate::ui::choose(&conflict_prompt(name), &conflict_options(name, adopt_as), 0)?;
        answers.insert(name.clone(), conflict_choice(picked));
    }
    Ok(answers)
}

/// Why an entry is coming in switched off, and what turns it back on. Shared
/// with the wizard's import step so both surfaces say it the same way.
pub fn command_missing_line(name: &str) -> String {
    format!(
        "command not found on this machine, importing disabled \
         (enable later: mcpgw toggle {name})"
    )
}

/// One candidate that addresses something already known and differs from it
/// only in what its headers are set to.
pub struct SharedAddress {
    /// The name the plan would file it under.
    pub planned: String,
    /// The name it has in the client it came from. Preferred in prose: on the
    /// run this exists for, the planned name is the invented `context7-2`,
    /// and "context7-2 looks like context7" explains nothing. What the user
    /// recognises is the entry they wrote themselves.
    pub original: String,
    pub client: String,
    pub same: SameAddress,
}

pub fn same_address_questions(plan: &ImportPlan) -> Vec<SharedAddress> {
    plan.new
        .iter()
        .filter_map(|candidate| {
            let same = candidate.same_address.clone()?;
            let (client_id, original) = candidate.origins.first()?;
            Some(SharedAddress {
                planned: candidate.name.clone(),
                original: original.clone(),
                client: display(client_id),
                same,
            })
        })
        .collect()
}

/// What the run saw, in a sentence — the fact that two definitions differ,
/// never the values that differ, because this line ends up in bug reports.
pub fn same_address_line(shared: &SharedAddress) -> String {
    let other = if shared.same.canonical {
        format!("your existing {}", shared.same.name)
    } else {
        format!("{}, also being imported", shared.same.name)
    };
    format!(
        "{} in {} points at the same address as {other}, \
         with different credentials — probably the same server.",
        shared.original, shared.client
    )
}

/// The question a shared address raises. Keeping both comes first because it
/// is the recommended answer: it is what every earlier release did, and it is
/// the one that cannot cost the user an account. Merging is the choice with a
/// consequence, so it is the one that has to be picked.
pub const SAME_ADDRESS_PROMPT: &str = "Which would you like?";

#[must_use]
pub fn same_address_options(shared: &SharedAddress) -> Vec<String> {
    vec![
        format!("Keep both — bring it in as {}", shared.planned),
        format!(
            "Keep just {} — leave {:?} where it is",
            shared.same.name, shared.original
        ),
    ]
}

/// Asks, per shared address, and returns the planned names to leave out.
fn ask_same_address(questions: &[SharedAddress]) -> anyhow::Result<BTreeSet<String>> {
    let mut skip = BTreeSet::new();
    for shared in questions {
        if crate::ui::choose(SAME_ADDRESS_PROMPT, &same_address_options(shared), 0)? == 1 {
            skip.insert(shared.planned.clone());
        }
    }
    Ok(skip)
}

fn display(client_id: &str) -> String {
    ClientKind::from_id(client_id).map_or_else(
        || client_id.to_owned(),
        |kind| kind.display_name().to_owned(),
    )
}

/// The name a conflict would be adopted under if both copies are kept.
///
/// The planner fills this in for every conflict. The fallback is only here so
/// that a plan without one cannot panic a run halfway through writing — and
/// it is a name that is inserted, never overwritten, so a bad guess fails
/// loudly on the duplicate rather than quietly replacing something.
pub fn adopt_as(candidate: &ImportCandidate) -> String {
    candidate
        .adopt_as
        .clone()
        .unwrap_or_else(|| format!("{}-2", candidate.name))
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
    if candidate.command_missing {
        parts.push(command_missing_line(&candidate.name));
    }
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
        println!(
            "! {} differs from the canonical entry (run interactively to keep yours, \
             overwrite it, or keep both as {})",
            candidate.name,
            adopt_as(candidate)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{ConflictChoice, conflict_choice, conflict_options};

    /// The prompt's order and the decoder's order are one contract split
    /// across two functions, and the one that must never drift is which index
    /// overwrites: getting it wrong replaces a canonical entry the user asked
    /// to keep.
    #[test]
    fn the_offered_order_is_the_decoded_order() {
        let options = conflict_options("context7", "context7-2");
        assert_eq!(options.len(), 3);
        assert!(options[0].contains("Keep context7 as it is"), "{options:?}");
        assert!(options[1].contains("as context7-2"), "{options:?}");
        assert!(options[2].contains("Overwrite context7"), "{options:?}");

        assert_eq!(conflict_choice(0), ConflictChoice::KeepCanonical);
        assert_eq!(conflict_choice(1), ConflictChoice::KeepBoth);
        assert_eq!(conflict_choice(2), ConflictChoice::Overwrite);
    }

    /// An index the prompt never offered is the safe answer, not a panic:
    /// this decodes a number that came off a terminal.
    #[test]
    fn an_answer_off_the_end_keeps_canonical() {
        assert_eq!(conflict_choice(99), ConflictChoice::KeepCanonical);
    }
}
