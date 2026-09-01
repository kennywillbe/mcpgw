//! The gateway's downstream face: an rmcp server that forwards tool
//! requests to upstreams managed by [`UpstreamManager`]. M9 scope: a pure
//! pipe to a single upstream — aggregation/namespacing arrives in M10.

use std::sync::Arc;

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, ErrorData, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo,
};
use rmcp::service::{RequestContext, RoleServer};

use crate::upstream::UpstreamManager;

#[derive(Clone)]
pub struct Gateway {
    manager: Arc<UpstreamManager>,
    /// M9: the single piped upstream.
    upstream: String,
}

impl Gateway {
    #[must_use]
    pub fn new(manager: Arc<UpstreamManager>, upstream: String) -> Self {
        Self { manager, upstream }
    }

    #[must_use]
    pub fn manager(&self) -> &Arc<UpstreamManager> {
        &self.manager
    }

    async fn upstream_service(&self) -> Result<Arc<crate::upstream::UpstreamService>, ErrorData> {
        // Upstream failures surface as loud MCP errors — never as a silent
        // empty result.
        self.manager
            .ready(&self.upstream)
            .await
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))
    }
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
        let service = self.upstream_service().await?;
        let tools = service
            .list_all_tools()
            .await
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        Ok(ListToolsResult {
            tools,
            ..ListToolsResult::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let service = self.upstream_service().await?;
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
