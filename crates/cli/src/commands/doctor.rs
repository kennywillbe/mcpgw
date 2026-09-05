use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use mcpgw_core::config::ClientScope;
use mcpgw_core::doctor::project_unmanaged;
use mcpgw_core::doctor::{
    ClientBudget, Finding, GatewayFault, GatewayPlan, GatewayTarget, Severity, check_server,
    classify_gateway_failure, classify_problems, client_budget, endpoint_server,
    gateway_unreachable, needs_oauth, over_tool_cap, tool_cap, tool_drift, unknown_config_keys,
    unknown_scoped_servers, unmatched_tool_rules, unserved_endpoint,
};
use mcpgw_core::probe::{ProbeError, ProbeSuccess, gateway_listening, probe_server};
use mcpgw_core::probe_state::{AuthObservation, ProbeState};
use mcpgw_core::projects::{ProjectConfig, Standing};
use mcpgw_core::state::ManagedState;
use mcpgw_core::{ClientKind, Config, Detection, Error, Server, Transport};
use owo_colors::OwoColorize as _;

/// Budget for the "is anything listening" check, independent of `--timeout`.
/// A refused connect answers instantly; the only thing this bounds is a
/// black-holed address, and waiting a full probe timeout to be told the
/// daemon is down would make a down gateway feel like a hang.
const REACH_TIMEOUT: Duration = Duration::from_secs(2);

/// How a target the canonical config named labels itself, and the source
/// name [`ProbePlan::collect`] recognises as that config.
const CANONICAL_SOURCE: &str = "canonical";

