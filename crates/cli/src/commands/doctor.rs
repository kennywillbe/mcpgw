use std::path::Path;
use std::process::ExitCode;

use mcpgw_core::doctor::{Finding, Severity, check_server, classify_problems};
use mcpgw_core::{ClientKind, Config, Detection, Error};
use owo_colors::OwoColorize as _;

pub fn run(json: bool, color: bool) -> anyhow::Result<ExitCode> {
    let command_exists = |cmd: &str| which::which(cmd).is_ok();
    let mut findings: Vec<Finding> = Vec::new();

    let path = super::canonical_config_path()?;
    let canonical_note = match Config::load(&path) {
        Ok(config) => {
            for (name, server) in &config.servers {
                findings.extend(check_server(None, name, server, &command_exists));
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

    let errors = count(&findings, Severity::Error);
    let warnings = count(&findings, Severity::Warning);

    if json {
        let clients: Vec<serde_json::Value> = detections
            .iter()
            .map(|(client, state)| serde_json::json!({ "client": client, "state": state }))
            .collect();
        let out = serde_json::json!({
            "config": { "path": path, "state": canonical_note },
            "clients": clients,
            "findings": findings,
            "errors": errors,
            "warnings": warnings,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        render(&path, &canonical_note, &detections, &findings, color);
        println!();
        summary_line(errors, warnings, color);
    }

    Ok(if errors > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
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
