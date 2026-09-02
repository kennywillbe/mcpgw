//! Wizard step 2: adopt the servers the clients already have.
//!
//! The step where the wizard first writes something, so it is the step where
//! "show and confirm" has to earn its keep. `mcpgw import` reports what it
//! did after the fact, in one line per entry; here the whole plan is laid out
//! *before* the question, and everything a plan can do that surprises someone
//! — quietly merging two entries into one, quietly renaming one of two
//! entries that share a name, bringing an entry in switched off because
//! nothing here can start it, adopting what is probably a second copy of a
//! server the user already has — gets its own announcement rather than a
//! parenthetical.
//!
//! See the contract in [`super`]: this module is `pending` + `run`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;

use anyhow::Context as _;
use mcpgw_core::import::{ImportCandidate, ImportPlan, plan_import};
use mcpgw_core::state::ManagedState;
use mcpgw_core::{ClientKind, ClientRead, ConfigStore, Detection, Server, Transport, paths};

use super::{Ctx, Outcome};
use crate::commands::import::{
    SAME_ADDRESS_PROMPT, command_missing_line, same_address_line, same_address_options,
    same_address_questions,
};
use crate::ui;

/// True when some configured client holds a server the canonical config has
/// never heard of. A client whose file will not parse is not counted as
/// having something to import — `mcpgw doctor` is where a broken client
/// config gets explained, and the wizard would only be guessing.
pub fn pending(cx: &Ctx) -> bool {
    unimported(cx).is_some()
}

/// Shows the whole import plan, asks once, and writes what was agreed to.
///
/// # Errors
///
/// Returns a failure if the terminal cannot be read, or if the canonical
/// config or the adoption record cannot be written.
pub fn run(cx: &mut Ctx) -> anyhow::Result<Outcome> {
    let (sources, unreadable) = sources(cx);
    let plan = plan_import(
        &sources,
        &cx.config.servers,
        &crate::commands::command_exists,
    );

    // `pending` said there was something here, but it answers from the
    // client files alone; the plan is what knows whether any of it survives
    // slugifying and canonical collisions.
    if plan.new.is_empty() && plan.already.is_empty() && plan.conflicts.is_empty() {
        ui::already_done(
            "· nothing to import — your config already has what your clients hold",
            cx.color,
        );
        return Ok(Outcome::Handled);
    }

    ui::step(
        "Importing what your clients already have.",
        &report(cx, &plan, &unreadable),
        cx.color,
    );

    let names: Vec<String> = plan.new.iter().map(|c| c.name.clone()).collect();
    let Some(mut leave_out) = ask(cx, &question(cx, &plan), &names)? else {
        return Ok(Outcome::Handled);
    };

    // The one question this step asks per entry rather than once for the set:
    // an entry that looks like a second copy of one the user already has.
    // Asked after the big yes, and never about an entry that yes already
    // struck out. `--yes` answers it the way every earlier release behaved —
    // keep both — having printed the observation above either way.
    for shared in same_address_questions(&plan) {
        if leave_out.contains(&shared.planned) {
            continue;
        }
        let picked = cx.choose(
            SAME_ADDRESS_PROMPT,
            &same_address_options(&shared),
            Some(0),
            "mcpgw import",
        )?;
        if picked == 1 {
            leave_out.insert(shared.planned);
        }
    }

    // Everything the user struck out is removed at the source, not at the
    // plan: dropping a candidate would leave its client entries unadopted
    // *and* let a name it was holding stay taken, so the survivors keep
    // suffixes they no longer need.
    let dropped: BTreeSet<(String, String)> = plan
        .new
        .iter()
        .filter(|c| leave_out.contains(&c.name))
        .flat_map(|c| c.origins.iter().cloned())
        .collect();
    let kept: Vec<(String, ClientRead)> = sources
        .into_iter()
        .map(|(id, mut read)| {
            read.servers
                .retain(|name, _| !dropped.contains(&(id.clone(), name.clone())));
            (id, read)
        })
        .collect();

    apply(cx, &kept)?;
    cx.refresh()?;
    Ok(Outcome::Handled)
}

