//! The gateway's downstream face: an rmcp server that forwards MCP requests
//! to upstreams managed by [`UpstreamManager`]. Two shapes: a pure pipe to a
//! single upstream (names untouched, every request family forwarded) and the
//! aggregate mode that merges the tools of N upstreams under `server__tool`
//! names.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CompleteRequestParams, CompleteResult,
    ErrorData, GetPromptRequestMethod, GetPromptRequestParams, GetPromptResponse, Implementation,
    ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult, ListToolsResult,
    PaginatedRequestParams, PromptsCapability, ProtocolVersion, ReadResourceRequestMethod,
    ReadResourceRequestParams, ReadResourceResponse, ResourcesCapability, ResultType,
    ServerCapabilities, ServerInfo, Tool, ToolsCapability,
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
    Aggregate(ServerList),
}

/// The set of servers an aggregate gateway fronts, shared and atomically
/// replaceable.
///
/// Shared rather than owned because the `/mcp` service is built once, from a
/// factory that clones one `Gateway` per session: a reload that rebuilt the
/// list by value would only reach sessions opened afterwards. Cloning this
/// handle shares the cell, so a swap is visible to every session at its next
/// request — and to none of them mid-request, since each read is one load.
#[derive(Clone)]
pub struct ServerList(Arc<arc_swap::ArcSwap<Vec<String>>>);

impl ServerList {
    #[must_use]
    pub fn new(names: Vec<String>) -> Self {
        Self(Arc::new(arc_swap::ArcSwap::from_pointee(names)))
    }

    /// Publishes `names` in place of the current list.
    pub fn store(&self, names: Vec<String>) {
        self.0.store(Arc::new(names));
    }

    #[must_use]
    pub fn load(&self) -> Arc<Vec<String>> {
        self.0.load_full()
    }
}