/// Key = the exact endpoint a probe would talk to; entries shared between
/// the canonical config and clients (or several clients) probe once. The
/// transport is part of the key so a stdio and an http entry never collapse.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum TargetKey {
    Stdio(String, Vec<String>, BTreeMap<String, String>),
    Http(String, Vec<String>, BTreeMap<String, String>),
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
    /// The `headers_command` line, for an entry whose headers are minted
    /// rather than written down. The row says so: a reader comparing this
    /// report against the config file otherwise finds an entry with no
    /// credential in it and no explanation of how it authenticated.
    helper: Option<String>,
    /// Whether the canonical config named this endpoint.
    ///
    /// Only what the canonical config points at is worth recording for
    /// `auth status` to read back: a client entry can carry the same name as
    /// a canonical server while pointing somewhere else entirely, and a
    /// record written under that name would answer for the wrong server.
    canonical: bool,
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
            // The command is part of the key: two entries at one URL that
            // mint their credentials differently are two things to dial.
            Transport::Http {
                url,
                headers_command,
                headers,
                ..
            } => TargetKey::Http(url.clone(), headers_command.clone(), headers.clone()),
        };
        let target = self.targets.entry(key).or_insert_with(|| ProbeTarget {
            helper: match &server.transport {
                Transport::Http {
                    headers_command, ..
                } if !headers_command.is_empty() => {
                    Some(mcpgw_core::headers::display(headers_command))
                }
                Transport::Http { .. } | Transport::Stdio { .. } => None,
            },
            server: server.clone(),
            labels: Vec::new(),
            name: name.to_owned(),
            canonical: false,
        });
        target.canonical |= source == CANONICAL_SOURCE;
        target.labels.push(format!("{name} ({source})"));
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
    let Canonical {
        note: canonical_note,
        servers: canonical_servers,
        scopes,
        retain_days,
    } = load_canonical(&path, &mut findings, &mut plan, &command_exists);
    let traffic = TrafficReport::gather(retain_days);

    findings.extend(drifted_tool_definitions(&canonical_servers));

    let detections = scan_clients(
        &mut findings,
        &mut plan,
        &mut gateway_plan,
        &managed,
        &command_exists,
    );
    findings.extend(daemon_path_gaps(&canonical_servers));
    findings.extend(stale_service_exe());
    findings.extend(stale_service_version());
    findings.extend(unauthenticated_bind());
    findings.extend(loose_permissions(&path));

    // Reported from the working directory, not from the machine: a
    // repo-local file is only in front of the user when they are standing in
    // that repo, and doctor is run there.
    let projects = ProjectReport::gather(&canonical_servers, &managed);

    let (probe_results, gateway_report) = match probe {
        Some(timeout) => {
            // One runtime for both passes: they ask the same machine the same
            // kind of question, and a second reactor would only add threads.
            let runtime = tokio::runtime::Runtime::new()?;
            let mut direct = run_probes(&runtime, plan, timeout);
            // Only under `--probe`: whether a rule matches anything can only
            // be answered by the server, and asking it is what `--probe` is.
            let stale = stale_tool_rules(&direct, &canonical_servers);
            direct.findings.extend(stale);
            let gateway = (!gateway_plan.is_empty())
                .then(|| run_gateway_probes(&runtime, gateway_plan, timeout));
            (Some(direct), gateway)
        }
        None => (None, None),
    };

    // Only under `--probe`: what a client is offered can be counted from the
    // config, but what it *costs* is the servers' own tool definitions, and
    // nothing on disk holds those.
    let (budgets, budget_findings) = probe_results.as_ref().map_or_else(
        || (Vec::new(), Vec::new()),
        |probes| budget_report(probes, &canonical_servers, &scopes, &managed),
    );

    let gateway_findings = gateway_report
        .as_ref()
        .map_or(&[][..], |report| &report.findings);
    let probe_findings = probe_results
        .as_ref()
        .map_or(&[][..], |report| &report.findings);
    let sets: [&[Finding]; 5] = [
        &findings,
        &projects.findings,
        probe_findings,
        gateway_findings,
        &budget_findings,
    ];
    let (errors, warnings) = tally(&sets, probe_results.as_ref(), gateway_report.as_ref());

    if json {
        let gateway_findings = gateway_report
            .as_ref()
            .map_or(&[][..], |report| &report.findings);
        let probe_findings = probe_results
            .as_ref()
            .map_or(&[][..], |report| &report.findings);
        // Gateway findings join the same array: a consumer counting problems
        // should not have to know which pass produced them.
        let all: Vec<Finding> = findings
            .iter()
            .chain(&projects.findings)
            .chain(probe_findings)
            .chain(gateway_findings)
            .chain(&budget_findings)
            .cloned()
            .collect::<Vec<_>>();
        emit_json(
            &path,
            &canonical_note,
            &detections,
            &all,
            &projects,
            &traffic,
            probe_results.as_ref(),
            gateway_report.as_ref(),
            &budgets,
            errors,
            warnings,
        )?;
    } else {
        render(&path, &canonical_note, &detections, &findings, color);
        render_traffic(&traffic, color);
        render_projects(&projects, color);
        if let Some(probes) = &probe_results {
            render_probes(probes, color);
        }
        if let Some(report) = &gateway_report {
            render_gateway(report, color);
        }
        render_budgets(&budgets, &budget_findings, color);
        println!();
        summary_line(errors, warnings, color);
    }

    Ok(u8::from(errors > 0))
}

/// What the traffic log costs on disk and how long it is kept.
///
/// Reported unconditionally, including when nothing has been captured yet: a
/// reader asking "how big is this going to get" needs the retention window
/// answered whether or not there are files today.
struct TrafficReport {
    dir: Option<std::path::PathBuf>,
    usage: mcpgw_core::capture::Usage,
    retain_days: u32,
}

impl TrafficReport {
    fn gather(retain_days: u32) -> Self {
        let dir = mcpgw_core::paths::state_dir()
            .map(|state| state.join(mcpgw_core::capture::TRAFFIC_DIR));
        let usage = dir
            .as_deref()
            .and_then(|dir| mcpgw_core::capture::usage(dir).ok())
            .unwrap_or_default();
        Self {
            dir,
            usage,
            retain_days,
        }
    }

