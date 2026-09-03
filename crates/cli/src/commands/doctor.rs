use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use mcpgw_core::doctor::project_unmanaged;
use mcpgw_core::doctor::{
    Finding, GatewayFault, GatewayPlan, GatewayTarget, Severity, check_server,
    classify_gateway_failure, classify_problems, endpoint_server, gateway_unreachable, needs_oauth,
    unserved_endpoint,
};
use mcpgw_core::probe::{ProbeError, ProbeSuccess, gateway_listening, probe_server};
use mcpgw_core::projects::{ProjectConfig, Standing};
use mcpgw_core::state::ManagedState;
use mcpgw_core::{ClientKind, Config, Detection, Error, Server, Transport};
use owo_colors::OwoColorize as _;

/// Budget for the "is anything listening" check, independent of `--timeout`.
/// A refused connect answers instantly; the only thing this bounds is a
/// black-holed address, and waiting a full probe timeout to be told the
/// daemon is down would make a down gateway feel like a hang.
const REACH_TIMEOUT: Duration = Duration::from_secs(2);

/// Key = the exact endpoint a probe would talk to; entries shared between
/// the canonical config and clients (or several clients) probe once. The
/// transport is part of the key so a stdio and an http entry never collapse.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum TargetKey {
    Stdio(String, Vec<String>, BTreeMap<String, String>),
    Http(String, BTreeMap<String, String>),
}

/// One endpoint to dial, the sources that named it, and the name a fix would
/// be spelled with.
struct ProbeTarget {
    server: Server,
    /// `name (source)` per place this endpoint was configured; joined into
    /// the row's label.
    labels: Vec<String>,
    /// The first name this endpoint was seen under. The canonical config is
    /// collected first, so for a server mcpgw knows about this is its
    /// canonical name — which is what any advice naming a command has to
    /// use, since that is the name the command would take.
    name: String,
}

#[derive(Default)]
struct ProbePlan {
    targets: BTreeMap<TargetKey, ProbeTarget>,
}

impl ProbePlan {
    fn collect(&mut self, source: &str, name: &str, server: &Server) {
        if !server.enabled {
            return;
        }
        let key = match &server.transport {
            Transport::Stdio { command, args, env } => {
                TargetKey::Stdio(command.clone(), args.clone(), env.clone())
            }
            Transport::Http { url, headers } => TargetKey::Http(url.clone(), headers.clone()),
        };
        self.targets
            .entry(key)
            .or_insert_with(|| ProbeTarget {
                server: server.clone(),
                labels: Vec::new(),
                name: name.to_owned(),
            })
            .labels
            .push(format!("{name} ({source})"));
    }
}

