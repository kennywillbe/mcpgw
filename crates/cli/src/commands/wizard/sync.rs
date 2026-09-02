//! Wizard step 4: point every client at the gateway, then check the path
//! actually works.
//!
//! The step is one plan and one question. `mcpgw sync` already asks nothing
//! and writes every client; the wizard's job is to show what
//! that would do first, in the language of "your Cursor entry keeps its name
//! and changes where it points", and to take a single yes for the whole set.
//! Thirteen prompts would be an interrogation rather than show-and-confirm.
//!
//! Writing is the easy half. The half that decides whether anyone trusts the
//! result is the check afterwards: the gateway answering, every enabled
//! server reachable through its own endpoint, and every entry the wizard just
//! wrote pointing at one of them. See the contract in [`super`].

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Context as _;
use mcpgw_core::Detection;
use mcpgw_core::doctor::{GatewayEntry, GatewayPlan};
use mcpgw_core::state::ManagedState;
use mcpgw_core::sync::{SyncPlan, per_server_gateway_servers};

use super::{Ctx, Outcome};
use crate::commands::doctor::{GatewayOutcome, bad_line, ok_line, probe_endpoint};
use crate::commands::sync::{
    Applied, Planned, PlannedClient, apply_client, bridge_command, plan_client,
};
use crate::ui;

/// How long one endpoint gets to answer during the check. Longer than the
/// liveness probe behind [`Ctx::reach`], shorter than `doctor --probe`'s own
/// default: the gateway is known to be up by the time this runs, so the only
/// thing being bounded is an upstream that hangs — and a wizard that looks
/// stuck is worse than one that reports a slow server.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(10);

/// True when there is something to push and it has not been pushed yet.
/// With an empty config there is nothing to point a client at, and once
/// every client holds what the config holds, keeping them current is `mcpgw
/// sync`'s job rather than the wizard's.
pub fn pending(cx: &Ctx) -> bool {
    if cx.enabled_servers() == 0 {
        return false;
    }
    if cx.synced_clients().is_empty() {
        return true;
    }
    // A non-empty record used to be the whole answer, and no longer is: the
    // step before this one *adopts* every client entry it imports, so after
    // an import mcpgw's record names entries it never wrote. Ask the plan
    // instead of the record. A first run that imported four servers must
    // still go on to point the clients at the gateway — telling that user
    // there was nothing to push would leave the machine half set up, which
    // is the one thing this step exists to prevent.
    match plan_all(cx) {
        Ok(plans) => plans.ready.iter().any(|p| p.plan.has_changes()),
        // A plan that will not build is not a step with nothing to do:
        // `run` reaches the same failure and reports it in full.
        Err(_) => true,
    }
}

/// Plans a per-server gateway sync for every client, applies it after one
/// confirmation, and reports whether the result answers.
///
/// # Errors
///
/// Returns a failure if the gateway URL cannot carry an endpoint path, if a
/// client file that exists cannot be read, or if a write — backup, state or
/// client file — fails.
pub fn run(cx: &mut Ctx) -> anyhow::Result<Outcome> {
    let plans = plan_all(cx)?;
    announce(cx, &plans);

    let changing = plans.ready.iter().filter(|p| p.plan.has_changes()).count();
    if changing == 0 {
        // Both no-op shapes get the same dim one-liner rather than a
        // question with no answer worth giving.
        ui::already_done(
            if plans.ready.is_empty() {
                "  no MCP client here to point at the gateway"
            } else {
                "  every client already points at the gateway — nothing to push"
            },
            cx.color,
        );
    } else {
        reassure(cx);
        if !cx.confirm("\nPoint them at the gateway?")? {
            println!();
            ui::already_done(
                "Left alone. `mcpgw sync` does this whenever you're ready.",
                cx.color,
            );
            return Ok(Outcome::Handled);
        }
        apply_all(cx, plans)?;
        // The check below reads the machine as it now is: the entries just
        // written, and a gateway that may have come up in the meantime.
        cx.refresh()?;
    }

    println!();
    verify(cx);
    println!();
    closing(cx);
    Ok(Outcome::Handled)
}