    /// `14 days` — or the fact that nothing prunes, spelled out rather than
    /// left as a bare `0`.
    fn retention(&self) -> String {
        if self.retain_days == 0 {
            "kept forever".to_owned()
        } else {
            format!("kept {} days", self.retain_days)
        }
    }

    fn line(&self) -> String {
        let size = human_bytes(self.usage.bytes);
        let files = self.usage.files;
        let plural = if files == 1 { "" } else { "s" };
        let oldest = self
            .usage
            .oldest
            .as_ref()
            .map_or(String::new(), |date| format!(", oldest {date}"));
        format!("{files} file{plural}, {size}, {}{oldest}", self.retention())
    }

    fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "dir": self.dir,
            "files": self.usage.files,
            "bytes": self.usage.bytes,
            "oldest": self.usage.oldest,
            "retain_days": self.retain_days,
        })
    }
}

/// Bytes at one decimal place, in the largest unit that keeps the number
/// under 1024 — a traffic log is read as "is this big", not as a byte count.
fn human_bytes(bytes: u64) -> String {
    #[expect(
        clippy::cast_precision_loss,
        reason = "a display size is allowed to round"
    )]
    let mut value = bytes as f64;
    let mut unit = "B";
    for next in ["KB", "MB", "GB", "TB"] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    if unit == "B" {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {unit}")
    }
}

/// The canonical config as the rest of the report needs it: the line that
/// describes it, the servers (which the project pass asks about too) and the
/// client scopes.
struct Canonical {
    note: String,
    servers: BTreeMap<String, Server>,
    scopes: BTreeMap<String, ClientScope>,
    /// `[capture] retain_days` as this config asks for it, so the traffic
    /// line reports the window the gateway would actually prune to.
    retain_days: u32,
}

