//! Live probing for `doctor --probe`: reach the server over its own
//! transport, run the MCP `initialize` handshake, count its tools.

use std::collections::BTreeMap;
use std::time::Duration;

use rmcp::ServiceExt as _;
use rmcp::transport::TokioChildProcess;
use tokio::process::Command;

use crate::config::{Server, Transport};

/// Appended to the stdio timeout message; the first `npx`/`uvx` run of a
/// package spends its budget downloading, which looks like a hang.
const DOWNLOAD_HINT: &str =
    " (first `npx`/`uvx` runs download packages — retry or raise --timeout)";

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

    #[error("no response within {seconds}s{hint}")]
    Timeout { seconds: u64, hint: &'static str },

    #[error("MCP handshake failed: {message}")]
    Handshake { message: String },
}

type Service = rmcp::service::RunningService<rmcp::RoleClient, ()>;

/// Probes `server` over its configured transport: `initialize` plus
/// `tools/list`.
///
/// The whole probe races `timeout`; on expiry the connection is dropped
/// (killing a spawned child with it) rather than gracefully cancelled.
///
/// # Errors
///
/// Returns [`ProbeError`] for spawn failures, handshake/protocol errors and
/// timeouts.
pub async fn probe_server(server: &Server, timeout: Duration) -> Result<ProbeSuccess, ProbeError> {
    match &server.transport {
        Transport::Stdio { command, args, env } => probe_stdio(command, args, env, timeout).await,
        Transport::Http { url, headers } => probe_http(url, headers, timeout).await,
    }
}

async fn probe_stdio(
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

    let probe = async move {
        let service = ().serve(transport).await.map_err(handshake)?;
        inspect(service).await
    };
    tokio::time::timeout(timeout, probe)
        .await
        .map_err(|_| ProbeError::Timeout {
            seconds: timeout.as_secs(),
            hint: DOWNLOAD_HINT,
        })?
}

async fn probe_http(
    url: &str,
    headers: &BTreeMap<String, String>,
    timeout: Duration,
) -> Result<ProbeSuccess, ProbeError> {
    let config = crate::upstream::http_config(url, headers)
        .map_err(|message| ProbeError::Handshake { message })?;
    let probe = async move {
        let transport = rmcp::transport::StreamableHttpClientTransport::from_config(config);
        // Connect errors (refused, TLS, 4xx) arrive as handshake failures —
        // there is no separate "spawn" step for a remote server.
        let service = ().serve(transport).await.map_err(handshake)?;
        inspect(service).await
    };
    tokio::time::timeout(timeout, probe)
        .await
        .map_err(|_| ProbeError::Timeout {
            seconds: timeout.as_secs(),
            hint: "",
        })?
}

fn handshake(err: impl std::fmt::Display) -> ProbeError {
    ProbeError::Handshake {
        message: err.to_string(),
    }
}

/// Reads identity and tool count off a connected server, then hangs up.
async fn inspect(service: Service) -> Result<ProbeSuccess, ProbeError> {
    let identity = service
        .peer_info()
        .and_then(|info| info.server_info.clone());
    let tools = service.list_all_tools().await.map_err(handshake)?;
    // Best-effort shutdown; the connection dies with the dropped transport anyway.
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
}
