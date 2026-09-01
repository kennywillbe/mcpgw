//! The gateway's downstream face: an rmcp server that forwards tool
//! requests to upstreams managed by [`UpstreamManager`]. Two shapes:
//! a pure pipe to a single upstream (tool names untouched) and the
//! aggregate mode that merges N upstreams under `server__tool` names.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, ErrorData, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};

use crate::capture::{CaptureRecord, CaptureWriter, Kind};
use crate::upstream::UpstreamManager;

/// Separator between server and tool name in aggregate mode. Server names
/// may not contain it (see `config::validate_name`), so the server half of
/// a prefixed name is always unambiguous.
pub const SEPARATOR: &str = "__";

/// Ceiling on one downstream request, covering both acquiring the upstream
/// (which can run a full connect ladder) and the forwarded call.
///
/// Deliberately generous: an MCP tool call may legitimately take minutes, so
/// this is a backstop against hanging forever, not a latency budget. It is
/// still shorter than the ~93s worst-case ladder plus an unbounded call,
/// which is what a client used to be able to wait for with no answer at all.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone)]
enum Mode {
    /// Single upstream, names passed through verbatim.
    Pipe(String),
    /// N upstreams, every tool exposed as `server__tool`.
    Aggregate(Vec<String>),
}

#[derive(Clone)]
pub struct Gateway {
    manager: Arc<UpstreamManager>,
    mode: Mode,
    unavailable_hint: Option<String>,
    capture: Option<Arc<CaptureWriter>>,
    request_timeout: Duration,
}

impl Gateway {
    /// A pure pipe to `upstream`: tool names are neither prefixed on the way
    /// out nor stripped on the way in.
    #[must_use]
    pub fn new(manager: Arc<UpstreamManager>, upstream: String) -> Self {
        Self {
            manager,
            mode: Mode::Pipe(upstream),
            unavailable_hint: None,
            capture: None,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        }
    }

    /// Aggregates `upstreams` under `server__tool` names. Prefixing happens
    /// even for a single upstream so tool names stay stable as servers are
    /// added later.
    #[must_use]
    pub fn aggregate(manager: Arc<UpstreamManager>, upstreams: Vec<String>) -> Self {
        Self {
            manager,
            mode: Mode::Aggregate(upstreams),
            unavailable_hint: None,
            capture: None,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        }
    }

    /// Appends `hint` to every unreachable-upstream error. The deployment,
    /// not the core error types, knows what the user should do about it —
    /// `mcpgw connect` uses this to say which gateway is down and how to
    /// start it.
    #[must_use]
    pub fn with_unavailable_hint(mut self, hint: String) -> Self {
        self.unavailable_hint = Some(hint);
        self
    }

    /// Records every upstream list/call into `writer`. Off by default:
    /// `mcpgw serve` turns it on, `mcpgw connect` deliberately leaves it off
    /// because the gateway it bridges to already records the same traffic.
    #[must_use]
    pub fn with_capture(mut self, writer: Arc<CaptureWriter>) -> Self {
        self.capture = Some(writer);
        self
    }

    /// Overrides [`DEFAULT_REQUEST_TIMEOUT`] for this gateway. Exists so the
    /// deployment can tighten or relax the ceiling (and so the suite can make
    /// it tiny); no CLI flag surfaces it yet.
    #[must_use]
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    #[must_use]
    pub fn manager(&self) -> &Arc<UpstreamManager> {
        &self.manager
    }

    /// Writes one record, if capture is on. Deliberately a blocking append
    /// on the request path: a record is a few hundred bytes to an appended
    /// file, which costs far less than the channel and flush machinery that
    /// moving it off-thread would need. Capture never fails a request.
    fn record(&self, build: impl FnOnce(&str) -> CaptureRecord) {
        let Some(writer) = &self.capture else { return };
        if let Err(err) = writer.append(&build(writer.session())) {
            eprintln!("warning: could not write traffic capture: {err}");
        }
    }

