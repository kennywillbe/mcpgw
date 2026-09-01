//! Live stdio probing for `doctor --probe`: spawn the server, run the MCP
//! `initialize` handshake, count its tools. HTTP servers are not probed
//! until the gateway transports land (M11).

use std::collections::BTreeMap;
use std::time::Duration;

use rmcp::ServiceExt as _;
use rmcp::transport::TokioChildProcess;
use tokio::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeSuccess {
    pub server_name: String,
    pub server_version: String,
    pub tool_count: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("failed to spawn {command:?}: {source}")]
    Spawn {
        command: String,
        source: std::io::Error,
    },

    #[error(
        "no response within {seconds}s (first `npx`/`uvx` runs download packages — retry or raise --timeout)"
    )]
    Timeout { seconds: u64 },

    #[error("MCP handshake failed: {message}")]
    Handshake { message: String },
}

/// Spawns a stdio MCP server and performs `initialize` + `tools/list`.
///
/// The whole probe races `timeout`; on expiry the child is dropped (and
/// killed with it) rather than gracefully cancelled.
///
/// # Errors
///
/// Returns [`ProbeError`] for spawn failures, handshake/protocol errors and
/// timeouts.
pub async fn probe_stdio(
    command: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
    timeout: Duration,
) -> Result<ProbeSuccess, ProbeError> {
    let mut cmd = Command::new(command);
    cmd.args(args);
    for (key, value) in env {
        cmd.env(key, value);
    }
    // Server logs on stderr are noise here; doctor reports outcomes only.
    cmd.stderr(std::process::Stdio::null());
    // A timed-out probe drops its future mid-handshake; without this the
    // spawned server would outlive us as an orphan.
    cmd.kill_on_drop(true);

    let transport = TokioChildProcess::new(cmd).map_err(|source| ProbeError::Spawn {
        command: command.to_owned(),
        source,
    })?;

    let handshake = |message: String| ProbeError::Handshake { message };
    let probe = async move {
        let service = ().serve(transport).await.map_err(|err| handshake(err.to_string()))?;
        let identity = service
            .peer_info()
            .and_then(|info| info.server_info.clone());
        let tools = service
            .list_all_tools()
            .await
            .map_err(|err| handshake(err.to_string()))?;
        // Best-effort shutdown; the child dies with the dropped transport anyway.
        let _ = service.cancel().await;
        let (server_name, server_version) = identity.map_or_else(
            || ("unknown".to_owned(), String::new()),
            |imp| (imp.name, imp.version),
        );
        Ok(ProbeSuccess {
            server_name,
            server_version,
            tool_count: tools.len(),
        })
    };

    tokio::time::timeout(timeout, probe)
        .await
        .map_err(|_| ProbeError::Timeout {
            seconds: timeout.as_secs(),
        })?
}
