//! Live probing for `doctor --probe`: reach the server over its own
//! transport, run whichever MCP handshake it speaks, count its tools.

use std::collections::BTreeMap;
use std::path::Path;
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
    /// Every tool the server offered, so a caller can check a
    /// `[servers.NAME.tools]` entry against what is actually there rather
    /// than only count what came back.
    pub tools: Vec<String>,
}

impl ProbeSuccess {
    #[must_use]
    pub fn tool_count(&self) -> usize {
        self.tools.len()
    }
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

    /// The server's `headers_command` did not produce headers, so there was
    /// nothing to dial with. Its own variant because what is broken is on
    /// this machine and named in the message — the server was never asked.
    #[error("{message}")]
    HeadersCommand { message: String },

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

type Service = crate::upstream::UpstreamService;

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
pub async fn probe_server(
    name: &str,
    server: &Server,
    state_dir: Option<&Path>,
    timeout: Duration,
) -> Result<ProbeSuccess, ProbeError> {
    connected(name, server, state_dir, timeout, inspect).await
}

/// Connects to `server`, hands the live service to `use_service`, and races
/// the whole thing against `timeout`. On expiry the future is dropped, which
/// closes the connection (and kills a spawned child with it).
async fn connected<T, F>(
    name: &str,
    server: &Server,
    state_dir: Option<&Path>,
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
            Transport::Http {
                url,
                headers_command,
                headers,
                ..
            } => connect_http(url, headers_command, headers, name, state_dir, timeout).await?,
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
    headers_command: &[String],
    headers: &BTreeMap<String, String>,
    name: &str,
    state_dir: Option<&Path>,
    timeout: Duration,
) -> Result<Service, ProbeError> {
    // Run here as well as in the gateway, and for the reason `--probe`
    // exists: what it proves is that this server answers *the way mcpgw
    // would reach it*, and a credential mcpgw would mint at connect time is
    // part of that. The whole probe is already racing `timeout`, so the
    // command inherits it rather than being given a second ceiling that
    // could outlast the probe holding it.
    let resolved;
    let headers = if headers_command.is_empty() {
        headers
    } else {
        resolved = crate::headers::resolve(headers_command, headers, timeout)
            .await
            .map_err(|err| ProbeError::HeadersCommand {
                message: err.to_string(),
            })?;
        &resolved
    };
    let config = crate::upstream::http_config(url, headers)
        .map_err(|message| ProbeError::Handshake { message })?;
    // The stored login, for the same reason the `headers_command` above runs:
    // `--probe` answers whether this server works *the way mcpgw reaches it*,
    // and a probe that ignored the token would report a healthy, logged-in
    // server as needing OAuth.
    let credentials = match state_dir {
        Some(state_dir) => crate::auth::client(state_dir, name, url)
            .await
            .map_err(|err| ProbeError::Handshake {
                message: err.to_string(),
            })?,
        None => None,
    };
    // Connect errors (refused, TLS, 4xx) arrive as handshake failures —
    // there is no separate "spawn" step for a remote server. The 401 is the
    // exception, and rmcp answers for it rather than the message being
    // matched: see `upstream::dial`.
    if let Some(client) = credentials {
        let transport = || {
            rmcp::transport::StreamableHttpClientTransport::with_client(
                client.clone(),
                config.clone(),
            )
        };
        match dial(transport(), Lifecycle::Legacy).await {
            Err(err) if err.refused_initialize => dial(transport(), Lifecycle::Modern).await,
            other => other,
        }
    } else {
        let transport =
            || rmcp::transport::StreamableHttpClientTransport::from_config(config.clone());
        match dial(transport(), Lifecycle::Legacy).await {
            Err(err) if err.refused_initialize => dial(transport(), Lifecycle::Modern).await,
            other => other,
        }
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
    // Detached: the probe asks one connection what it can do and drops it,
    // so a list-changed notification arriving on it has nobody to reach.
    crate::upstream::dial(
        transport,
        lifecycle,
        None,
        crate::upstream::UpstreamClient::detached(),
    )
    .await
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
        tools: tools
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect(),
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

/// Connects to `server` and fingerprints every tool it offers, in list
/// order.
///
/// Separate from [`inspect_server`] because a fingerprint needs the whole
/// definition — schemas and annotations included — and [`Inspection`] is a
/// display model that keeps the name and the description. This is what
/// `mcpgw tools NAME pin` writes and what its drift column is read against,
/// so it has to hash exactly what the gateway hashes.
///
/// # Errors
///
/// Returns [`ProbeError`] for spawn failures, handshake/protocol errors and
/// timeouts, exactly like [`probe_server`].
pub async fn fingerprint_tools(
    name: &str,
    server: &Server,
    state_dir: Option<&Path>,
    timeout: Duration,
) -> Result<Vec<crate::pins::ToolFingerprint>, ProbeError> {
    connected(name, server, state_dir, timeout, |service| async move {
        let tools = service.list_all_tools().await.map_err(handshake)?;
        let _ = service.cancel().await;
        Ok(tools.iter().map(crate::pins::ToolFingerprint::of).collect())
    })
    .await
}

/// Connects to `server` and lists everything it offers: identity, tools and
/// (where supported) resources.
///
/// # Errors
///
/// Returns [`ProbeError`] for spawn failures, handshake/protocol errors and
/// timeouts, exactly like [`probe_server`].
pub async fn inspect_server(
    name: &str,
    server: &Server,
    state_dir: Option<&Path>,
    timeout: Duration,
) -> Result<Inspection, ProbeError> {
    connected(name, server, state_dir, timeout, list_everything).await
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