    /// Runs `work` under the per-request deadline, reporting expiry as an
    /// error that names the upstream and the ceiling it hit.
    ///
    /// The deadline covers acquiring the upstream as well as the forwarded
    /// call: acquisition is the half that can run a whole connect ladder, and
    /// a client with nothing to wait on is the failure this exists to
    /// prevent.
    async fn within_deadline<T>(
        &self,
        upstream: &str,
        work: impl Future<Output = Result<T, ErrorData>>,
    ) -> Result<T, ErrorData> {
        match tokio::time::timeout(self.request_timeout, work).await {
            Ok(result) => result,
            Err(_) => Err(ErrorData::internal_error(
                timed_out(upstream, self.request_timeout),
                None,
            )),
        }
    }

    async fn upstream_service(
        &self,
        name: &str,
    ) -> Result<Arc<crate::upstream::UpstreamService>, ErrorData> {
        // Upstream failures surface as loud MCP errors — never as a silent
        // empty result.
        self.manager.ready(name).await.map_err(|err| {
            let message = match &self.unavailable_hint {
                Some(hint) => format!("{err} — {hint}"),
                None => err.to_string(),
            };
            ErrorData::internal_error(message, None)
        })
    }

    /// Lists every upstream's tools in parallel and merges them under their
    /// `server__` prefixes. An upstream that cannot answer is reported on
    /// the gateway console and omitted: degraded, but never silent and never
    /// fatal for the healthy upstreams.
    async fn aggregate_tools(&self, upstreams: &[String]) -> ListToolsResult {
        let mut tasks = tokio::task::JoinSet::new();
        for name in upstreams {
            let manager = Arc::clone(&self.manager);
            let name = name.clone();
            // Per upstream rather than over the whole merge: one hung server
            // must not decide how long the healthy ones get.
            let deadline = self.request_timeout;
            tasks.spawn(async move {
                let started = Instant::now();
                let work = async {
                    let service = manager.ready(&name).await.map_err(|err| err.to_string())?;
                    service
                        .list_all_tools()
                        .await
                        .map_err(|err| err.to_string())
                };
                let tools = match tokio::time::timeout(deadline, work).await {
                    Ok(tools) => tools,
                    Err(_) => Err(timed_out(&name, deadline)),
                };
                (name, started.elapsed(), tools)
            });
        }

        // Collected by name so the merged list is ordered by server
        // regardless of which upstream answers first.
        let mut by_server: BTreeMap<String, Vec<Tool>> = BTreeMap::new();
        while let Some(joined) = tasks.join_next().await {
            let (name, elapsed, tools) = match joined {
                Ok(result) => result,
                Err(err) => {
                    eprintln!("warning: listing tools panicked: {err}");
                    continue;
                }
            };
            // Every upstream attempt is recorded, failures included — a
            // degraded merge is exactly what `watch` needs to show.
            self.record(|session| {
                let record = CaptureRecord::new(session, &name, Kind::List, elapsed);
                match &tools {
                    Ok(tools) => record.with_response(format!("{} tool(s)", tools.len())),
                    Err(err) => record.with_error(err),
                }
            });
            match tools {
                Ok(tools) => {
                    by_server.insert(name, tools);
                }
                Err(err) => eprintln!(
                    "warning: upstream {name:?} failed ({err}); its tools are omitted from tools/list"
                ),
            }
        }

        let tools = by_server
            .into_iter()
            .flat_map(|(server, tools)| {
                tools.into_iter().map(move |mut tool| {
                    tool.name = format!("{server}{SEPARATOR}{}", tool.name).into();
                    tool
                })
            })
            .collect();
        ListToolsResult {
            tools,
            ..ListToolsResult::default()
        }
    }
}