/// Reads it, checks every entry, and feeds the probe plan.
fn load_canonical(
    path: &Path,
    findings: &mut Vec<Finding>,
    plan: &mut ProbePlan,
    command_exists: &dyn Fn(&str) -> bool,
) -> Canonical {
    match Config::load_reporting(path) {
        Ok((config, unknown)) => {
            findings.extend(unknown_config_keys(&unknown));
            for (name, server) in &config.servers {
                findings.extend(check_server(None, name, server, command_exists));
                plan.collect(CANONICAL_SOURCE, name, server);
            }
            for (client, scope) in &config.clients {
                findings.extend(unknown_scoped_servers(client, scope, &config.servers));
            }
            Canonical {
                note: format!("{} servers", config.servers.len()),
                retain_days: config.capture.retain_days,
                servers: config.servers,
                scopes: config.clients,
            }
        }
        Err(err) => {
            let note = if matches!(err, Error::NotFound { .. }) {
                "not created yet (run `mcpgw add`)".to_owned()
            } else {
                findings.push(Finding {
                    client: None,
                    server: None,
                    severity: Severity::Error,
                    message: error_chain(&err),
                    code: None,
                });
                "invalid".to_owned()
            };
            Canonical {
                note,
                servers: BTreeMap::new(),
                scopes: BTreeMap::new(),
                // A config that will not load is a gateway running on the
                // defaults or not running at all; either way the default
                // window is the honest thing to report.
                retain_days: mcpgw_core::capture::DEFAULT_RETAIN_DAYS,
            }
        }
    }
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
                            // Only for an entry mcpgw wrote and that does aim
                            // at this gateway: an entry pointing somewhere
                            // else has no reason to hold this token.
                            if via_gateway {
                                findings.extend(mcpgw_core::doctor::missing_gateway_token(
                                    name,
                                    server_name,
                                    server,
                                    kind.carries_gateway_token(),
                                ));
                            }
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

/// The error for a gateway on this machine listening where other machines can
/// reach it with nothing to authenticate them.
///
/// Both the installed service and whatever is actually running are asked: a
/// `daemon install --bind` that passed preflight under `require_token` and a
/// config edit that has since turned the knob off are the same problem, and
/// only the running gateway's own record says what a foreground `mcpgw serve
/// --bind 0.0.0.0` did.
fn unauthenticated_bind() -> Vec<Finding> {
    let Some(state_dir) = mcpgw_core::paths::state_dir() else {
        return Vec::new();
    };
    let require = super::token::require_token();
    let installed = mcpgw_core::daemon::load_spec(&state_dir);
    let binds = installed
        .iter()
        .map(|spec| (spec.bind.clone(), spec.port))
        .chain(
            installed
                .iter()
                .filter_map(|spec| mcpgw_core::runtime::read_record(&state_dir, spec.port).ok()?)
                .map(|record| (record.bind, record.port)),
        );
    let mut seen = std::collections::BTreeSet::new();
    binds
        .filter(|bind| seen.insert(bind.clone()))
        .filter_map(|(bind, _)| mcpgw_core::doctor::unauthenticated_bind(&bind, require))
        .collect()
}

/// The warnings for stdio servers this shell can start and the daemon cannot.
///
/// Only the canonical config: these are the entries the installed service
/// spawns, and a client's own entry is started by that client with that
/// client's environment, so the daemon's `PATH` says nothing about it.
///
/// Silent on a machine with no service definition — there is no second `PATH`
/// to disagree with — and silent for an entry carrying its own `PATH` in
/// `env`, which reaches the child whatever the service was installed with.
fn daemon_path_gaps(servers: &BTreeMap<String, Server>) -> Vec<Finding> {
    let Some(service_path) = mcpgw_core::daemon_check::service_path() else {
        return Vec::new();
    };
    servers
        .iter()
        .filter(|(_, server)| server.enabled)
        .filter_map(|(name, server)| match &server.transport {
            Transport::Stdio { command, env, .. } if !env.contains_key("PATH") => {
                mcpgw_core::daemon_check::stdio_command_reach(command, Some(&service_path))
                    .finding(name)
            }
            Transport::Stdio { .. } | Transport::Http { .. } => None,
        })
        .collect()
}

/// Every file mcpgw writes its owner's secrets into, checked against the
/// mode it was written with.
///
/// The state directory is walked rather than listed from a manifest: the
/// OAuth store holds one file per server and the names are the user's, and
/// a check that only knew about the servers in the canonical config would
/// miss the login for a server that has since been removed from it — which
/// is precisely the token still sitting there readable.
///
/// Nothing here is fatal and nothing here is a probe, so this runs on every
/// `doctor`. Off unix it produces nothing: Windows ACLs are not these bits,
/// and `chmod` is not the fix there.
#[cfg(unix)]
fn loose_permissions(config: &Path) -> Vec<Finding> {
    use std::os::unix::fs::PermissionsExt as _;

    let mut paths: Vec<std::path::PathBuf> = vec![config.to_path_buf()];
    if let Some(state_dir) = mcpgw_core::paths::state_dir() {
        paths.push(mcpgw_core::gateway_token::GatewayToken::path(&state_dir));
        paths.push(mcpgw_core::probe_state::path(&state_dir));
        let auth = mcpgw_core::auth::dir(&state_dir);
        if let Ok(entries) = std::fs::read_dir(&auth) {
            paths.extend(
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.extension().is_some_and(|ext| ext == "json")),
            );
        }
        // The directories last, so a report reads file, file, then the two
        // places they sit in.
        paths.push(auth);
        paths.push(state_dir);
    }
    paths
        .iter()
        .filter_map(|path| {
            let meta = std::fs::metadata(path).ok()?;
            mcpgw_core::doctor::loose_permissions(path, meta.permissions().mode(), meta.is_dir())
        })
        .collect()
}