pub fn run(
    json: bool,
    color: bool,
    probe: Option<Duration>,
    gateway_url: &str,
) -> anyhow::Result<u8> {
    let command_exists = |cmd: &str| super::command_exists(cmd);
    let mut findings: Vec<Finding> = Vec::new();
    let mut plan = ProbePlan::default();
    let mut gateway_plan = GatewayPlan::new(gateway_url);
    // Which client entries mcpgw wrote. A lost or unreadable state file means
    // nothing is managed, which is exactly the answer sync gives it too — the
    // gateway pass then finds no entries and stays quiet rather than
    // second-guessing entries that are the user's.
    let managed = managed_state();

    let path = super::canonical_config_path()?;
    // Kept past the match: the project pass below asks of every repo-local
    // entry whether the canonical config already speaks for it.
    let mut canonical_servers: BTreeMap<String, Server> = BTreeMap::new();
    let canonical_note = match Config::load(&path) {
        Ok(config) => {
            for (name, server) in &config.servers {
                findings.extend(check_server(None, name, server, &command_exists));
                plan.collect("canonical", name, server);
            }
            let note = format!("{} servers", config.servers.len());
            canonical_servers = config.servers;
            note
        }
        Err(Error::NotFound { .. }) => "not created yet (run `mcpgw add`)".to_owned(),
        Err(err) => {
            findings.push(Finding {
                client: None,
                server: None,
                severity: Severity::Error,
                message: error_chain(&err),
                code: None,
            });
            "invalid".to_owned()
        }
    };

    let detections = scan_clients(
        &mut findings,
        &mut plan,
        &mut gateway_plan,
        &managed,
        &command_exists,
    );
    findings.extend(stale_service_exe());
    findings.extend(stale_service_version());

    // Reported from the working directory, not from the machine: a
    // repo-local file is only in front of the user when they are standing in
    // that repo, and doctor is run there.
    let projects = ProjectReport::gather(&canonical_servers, &managed);

    let (probe_results, gateway_report) = match probe {
        Some(timeout) => {
            // One runtime for both passes: they ask the same machine the same
            // kind of question, and a second reactor would only add threads.
            let runtime = tokio::runtime::Runtime::new()?;
            let direct = run_probes(&runtime, plan, timeout);
            let gateway = (!gateway_plan.is_empty())
                .then(|| run_gateway_probes(&runtime, gateway_plan, timeout));
            (Some(direct), gateway)
        }
        None => (None, None),
    };

    let gateway_findings = gateway_report
        .as_ref()
        .map_or(&[][..], |report| &report.findings);
    let probe_findings = probe_results
        .as_ref()
        .map_or(&[][..], |report| &report.findings);
    let errors = count(&findings, Severity::Error)
        + count(&projects.findings, Severity::Error)
        + count(gateway_findings, Severity::Error)
        + count(probe_findings, Severity::Error)
        + probe_results.as_ref().map_or(0, ProbeReport::failures)
        + gateway_report.as_ref().map_or(0, GatewayReport::failures);
    let warnings = count(&findings, Severity::Warning)
        + count(&projects.findings, Severity::Warning)
        + count(gateway_findings, Severity::Warning)
        + count(probe_findings, Severity::Warning);

    if json {
        // Gateway findings join the same array: a consumer counting problems
        // should not have to know which pass produced them.
        let all: Vec<Finding> = findings
            .iter()
            .chain(&projects.findings)
            .chain(probe_findings)
            .chain(gateway_findings)
            .cloned()
            .collect::<Vec<_>>();
        emit_json(
            &path,
            &canonical_note,
            &detections,
            &all,
            &projects,
            probe_results.as_ref(),
            gateway_report.as_ref(),
            errors,
            warnings,
        )?;
    } else {
        render(&path, &canonical_note, &detections, &findings, color);
        render_projects(&projects, color);
        if let Some(probes) = &probe_results {
            render_probes(probes, color);
        }
        if let Some(report) = &gateway_report {
            render_gateway(report, color);
        }
        println!();
        summary_line(errors, warnings, color);
    }

    Ok(u8::from(errors > 0))
}