/// Splits a prefixed tool name into `(server, tool)` by longest known server
/// prefix. Matching requires the separator right after the server name, so
/// servers whose names are prefixes of one another stay distinguishable and
/// `__` inside tool names remains legal.
#[must_use]
pub fn resolve<'a>(name: &'a str, servers: &'a [String]) -> Option<(&'a str, &'a str)> {
    servers
        .iter()
        .filter_map(|server| {
            let tool = name
                .strip_prefix(server.as_str())?
                .strip_prefix(SEPARATOR)?;
            Some((server.as_str(), tool))
        })
        .max_by_key(|(server, _)| server.len())
}

impl ServerHandler for Gateway {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = Implementation::new("mcpgw", env!("CARGO_PKG_VERSION"));
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        match &self.mode {
            Mode::Pipe(upstream) => {
                let started = Instant::now();
                let tools = self
                    .within_deadline(upstream, async {
                        let service = self.upstream_service(upstream).await?;
                        service
                            .list_all_tools()
                            .await
                            .map_err(|err| ErrorData::internal_error(err.to_string(), None))
                    })
                    .await;
                let elapsed = started.elapsed();
                self.record(|session| {
                    let record = CaptureRecord::new(session, upstream, Kind::List, elapsed);
                    match &tools {
                        Ok(tools) => record.with_response(format!("{} tool(s)", tools.len())),
                        Err(err) => record.with_error(&err.message),
                    }
                });
                Ok(ListToolsResult {
                    tools: tools?,
                    ..ListToolsResult::default()
                })
            }
            Mode::Aggregate(upstreams) => Ok(self.aggregate_tools(upstreams).await),
        }
    }

    async fn call_tool(
        &self,
        mut request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let upstream = match &self.mode {
            Mode::Pipe(upstream) => upstream.clone(),
            Mode::Aggregate(upstreams) => {
                let Some((server, tool)) = resolve(&request.name, upstreams) else {
                    return Err(ErrorData::invalid_params(
                        format!(
                            "tool {:?} does not name a known server (expected \
                             <server>{SEPARATOR}<tool> with server one of: {})",
                            request.name,
                            upstreams.join(", ")
                        ),
                        None,
                    ));
                };
                let (server, tool) = (server.to_owned(), tool.to_owned());
                request.name = tool.into();
                server
            }
        };
        // Captured before the request moves upstream; `request.name` is the
        // bare tool name by now, which is what a per-server view wants.
        let tool = request.name.to_string();
        let args = request.arguments.clone().map(|args| {
            crate::capture::body(&serde_json::Value::Object(args.into_iter().collect()))
        });

        let started = Instant::now();
        let response = self
            .within_deadline(&upstream, async {
                let service = self.upstream_service(&upstream).await?;
                service
                    .call_tool(request)
                    .await
                    .map(CallToolResponse::from)
                    .map_err(|err| ErrorData::internal_error(err.to_string(), None))
            })
            .await;
        let elapsed = started.elapsed();

        self.record(|session| {
            let mut record =
                CaptureRecord::new(session, &upstream, Kind::Call, elapsed).with_tool(&tool);
            if let Some(args) = args.clone() {
                record = record.with_args(args);
            }
            match &response {
                Ok(response) => record.with_response(preview(response)),
                Err(err) => record.with_error(&err.message),
            }
        });
        response
    }
}

/// The one wording for a request that ran out its deadline. Names the
/// upstream and the ceiling, because "which server, and how long did I
/// actually wait" is what the user needs in order to act on it.
fn timed_out(upstream: &str, deadline: Duration) -> String {
    format!("upstream {upstream:?} did not answer within {deadline:?} (request deadline)")
}

/// Best-effort JSON rendering of a tool response for the capture log; the
/// debug form is a readable fallback for anything that will not serialize.
fn preview(response: &CallToolResponse) -> String {
    let text = match response {
        CallToolResponse::Complete(result) => {
            serde_json::to_string(result).unwrap_or_else(|_| format!("{result:?}"))
        }
        // Elicitation and task responses carry no result body worth
        // serializing here; their debug form names the shape well enough.
        other => format!("{other:?}"),
    };
    crate::capture::truncate(&text)
}

