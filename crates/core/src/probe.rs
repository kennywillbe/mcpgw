//! Live probing for `doctor --probe`: reach the server over its own
//! transport, run whichever MCP handshake it speaks, count its tools.

use std::collections::BTreeMap;
use std::time::Duration;

use rmcp::transport::TokioChildProcess;
use tokio::process::Command;

use crate::config::{Server, Transport};
use crate::upstream::{DialError, Lifecycle};

/// Appended to the stdio timeout message; the first `npx`/`uvx` run of a
/// package spends its budget downloading, which looks like a hang.
const DOWNLOAD_HINT: &str =
    " (first `npx`/`uvx` runs download packages — retry or raise --timeout)";

/// Whether anything is listening at `base`'s host and port.
///
/// A bare TCP connect rather than an MCP handshake, because this answers one
/// question only: is the daemon up. That is the single failure whose fix is
/// `mcpgw serve`, and it has to be told apart from a gateway that is up but
/// does not serve some endpoint a client dials — which is a per-entry problem
/// with an entirely different fix. Rolling both into one handshake would
/// report the wrong one half the time.
pub async fn gateway_listening(base: &str, timeout: Duration) -> bool {
    let Ok(url) = url::Url::parse(base) else {
        return false;
    };
    let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
        return false;
    };
    // `host_str` keeps the brackets on an IPv6 literal; the resolver wants
    // the address without them.
    let host = host.trim_matches(['[', ']']);
    matches!(
        tokio::time::timeout(timeout, tokio::net::TcpStream::connect((host, port))).await,
        Ok(Ok(_))
    )
}

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

    /// The server answered 401. Its own variant because the report says
    /// something else entirely about it: nothing here is broken, and no
    /// retry, timeout bump or restart is the fix.
    #[error("the server requires OAuth")]
    AuthRequired,

    /// The probe never produced an outcome — its task panicked or was
    /// cancelled. Reported like any other failure so one broken target
    /// costs one row instead of the whole report.
    #[error("probe did not complete: {reason}")]
    Aborted { reason: String },
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
    connected(server, timeout, inspect).await
}

/// Connects to `server`, hands the live service to `use_service`, and races
/// the whole thing against `timeout`. On expiry the future is dropped, which
/// closes the connection (and kills a spawned child with it).
async fn connected<T, F>(
    server: &Server,
    timeout: Duration,
    use_service: impl FnOnce(Service) -> F,
) -> Result<T, ProbeError>
where
    F: Future<Output = Result<T, ProbeError>>,
{
    let hint = match &server.transport {
        // Only a spawned server can be busy downloading itself.
        Transport::Stdio { .. } => DOWNLOAD_HINT,
        Transport::Http { .. } => "",
    };
    let work = async {
        let service = match &server.transport {
            Transport::Stdio { command, args, env } => connect_stdio(command, args, env).await?,
            Transport::Http { url, headers } => connect_http(url, headers).await?,
        };
        use_service(service).await
    };
    tokio::time::timeout(timeout, work)
        .await
        .map_err(|_| ProbeError::Timeout {
            seconds: timeout.as_secs(),
            hint,
        })?
}

async fn connect_stdio(
    command: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
) -> Result<Service, ProbeError> {
    // Rebuilt per attempt: the first child owns the pipes it was handed, so a
    // second lifecycle needs a second process.
    let spawn = || {
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
        TokioChildProcess::new(cmd).map_err(|source| ProbeError::Spawn {
            command: command.to_owned(),
            source,
        })
    };
    // The probe runs both lifecycles for the same reason the gateway does:
    // a 2026-07-28 server has no `initialize` to answer, and reporting it as
    // unreachable would be doctor lying about a healthy server.
    match dial(spawn()?, Lifecycle::Legacy).await {
        Err(err) if err.refused_initialize => dial(spawn()?, Lifecycle::Modern).await,
        other => other,
    }
    .map_err(failure)
}