/// Every installed client's plan, plus the ones there was nothing to plan
/// for and why.
struct Plans {
    ready: Vec<PlannedClient>,
    /// Installed clients with a reason, worded as `mcpgw sync` words it.
    skipped: Vec<(&'static str, String)>,
}

/// Plans the same per-server gateway sync `mcpgw sync` would, for every
/// client that is installed.
///
/// Clients that are not installed are left out entirely rather than listed as
/// skipped: the survey step already said how many there are, and repeating
/// eleven "not found" lines here would bury the two the user actually has.
fn plan_all(cx: &Ctx) -> anyhow::Result<Plans> {
    // Checked once, before a single client is read: a base URL that cannot
    // take an endpoint path is wrong for all of them, and finding out halfway
    // would leave some clients flipped and some not.
    mcpgw_core::endpoints::per_server_url(&cx.gateway_url, "probe")
        .with_context(|| format!("--gateway-url {} is not a URL", cx.gateway_url))?;
    let bridge = bridge_command();

    let mut plans = Plans {
        ready: Vec::new(),
        skipped: Vec::new(),
    };
    for (kind, detection) in &cx.detections {
        if matches!(detection, Detection::NotInstalled) {
            continue;
        }
        let desired =
            per_server_gateway_servers(*kind, &cx.config.servers, &cx.gateway_url, &bridge)?;
        let managed = cx.state.clients.get(kind.id()).cloned().unwrap_or_default();
        match plan_client(*kind, &desired, &managed)? {
            Planned::Ready(planned) => plans.ready.push(*planned),
            Planned::Skipped(reason) => plans.skipped.push((kind.display_name(), reason)),
        }
    }
    Ok(plans)
}

fn announce(cx: &Ctx, plans: &Plans) {
    let width = plans
        .ready
        .iter()
        .map(|p| p.kind.display_name())
        .chain(plans.skipped.iter().map(|(name, _)| *name))
        .map(|name| name.chars().count())
        .max()
        .unwrap_or(0);

    let mut bullets: Vec<String> = plans
        .ready
        .iter()
        .map(|planned| {
            let name = planned.kind.display_name();
            format!("{name:width$}  {}", summary(&planned.plan, cx.color))
        })
        .collect();
    bullets.extend(
        plans
            .skipped
            .iter()
            .map(|(name, reason)| format!("{name:width$}  {}", ui::dim(reason, cx.color))),
    );

    let changing = plans.ready.iter().filter(|p| p.plan.has_changes()).count();
    let heading = if changing == 0 {
        "Pointing your clients at the gateway.".to_owned()
    } else {
        format!("Pointing your clients at the gateway — {changing} to update.")
    };
    ui::step(&heading, &bullets, cx.color);
}

/// One client's plan on one line: what changes, by name, and what will not be
/// touched.
fn summary(plan: &SyncPlan, color: bool) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (mark, names) in [
        ("~", &plan.updates),
        ("+", &plan.adds),
        ("-", &plan.removes),
    ] {
        if !names.is_empty() {
            parts.push(format!("{mark} {}", names.join(", ")));
        }
    }
    if !plan.unexclude.is_empty() {
        parts.push(format!(
            "~ {} (taken off the client's exclusion list)",
            plan.unexclude.join(", ")
        ));
    }
    if parts.is_empty() {
        parts.push(ui::dim("already pointing at the gateway", color));
    }
    // Named, dimmed, and out of the way. Entries mcpgw did not write stay
    // exactly where they are, and a user scanning this line for their own
    // hand-made entry should find it here rather than wonder.
    let untouched: Vec<&str> = plan
        .foreign
        .iter()
        .chain(&plan.conflicts)
        .map(String::as_str)
        .collect();
    if !untouched.is_empty() {
        parts.push(ui::dim(
            &format!("{} (not mine — left untouched)", untouched.join(", ")),
            color,
        ));
    }
    parts.join("   ")
}

/// The three things a user needs to hear before saying yes to a tool
/// rewriting thirteen config files.
fn reassure(cx: &Ctx) {
    println!();
    for line in [
        "Each server keeps its name and its entry — only where it points changes.",
        "Tool names don't change. Anything you added by hand is left alone.",
        "Every file is backed up first; `mcpgw sync --rollback` undoes all of it.",
    ] {
        println!("  {}", ui::dim(line, cx.color));
    }
}