/// Reads every configured client, keeping the ones that fail to parse so the
/// step can say so rather than silently importing less than it found.
fn sources(cx: &Ctx) -> (Vec<(String, ClientRead)>, Vec<&'static str>) {
    let mut sources = Vec::new();
    let mut unreadable = Vec::new();
    for (kind, detection) in &cx.detections {
        let Detection::Configured(path) = detection else {
            continue;
        };
        match kind.load(path) {
            Ok(read) => sources.push((kind.id().to_owned(), read)),
            Err(_) => unreadable.push(kind.display_name()),
        }
    }
    (sources, unreadable)
}

/// The plan, as bullets: the two surprising outcomes first and loudly, then
/// everything that needs no explanation as a single line of names.
fn report(cx: &Ctx, plan: &ImportPlan, unreadable: &[&'static str]) -> Vec<String> {
    let clashes = clashes(plan);
    let clashing: BTreeSet<&str> = clashes
        .iter()
        .flat_map(|(_, members)| members.iter().map(|c| c.name.as_str()))
        .collect();

    let mut bullets = Vec::new();
    if !plan.new.is_empty() {
        bullets.push(format!(
            "{} to bring in, from {}.",
            servers(plan.new.len()),
            sentence(&client_names(&plan.new))
        ));
    }

    // A candidate in a name clash is listed under the clash and nowhere
    // else — its line there already names every client it came from, and
    // reading the same server twice under two headings reads like a bug.
    let shared: Vec<&ImportCandidate> = plan
        .new
        .iter()
        .filter(|c| c.origins.len() > 1 && !clashing.contains(c.name.as_str()))
        .collect();
    if !shared.is_empty() {
        bullets.push(shared_heading(shared.len()));
        for candidate in &shared {
            bullets.push(format!(
                "  {} — from {}{}",
                candidate.name,
                client_names([*candidate]).join(", "),
                aliases(candidate)
            ));
        }
    }

    for (original, members) in &clashes {
        bullets.push(if members.len() == 2 {
            format!(
                "Two clients call a server {original:?} but configure it differently, \
                 so both are kept:"
            )
        } else {
            format!(
                "{} clients call a server {original:?} but configure it differently, \
                 so all of them are kept:",
                members.len()
            )
        });
        for candidate in members {
            bullets.push(format!(
                "  {} — {}: {}",
                candidate.name,
                client_names([*candidate]).join(", "),
                summarize(&candidate.server)
            ));
        }
    }

    bullets.extend(hygiene(plan));

    let rest: Vec<String> = plan
        .new
        .iter()
        .filter(|c| {
            c.origins.len() == 1
                && !clashing.contains(c.name.as_str())
                && !c.command_missing
                && c.same_address.is_none()
        })
        .map(|c| {
            if c.renamed {
                format!("{} (from {:?})", c.name, c.origins[0].1)
            } else {
                c.name.clone()
            }
        })
        .collect();
    if !rest.is_empty() {
        bullets.push(format!(
            "The rest come across as they are: {}.",
            rest.join(", ")
        ));
    }

    if !plan.already.is_empty() {
        let names: Vec<&str> = plan.already.iter().map(|c| c.name.as_str()).collect();
        bullets.push(format!(
            "{} already in your config, unchanged — I'll just record where they came \
             from so sync stops asking: {}.",
            plan.already.len(),
            names.join(", ")
        ));
    }
    if !plan.conflicts.is_empty() {
        let names: Vec<&str> = plan.conflicts.iter().map(|c| c.name.as_str()).collect();
        bullets.push(format!(
            "{} differ from what your config already says — yours wins, I won't touch \
             them: {}.",
            plan.conflicts.len(),
            names.join(", ")
        ));
    }
    for client in unreadable {
        bullets.push(ui::dim(
            &format!("{client}'s config could not be read — `mcpgw doctor` explains it"),
            cx.color,
        ));
    }
    bullets
}

/// The two things the plan does that a list of names cannot show: an entry
/// coming in switched off because nothing on this machine can start it, and
/// an entry that is very likely a second copy of one the user already has.
///
/// Both are kept out of the "the rest come across as they are" list, which is
/// by definition the entries that need no explanation.
fn hygiene(plan: &ImportPlan) -> Vec<String> {
    let mut bullets: Vec<String> = plan
        .new
        .iter()
        .filter(|c| c.command_missing)
        .map(|c| format!("{} — {}", c.name, command_missing_line(&c.name)))
        .collect();
    bullets.extend(same_address_questions(plan).iter().map(same_address_line));
    bullets
}

/// The heading over the servers that turned up in more than one client.
///
/// Written out twice rather than pluralized by rule: one duplicate is the
/// common case on a first run, and "1 of them are" on the first screen mcpgw
/// ever shows reads like a machine talking.
fn shared_heading(count: usize) -> String {
    if count == 1 {
        "1 of them is the same server configured in more than one place — I'll keep one copy:"
            .to_owned()
    } else {
        format!(
            "{count} of them are the same server configured in more than one place — \
             I'll keep one copy of each:"
        )
    }
}

/// Candidates grouped by the client-side name two of them disagreed about:
/// the entries `plan_import` silently suffixed (`db`, `db-2`) because two
/// clients used one name for two different servers.
fn clashes(plan: &ImportPlan) -> Vec<(&str, Vec<&ImportCandidate>)> {
    let mut by_original: BTreeMap<&str, Vec<&ImportCandidate>> = BTreeMap::new();
    for candidate in &plan.new {
        by_original
            .entry(candidate.origins[0].1.as_str())
            .or_default()
            .push(candidate);
    }
    by_original
        .into_iter()
        .filter(|(_, members)| members.len() > 1)
        .collect()
}

/// The other names this same server is filed under, for the case where one
/// definition was found twice under two names.
fn aliases(candidate: &ImportCandidate) -> String {
    let others: Vec<String> = candidate
        .origins
        .iter()
        .filter(|(_, original)| *original != candidate.name)
        .map(|(client, original)| format!("{original:?} in {}", display(client)))
        .collect();
    if others.is_empty() {
        String::new()
    } else {
        format!(" (also configured as {})", sentence(&others))
    }
}

/// A one-line "what does this actually run" for a transport.
///
/// Names only, never values: an env var or a header on an MCP server is
/// where the API token lives, and the wizard prints to a terminal that ends
/// up pasted into bug reports.
fn summarize(server: &Server) -> String {
    let line = match &server.transport {
        Transport::Stdio { command, args, env } => {
            let line = std::iter::once(command.as_str())
                .chain(args.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" ");
            format!("{line}{}", keys("env", env))
        }
        Transport::Http { url, headers } => format!("{url}{}", keys("headers", headers)),
    };
    truncate(&line, 72)
}

/// `" (env: A, B)"`, or nothing at all for an empty map. The map's *keys*,
/// which is the whole point of the helper.
fn keys(label: &str, map: &BTreeMap<String, String>) -> String {
    if map.is_empty() {
        return String::new();
    }
    let names: Vec<&str> = map.keys().map(String::as_str).collect();
    format!(" ({label}: {})", names.join(", "))
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let kept: String = text.chars().take(limit - 1).collect();
    format!("{kept}…")
}

fn question(cx: &Ctx, plan: &ImportPlan) -> String {
    if plan.new.is_empty() {
        return "\nRecord where these came from?".to_owned();
    }
    let lead = format!("\nImport {}?", servers(plan.new.len()));
    if cx.assume_yes {
        // The escape hatch is a line of input, and `--yes` has promised not
        // to read one — offering it here would be offering nothing.
        lead
    } else {
        format!("{lead} (or type names to leave out, comma-separated)")
    }
}

/// Asks the one question this step has, and returns the names to leave out —
/// or `None` when the answer was no.
///
/// Yes-or-no goes through [`Ctx::confirm`], which owns the `--yes`
/// transcript. The names path reads the same single line itself, because
/// "yes, except these two" is not a question `confirm` or `choose` can carry:
/// there is no fixed set of answers to number, and the multi-select widget
/// that would replace this is exactly the raw-mode terminal takeover the
/// wizard set out not to have.
fn ask(cx: &Ctx, question: &str, offered: &[String]) -> anyhow::Result<Option<BTreeSet<String>>> {
    if cx.assume_yes || offered.is_empty() {
        return Ok(cx.confirm(question)?.then(BTreeSet::new));
    }
    loop {
        print!("{question} [Y/n] ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            // EOF: no answer is coming, so take the recommended one rather
            // than re-asking a stdin that is closed.
            return Ok(Some(BTreeSet::new()));
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "" | "y" | "yes" => return Ok(Some(BTreeSet::new())),
            "n" | "no" => return Ok(None),
            _ => {}
        }

        let asked: Vec<&str> = line
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .collect();
        if let Some(unknown) = asked
            .iter()
            .find(|name| !offered.iter().any(|offer| offer == *name))
        {
            println!(
                "  I don't have a server called {unknown:?} to leave out — the names are: {}",
                offered.join(", ")
            );
            continue;
        }
        return Ok(Some(asked.into_iter().map(str::to_owned).collect()));
    }
}

/// Writes the agreed plan, re-planned under the config lock.
///
/// The plan that was *shown* was built from the snapshot on [`Ctx`], with no
/// lock held — a lock across a prompt stalls every other mcpgw process for as
/// long as the terminal goes unanswered. So the plan that is *applied* is
/// built again from the same sources against the config the lock protects,
/// exactly as `mcpgw import` does it.
fn apply(cx: &Ctx, sources: &[(String, ClientRead)]) -> anyhow::Result<()> {
    let mut store = ConfigStore::edit_or_create(&cx.config_path)?;
    let plan = plan_import(
        sources,
        &store.config().servers,
        &crate::commands::command_exists,
    );

    let state_path = paths::state_dir()
        .context("cannot determine a home directory for the state dir")?
        .join("managed.json");
    let _state_lock = ManagedState::lock(&state_path)?;
    let mut state = ManagedState::load(&state_path)?;

    for candidate in &plan.new {
        store.upsert_server(&candidate.name, &candidate.server, false)?;
        adopt(&mut state, candidate);
    }
    for candidate in &plan.already {
        adopt(&mut state, candidate);
    }
    // Conflicts are never resolved here. The wizard's promise is that it does
    // not touch what you already have, and overwriting a canonical entry is
    // the one import outcome that loses something — `mcpgw import` is where
    // that decision is offered, entry by entry.

    // Canonical config first, adoption record second: a state file claiming
    // client entries that no canonical server backs is read by the next sync
    // as a removal, and it would delete the user's entries. This order fails
    // the other way — the entries stay unmanaged and re-running adopts them.
    store.save()?;
    state.save(&state_path).context(
        "the canonical config was written but the adoption record was not; re-run \
         `mcpgw import` to finish adopting — your client entries are untouched \
         until it succeeds",
    )?;

    println!();
    for candidate in &plan.conflicts {
        println!(
            "  {} left alone — your config already has something else under that name",
            candidate.name
        );
    }
    println!(
        "  Imported {}. Your config now has {}.",
        servers(plan.new.len()),
        servers(store.config().servers.len())
    );
    Ok(())
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

/// The display names of every client these candidates came from, in the
/// order the candidates carry them and without repeats.
fn client_names<'a>(candidates: impl IntoIterator<Item = &'a ImportCandidate>) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for (client, _) in candidates.into_iter().flat_map(|c| &c.origins) {
        let name = display(client);
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

fn display(client_id: &str) -> String {
    ClientKind::from_id(client_id).map_or_else(
        || client_id.to_owned(),
        |kind| kind.display_name().to_owned(),
    )
}

/// `a, b and c` — prose, because these appear mid-sentence.
fn sentence(items: &[String]) -> String {
    match items {
        [] => "nowhere".to_owned(),
        [only] => only.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

fn servers(n: usize) -> String {
    if n == 1 {
        "1 server".to_owned()
    } else {
        format!("{n} servers")
    }
}

/// The id of the first client holding an unknown server, for [`pending`].
fn unimported(cx: &Ctx) -> Option<&'static str> {
    cx.detections.iter().find_map(|(kind, detection)| {
        let Detection::Configured(path) = detection else {
            return None;
        };
        let read = kind.load(path).ok()?;
        read.servers
            .keys()
            .any(|name| !cx.config.servers.contains_key(name))
            .then_some(kind.id())
    })
}