/// Serves the gateway over Streamable HTTP at `/mcp` on `listener` until
/// `shutdown` resolves. Used by both `mcpgw serve` and the test suite.
///
/// # Errors
///
/// Returns the underlying I/O error when the HTTP server fails.
pub async fn serve_http(
    gateway: Gateway,
    listener: tokio::net::TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
    };

    let service = StreamableHttpService::new(
        move || Ok(gateway.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(),
    );
    let router = axum::Router::new()
        .nest_service("/mcp", service)
        .layer(axum::middleware::from_fn(guard_origin));
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
}

/// Rejects browser requests that do not come from a loopback page.
///
/// Binding to loopback is not protection on its own: under DNS rebinding a
/// hostile page's own domain resolves to 127.0.0.1, which makes its requests
/// same-origin and lets it drive `POST /mcp` with no CORS preflight. The MCP
/// spec therefore requires servers to validate `Origin`. Non-browser MCP
/// clients send no `Origin` at all, so an absent header passes untouched.
async fn guard_origin(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;

    match request.headers().get(http::header::ORIGIN) {
        Some(origin) if !origin.to_str().is_ok_and(is_local_origin) => (
            http::StatusCode::FORBIDDEN,
            "origin not allowed: mcpgw only accepts requests from loopback origins\n",
        )
            .into_response(),
        _ => next.run(request).await,
    }
}

/// Whether an `Origin` header value names a loopback web origin
/// (`http(s)://localhost|127.0.0.1|[::1]` with an optional port).
fn is_local_origin(origin: &str) -> bool {
    let Some(rest) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        // Anything else — including the `null` origin a `file://` page
        // sends — is not a local page.
        return false;
    };
    // Strip a trailing `:port`; the bracketed IPv6 host keeps its brackets,
    // whose closing `]` is what distinguishes it from a port.
    let host = match rest.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => rest,
    };
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

/// Errors that end a stdio serving session.
#[derive(Debug, thiserror::Error)]
pub enum StdioError {
    // Boxed: rmcp's initialize error is several hundred bytes and would
    // otherwise bloat every Result in this path.
    #[error("stdio handshake failed: {0}")]
    Initialize(#[from] Box<rmcp::service::ServerInitializeError>),
    #[error("stdio service ended abnormally: {0}")]
    Join(#[from] tokio::task::JoinError),
}

/// Serves the gateway over stdin/stdout until the client hangs up. This is
/// the downstream face `mcpgw connect` presents to stdio-only clients, so
/// stdout belongs to the protocol: nothing else may write to it.
///
/// # Errors
///
/// Returns [`StdioError`] when the initialize handshake fails or the
/// service task panics.
pub async fn serve_stdio(gateway: Gateway) -> Result<rmcp::service::QuitReason, StdioError> {
    use rmcp::ServiceExt as _;
    use rmcp::transport::io::stdio;

    // Boxed: the serve future embeds the handler futures, which carry the
    // per-request deadline timers, and the whole thing is ~20 KB of stack if
    // left inline. It is created once per process, so the allocation is free.
    let running = Box::pin(gateway.serve(stdio())).await.map_err(Box::new)?;
    Ok(running.waiting().await?)
}

#[cfg(test)]
mod tests {
    use super::is_local_origin;

    #[test]
    fn loopback_origins_pass_in_every_spelling() {
        for origin in [
            "http://localhost",
            "http://localhost:8137",
            "https://localhost:3000",
            "http://127.0.0.1:8137",
            "http://[::1]",
            "http://[::1]:8137",
        ] {
            assert!(is_local_origin(origin), "{origin}");
        }
    }

    #[test]
    fn remote_and_lookalike_origins_are_rejected() {
        for origin in [
            "https://evil.example",
            // The rebinding shape: a hostile name, not the loopback literal.
            "http://localhost.evil.example",
            "http://127.0.0.1.evil.example",
            "http://notlocalhost",
            "null",
            "file://",
            "",
        ] {
            assert!(!is_local_origin(origin), "{origin}");
        }
    }
}