/// The static pass over every detected client: appends its findings, feeds
/// both probe plans, and returns the one-line state of each client.
fn scan_clients(
    findings: &mut Vec<Finding>,
    plan: &mut ProbePlan,
    gateway_plan: &mut GatewayPlan,
    managed: &ManagedState,
    command_exists: &dyn Fn(&str) -> bool,
) -> Vec<(&'static str, String)> {
    let mut detections: Vec<(&'static str, String)> = Vec::new();
    for kind in ClientKind::ALL {
        let name = kind.display_name();
        match kind.detect() {
            Detection::NotInstalled => detections.push((name, "not found".to_owned())),
            Detection::Installed => {
                detections.push((name, "installed, no MCP config".to_owned()));
                findings.push(Finding {
                    client: Some(name.to_owned()),
                    server: None,
                    severity: Severity::Warning,
                    message: "installed but has no MCP config yet".to_owned(),
                    code: None,
                });
            }
            Detection::Configured(config_path) => {
                detections.push((name, format!("configured ({})", config_path.display())));
                match kind.load(&config_path) {
                    Ok(read) => {
                        findings.extend(classify_problems(name, &read));
                        let mine = managed.clients.get(kind.id());
                        for (server_name, server) in &read.servers {
                            findings.extend(check_server(
                                Some(name),
                                server_name,
                                server,
                                command_exists,
                            ));
                            // Only entries mcpgw wrote: an entry the user
                            // pointed at the gateway by hand is theirs to
                            // keep working, and doctor has no record of what
                            // they meant it to reach.
                            let via_gateway = mine.is_some_and(|names| names.contains(server_name))
                                && gateway_plan.collect(name, server_name, server);
                            // An entry the gateway pass owns is not also a
                            // direct target: it is the same URL, so probing
                            // it twice reports one failure under two headings
                            // and counts it as two errors.
                            if !via_gateway {
                                plan.collect(name, server_name, server);
                            }
                        }
                    }
                    Err(err) => findings.push(Finding {
                        client: Some(name.to_owned()),
                        server: None,
                        severity: Severity::Error,
                        message: error_chain(&err),
                        code: None,
                    }),
                }
            }
        }
    }
    detections
}

/// The warning for a login service pointed at an mcpgw that moved.
///
/// Reported without probing anything, and without `--probe`: the service can
/// be running an old binary perfectly, so there is nothing a dial would
/// reveal. Only when a service was actually recorded — a machine with no
/// daemon has no stale binary to be aimed at.
fn stale_service_exe() -> Option<Finding> {
    let state_dir = mcpgw_core::paths::state_dir()?;
    let spec = mcpgw_core::daemon::load_spec(&state_dir)?;
    mcpgw_core::daemon_check::service_exe(&spec)?.finding()
}

/// The warning for a service that is answering on a build other than this
/// one — the state a `brew upgrade` leaves behind, where every command looks
/// healthy and the new binary is serving nobody.
///
/// The one check in doctor that dials, `--probe` or not: what the running
/// gateway published is only worth reading together with a live answer on
/// its port, so there is no version of this that costs nothing. The dial is
/// paid for only where a gateway has published something at the address the
/// service was installed with — a machine with no service, or one whose
/// gateway predates the record, gets no connect out of `doctor` at all.
fn stale_service_version() -> Option<Finding> {
    let state_dir = mcpgw_core::paths::state_dir()?;
    let spec = mcpgw_core::daemon::load_spec(&state_dir)?;
    mcpgw_core::runtime::read_record(&state_dir, spec.port)
        .ok()
        .flatten()?;
    let reach = reach_gateway(&spec.url());
    mcpgw_core::daemon_check::service_version(&state_dir, spec.port, reach).finding()
}

/// One loopback probe on a runtime built for it and dropped again, the way
/// the wizard does it: doctor's own runtime exists only under `--probe`, and
/// this question is asked either way.
fn reach_gateway(url: &str) -> mcpgw_core::daemon::GatewayReach {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return mcpgw_core::daemon::GatewayReach::Down;
    };
    runtime.block_on(mcpgw_core::daemon::probe_gateway(url, REACH_TIMEOUT))
}

/// mcpgw's record of which client entries it wrote, or an empty one when the
/// state directory cannot even be resolved.
fn managed_state() -> ManagedState {
    mcpgw_core::paths::state_dir()
        .map(|dir| dir.join("managed.json"))
        .and_then(|path| ManagedState::load(&path).ok())
        .unwrap_or_default()
}

/// One repo-local config file as the report sees it.
struct ProjectRow {
    config: ProjectConfig,
    /// Every server in the file with its standing, computed once so the text
    /// and the JSON cannot disagree about it.
    servers: Vec<(String, Standing)>,
}

/// The repo-local pass: the files found around the working directory, and
/// the findings they earn.
///
/// Its findings are kept apart from the main list rather than appended to
/// it, because the renderer files a finding under the client section its
/// `client` names — and these belong under the project section, next to the
/// file they are about.
struct ProjectReport {
    files: Vec<ProjectRow>,
    findings: Vec<Finding>,
}

