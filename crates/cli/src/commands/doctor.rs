use std::collections::BTreeMap;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use mcpgw_core::doctor::{Finding, Severity, check_server, classify_problems};
use mcpgw_core::probe::probe_server;
use mcpgw_core::{ClientKind, Config, Detection, Error, Server, Transport};
use owo_colors::OwoColorize as _;

/// Key = the exact endpoint a probe would talk to; entries shared between
/// the canonical config and clients (or several clients) probe once. The
/// transport is part of the key so a stdio and an http entry never collapse.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum TargetKey {
    Stdio(String, Vec<String>, BTreeMap<String, String>),
    Http(String, BTreeMap<String, String>),
}

#[derive(Default)]
struct ProbePlan {
    targets: BTreeMap<TargetKey, (Server, Vec<String>)>,
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
            .or_insert_with(|| (server.clone(), Vec::new()))
            .1
            .push(format!("{name} ({source})"));
    }
}

pub fn run(json: bool, color: bool, probe: Option<Duration>) -> anyhow::Result<ExitCode> {
    let command_exists = |cmd: &str| which::which(cmd).is_ok();
    let mut findings: Vec<Finding> = Vec::new();
    let mut plan = ProbePlan::default();

    let path = super::canonical_config_path()?;
    let canonical_note = match Config::load(&path) {
        Ok(config) => {
            for (name, server) in &config.servers {
                findings.extend(check_server(None, name, server, &command_exists));
                plan.collect("canonical", name, server);
            }
            format!("{} servers", config.servers.len())
        }
        Err(Error::NotFound { .. }) => "not created yet (run `mcpgw add`)".to_owned(),
        Err(err) => {
            findings.push(Finding {
                client: None,
                server: None,
                severity: Severity::Error,
                message: error_chain(&err),
            });
            "invalid".to_owned()
        }
    };

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
                });
            }
            Detection::Configured(config_path) => {
                detections.push((name, format!("configured ({})", config_path.display())));
                match kind.load(&config_path) {
                    Ok(read) => {
                        findings.extend(classify_problems(name, &read));
                        for (server_name, server) in &read.servers {
                            findings.extend(check_server(
                                Some(name),
                                server_name,
                                server,
                                &command_exists,
                            ));
                            plan.collect(name, server_name, server);
                        }
                    }
                    Err(err) => findings.push(Finding {
                        client: Some(name.to_owned()),
                        server: None,
                        severity: Severity::Error,
                        message: error_chain(&err),
                    }),
                }
            }
        }
    }

    let probe_results = match probe {
        Some(timeout) => Some(run_probes(plan, timeout)?),
        None => None,
    };

    let errors = count(&findings, Severity::Error)
        + probe_results
            .as_ref()
            .map_or(0, |p| p.results.iter().filter(|(_, r)| r.is_err()).count());
    let warnings = count(&findings, Severity::Warning);

    if json {
        emit_json(
            &path,
            &canonical_note,
            &detections,
            &findings,
            probe_results.as_ref(),
            errors,
            warnings,
        )?;
    } else {
        render(&path, &canonical_note, &detections, &findings, color);
        if let Some(probes) = &probe_results {
            render_probes(probes, color);
        }
        println!();
        summary_line(errors, warnings, color);
    }

    Ok(if errors > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

// Pure serialization of already-computed pieces; bundling them into a
// struct would exist only to appease the lint.
#[allow(clippy::too_many_arguments)]
fn emit_json(
    path: &Path,
    canonical_note: &str,
    detections: &[(&'static str, String)],
    findings: &[Finding],
    probes: Option<&ProbeReport>,
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
                Err(err) => serde_json::json!({
                    "servers": label, "ok": false, "error": err.to_string(),
                }),
            })
            .collect();
        out["probes"] = serde_json::json!({ "results": entries });
    }
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

struct ProbeReport {
    results: Vec<(
        String,
        Result<mcpgw_core::probe::ProbeSuccess, mcpgw_core::probe::ProbeError>,
    )>,
}

fn run_probes(plan: ProbePlan, timeout: Duration) -> anyhow::Result<ProbeReport> {
    let runtime = tokio::runtime::Runtime::new()?;
    let mut results = runtime.block_on(async {
        let mut set = tokio::task::JoinSet::new();
        for (server, labels) in plan.targets.into_values() {
            set.spawn(async move {
                let outcome = probe_server(&server, timeout).await;
                (labels.join(", "), outcome)
            });
        }
        let mut collected = Vec::new();
        while let Some(joined) = set.join_next().await {
            collected.push(joined.expect("probe task panicked"));
        }
        collected
    });
    results.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(ProbeReport { results })
}

fn render_probes(probes: &ProbeReport, color: bool) {
    println!();
    if color {
        println!("{}", "probes".bold());
    } else {
        println!("probes");
    }
    for (label, outcome) in &probes.results {
        match outcome {
            Ok(success) => {
                let line = format!(
                    "{label}: {} {}, {} tools",
                    success.server_name, success.server_version, success.tool_count
                );
                if color {
                    println!("  {} {line}", "✓".green());
                } else {
                    println!("  ✓ {line}");
                }
            }
            Err(err) => {
                if color {
                    println!("  {} {label}: {err}", "✗".red());
                } else {
                    println!("  ✗ {label}: {err}");
                }
            }
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
    let heading = |text: String| {
        if color {
            println!("{}", text.bold());
        } else {
            println!("{text}");
        }
    };

    heading(format!(
        "canonical config ({}) — {canonical_note}",
        path.display()
    ));
    print_findings(findings, None, color);

    for (client, state) in detections {
        println!();
        heading(format!("{client} — {state}"));
        print_findings(findings, Some(client), color);
    }
}

fn print_findings(findings: &[Finding], client: Option<&str>, color: bool) {
    let mut any = false;
    for finding in findings.iter().filter(|f| f.client.as_deref() == client) {
        any = true;
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
    if !any && client.is_none() {
        // Only the canonical section gets an explicit all-clear; client
        // sections without findings stay quiet to keep the report short.
        println!("  ✓ no problems");
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