fn apply_all(cx: &Ctx, plans: Plans) -> anyhow::Result<()> {
    let state_dir = mcpgw_core::paths::state_dir()
        .context("cannot determine a home directory for the state dir")?;
    let state_path = state_dir.join("managed.json");
    // The same lock `mcpgw sync` holds across its whole load→modify→save
    // window, for the same reason: a sync running beside the wizard would
    // otherwise write back a state it read before these changes.
    let _state_lock = ManagedState::lock(&state_path)?;
    let mut state = ManagedState::load(&state_path)?;

    println!();
    for mut planned in plans.ready {
        if !planned.plan.has_changes() {
            continue;
        }
        let name = planned.kind.display_name();
        match apply_client(&mut planned, &mut state, &state_dir, &state_path)? {
            Applied::Written => ok_line(&format!("{name} — {}", planned.path.display()), cx.color),
            // One client that refuses is not a reason to abandon the others,
            // and nothing of its file was touched.
            Applied::Refused(reason) => bad_line(&format!("{name} — {reason}"), cx.color),
        }
    }
    Ok(())
}

/// One endpoint to check, and the managed entries that dial it.
struct Endpoint {
    url: String,
    /// The server whose own endpoint this is, or `None` for the gateway's
    /// aggregate face.
    server: Option<String>,
    entries: Vec<GatewayEntry>,
}

/// The honest answer to "did that work": is the gateway there, does every
/// enabled server answer through it, and does every entry just written point
/// at one that does.
fn verify(cx: &Ctx) {
    ui::step("Checking that it actually works…", &[], cx.color);

    if !cx.reach.is_up() {
        // The daemon step was skipped or declined. The client files are
        // written and correct; the only thing missing is something running,
        // and failing the wizard over that would be a lie about the work it
        // did do.
        bad_line(
            &format!(
                "nothing is answering at {} yet, so nothing was checked",
                cx.gateway_url
            ),
            cx.color,
        );
        println!(
            "    {}",
            ui::dim(
                "`mcpgw daemon install` runs it in the background, or `mcpgw serve` \
                 in a terminal of its own",
                cx.color,
            )
        );
        return;
    }

    ok_line(
        &format!("gateway answering at {}", cx.gateway_url),
        cx.color,
    );

    let endpoints = endpoints_to_check(cx);
    if endpoints.is_empty() {
        return;
    }
    let results = probe_all(&endpoints);

    let width = endpoints
        .iter()
        .map(|e| label(e).chars().count())
        .chain(cx.synced_clients().iter().map(|name| name.chars().count()))
        .max()
        .unwrap_or(0);

    for (endpoint, outcome) in &results {
        let label = format!("{:width$}", label(endpoint));
        match outcome {
            GatewayOutcome::Ok(success) => ok_line(
                &format!(
                    "{label}  {} — {} tools",
                    ui::dim(&endpoint.url, cx.color),
                    success.tool_count
                ),
                cx.color,
            ),
            GatewayOutcome::Unserved(detail) => {
                bad_line(&format!("{label}  {} — {detail}", endpoint.url), cx.color);
            }
            GatewayOutcome::Failed(err) => {
                bad_line(&format!("{label}  {} — {err}", endpoint.url), cx.color);
            }
        }
    }

    // The rows above say the gateway serves what it should; these say each
    // client is actually pointed at it. They are different failures — a
    // healthy endpoint nobody dials is a sync that did not land — so they get
    // their own lines.
    for line in client_lines(&results, width) {
        match line {
            Ok(text) => ok_line(&text, cx.color),
            Err(text) => bad_line(&text, cx.color),
        }
    }
}

/// How an endpoint names itself in the left column.
fn label(endpoint: &Endpoint) -> &str {
    endpoint.server.as_deref().unwrap_or("gateway")
}

/// Every endpoint worth dialing: one per enabled server, plus anything the
/// managed client entries point at that is not already in that set.
///
/// Both halves are needed. The config side is what the gateway promises to
/// serve; the client side is the path a harness will actually take, and an
/// entry left over from an older layout shows up only there.
fn endpoints_to_check(cx: &Ctx) -> Vec<Endpoint> {
    let mut by_url: BTreeMap<String, Endpoint> = BTreeMap::new();
    for (name, server) in &cx.config.servers {
        if !server.enabled {
            continue;
        }
        let Ok(url) = mcpgw_core::endpoints::per_server_url(&cx.gateway_url, name) else {
            continue;
        };
        by_url.insert(
            url.clone(),
            Endpoint {
                url,
                server: Some(name.clone()),
                entries: Vec::new(),
            },
        );
    }

    for target in managed_targets(cx) {
        by_url
            .entry(target.url.clone())
            .or_insert_with(|| Endpoint {
                url: target.url,
                server: target.server,
                entries: Vec::new(),
            })
            .entries
            .extend(target.entries);
    }
    by_url.into_values().collect()
}