impl ProjectReport {
    fn gather(canonical: &BTreeMap<String, Server>, state: &ManagedState) -> Self {
        let mut report = Self {
            files: Vec::new(),
            findings: Vec::new(),
        };
        for config in mcpgw_core::projects::discover_cwd() {
            let name = config.kind.display_name();
            // The path leads every message: a project file and the client's
            // per-user file are read by the same client, so the client name
            // alone would not say which of the two is broken.
            report
                .findings
                .extend(
                    classify_problems(name, &config.read)
                        .into_iter()
                        .map(|mut finding| {
                            finding.message =
                                format!("{}: {}", config.path.display(), finding.message);
                            finding
                        }),
                );
            let unmanaged = config.unmanaged_in(canonical, state);
            if unmanaged > 0 {
                report
                    .findings
                    .push(project_unmanaged(name, &config.path, unmanaged));
            }
            let servers = config
                .standings_in(canonical, state)
                .into_iter()
                .map(|(name, standing)| (name.to_owned(), standing))
                .collect();
            report.files.push(ProjectRow { config, servers });
        }
        report
    }

    fn json(&self) -> Vec<serde_json::Value> {
        self.files
            .iter()
            .map(|row| {
                let servers: Vec<serde_json::Value> = row
                    .servers
                    .iter()
                    .map(|(name, standing)| {
                        serde_json::json!({
                            "name": name,
                            // Two flags rather than one enum string: the
                            // older one keeps meaning what it meant, and
                            // "mcpgw writes this entry" is the new question.
                            "mirrors_canonical": *standing != Standing::Unmanaged,
                            "managed": *standing == Standing::Managed,
                        })
                    })
                    .collect();
                serde_json::json!({
                    "path": row.config.path,
                    "dir": row.config.dir,
                    "client": row.config.kind.display_name(),
                    "client_id": row.config.kind.id(),
                    "servers": servers,
                    "unmanaged": row
                        .servers
                        .iter()
                        .filter(|(_, standing)| *standing == Standing::Unmanaged)
                        .count(),
                })
            })
            .collect()
    }
}

/// What one repo-local entry means for the user, in the two words the
/// section exists to say.
fn standing_text(standing: Standing) -> &'static str {
    match standing {
        Standing::Managed => "managed by sync",
        // Right today and nobody's to keep right: the canonical entry can
        // change tomorrow and this file will not follow it.
        Standing::Mirrors => "mirrors canonical, not managed",
        Standing::Unmanaged => "not managed: direct entry stays live after sync",
    }
}

/// The project section, printed only where there is a file to name — a
/// machine-wide report has no business claiming a repo the user is not in.
fn render_projects(report: &ProjectReport, color: bool) {
    if report.files.is_empty() {
        return;
    }
    println!();
    heading("project configs", color);
    for row in &report.files {
        let count = row.servers.len();
        let plural = if count == 1 { "server" } else { "servers" };
        println!(
            "  {} — {}, {count} {plural}",
            row.config.path.display(),
            row.config.kind.display_name()
        );
        for (name, standing) in &row.servers {
            println!("      {name}: {}", standing_text(*standing));
        }
    }
    for finding in &report.findings {
        print_finding(finding, color);
    }
}

