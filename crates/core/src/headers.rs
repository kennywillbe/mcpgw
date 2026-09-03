//! Running a server's `headers_command` and turning its stdout into request
//! headers.
//!
//! The output is a credential and is treated as one everywhere: it is parsed
//! and handed to the transport, and no error, log line or capture record ever
//! carries it. Only the command line and a tail of its *stderr* can reach a
//! message — the command's own diagnostics are what a user needs to fix it,
//! and they are not where the token is.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

/// How long the command gets before it is killed. The same ceiling Claude
/// Code puts on its `headersHelper`, and for the same reason: a helper that
/// blocks on a login prompt would otherwise hang every connect behind it.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// How much of the command's stderr is quoted back. Stderr is a diagnostic,
/// not the secret — but a command that decides to dump a page of it must not
/// be able to fill the report either.
const STDERR_TAIL: usize = 400;

#[derive(Debug, thiserror::Error)]
pub enum HeadersError {
    #[error("headers_command {command} could not be started: {source}")]
    Spawn {
        command: String,
        #[source]
        source: std::io::Error,
    },

    #[error("headers_command {command} did not finish within {}s", TIMEOUT.as_secs())]
    Timeout { command: String },

    #[error("headers_command {command} failed: {status}{}", tail(stderr))]
    Failed {
        command: String,
        status: String,
        stderr: String,
    },

    /// The command ran and its stdout was not a JSON object of strings.
    ///
    /// What it *did* print is deliberately absent: an output that failed to
    /// parse is still an output, and a half-written token quoted into an
    /// error would end up in exactly the logs this whole module exists to
    /// stay out of.
    #[error(
        "headers_command {command} did not print a JSON object of header names and values{}",
        tail(stderr)
    )]
    Output { command: String, stderr: String },
}

/// The command as one line, for a message. Quoted so an empty or
/// space-carrying argument is visible rather than silently joined away.
#[must_use]
pub fn display(argv: &[String]) -> String {
    format!("{:?}", argv.join(" "))
}

/// The executable a `headers_command` names, which is what `doctor` resolves
/// the way it resolves a stdio `command`.
#[must_use]
pub fn program(argv: &[String]) -> Option<&str> {
    argv.first().map(String::as_str)
}

fn tail(stderr: &str) -> String {
    let text = stderr.trim();
    if text.is_empty() {
        return String::new();
    }
    // Counted in chars rather than bytes: a command writing UTF-8 must not
    // be able to make this panic on a slice boundary.
    let cut: String = match text.chars().count().checked_sub(STDERR_TAIL) {
        Some(over) if over > 0 => format!("…{}", text.chars().skip(over).collect::<String>()),
        _ => text.to_owned(),
    };
    format!(" ({cut})")
}

/// Runs `argv` and returns `headers` with the command's own output merged
/// over it.
///
/// The command is spawned directly — no shell — with the process environment
/// inherited and the working directory set to the user's home. A gateway
/// under launchd or systemd starts in a directory the user never chose (`/`
/// as often as not), and a helper that reads a relative credential path
/// would work from a terminal and fail as a service; home is the one
/// directory that means the same thing in both.
///
/// A name the command produces replaces a static one spelled the same way,
/// which is what makes `headers` the fallback and the command the source of
/// truth — the same precedence Claude Code's `headersHelper` has.
///
/// # Errors
///
/// Returns [`HeadersError`] when the command cannot be started, outruns
/// [`TIMEOUT`], exits non-zero, or prints anything but a JSON object of
/// strings.
pub async fn resolve(
    argv: &[String],
    headers: &BTreeMap<String, String>,
    timeout: Duration,
) -> Result<BTreeMap<String, String>, HeadersError> {
    let command = display(argv);
    let Some((exe, rest)) = argv.split_first() else {
        // Unreachable through a parsed config, which rejects an empty
        // command; a caller constructing a `Server` by hand gets a message
        // rather than a panic.
        return Err(HeadersError::Output {
            command,
            stderr: String::new(),
        });
    };

    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(rest);
    if let Some(home) = home_dir() {
        cmd.current_dir(home);
    }
    cmd.stdin(std::process::Stdio::null());
    // Killed with the future the timeout below drops, so a helper waiting on
    // a network call it will never get an answer to does not outlive us.
    cmd.kill_on_drop(true);

    let output = match tokio::time::timeout(timeout, cmd.output()).await {
        Err(_) => return Err(HeadersError::Timeout { command }),
        Ok(Err(source)) => return Err(HeadersError::Spawn { command, source }),
        Ok(Ok(output)) => output,
    };
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() {
        return Err(HeadersError::Failed {
            command,
            status: output.status.to_string(),
            stderr,
        });
    }

    let parsed: BTreeMap<String, String> =
        serde_json::from_slice(&output.stdout).map_err(|_| HeadersError::Output {
            command,
            stderr: stderr.clone(),
        })?;
    let mut merged = headers.clone();
    merged.extend(parsed);
    Ok(merged)
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    const HOME: &str = "USERPROFILE";
    #[cfg(not(windows))]
    const HOME: &str = "HOME";
    std::env::var_os(HOME)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::{HeadersError, STDERR_TAIL, display, tail};

    #[test]
    fn the_command_line_is_quoted_whole() {
        let argv = ["corp-auth".to_owned(), "print-headers".to_owned()];
        assert_eq!(display(&argv), "\"corp-auth print-headers\"");
    }

    /// A command that talks too much cannot push the rest of a report off
    /// the screen.
    #[test]
    fn a_long_stderr_is_cut_to_its_tail() {
        let noise = "x".repeat(STDERR_TAIL * 2);
        let cut = tail(&noise);
        assert!(cut.starts_with(" (…"), "{cut}");
        assert!(cut.chars().count() < STDERR_TAIL + 10, "{cut}");
    }

    #[test]
    fn a_silent_command_adds_nothing_to_the_message() {
        assert_eq!(tail("   \n"), "");
    }

    /// The rule the whole module exists for: stdout never reaches a message,
    /// however the command failed.
    #[test]
    fn no_error_can_carry_what_the_command_printed() {
        let errors = [
            HeadersError::Timeout {
                command: "\"corp-auth\"".to_owned(),
            },
            HeadersError::Failed {
                command: "\"corp-auth\"".to_owned(),
                status: "exit status: 1".to_owned(),
                stderr: "no vault session".to_owned(),
            },
            HeadersError::Output {
                command: "\"corp-auth\"".to_owned(),
                stderr: "no vault session".to_owned(),
            },
        ];
        for err in errors {
            let message = err.to_string();
            assert!(message.contains("corp-auth"), "{message}");
            assert!(!message.contains("Bearer"), "{message}");
        }
    }
}