/// What the entries mcpgw wrote actually dial, read back out of the client
/// files — the same pass `doctor` makes, and for the same reason: only the
/// files a client reads can say which path it takes.
fn managed_targets(cx: &Ctx) -> Vec<mcpgw_core::doctor::GatewayTarget> {
    let mut plan = GatewayPlan::new(&cx.gateway_url);
    for (kind, detection) in &cx.detections {
        let Detection::Configured(path) = detection else {
            continue;
        };
        let Some(mine) = cx.state.clients.get(kind.id()) else {
            continue;
        };
        let Ok(read) = kind.load(path) else {
            continue;
        };
        for (name, server) in &read.servers {
            // Only entries mcpgw wrote: one the user pointed somewhere by
            // hand is theirs, and the wizard has no business grading it.
            if mine.contains(name) {
                plan.collect(kind.display_name(), name, server);
            }
        }
    }
    plan.into_targets()
}

/// Dials every endpoint on a runtime built for it and torn down again — the
/// wizard is otherwise synchronous, and carrying a runtime through four steps
/// for one check would put every step in async colour.
///
/// One after another rather than all at once: the gateway is already known to
/// be answering and every endpoint is a loopback hop away, so the wall clock
/// here is a handful of handshakes, and the wizard printing its results in
/// the order it announced them is worth more than the milliseconds.
fn probe_all(endpoints: &[Endpoint]) -> Vec<(&Endpoint, GatewayOutcome)> {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return Vec::new();
    };
    runtime.block_on(async {
        let mut results = Vec::with_capacity(endpoints.len());
        for endpoint in endpoints {
            results.push((
                endpoint,
                probe_endpoint(&endpoint.url, VERIFY_TIMEOUT).await,
            ));
        }
        results
    })
}

/// One line per client that has managed entries: whether all of them landed
/// on an endpoint that answered.
fn client_lines(
    results: &[(&Endpoint, GatewayOutcome)],
    width: usize,
) -> Vec<Result<String, String>> {
    let mut per_client: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for (endpoint, outcome) in results {
        let answering = matches!(outcome, GatewayOutcome::Ok(_));
        for entry in &endpoint.entries {
            let counts = per_client.entry(entry.client.as_str()).or_insert((0, 0));
            counts.1 += 1;
            if answering {
                counts.0 += 1;
            }
        }
    }
    per_client
        .into_iter()
        .map(|(client, (ok, total))| {
            let label = format!("{client:width$}");
            match (ok == total, total) {
                (true, 1) => Ok(format!(
                    "{label}  1 entry, pointing at an endpoint that answers"
                )),
                (true, _) => Ok(format!(
                    "{label}  {total} entries, all pointing at endpoints that answer"
                )),
                (false, _) => Err(format!(
                    "{label}  {} of {total} entries point at an endpoint that did not answer",
                    total - ok
                )),
            }
        })
        .collect()
}

/// The last thing the wizard says.
fn closing(cx: &Ctx) {
    // Mandatory, not decoration. No harness re-reads its MCP config while
    // running, so a user who does not restart sees nothing change and files
    // the single most common bug report there is against a tool like this.
    ui::step(
        "Done. Restart your clients to pick up the new config.",
        &[],
        cx.color,
    );
    println!();
    for (command, what) in [
        ("mcpgw watch", "see traffic as it happens"),
        (
            "mcpgw add <name> -- <cmd>",
            "the gateway picks it up immediately",
        ),
        ("mcpgw doctor --probe", "this check, any time"),
    ] {
        println!("  {command:<28}{}", ui::dim(what, cx.color));
    }
    println!();
    ui::already_done(
        "Ever want your old setup back? `mcpgw eject` restores everything to how it was.",
        cx.color,
    );
}