// Pure serialization of already-computed pieces; bundling them into a
// struct would exist only to appease the lint.
#[allow(clippy::too_many_arguments)]
fn emit_json(
    path: &Path,
    canonical_note: &str,
    detections: &[(&'static str, String)],
    findings: &[Finding],
    projects: &ProjectReport,
    probes: Option<&ProbeReport>,
    gateway: Option<&GatewayReport>,
    errors: usize,
    warnings: usize,
) -> anyhow::Result<()> {
    let clients: Vec<serde_json::Value> = detections
        .iter()
        .map(|(client, state)| serde_json::json!({ "client": client, "state": state }))
        .collect();
    let mut out = serde_json::json!({
        "config": { "path": path, "state": canonical_note },
        "clients": clients,
        // Always present, empty included: a consumer should not have to
        // tell "no project configs" from "an mcpgw that does not look".
        "projects": projects.json(),
        "findings": findings,
        "errors": errors,
        "warnings": warnings,
    });
    if let Some(probes) = probes {
        let entries: Vec<serde_json::Value> = probes
            .results
            .iter()
            .map(|(label, outcome)| match outcome {
                Ok(success) => serde_json::json!({
                    "servers": label, "ok": true,
                    "server_name": success.server_name,
                    "server_version": success.server_version,
                    "tools": success.tool_count,
                }),
                Err(ProbeError::AuthRequired) => serde_json::json!({
                    "servers": label, "ok": false,
                    "code": mcpgw_core::doctor::NEEDS_OAUTH,
                    "error": probes.oauth_message(label),
                }),
                Err(err) => serde_json::json!({
                    "servers": label, "ok": false, "error": err.to_string(),
                }),
            })
            .collect();
        out["probes"] = serde_json::json!({ "results": entries });
    }
    if let Some(gateway) = gateway {
        let entries: Vec<serde_json::Value> = gateway
            .results
            .iter()
            .map(|(target, outcome)| {
                let mut row = serde_json::json!({
                    "url": target.url, "entries": target.entries,
                });
                if let Some(server) = &target.server {
                    row["server"] = serde_json::json!(server);
                }
                match outcome {
                    GatewayOutcome::Ok(success) => {
                        row["ok"] = serde_json::json!(true);
                        row["server_name"] = serde_json::json!(success.server_name);
                        row["server_version"] = serde_json::json!(success.server_version);
                        row["tools"] = serde_json::json!(success.tool_count);
                    }
                    GatewayOutcome::Unserved(detail) => {
                        row["ok"] = serde_json::json!(false);
                        row["unserved"] = serde_json::json!(true);
                        row["error"] = serde_json::json!(detail);
                    }
                    GatewayOutcome::NeedsOAuth(name) => {
                        row["ok"] = serde_json::json!(false);
                        row["code"] = serde_json::json!(mcpgw_core::doctor::NEEDS_OAUTH);
                        row["error"] = serde_json::json!(needs_oauth(None, name).message);
                    }
                    GatewayOutcome::Failed(err) => {
                        row["ok"] = serde_json::json!(false);
                        row["error"] = serde_json::json!(err.to_string());
                    }
                }
                row
            })
            .collect();
        out["gateway"] = serde_json::json!({
            "base": gateway.base,
            "reachable": gateway.reachable,
            "skipped": gateway.skipped,
            "results": entries,
        });
    }
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

type ProbeRow = (
    String,
    Result<mcpgw_core::probe::ProbeSuccess, mcpgw_core::probe::ProbeError>,
);

struct ProbeReport {
    results: Vec<ProbeRow>,
    /// Row label → the name a command about that row would name.
    names: BTreeMap<String, String>,
    /// The rows that are not failures at all: a server behind OAuth is
    /// working, and what it needs is a login on this machine. Kept beside
    /// the rows the way the gateway pass keeps its own, so they are counted
    /// once and rendered from the text they were built with.
    findings: Vec<Finding>,
}

impl ProbeReport {
    /// Rows that are genuinely broken, which is every failed one except the
    /// servers waiting on a login.
    fn failures(&self) -> usize {
        self.results
            .iter()
            .filter(|(_, outcome)| !matches!(outcome, Ok(_) | Err(ProbeError::AuthRequired)))
            .count()
    }

    /// The finding text for `label`'s row, so the rendered line and the
    /// `--json` entry say what the finding says rather than a second wording
    /// of it.
    fn oauth_message(&self, label: &str) -> String {
        needs_oauth(None, self.name(label)).message
    }

    fn name<'a>(&'a self, label: &'a str) -> &'a str {
        self.names.get(label).map_or(label, String::as_str)
    }
}

fn run_probes(
    runtime: &tokio::runtime::Runtime,
    plan: ProbePlan,
    timeout: Duration,
) -> ProbeReport {
    let mut names: BTreeMap<String, String> = BTreeMap::new();
    let mut results = runtime.block_on(async {
        let mut set = tokio::task::JoinSet::new();
        let mut labels: BTreeMap<tokio::task::Id, String> = BTreeMap::new();
        for target in plan.targets.into_values() {
            let label = target.labels.join(", ");
            names.insert(label.clone(), target.name);
            let server = target.server;
            let handle = set.spawn({
                let label = label.clone();
                async move { (label, probe_server(&server, timeout).await) }
            });
            labels.insert(handle.id(), label);
        }
        collect_probes(set, &labels).await
    });
    results.sort_by(|a, b| a.0.cmp(&b.0));
    let findings = results
        .iter()
        .filter(|(_, outcome)| matches!(outcome, Err(ProbeError::AuthRequired)))
        .map(|(label, _)| {
            needs_oauth(
                None,
                names.get(label).map_or(label.as_str(), String::as_str),
            )
        })
        .collect();
    ProbeReport {
        results,
        names,
        findings,
    }
}

/// What one managed entry's endpoint answered.
pub(super) enum GatewayOutcome {
    Ok(ProbeSuccess),
    /// The gateway is up but serves nothing there; the string is its own
    /// answer, which names what it does serve. Reported as findings rather
    /// than as a failed row, so it is counted once.
    Unserved(String),
    /// The endpoint is served and answers; the server behind it is waiting
    /// on a login. The string is that server's name.
    NeedsOAuth(String),
    Failed(ProbeError),
}

struct GatewayReport {
    base: String,
    reachable: bool,
    /// How many endpoints were never dialed because the gateway is down.
    skipped: usize,
    results: Vec<(GatewayTarget, GatewayOutcome)>,
    findings: Vec<Finding>,
}

impl GatewayReport {
    fn failures(&self) -> usize {
        self.results
            .iter()
            .filter(|(_, outcome)| matches!(outcome, GatewayOutcome::Failed(_)))
            .count()
    }
}

/// Dials every gateway endpoint the managed client entries point at, the same
/// way the clients themselves would.
fn run_gateway_probes(
    runtime: &tokio::runtime::Runtime,
    plan: GatewayPlan,
    timeout: Duration,
) -> GatewayReport {
    let mut report = GatewayReport {
        base: plan.base().to_owned(),
        reachable: false,
        skipped: plan.len(),
        results: Vec::new(),
        findings: Vec::new(),
    };
    let targets = plan.into_targets();

    runtime.block_on(async {
        report.reachable = gateway_listening(&report.base, REACH_TIMEOUT.min(timeout)).await;
        if !report.reachable {
            report.findings.push(gateway_unreachable(&report.base));
            return;
        }
        report.skipped = 0;

        let mut set = tokio::task::JoinSet::new();
        let mut pending: BTreeMap<tokio::task::Id, GatewayTarget> = BTreeMap::new();
        for target in targets {
            let handle = set.spawn({
                let target = target.clone();
                async move { (target.clone(), probe_endpoint(&target.url, timeout).await) }
            });
            pending.insert(handle.id(), target);
        }
        while let Some(joined) = set.join_next_with_id().await {
            report.results.push(match joined {
                Ok((_, row)) => row,
                // Same rule as the direct pass: one broken task costs its own
                // row, not the report.
                Err(err) => {
                    let target = pending.remove(&err.id());
                    let outcome = GatewayOutcome::Failed(ProbeError::Aborted {
                        reason: err.to_string(),
                    });
                    match target {
                        Some(target) => (target, outcome),
                        None => continue,
                    }
                }
            });
        }
    });

    report.results.sort_by(|a, b| a.0.url.cmp(&b.0.url));
    for (target, outcome) in &report.results {
        match outcome {
            GatewayOutcome::Unserved(detail) => {
                report.findings.extend(unserved_endpoint(target, detail));
            }
            GatewayOutcome::NeedsOAuth(name) => report.findings.push(needs_oauth(None, name)),
            GatewayOutcome::Ok(_) | GatewayOutcome::Failed(_) => {}
        }
    }
    report
}

/// Runs the full client handshake against one gateway endpoint.
///
/// No headers are sent: the gateway has no authentication yet (`serve` says
/// so out loud when bound off loopback), so anything an entry carries is for
/// an upstream the gateway holds, not for the gateway itself.
pub(super) async fn probe_endpoint(url: &str, timeout: Duration) -> GatewayOutcome {
    let server = Server {
        enabled: true,
        tags: Vec::new(),
        transport: Transport::Http {
            url: url.to_owned(),
            headers: BTreeMap::new(),
        },
    };
    match probe_server(&server, timeout).await {
        Ok(success) => GatewayOutcome::Ok(success),
        Err(err) => match classify_gateway_failure(&err.to_string()) {
            GatewayFault::Unserved(detail) => GatewayOutcome::Unserved(detail),
            // The name comes from the path rather than from the message:
            // `/s/<name>` is the endpoint for exactly one server, and that
            // is the name the login would be spelled with. `/mcp` fronts
            // every server at once and has no such name, so its error is
            // left to say for itself which upstream it was.
            GatewayFault::NeedsOAuth => {
                endpoint_server(url).map_or(GatewayOutcome::Failed(err), GatewayOutcome::NeedsOAuth)
            }
            GatewayFault::Failed => GatewayOutcome::Failed(err),
        },
    }
}

/// Drains the probe tasks. A task that panics costs its own row, not the
/// whole report: doctor exists to tell the user what is wrong with their
/// setup, and it can still do that for the other servers. `labels` maps task
/// ids back to target names so even the failed row says which server it was.
async fn collect_probes(
    mut set: tokio::task::JoinSet<ProbeRow>,
    labels: &BTreeMap<tokio::task::Id, String>,
) -> Vec<ProbeRow> {
    let mut collected = Vec::new();
    while let Some(joined) = set.join_next_with_id().await {
        collected.push(match joined {
            Ok((_, row)) => row,
            Err(err) => {
                let label = labels
                    .get(&err.id())
                    .cloned()
                    .unwrap_or_else(|| "unknown server".to_owned());
                (
                    label,
                    Err(mcpgw_core::probe::ProbeError::Aborted {
                        reason: err.to_string(),
                    }),
                )
            }
        });
    }
    collected
}

/// Prints `text` as a section heading.
fn heading(text: &str, color: bool) {
    if color {
        println!("{}", text.bold());
    } else {
        println!("{text}");
    }
}

pub(super) fn ok_line(line: &str, color: bool) {
    if color {
        println!("  {} {line}", "✓".green());
    } else {
        println!("  ✓ {line}");
    }
}

pub(super) fn warn_line(line: &str, color: bool) {
    if color {
        println!("  {} {line}", "⚠".yellow());
    } else {
        println!("  ⚠ {line}");
    }
}

pub(super) fn bad_line(line: &str, color: bool) {
    if color {
        println!("  {} {line}", "✗".red());
    } else {
        println!("  ✗ {line}");
    }
}

fn render_probes(probes: &ProbeReport, color: bool) {
    println!();
    // Named "direct" against the gateway section below it: the two dial
    // different things and fail for different reasons, and which one is red
    // is the whole diagnosis.
    heading("probes — direct to each server", color);
    for (label, outcome) in &probes.results {
        match outcome {
            Ok(success) => {
                let line = format!(
                    "{label}: {} {}, {} tools",
                    success.server_name, success.server_version, success.tool_count
                );
                ok_line(&line, color);
            }
            // Not a failure and not rendered as one: the fix is a login,
            // and the sentence that says so is the finding's own, so the two
            // renderings cannot drift apart.
            Err(ProbeError::AuthRequired) => {
                warn_line(&format!("{label}: {}", probes.oauth_message(label)), color);
            }
            Err(err) => bad_line(&format!("{label}: {err}"), color),
        }
    }
}

/// The second probe section: the path clients actually take. A server can be
/// perfectly healthy on the row above and unreachable on this one — a gateway
/// that is down, an endpoint it never served, a name that went stale — which
/// is why they are two sections and not one.
fn render_gateway(report: &GatewayReport, color: bool) {
    println!();
    heading(
        &format!("probes — through the gateway at {}", report.base),
        color,
    );
    if !report.reachable {
        bad_line(
            &format!(
                "not reachable — start it with `mcpgw serve` ({} endpoint(s) not checked)",
                report.skipped
            ),
            color,
        );
        return;
    }
    for (target, outcome) in &report.results {
        let where_ = format!("{} ← {}", target.url, target.label());
        match outcome {
            GatewayOutcome::Ok(success) => ok_line(
                &format!(
                    "{where_}: {} {}, {} tools",
                    success.server_name, success.server_version, success.tool_count
                ),
                color,
            ),
            // One line per entry, matching the findings one for one: each is
            // a different client file, and the summary counts them that way.
            // Rebuilt from the same function that produced them so the two
            // renderings cannot drift apart.
            GatewayOutcome::Unserved(detail) => {
                for finding in unserved_endpoint(target, detail) {
                    let client = finding.client.unwrap_or_default();
                    let entry = finding.server.unwrap_or_default();
                    bad_line(&format!("{client} {entry:?} {}", finding.message), color);
                }
            }
            GatewayOutcome::NeedsOAuth(name) => {
                warn_line(
                    &format!("{where_}: {}", needs_oauth(None, name).message),
                    color,
                );
            }
            GatewayOutcome::Failed(err) => bad_line(&format!("{where_}: {err}"), color),
        }
    }
}

fn count(findings: &[Finding], severity: Severity) -> usize {
    findings.iter().filter(|f| f.severity == severity).count()
}

// anyhow renders source chains for propagated errors; findings embed errors
// as strings, so the chain must be flattened by hand.
fn error_chain(err: &dyn std::error::Error) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        out.push_str(": ");
        out.push_str(&cause.to_string());
        source = cause.source();
    }
    out
}

fn render(
    path: &Path,
    canonical_note: &str,
    detections: &[(&'static str, String)],
    findings: &[Finding],
    color: bool,
) {
    heading(
        &format!("canonical config ({}) — {canonical_note}", path.display()),
        color,
    );
    print_findings(findings, None, color);

    for (client, state) in detections {
        println!();
        heading(&format!("{client} — {state}"), color);
        print_findings(findings, Some(client), color);
    }
}

fn print_findings(findings: &[Finding], client: Option<&str>, color: bool) {
    let mut any = false;
    for finding in findings.iter().filter(|f| f.client.as_deref() == client) {
        any = true;
        print_finding(finding, color);
    }
    if !any && client.is_none() {
        // Only the canonical section gets an explicit all-clear; client
        // sections without findings stay quiet to keep the report short.
        println!("  ✓ no problems");
    }
}

fn print_finding(finding: &Finding, color: bool) {
    let subject = finding
        .server
        .as_ref()
        .map_or(String::new(), |s| format!("server {s:?}: "));
    let line = format!("{subject}{}", finding.message);
    match (finding.severity, color) {
        (Severity::Error, true) => println!("  {} {line}", "✗".red()),
        (Severity::Error, false) => println!("  ✗ {line}"),
        (Severity::Warning, true) => println!("  {} {line}", "⚠".yellow()),
        (Severity::Warning, false) => println!("  ⚠ {line}"),
    }
}

fn summary_line(errors: usize, warnings: usize, color: bool) {
    let text = format!("{errors} errors, {warnings} warnings");
    if !color {
        println!("{text}");
    } else if errors > 0 {
        println!("{}", text.red());
    } else if warnings > 0 {
        println!("{}", text.yellow());
    } else {
        println!("{}", text.green());
    }
}

#[cfg(test)]
mod tests {
    use mcpgw_core::probe::{ProbeError, ProbeSuccess};

    use super::{ProbeRow, collect_probes};

    #[tokio::test]
    async fn a_panicking_probe_becomes_one_failed_row() {
        let mut set = tokio::task::JoinSet::<ProbeRow>::new();
        let mut labels = std::collections::BTreeMap::new();

        let handle = set.spawn(async { panic!("probe blew up") });
        labels.insert(handle.id(), "boom (canonical)".to_owned());
        set.spawn(async {
            (
                "fine (canonical)".to_owned(),
                Ok(ProbeSuccess {
                    server_name: "fixture".to_owned(),
                    server_version: "1".to_owned(),
                    tool_count: 2,
                }),
            )
        });

        let mut rows = collect_probes(set, &labels).await;
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(rows.len(), 2);
        // The panicked target is named, not lost, and the healthy one still
        // reports.
        assert_eq!(rows[0].0, "boom (canonical)");
        assert!(matches!(rows[0].1, Err(ProbeError::Aborted { .. })));
        assert_eq!(rows[1].0, "fine (canonical)");
        assert!(rows[1].1.is_ok());
    }
}