#[derive(Clone)]
pub struct Gateway {
    manager: Arc<UpstreamManager>,
    mode: Mode,
    unavailable_hint: Option<String>,
    capture: Option<Arc<CaptureWriter>>,
    endpoint: Option<String>,
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
            endpoint: None,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        }
    }

    /// Aggregates `upstreams` under `server__tool` names. Prefixing happens
    /// even for a single upstream so tool names stay stable as servers are
    /// added later.
    #[must_use]
    pub fn aggregate(manager: Arc<UpstreamManager>, upstreams: Vec<String>) -> Self {
        Self::aggregate_shared(manager, ServerList::new(upstreams))
    }

    /// Aggregates whatever `upstreams` holds at each request, so a config
    /// reload can change the set under a running `/mcp` service.
    #[must_use]
    pub fn aggregate_shared(manager: Arc<UpstreamManager>, upstreams: ServerList) -> Self {
        Self {
            manager,
            mode: Mode::Aggregate(upstreams),
            unavailable_hint: None,
            capture: None,
            endpoint: None,
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

    /// Names the face this gateway serves — `s/github`, `mcp` — so every
    /// record it writes says which endpoint the request arrived on. Left
    /// unset for the stdio face, which has no path to name.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
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

    /// Which downstream client a request belongs to, or `None` when the
    /// transport cannot say.
    ///
    /// rmcp's Streamable HTTP service injects the HTTP [`http::request::Parts`]
    /// into every request's extensions, which is where the `Mcp-Session-Id`
    /// its session manager minted at `initialize` is legible from a handler.
    /// That id — not the MCP protocol, which dropped sessions in 2026-07-28
    /// (SEP-2567) — is the only thing that survives to identify one downstream
    /// connection, so it is what attribution is built on. It is fingerprinted
    /// rather than stored, because the raw value is a session credential; see
    /// [`session_fingerprint`](crate::capture::session_fingerprint).
    ///
    /// `None` covers the stdio face and any HTTP client negotiating a version
    /// with no sessions: those requests are attributed to the gateway process
    /// instead, which is the pre-N13 behaviour and cannot separate clients.
    fn session_of(context: &RequestContext<RoleServer>) -> Option<String> {
        let parts = context.extensions.get::<http::request::Parts>()?;
        let id = parts.headers.get("mcp-session-id")?.to_str().ok()?;
        Some(crate::capture::session_fingerprint(id))
    }

    /// Writes one record, if capture is on. Deliberately a blocking append
    /// on the request path: a record is a few hundred bytes to an appended
    /// file, which costs far less than the channel and flush machinery that
    /// moving it off-thread would need. Capture never fails a request.
    ///
    /// `session` is the downstream session from [`Gateway::session_of`]; the
    /// writer's per-process id stands in when there was none.
    fn record(&self, session: Option<&str>, build: impl FnOnce(&str) -> CaptureRecord) {
        let Some(writer) = &self.capture else { return };
        let mut record = build(session.unwrap_or_else(|| writer.session()));
        // Stamped centrally: the endpoint is a property of this gateway, not
        // of any one request, so no call site can forget it.
        record.endpoint.clone_from(&self.endpoint);
        if let Err(err) = writer.append(&record) {
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

    /// Runs one request against `name` through the manager, which is where a
    /// transport failure retires the connection behind it.
    async fn call_upstream<T, F>(
        &self,
        name: &str,
        call: impl FnOnce(Arc<crate::upstream::UpstreamService>) -> F,
    ) -> Result<T, ErrorData>
    where
        F: Future<Output = Result<T, rmcp::service::ServiceError>>,
    {
        // Upstream failures surface as loud MCP errors — never as a silent
        // empty result.
        self.manager.call(name, call).await.map_err(|err| {
            // The hint is about a gateway that cannot reach its upstream, so
            // it has no business on an answer the upstream itself gave.
            let message = match (&err, &self.unavailable_hint) {
                (crate::upstream::CallError::Upstream(_), Some(hint)) => format!("{err} — {hint}"),
                _ => err.to_string(),
            };
            ErrorData::internal_error(message, None)
        })
    }

    /// The single upstream a request belongs to, or `None` in aggregate mode.
    ///
    /// Only a pipe can answer the resource, prompt and completion families,
    /// and that is a decision rather than a gap: those families are addressed
    /// by opaque strings with no namespace to prefix the way `server__tool`
    /// does. Two servers can both serve `file:///README.md` — one name, two
    /// different documents — and rewriting the URIs would break every link
    /// inside the contents that refer to them. So the aggregate keeps merging
    /// tools only, and `/s/<name>` is where a client goes for the rest.
    fn pipe_upstream(&self) -> Option<&str> {
        match &self.mode {
            Mode::Pipe(upstream) => Some(upstream),
            Mode::Aggregate(_) => None,
        }
    }

    /// What this face advertises at `initialize`.
    ///
    /// A pipe reports the upstream's own capabilities (narrowed by
    /// [`forwarded`]), because it forwards every family and guessing costs
    /// either way: claim too much and a client asks a tools-only server for
    /// resources, claim too little and a server's prompts stay invisible.
    ///
    /// The source is the snapshot from the last successful connect, never a
    /// fresh one: rmcp calls this synchronously while answering `initialize`,
    /// so reaching the upstream here would mean running a connect ladder
    /// inside a handshake — up to a minute and a half of a client waiting to
    /// be told what this gateway can do. Before first contact there is no
    /// snapshot, and the honest answer is the conservative one: tools only.
    /// A client that initialized in that window sees the rest after it
    /// reconnects, which is the same cost as a client that connected before
    /// the upstream had started.
    fn capabilities(&self) -> ServerCapabilities {
        let Some(upstream) = self.pipe_upstream() else {
            return tools_only();
        };
        self.manager
            .last_server_info(upstream)
            .map_or_else(tools_only, |info| forwarded(&info.capabilities))
    }

    /// Who this face says it is at `initialize`.
    ///
    /// A pipe answers with the upstream's own name and version once it has
    /// heard them: a harness shows this to the user, and one gateway serving
    /// N servers under N endpoints all called "mcpgw" tells them nothing
    /// about which server they are looking at. The source is the same
    /// snapshot [`Gateway::capabilities`] uses, and for the same reason —
    /// this runs inside the handshake, so it may not go and ask. Before
    /// first contact, and for the aggregate, the honest answer is that this
    /// is mcpgw.
    fn identity(&self) -> Implementation {
        self.pipe_upstream()
            .and_then(|upstream| self.manager.last_server_info(upstream))
            .and_then(|info| info.server_info.clone())
            .unwrap_or_else(|| Implementation::new("mcpgw", env!("CARGO_PKG_VERSION")))
    }

    /// Forwards one request to `upstream` under the request deadline and
    /// records the attempt.
    ///
    /// `subject` is what the request named — a prompt, a resource URI, an
    /// argument — for the capture record; the list families name nothing.
    /// `describe` renders the successful answer for the same record.
    async fn forward<T, F>(
        &self,
        session: Option<&str>,
        upstream: &str,
        kind: Kind,
        subject: Option<String>,
        call: impl FnOnce(Arc<crate::upstream::UpstreamService>) -> F,
        describe: impl FnOnce(&T) -> String,
    ) -> Result<T, ErrorData>
    where
        F: Future<Output = Result<T, rmcp::service::ServiceError>>,
    {
        let started = Instant::now();
        let result = self
            .within_deadline(upstream, self.call_upstream(upstream, call))
            .await;
        let elapsed = started.elapsed();
        self.record(session, |session| {
            let mut record = CaptureRecord::new(session, upstream, kind, elapsed);
            if let Some(subject) = subject {
                record = record.with_tool(subject);
            }
            match &result {
                Ok(value) => record.with_response(describe(value)),
                Err(err) => record.with_error(&err.message),
            }
        });
        result
    }

    /// Lists every upstream's tools in parallel and merges them under their
    /// `server__` prefixes. An upstream that cannot answer is reported on
    /// the gateway console and omitted: degraded, but never silent and never
    /// fatal for the healthy upstreams.
    async fn aggregate_tools(
        &self,
        session: Option<&str>,
        upstreams: &[String],
    ) -> ListToolsResult {
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
                    manager
                        .call(
                            &name,
                            |service| async move { service.list_all_tools().await },
                        )
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
            self.record(session, |session| {
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

/// The conservative answer for a face that cannot know better yet: tools are
/// the one family both gateway shapes always serve.
fn tools_only() -> ServerCapabilities {
    ServerCapabilities::builder().enable_tools().build()
}

/// The upstream's capabilities, narrowed to the families a pipe actually
/// forwards.
///
/// Copying the upstream's set verbatim would over-claim: `resources.subscribe`
/// and the `listChanged` flags promise subscriptions and notifications that
/// stop at the gateway, and a client that took them at their word would sit
/// waiting for updates that never arrive. What is advertised here is exactly
/// what [`Gateway`] implements.
fn forwarded(upstream: &ServerCapabilities) -> ServerCapabilities {
    let mut capabilities = ServerCapabilities::default();
    if upstream.tools.is_some() {
        capabilities.tools = Some(ToolsCapability::default());
    }
    if upstream.resources.is_some() {
        capabilities.resources = Some(ResourcesCapability::default());
    }
    if upstream.prompts.is_some() {
        capabilities.prompts = Some(PromptsCapability::default());
    }
    if upstream.completions.is_some() {
        capabilities.completions = Some(serde_json::Map::new());
    }
    capabilities
}

/// The revision that made `resultType` required on every result (SEP-2322)
/// and `ttlMs`/`cacheScope` required on the cacheable ones (SEP-2549).
const STRICT_RESULTS: ProtocolVersion = ProtocolVersion::V_2026_07_28;

/// Makes `result` self-consistent with the revision the *downstream* session
/// negotiated — which is the only revision it has to be consistent with,
/// whoever actually produced it.
///
/// A pipe holds two protocol conversations and nothing makes them agree. A
/// client reaches us over the 2026-07-28 lifecycle (no handshake: the
/// revision travels in each request's `_meta`), while the upstream
/// connection behind it negotiated 2025-11-25, because that is the newest
/// revision rmcp still has an `initialize` handshake for. Relaying that
/// reply untouched hands a 2026-07-28 client a result shaped for the older
/// revision — no `resultType`, no `ttlMs`/`cacheScope` — and a client that
/// validates against the revision it negotiated rejects the whole answer.
/// That is what "Connected · tools fetch failed" looked like in the field,
/// with the upstream perfectly healthy and the request logged as a success.
///
/// Only absent fields are filled: an upstream that speaks 2026-07-28 already
/// said what it meant, and a pipe does not second-guess it. Nothing is
/// stripped on the way to an older client either — results are open to
/// fields a client does not know, and a client of an earlier revision
/// ignores them, so removing them would only lose information.
fn bridged<T: SelfConsistent>(context: &RequestContext<RoleServer>, mut result: T) -> T {
    if context
        .protocol_version()
        .is_some_and(|version| version >= STRICT_RESULTS)
    {
        result.fill_required_fields();
    }
    result
}

/// A reply that can fill in the fields 2026-07-28 made mandatory.
trait SelfConsistent {
    fn fill_required_fields(&mut self);
}

/// `resultType` for the results that carry nothing else new.
///
/// `"complete"` is not a guess: the spec's own rule is that a result from an
/// earlier-revision server which omits the field means `"complete"`. The
/// pipe applies that reading on the client's behalf, because from where the
/// client stands mcpgw is not an earlier-revision server and the exemption
/// does not cover it.
macro_rules! completes {
    ($($result:ty),+ $(,)?) => {$(
        impl SelfConsistent for $result {
            fn fill_required_fields(&mut self) {
                self.result_type.get_or_insert(ResultType::COMPLETE);
            }
        }
    )+};
}

completes!(
    CompleteResult,
    rmcp::model::GetPromptResult,
    rmcp::model::CallToolResult
);

/// The same, plus the SEP-2549 caching fields the revision requires on
/// `tools/list`, `prompts/list`, `resources/list`, `resources/templates/list`
/// and `resources/read`.
///
/// An upstream that never said how long its answer stays fresh has not given
/// the pipe one to pass on, and inventing a freshness window would be the
/// gateway making a promise the server never made: `ttlMs: 0` is the
/// spec's "already stale", which asks the client to come back rather than
/// reuse this. `private` because a pipe answers with the operator's own
/// credentials attached — whatever comes back was fetched as one particular
/// user, and no shared intermediary may hand it to another.
macro_rules! completes_and_cacheable {
    ($($result:ty),+ $(,)?) => {$(
        impl SelfConsistent for $result {
            fn fill_required_fields(&mut self) {
                self.result_type.get_or_insert(ResultType::COMPLETE);
                self.ttl_ms.get_or_insert(0);
                self.cache_scope.get_or_insert(CacheScope::Private);
            }
        }
    )+};
}

completes_and_cacheable!(
    ListToolsResult,
    ListResourcesResult,
    ListResourceTemplatesResult,
    ListPromptsResult,
    rmcp::model::ReadResourceResult,
);

/// The multi-round-trip responses: only the completed branch is a result of
/// an earlier revision's shape. `input_required` and task acknowledgements
/// exist solely in 2026-07-28 and carry their own discriminator already.
macro_rules! completes_when_complete {
    ($($response:ty => $variant:path),+ $(,)?) => {$(
        impl SelfConsistent for $response {
            fn fill_required_fields(&mut self) {
                if let $variant(result) = self {
                    result.fill_required_fields();
                }
            }
        }
    )+};
}

completes_when_complete!(
    CallToolResponse => CallToolResponse::Complete,
    ReadResourceResponse => ReadResourceResponse::Complete,
    GetPromptResponse => GetPromptResponse::Complete,
);

impl ServerHandler for Gateway {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = self.capabilities();
        info.server_info = self.identity();
        info
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let session = Self::session_of(&context);
        let result = match &self.mode {
            // One request, one answer, handed back exactly as the upstream
            // wrote it. The pipe used to collect every page with
            // `list_all_tools` and rebuild the result around the tools it
            // found, which threw away everything else the upstream had put
            // there — the SEP-2549 caching fields (`ttlMs`, `cacheScope`)
            // among them, which a strict client rejects the answer for — and
            // left the client with no cursor to page with either.
            Mode::Pipe(upstream) => {
                self.forward(
                    session.as_deref(),
                    upstream,
                    Kind::List,
                    None,
                    |service| async move { service.list_tools(request).await },
                    |result| format!("{} tool(s)", result.tools.len()),
                )
                .await
            }
            Mode::Aggregate(upstreams) => Ok(self
                .aggregate_tools(session.as_deref(), &upstreams.load())
                .await),
        };
        Ok(bridged(&context, result?))
    }

    async fn call_tool(
        &self,
        mut request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let session = Self::session_of(&context);
        let upstream = match &self.mode {
            Mode::Pipe(upstream) => upstream.clone(),
            Mode::Aggregate(upstreams) => {
                // One load for the whole request: the name is resolved
                // against exactly the list the error message would name.
                let upstreams = upstreams.load();
                let Some((server, tool)) = resolve(&request.name, &upstreams) else {
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
            .within_deadline(
                &upstream,
                self.call_upstream(&upstream, |service| async move {
                    service.call_tool(request).await.map(CallToolResponse::from)
                }),
            )
            .await;
        let elapsed = started.elapsed();

        self.record(session.as_deref(), |session| {
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
        Ok(bridged(&context, response?))
    }

    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let Some(upstream) = self.pipe_upstream() else {
            return Ok(bridged(&context, ListResourcesResult::default()));
        };
        // Pagination is forwarded rather than collapsed the way tools/list
        // does it: the cursor a pipe hands back came from the one upstream
        // that will be asked for the next page, so it stays meaningful.
        self.forward(
            Self::session_of(&context).as_deref(),
            upstream,
            Kind::Resources,
            None,
            |service| async move { service.list_resources(request).await },
            |result| format!("{} resource(s)", result.resources.len()),
        )
        .await
        .map(|result| bridged(&context, result))
    }

    async fn list_resource_templates(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        let Some(upstream) = self.pipe_upstream() else {
            return Ok(bridged(&context, ListResourceTemplatesResult::default()));
        };
        self.forward(
            Self::session_of(&context).as_deref(),
            upstream,
            Kind::ResourceTemplates,
            None,
            |service| async move { service.list_resource_templates(request).await },
            |result| format!("{} template(s)", result.resource_templates.len()),
        )
        .await
        .map(|result| bridged(&context, result))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let Some(upstream) = self.pipe_upstream() else {
            // The aggregate serves no resources, so the honest answer is the
            // one rmcp's default handler gives: the method is not here.
            return Err(ErrorData::method_not_found::<ReadResourceRequestMethod>());
        };
        let uri = request.uri.clone();
        // The `_once` form forwards an `input_required` answer downstream
        // instead of trying to satisfy it here: the client on the other side
        // is the one that can ask a human, and a pipe must not swallow a
        // round it cannot complete.
        self.forward(
            Self::session_of(&context).as_deref(),
            upstream,
            Kind::ResourceRead,
            Some(uri),
            |service| async move { service.read_resource_once(request).await },
            |response| match response {
                ReadResourceResponse::Complete(result) => crate::capture::body(
                    &serde_json::to_value(result).unwrap_or_else(|_| format!("{result:?}").into()),
                ),
                other => crate::capture::truncate(&format!("{other:?}")),
            },
        )
        .await
        .map(|result| bridged(&context, result))
    }

    async fn list_prompts(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        let Some(upstream) = self.pipe_upstream() else {
            return Ok(bridged(&context, ListPromptsResult::default()));
        };
        self.forward(
            Self::session_of(&context).as_deref(),
            upstream,
            Kind::Prompts,
            None,
            |service| async move { service.list_prompts(request).await },
            |result| format!("{} prompt(s)", result.prompts.len()),
        )
        .await
        .map(|result| bridged(&context, result))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        let Some(upstream) = self.pipe_upstream() else {
            return Err(ErrorData::method_not_found::<GetPromptRequestMethod>());
        };
        let name = request.name.clone();
        self.forward(
            Self::session_of(&context).as_deref(),
            upstream,
            Kind::PromptGet,
            Some(name),
            |service| async move { service.get_prompt_once(request).await },
            |response| match response {
                GetPromptResponse::Complete(result) => crate::capture::body(
                    &serde_json::to_value(result).unwrap_or_else(|_| format!("{result:?}").into()),
                ),
                other => crate::capture::truncate(&format!("{other:?}")),
            },
        )
        .await
        .map(|result| bridged(&context, result))
    }

    async fn complete(
        &self,
        request: CompleteRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CompleteResult, ErrorData> {
        let Some(upstream) = self.pipe_upstream() else {
            // An empty completion, which is what rmcp's default answers and
            // what the spec expects of a server with nothing to suggest.
            return Ok(bridged(&context, CompleteResult::default()));
        };
        let argument = request.argument.name.clone();
        self.forward(
            Self::session_of(&context).as_deref(),
            upstream,
            Kind::Complete,
            Some(argument),
            |service| async move { service.complete(request).await },
            |result| format!("{} completion(s)", result.completion.values.len()),
        )
        .await
        .map(|result| bridged(&context, result))
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
    serve_http_with(gateway, None, listener, shutdown).await
}

/// Serves the gateway at `/mcp` and, when `endpoints` is given, one
/// per-server face under `/s/<name>` for each of them. Both share the
/// listener, the origin guard and — because every gateway is built over the
/// same [`UpstreamManager`] — the upstream connections.
///
/// # Errors
///
/// Returns the underlying I/O error when the HTTP server fails.
pub async fn serve_http_with(
    gateway: Gateway,
    endpoints: Option<crate::endpoints::Endpoints>,
    listener: tokio::net::TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
    };

    // The aggregate learns its own name here for the same reason the endpoint
    // table stamps its pipes: this function owns the `/mcp` route.
    let gateway = gateway.with_endpoint(crate::endpoints::AGGREGATE_LABEL);
    let service = StreamableHttpService::new(
        move || Ok(gateway.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(),
    );
    let mut router = axum::Router::new().nest_service("/mcp", service);
    if let Some(endpoints) = endpoints {
        router = router.merge(crate::endpoints::router(endpoints));
    }
    // Layered over the merged router, so the guard covers every face.
    let router = router.layer(axum::middleware::from_fn(guard_origin));
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