#[cfg(not(unix))]
fn loose_permissions(_config: &Path) -> Vec<Finding> {
    Vec::new()
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

/// The project section.
///
/// Printed even with nothing to name: a section that disappears reads as a
/// check that never ran, and the empty answer — no repo-local config around
/// this directory — is one of the things people run `doctor` to find out.
fn render_traffic(report: &TrafficReport, color: bool) {
    let where_it_is = report.dir.as_deref().map_or_else(
        || "no state directory".to_owned(),
        |dir| dir.display().to_string(),
    );
    println!();
    heading(
        &format!("traffic capture ({where_it_is}) — {}", report.line()),
        color,
    );
}

fn render_projects(report: &ProjectReport, color: bool) {
    println!();
    heading("project configs", color);
    if report.files.is_empty() {
        let from = std::env::current_dir().map_or_else(
            |_| "the working directory".to_owned(),
            |dir| dir.display().to_string(),
        );
        println!(
            "  {}",
            crate::ui::dim(&format!("none found from {from}"), color)
        );
        return;
    }
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
    traffic: &TrafficReport,
    probes: Option<&ProbeReport>,
    gateway: Option<&GatewayReport>,
    budgets: &[(ClientKind, ClientBudget)],
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
        "capture": traffic.json(),
        "findings": findings,
        "errors": errors,
        "warnings": warnings,
    });
    if !budgets.is_empty() {
        out["budgets"] =
            serde_json::json!(budgets.iter().map(|(_, budget)| budget).collect::<Vec<_>>());
    }
    if let Some(probes) = probes {
        let entries: Vec<serde_json::Value> = probes
            .results
            .iter()
            .map(|(label, outcome)| {
                let mut row = match outcome {
                    Ok(success) => serde_json::json!({
                        "servers": label, "ok": true,
                        "server_name": success.server_name,
                        "server_version": success.server_version,
                        "tools": success.tool_count(),
                    }),
                    Err(ProbeError::AuthRequired) => serde_json::json!({
                        "servers": label, "ok": false,
                        "code": mcpgw_core::doctor::NEEDS_OAUTH,
                        "error": probes.oauth_message(label),
                    }),
                    Err(err) => serde_json::json!({
                        "servers": label, "ok": false, "error": err.to_string(),
                    }),
                };
                // On every row for the same reason the rendered ones carry
                // it: how a target authenticates is a property of the target,
                // not of how the probe went.
                if let Some(command) = probes.helpers.get(label) {
                    row["headers_from_command"] = serde_json::json!(command);
                }
                row
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
                        row["tools"] = serde_json::json!(success.tool_count());
                    }
                    GatewayOutcome::Unserved(detail) => {
                        row["ok"] = serde_json::json!(false);
                        row["unserved"] = serde_json::json!(true);
                        row["error"] = serde_json::json!(detail);
                    }
                    GatewayOutcome::NeedsOAuth(name) => {
                        row["ok"] = serde_json::json!(false);
                        row["code"] = serde_json::json!(mcpgw_core::doctor::NEEDS_OAUTH);
                        row["error"] =
                            serde_json::json!(needs_oauth(None, name, token_state(name)).message);
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
    /// Row label → the `headers_command` behind it, for the rows that have
    /// one.
    helpers: BTreeMap<String, String>,
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
        let name = self.name(label);
        needs_oauth(None, name, token_state(name)).message
    }

    fn name<'a>(&'a self, label: &'a str) -> &'a str {
        self.names.get(label).map_or(label, String::as_str)
    }

    /// `" (headers from command …)"` for a row that has one, nothing for the
    /// rest.
    fn helper_note(&self, label: &str) -> String {
        self.helpers.get(label).map_or_else(String::new, |command| {
            format!(
                " (headers {} {command})",
                mcpgw_core::doctor::HEADERS_FROM_COMMAND
            )
        })
    }
}

/// The findings for every server whose tool definitions have moved since the
/// gateway pinned them.
///
/// Read off the pin files rather than by dialing anything, so it costs
/// nothing and works without `--probe`: the gateway already did the
/// comparison, on the list a client actually received, and left the answer
/// on disk. A server the state directory knows nothing about has never been
/// listed through a gateway, and has nothing to say here.
fn drifted_tool_definitions(canonical: &BTreeMap<String, Server>) -> Vec<Finding> {
    let Some(dir) = mcpgw_core::paths::state_dir() else {
        return Vec::new();
    };
    let store = mcpgw_core::pins::PinStore::under_state_dir(&dir);
    canonical
        .iter()
        .filter_map(|(name, server)| {
            let file = match store.read(name) {
                Ok(file) => file?,
                // An unreadable pin file is its own problem and says so; it
                // must not read as "nothing has drifted".
                Err(err) => {
                    return Some(Finding {
                        client: None,
                        server: Some(name.clone()),
                        severity: Severity::Warning,
                        message: error_chain(&err),
                        code: None,
                    });
                }
            };
            // A server whose watching was turned off after it drifted keeps
            // a stale file; the config is what says whether to report it.
            server.drift().is_watched().then_some(())?;
            tool_drift(name, &file.drift)
        })
        .collect()
}

/// What each client is offered and what it costs, for every client that has
/// a scope of its own or that mcpgw syncs.
///
/// Both halves are needed: a scoped client is the point of the report, and
/// an unscoped one is where the number people were surprised by comes from —
/// "every client gets every server" is exactly the state worth pricing.
fn budget_report(
    probes: &ProbeReport,
    canonical: &BTreeMap<String, Server>,
    scopes: &BTreeMap<String, ClientScope>,
    managed: &ManagedState,
) -> (Vec<(ClientKind, ClientBudget)>, Vec<Finding>) {
    let listings: BTreeMap<String, BTreeMap<String, usize>> = probes
        .results
        .iter()
        .filter_map(|(label, outcome)| {
            let success = outcome.as_ref().ok()?;
            Some((probes.name(label).to_owned(), success.tokens.clone()))
        })
        .collect();
    let budgets: Vec<(ClientKind, ClientBudget)> = ClientKind::ALL
        .into_iter()
        .filter(|kind| scopes.contains_key(kind.id()) || managed.clients.contains_key(kind.id()))
        .map(|kind| {
            (
                kind,
                client_budget(kind, scopes.get(kind.id()), canonical, &listings),
            )
        })
        .collect();
    let findings = budgets
        .iter()
        .filter_map(|(kind, budget)| {
            let (cap, source) = tool_cap(*kind, scopes.get(kind.id()))?;
            over_tool_cap(budget, cap, &source)
        })
        .collect();
    (budgets, findings)
}

/// The report's totals, counted in one place so the summary line and the
/// `--json` numbers cannot disagree about what a problem is.
fn tally(
    sets: &[&[Finding]],
    probes: Option<&ProbeReport>,
    gateway: Option<&GatewayReport>,
) -> (usize, usize) {
    let of = |severity| sets.iter().map(|set| count(set, severity)).sum::<usize>();
    (
        of(Severity::Error)
            + probes.map_or(0, ProbeReport::failures)
            + gateway.map_or(0, GatewayReport::failures),
        of(Severity::Warning),
    )
}

/// The findings for every `[servers.NAME.tools]` entry that matched nothing
/// the server just listed.
///
/// Read off the direct rows rather than the gateway ones: a rule is about
/// what the server offers, and the gateway pass sees the filtered list — in
/// which an over-narrow `allow` looks perfectly consistent with itself.
fn stale_tool_rules(probes: &ProbeReport, canonical: &BTreeMap<String, Server>) -> Vec<Finding> {
    probes
        .results
        .iter()
        .filter_map(|(label, outcome)| {
            let success = outcome.as_ref().ok()?;
            let name = probes.name(label);
            let server = canonical.get(name)?;
            Some(unmatched_tool_rules(name, server, &success.tools))
        })
        .flatten()
        .collect()
}

/// Whether a probe of `server` would dial with no credential of any kind:
/// no stored login, no `[auth]` table, no headers written down or minted.
///
/// Only such a probe can prove a server needs no login, which is the whole
/// point of recording one.
fn presents_nothing(server: &Server, name: &str, state_dir: Option<&Path>) -> bool {
    let bare_transport = match &server.transport {
        Transport::Http {
            url: _,
            headers,
            headers_command,
            auth,
            ..
        } => headers.is_empty() && headers_command.is_empty() && auth.is_none(),
        Transport::Stdio { .. } => false,
    };
    bare_transport
        && state_dir.is_none_or(|dir| {
            mcpgw_core::auth::Tokens::load(dir, name)
                .ok()
                .flatten()
                .is_none()
        })
}

/// Leaves what this pass learned about authentication where `auth status`
/// can read it back.
///
/// Best effort: a state directory that will not take a write costs the user
/// a sharper `auth status` line later, and nothing about the report they
/// asked for now, so a failure here is not one to report.
fn record_observations(
    state_dir: &Path,
    results: &[(String, Result<ProbeSuccess, ProbeError>)],
    recordable: &BTreeMap<String, (String, bool)>,
) {
    let seen = results.iter().filter_map(|(label, outcome)| {
        let (name, bare) = recordable.get(label)?;
        let observed = match outcome {
            Ok(_) if *bare => AuthObservation::NoAuthNeeded,
            Err(ProbeError::AuthRequired) => AuthObservation::LoginRequired,
            // Every other outcome — a timeout, a spawn failure, a handshake
            // error, a success that presented a credential — says nothing
            // about whether a login is wanted, so the last thing that did
            // say something is left standing.
            _ => return None,
        };
        Some((name.clone(), observed))
    });
    drop(ProbeState::record(state_dir, seen));
}

fn run_probes(
    runtime: &tokio::runtime::Runtime,
    plan: ProbePlan,
    timeout: Duration,
) -> ProbeReport {
    let mut names: BTreeMap<String, String> = BTreeMap::new();
    let mut helpers: BTreeMap<String, String> = BTreeMap::new();
    // Read once for the whole pass rather than per target: it is the same
    // directory for every one of them, and a probe that dialed with a token
    // for one server and without for the next would be reporting on two
    // different gateways.
    let state_dir = mcpgw_core::paths::state_dir();
    // Which rows may leave a record behind, and whether the probe dialed
    // with nothing at all: a handshake that only worked because a token was
    // attached says nothing about whether the server would take a caller
    // without one.
    let mut recordable: BTreeMap<String, (String, bool)> = BTreeMap::new();
    let mut results = runtime.block_on(async {
        let mut set = tokio::task::JoinSet::new();
        let mut labels: BTreeMap<tokio::task::Id, String> = BTreeMap::new();
        for target in plan.targets.into_values() {
            let label = target.labels.join(", ");
            let probe_name = target.name.clone();
            if target.canonical {
                let bare = presents_nothing(&target.server, &probe_name, state_dir.as_deref());
                recordable.insert(label.clone(), (probe_name.clone(), bare));
            }
            names.insert(label.clone(), target.name);
            if let Some(helper) = target.helper {
                helpers.insert(label.clone(), helper);
            }
            let server = target.server;
            let handle = set.spawn({
                let label = label.clone();
                let name = probe_name.clone();
                let state_dir = state_dir.clone();
                async move {
                    (
                        label,
                        probe_server(&name, &server, state_dir.as_deref(), timeout).await,
                    )
                }
            });
            labels.insert(handle.id(), label);
        }
        collect_probes(set, &labels).await
    });
    results.sort_by(|a, b| a.0.cmp(&b.0));
    if let Some(dir) = &state_dir {
        record_observations(dir, &results, &recordable);
    }
    let findings = results
        .iter()
        .filter(|(_, outcome)| matches!(outcome, Err(ProbeError::AuthRequired)))
        .map(|(label, _)| {
            let name = names.get(label).map_or(label.as_str(), String::as_str);
            needs_oauth(None, name, token_state(name))
        })
        .collect();
    ProbeReport {
        results,
        names,
        helpers,
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
            GatewayOutcome::NeedsOAuth(name) => {
                report
                    .findings
                    .push(needs_oauth(None, name, token_state(name)));
            }
            GatewayOutcome::Ok(_) | GatewayOutcome::Failed(_) => {}
        }
    }
    report
}

