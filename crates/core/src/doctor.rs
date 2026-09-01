//! Pure finding-generation for `mcpgw doctor`. Everything environmental
//! (PATH lookups, filesystem, detection) is injected or done by the caller,
//! so these rules are unit-testable without a real machine state.

use serde::Serialize;

use crate::clients::ClientRead;
use crate::config::{Server, Transport};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    /// `None` means the finding is about the canonical config.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    pub severity: Severity,
    pub message: String,
}

/// Static health checks for one server entry: command resolution for stdio,
/// URL syntax for http. `command_exists` abstracts the PATH lookup.
///
/// Disabled servers are skipped entirely — they cannot break anything while
/// off, and a red doctor over an intentionally parked entry helps no one.
#[must_use]
pub fn check_server(
    client: Option<&str>,
    name: &str,
    server: &Server,
    command_exists: &dyn Fn(&str) -> bool,
) -> Vec<Finding> {
    if !server.enabled {
        return Vec::new();
    }
    let finding = |severity, message| Finding {
        client: client.map(str::to_owned),
        server: Some(name.to_owned()),
        severity,
        message,
    };
    match &server.transport {
        Transport::Stdio { command, .. } => {
            if command_exists(command) {
                Vec::new()
            } else {
                vec![finding(
                    Severity::Error,
                    format!("command {command:?} not found in PATH"),
                )]
            }
        }
        Transport::Http { url, .. } => match url::Url::parse(url) {
            Err(err) => vec![finding(
                Severity::Error,
                format!("invalid url {url:?}: {err}"),
            )],
            Ok(parsed) if !matches!(parsed.scheme(), "http" | "https") => vec![finding(
                Severity::Warning,
                format!("unusual url scheme {:?}", parsed.scheme()),
            )],
            Ok(_) => Vec::new(),
        },
    }
}

/// Turns a lenient client read's problems into findings.
///
/// Severity rule: if the named server still exists in the parsed map, the
/// problem was a lossy-but-successful note (warning); if the entry was
/// dropped, something is actually broken (error). File-level problems
/// (no server name) are always errors.
#[must_use]
pub fn classify_problems(client: &str, read: &ClientRead) -> Vec<Finding> {
    read.problems
        .iter()
        .map(|problem| {
            let survived = problem
                .server
                .as_ref()
                .is_some_and(|name| read.servers.contains_key(name));
            Finding {
                client: Some(client.to_owned()),
                server: problem.server.clone(),
                severity: if survived {
                    Severity::Warning
                } else {
                    Severity::Error
                },
                message: problem.message.clone(),
            }
        })
        .collect()
}