async fn connect_http(
    url: &str,
    headers: &BTreeMap<String, String>,
) -> Result<Service, ProbeError> {
    let config = crate::upstream::http_config(url, headers)
        .map_err(|message| ProbeError::Handshake { message })?;
    let transport = || rmcp::transport::StreamableHttpClientTransport::from_config(config.clone());
    // Connect errors (refused, TLS, 4xx) arrive as handshake failures —
    // there is no separate "spawn" step for a remote server. The 401 is the
    // exception, and rmcp answers for it rather than the message being
    // matched: see `upstream::dial`.
    match dial(transport(), Lifecycle::Legacy).await {
        Err(err) if err.refused_initialize => dial(transport(), Lifecycle::Modern).await,
        other => other,
    }
    .map_err(failure)
}

/// One handshake, with no deadline of its own: [`connected`] already races
/// the whole probe against the caller's timeout, and a second one inside it
/// would only decide the same thing twice.
async fn dial<T, E, A>(transport: T, lifecycle: Lifecycle) -> Result<Service, DialError>
where
    T: rmcp::transport::IntoTransport<rmcp::RoleClient, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    crate::upstream::dial(transport, lifecycle, None).await
}

/// What a failed handshake means for the report: a server that will not talk
/// without a credential is not a broken server, and no retry or timeout bump
/// is its fix.
fn failure(err: DialError) -> ProbeError {
    if err.auth_required {
        ProbeError::AuthRequired
    } else {
        ProbeError::Handshake {
            message: err.message,
        }
    }
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

/// A tool or resource listing read straight off a server, for `mcpgw inspect`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Inspection {
    pub server_name: String,
    pub server_version: String,
    pub tools: Vec<ToolInfo>,
    pub resources: Vec<ResourceInfo>,
    /// False when the server advertises no `resources` capability, so an
    /// empty list can be told apart from "this server has no resources API".
    pub supports_resources: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ToolInfo {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ResourceInfo {
    pub uri: String,
    pub name: String,
    pub description: Option<String>,
    pub mime_type: Option<String>,
}

/// Connects to `server` and lists everything it offers: identity, tools and
/// (where supported) resources.
///
/// # Errors
///
/// Returns [`ProbeError`] for spawn failures, handshake/protocol errors and
/// timeouts, exactly like [`probe_server`].
pub async fn inspect_server(server: &Server, timeout: Duration) -> Result<Inspection, ProbeError> {
    connected(server, timeout, list_everything).await
}

async fn list_everything(service: Service) -> Result<Inspection, ProbeError> {
    let info = service.peer_info();
    let identity = info.as_ref().and_then(|info| info.server_info.clone());
    // Tools-only servers are the common case; asking such a server for
    // resources answers "method not found", so the capability decides.
    let supports_resources = info
        .as_ref()
        .is_some_and(|info| info.capabilities.resources.is_some());

    let tools = service.list_all_tools().await.map_err(handshake)?;
    let resources = if supports_resources {
        // A server may advertise the capability and still refuse the call;
        // that is a thin listing, not a failed inspection.
        service.list_all_resources().await.unwrap_or_default()
    } else {
        Vec::new()
    };
    let _ = service.cancel().await;

    let (server_name, server_version) = identity.map_or_else(
        || ("unknown".to_owned(), String::new()),
        |imp| (imp.name, imp.version),
    );
    Ok(Inspection {
        server_name,
        server_version,
        tools: tools
            .into_iter()
            .map(|tool| ToolInfo {
                name: tool.name.into_owned(),
                description: tool.description.map(std::borrow::Cow::into_owned),
            })
            .collect(),
        resources: resources
            .into_iter()
            .map(|resource| ResourceInfo {
                uri: resource.uri,
                name: resource.name,
                description: resource.description,
                mime_type: resource.mime_type,
            })
            .collect(),
        supports_resources,
    })
}
