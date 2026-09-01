//! The gateway's downstream face: an rmcp server that forwards tool
//! requests to upstreams managed by [`UpstreamManager`]. Two shapes:
//! a pure pipe to a single upstream (tool names untouched) and the
//! aggregate mode that merges N upstreams under `server__tool` names.

use std::collections::BTreeMap;
use std::sync::Arc;

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, ErrorData, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};

use crate::upstream::UpstreamManager;

/// Separator between server and tool name in aggregate mode. Server names
/// may not contain it (see `config::validate_name`), so the server half of
/// a prefixed name is always unambiguous.
pub const SEPARATOR: &str = "__";

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
}

impl Gateway {
    /// A pure pipe to `upstream`: tool names are neither prefixed on the way
    /// out nor stripped on the way in.
    #[must_use]
    pub fn new(manager: Arc<UpstreamManager>, upstream: String) -> Self {
        Self {
            manager,
            mode: Mode::Pipe(upstream),
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
        }
    }

    #[must_use]
    pub fn manager(&self) -> &Arc<UpstreamManager> {
        &self.manager
    }

    async fn upstream_service(
        &self,
        name: &str,
    ) -> Result<Arc<crate::upstream::UpstreamService>, ErrorData> {
        // Upstream failures surface as loud MCP errors — never as a silent
        // empty result.
        self.manager
            .ready(name)
            .await
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))
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
            tasks.spawn(async move {
                let tools = async {
                    let service = manager.ready(&name).await.map_err(|err| err.to_string())?;
                    service
                        .list_all_tools()
                        .await
                        .map_err(|err| err.to_string())
                }
                .await;
                (name, tools)
            });
        }

        // Collected by name so the merged list is ordered by server
        // regardless of which upstream answers first.
        let mut by_server: BTreeMap<String, Vec<Tool>> = BTreeMap::new();
        while let Some(joined) = tasks.join_next().await {
            let (name, tools) = match joined {
                Ok(result) => result,
                Err(err) => {
                    eprintln!("warning: listing tools panicked: {err}");
                    continue;
                }
            };
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
                let service = self.upstream_service(upstream).await?;
                let tools = service
                    .list_all_tools()
                    .await
                    .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
                Ok(ListToolsResult {
                    tools,
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
        let service = self.upstream_service(&upstream).await?;
        service
            .call_tool(request)
            .await
            .map(Into::into)
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))
    }
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
    let router = axum::Router::new().nest_service("/mcp", service);
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
}