/// What `mcpgw auth login` has stored for `name`, if anything.
///
/// Read from disk at each call rather than carried: every caller is on a
/// reporting path that already touches the filesystem several times, and a
/// snapshot taken at the top of a probe pass would be a snapshot from before
/// the pass refreshed a token.
fn token_state(name: &str) -> Option<mcpgw_core::auth::TokenState> {
    let state_dir = mcpgw_core::paths::state_dir()?;
    mcpgw_core::auth::Tokens::load(&state_dir, name)
        .ok()
        .flatten()
        .map(|tokens| tokens.state())
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
        calls_per_minute: 0,
        tools: None,
        transport: Transport::Http {
            url: url.to_owned(),
            headers_command: Vec::new(),
            headers: BTreeMap::new(),
            auth: None,
        },
    };
    // No name and no state directory: the token a server was logged in for
    // belongs to that server, and the gateway endpoint in front of it is not
    // the resource server it was minted for.
    match probe_server("", &server, None, timeout).await {
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
        // On every row, not only the healthy ones: how a target
        // authenticates is a property of the target, and a failed row is
        // where a reader most needs to know that the credential was minted
        // rather than read out of the config.
        let helper = probes.helper_note(label);
        match outcome {
            Ok(success) => {
                let line = format!(
                    "{label}: {} {}, {} tools{helper}",
                    success.server_name,
                    success.server_version,
                    success.tool_count(),
                );
                ok_line(&line, color);
            }
            // Not a failure and not rendered as one: the fix is a login,
            // and the sentence that says so is the finding's own, so the two
            // renderings cannot drift apart.
            Err(ProbeError::AuthRequired) => {
                warn_line(
                    &format!("{label}: {}{helper}", probes.oauth_message(label)),
                    color,
                );
            }
            Err(err) => bad_line(&format!("{label}: {err}{helper}"), color),
        }
    }
    // The OAuth findings already have a row of their own above; what is left
    // is the rules pass, which has none.
    for finding in probes
        .findings
        .iter()
        .filter(|finding| finding.code != Some(mcpgw_core::doctor::NEEDS_OAUTH))
    {
        print_finding(finding, color);
    }
}

/// The token budget section: one line per client, in the terms the request
/// was made in — how many tools it is offered and what they cost it before
/// anybody types anything.
fn render_budgets(budgets: &[(ClientKind, ClientBudget)], findings: &[Finding], color: bool) {
    if budgets.is_empty() {
        return;
    }
    println!();
    heading("token budget — what each client is offered", color);
    for (kind, budget) in budgets {
        let over = findings
            .iter()
            .any(|finding| finding.client.as_deref() == Some(kind.id()));
        if over {
            warn_line(&budget.line(), color);
        } else {
            ok_line(&budget.line(), color);
        }
    }
    for finding in findings {
        print_finding(finding, color);
    }
    println!(
        "  {}",
        crate::ui::dim(
            "estimated at (name + description + schema) / 4 characters per token",
            color,
        )
    );
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
                    success.server_name,
                    success.server_version,
                    success.tool_count()
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
                    &format!(
                        "{where_}: {}",
                        needs_oauth(None, name, token_state(name)).message
                    ),
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
                    tools: vec!["echo".to_owned(), "reverse".to_owned()],
                    tokens: std::collections::BTreeMap::new(),
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
