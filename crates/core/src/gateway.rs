//! The gateway's downstream face: an rmcp server that forwards MCP requests
//! to upstreams managed by [`UpstreamManager`]. One shape only — a pure pipe
//! to a single upstream, names untouched, every request family forwarded —
//! raised once per server and served at `/s/<name>`.
//!
//! The one thing the pipe does not pass through unchanged is `tools/list`
//! pagination, which it merges into a single answer, because the clients
//! that matter do not follow `nextCursor` and lose every tool past page one.
//!
//! Traffic also runs the other way: an upstream's `list_changed`
//! notifications reach the sessions this file serves, over the session's own
//! stream for a client that handshook and over `subscriptions/listen` for one
//! on 2026-07-28. The upstream half of that is [`UpstreamManager::subscribe`].
//!
//! [`Base`] is the other service here and forwards nothing. It is what
//! answers on `/mcp`, so that probing the gateway and asking it who it is
//! have somewhere to land.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CompleteRequestParams, CompleteResult,
    Cursor, ErrorCode, ErrorData, GetPromptRequestParams, GetPromptResponse, Implementation,
    ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult, ListToolsResult,
    PaginatedRequestParams, ProtocolVersion, ReadResourceRequestParams, ReadResourceResponse,
    ResultType, ServerCapabilities, ServerInfo, SubscriptionFilter,
};
use rmcp::service::{
    NotificationContext, Peer, RequestContext, RoleServer, SubscriptionContext, SubscriptionSink,
};

use crate::capture::{CaptureRecord, CaptureWriter, Kind};
use crate::upstream::{ListChanged, UpstreamManager};

/// Reserved inside server names (see `config::validate_name`) and the join
/// `mcpgw watch` renders a captured call under. Nothing on the wire is
/// spelled with it: no face of this gateway prefixes a tool name.
pub const SEPARATOR: &str = "__";

/// Ceiling on one downstream request, covering both acquiring the upstream
/// (which can run a full connect ladder) and the forwarded call.
///
/// Deliberately generous: an MCP tool call may legitimately take minutes, so
/// this is a backstop against hanging forever, not a latency budget. It is
/// still shorter than the ~93s worst-case ladder plus an unbounded call,
/// which is what a client used to be able to wait for with no answer at all.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Who a request came from, as far as the capture log can tell.
///
/// Two answers rather than one, because the protocol stopped being able to
/// give a single one: `session` says which connection, `client` says which
/// program. Under 2026-07-28 there are no sessions, so the connection half
/// degrades to the software identity and every window of one editor collapses
/// into a single row — the client half is what still separates Claude Code
/// from Cursor. Either may be absent without the other.
#[derive(Debug)]
struct Attribution {
    session: Option<String>,
    client: Option<String>,
}

#[derive(Clone)]
pub struct Gateway {
    manager: Arc<UpstreamManager>,
    upstream: String,
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
            upstream,
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

    /// Names the face this gateway serves — `s/github` — so every record it
    /// writes says which endpoint the request arrived on. Left unset for the
    /// stdio face, which has no path to name.
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

    /// Who one request came from, for the capture log.
    ///
    /// The session half is the `Mcp-Session-Id` rmcp's session manager minted
    /// at `initialize`, which identifies one downstream connection for as
    /// long as a client speaks a revision that has sessions at all. It is
    /// fingerprinted rather than stored, because the raw value is a session
    /// credential; see
    /// [`session_fingerprint`](crate::capture::session_fingerprint). A
    /// 2026-07-28 client has no session id to fingerprint — sessions are
    /// gone — so it falls back to a fingerprint of the client identity, which
    /// separates clients by software rather than by connection. `None` is
    /// left for a request with neither, and those are filed under the gateway
    /// process, which cannot separate clients at all.
    ///
    /// The client half is that same identity spelled out instead of
    /// fingerprinted, which is what makes the log answer "which harness made
    /// this call" rather than only "were these two calls the same caller".
    /// Storing it as it stands is safe where storing a session id is not: a
    /// name and a version are what a client publishes about itself.
    ///
    /// Where the identity is read from depends on the revision, and the two
    /// places are not interchangeable. A 2026-07-28 client repeats it in
    /// every request's `_meta` (`io.modelcontextprotocol/clientInfo`,
    /// SEP-2575); a client on a revision that still handshakes sends it once,
    /// at `initialize`, and it lives on the peer from then on.
    ///
    /// The peer is asked only when there is a handshake behind it. rmcp
    /// synthesizes a peer for a stateless request — an `Implementation`
    /// naming the SDK, built so `protocol_version()` has something to read —
    /// and reading the caller off that would file every unattributed request
    /// under `rmcp`, which is a worse answer than none. `RequestContext`'s
    /// own `client_info` does exactly that, which is why this does not use
    /// it. A transport session, or a face with no HTTP request under it at
    /// all (stdio), is what says the peer is a real one.
    ///
    /// Naming yourself is a SHOULD, so a client that declines stays
    /// unattributed. Nothing here guesses.
    fn attribution(context: &RequestContext<RoleServer>) -> Attribution {
        // rmcp's Streamable HTTP service injects the HTTP
        // [`http::request::Parts`] into every request's extensions, which is
        // where the `Mcp-Session-Id` its session manager minted at
        // `initialize` is legible from a handler.
        let http = context.extensions.get::<http::request::Parts>();
        let session_id = http.and_then(|parts| parts.headers.get("mcp-session-id")?.to_str().ok());
        let handshaked = session_id.is_some() || http.is_none();
        let named = context.meta.client_info().or_else(|| {
            handshaked
                .then(|| context.peer.peer_info())
                .flatten()
                .map(|info| info.client_info.clone())
        });
        let client = named.map(|client| {
            // Version included: an upgrade is a different client as far as
            // "which of these is misbehaving" is concerned. A client that
            // sends an empty version gets its bare name rather than a
            // trailing slash, which would read as a version nobody has.
            if client.version.is_empty() {
                client.name
            } else {
                format!("{}/{}", client.name, client.version)
            }
        });
        let session = session_id
            .map(ToOwned::to_owned)
            .or_else(|| client.clone())
            .map(|id| crate::capture::session_fingerprint(&id));
        Attribution { session, client }
    }

    /// Writes one record, if capture is on. Deliberately a blocking append
    /// on the request path: a record is a few hundred bytes to an appended
    /// file, which costs far less than the channel and flush machinery that
    /// moving it off-thread would need. Capture never fails a request.
    ///
    /// `who` is [`Gateway::attribution`] for the request; the writer's
    /// per-process id stands in when it found no session.
    fn record(&self, who: &Attribution, build: impl FnOnce(&str) -> CaptureRecord) {
        let Some(writer) = &self.capture else { return };
        let mut record = build(who.session.as_deref().unwrap_or_else(|| writer.session()));
        // Stamped centrally: the endpoint is a property of this gateway and
        // the caller a property of the request, neither of them of the family
        // being recorded, so no call site can forget either.
        record.endpoint.clone_from(&self.endpoint);
        record.client.clone_from(&who.client);
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
            // A 401 upstream is the one failure the client is not supposed to
            // fix by trying again or by starting something, so it carries its
            // own sentence and none of the hints: the `WWW-Authenticate` the
            // server sent is deliberately not relayed (a client that answered
            // it would send the upstream's token through the gateway), which
            // makes naming the command the whole of the help there is.
            if let crate::upstream::CallError::Upstream(
                auth @ crate::upstream::UpstreamError::AuthRequired { .. },
            ) = &err
            {
                return ErrorData::internal_error(auth.to_string(), None);
            }
            // The hint is about a gateway that cannot reach its upstream, so
            // it has no business on an answer the upstream itself gave.
            let message = match (&err, &self.unavailable_hint) {
                (crate::upstream::CallError::Upstream(_), Some(hint)) => format!("{err} — {hint}"),
                _ => err.to_string(),
            };
            ErrorData::internal_error(message, None)
        })
    }

    /// What this face advertises at `initialize`.
    ///
    /// This face reports the upstream's own capabilities (narrowed by
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
        self.manager
            .last_server_info(&self.upstream)
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
    /// first contact the honest answer is that this is mcpgw.
    fn identity(&self) -> Implementation {
        self.manager
            .last_server_info(&self.upstream)
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
        who: &Attribution,
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
        self.record(who, |session| {
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
}

/// The conservative answer for a face that cannot know better yet: tools are
/// the one family every face of this gateway serves.
fn tools_only() -> ServerCapabilities {
    ServerCapabilities::builder().enable_tools().build()
}

/// The upstream's capabilities, minus the ones a pipe cannot honour.
///
/// Everything else is forwarded as the upstream declared it, including
/// whatever a later revision adds. The rule used to be the other way round —
/// an allow-list of the families this file knew about — which quietly dropped
/// every capability the spec grew afterwards: `extensions` (SEP-1724) went
/// missing the day it landed, and each future one would have too. A pipe that
/// forwards the requests has no business hiding the advertisement.
///
/// `listChanged` on tools, resources and prompts is *not* subtracted, and used
/// to be: the pipe now carries those notifications in both directions the two
/// revisions define — to a 2025-11-25 session's peer, and over
/// `subscriptions/listen` for 2026-07-28 (issue #140) — so advertising what
/// the upstream declared is a promise it keeps.
///
/// The subtractions, and why each is a promise this gateway would break:
///
/// - `resources.subscribe`: per-resource `notifications/resources/updated`
///   needs the pipe to hold one subscription per URI against the upstream and
///   fan it out, which it does not do. Only the list-changed half of the
///   notification story crossed with #140.
/// - `logging`: `logging/setLevel` is not forwarded and `notifications/message`
///   does not cross the pipe either. Deprecated in 2026-07-28 (SEP-2577), so
///   this one is not waiting on anything.
/// - the `io.modelcontextprotocol/tasks` extension (SEP-2663): advertising it
///   makes the SDK accept `tasks/get`, `tasks/update` and `tasks/cancel` on
///   this face, which the pipe answers "method not found". A client would be
///   handed a task handle it could never poll, which is worse than never
///   being offered one.
fn forwarded(upstream: &ServerCapabilities) -> ServerCapabilities {
    let mut capabilities = upstream.clone();
    if let Some(resources) = capabilities.resources.as_mut() {
        resources.subscribe = None;
    }
    capabilities.logging = None;
    if let Some(extensions) = capabilities.extensions.as_mut() {
        extensions.remove(TASKS_EXTENSION);
        if extensions.is_empty() {
            capabilities.extensions = None;
        }
    }
    capabilities
}

/// The tasks extension's key in `capabilities.extensions` (SEP-2663). Spelled
/// out rather than taken from rmcp, which keeps its copy private.
const TASKS_EXTENSION: &str = "io.modelcontextprotocol/tasks";

/// The list-changed families `capabilities` promises.
///
/// This is what a session is allowed to be sent, and it is read from what the
/// session was *told* rather than from what the upstream says right now: a
/// client acts on the capabilities it saw at `initialize`, and a notification
/// for a family it was never offered is one it has no reason to expect.
fn promised(capabilities: &ServerCapabilities) -> Vec<ListChanged> {
    declared([
        (
            capabilities.tools.as_ref().and_then(|it| it.list_changed),
            ListChanged::Tools,
        ),
        (
            capabilities
                .resources
                .as_ref()
                .and_then(|it| it.list_changed),
            ListChanged::Resources,
        ),
        (
            capabilities.prompts.as_ref().and_then(|it| it.list_changed),
            ListChanged::Prompts,
        ),
    ])
}

/// The same, read off an accepted `subscriptions/listen` filter.
fn subscribed(filter: &SubscriptionFilter) -> Vec<ListChanged> {
    declared([
        (filter.tools_list_changed, ListChanged::Tools),
        (filter.resources_list_changed, ListChanged::Resources),
        (filter.prompts_list_changed, ListChanged::Prompts),
    ])
}

/// The families whose flag is an explicit yes. `None` and `Some(false)` are
/// the same answer here: neither is a promise.
fn declared(flags: [(Option<bool>, ListChanged); 3]) -> Vec<ListChanged> {
    flags
        .into_iter()
        .filter(|(flag, _)| *flag == Some(true))
        .map(|(_, what)| what)
        .collect()
}

/// How often a session that has heard nothing is checked for still being
/// there.
///
/// A relay task parks on the upstream's channel, and an upstream that never
/// changes its lists never wakes it — so without this a session that ended
/// quietly would leave its task parked for the life of the process. Long,
/// because the cost of noticing late is one idle task and the cost of
/// noticing often is a timer per session.
const LIVENESS_POLL: Duration = Duration::from_secs(30);

/// Forwards `changes` to a 2025-11-25 session's peer until the session ends.
///
/// This is the revision that has nowhere else to put a server-initiated
/// notification: it rides the session's standalone stream, which is what the
/// peer writes to. A send that fails is not the end of the relay — a client
/// is entitled to open that stream late, or never — so only the transport
/// going away stops it.
async fn relay(peer: Peer<RoleServer>, mut changes: Changes, promised: Vec<ListChanged>) {
    loop {
        let event = tokio::select! {
            () = tokio::time::sleep(LIVENESS_POLL) => {
                if peer.is_transport_closed() {
                    return;
                }
                continue;
            }
            event = changes.recv() => event,
        };
        let due = match event {
            Ok(what) if promised.contains(&what) => vec![what],
            Ok(_) => continue,
            // Nothing is lost by a full lap: the notification carries no
            // payload, so N missed events and one event ask the client for
            // exactly the same thing — read the list again.
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => promised.clone(),
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        };
        for what in due {
            let sent = match what {
                ListChanged::Tools => peer.notify_tool_list_changed().await,
                ListChanged::Resources => peer.notify_resource_list_changed().await,
                ListChanged::Prompts => peer.notify_prompt_list_changed().await,
            };
            if sent.is_err() && peer.is_transport_closed() {
                return;
            }
        }
    }
}

/// The receiving half of one upstream's list-changed stream.
type Changes = tokio::sync::broadcast::Receiver<ListChanged>;

/// Sends one list-changed over a `subscriptions/listen` stream. The sink
/// enforces the accepted filter itself, so a family that was not subscribed
/// is refused here rather than reaching the client.
async fn push(sink: &SubscriptionSink, what: ListChanged) -> bool {
    let sent = match what {
        ListChanged::Tools => sink.notify_tool_list_changed().await,
        ListChanged::Resources => sink.notify_resource_list_changed().await,
        ListChanged::Prompts => sink.notify_prompt_list_changed().await,
    };
    sent.is_ok()
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

/// How far the pipe will walk a server's `tools/list` before it stops and
/// answers with what it has: at most this many pages, carrying at most
/// [`MAX_MERGED_TOOLS`] tools.
///
/// Both ceilings are far above any real server — the largest lists in the
/// wild are a few hundred tools over a handful of pages — because they are
/// not a policy about list size. They exist so that a server which is broken,
/// or hostile, cannot hold one client request open for as long as it feels
/// like handing out cursors.
pub const MAX_TOOL_PAGES: usize = 64;

/// The tool ceiling on a merged `tools/list`; see [`MAX_TOOL_PAGES`].
pub const MAX_MERGED_TOOLS: usize = 10_000;

/// Collects every page of the upstream's `tools/list` into one answer.
///
/// Pagination is a promise the client has to keep, and two of the three
/// harnesses most people run do not keep it: Cursor and Codex both ignore
/// `nextCursor` on `tools/list`, so a server with more tools than one page
/// shows a truncated list with no error anywhere to say so. Cursor's own
/// staff suggest the fix is a proxy that merges the pages, and a pipe already
/// standing in the path is exactly that proxy.
///
/// The merged result *is* page one's, with the later pages' tools appended:
/// its `ttlMs`, `cacheScope` and `_meta` are what the upstream said about
/// this list, and rebuilding the result around the tools is what once dropped
/// them (issue #62). What page two onwards say about caching is discarded —
/// there is one answer now, and one thing it can claim.
async fn merged_tools(
    service: &crate::upstream::UpstreamService,
    upstream: &str,
    request: Option<PaginatedRequestParams>,
) -> Result<ListToolsResult, rmcp::service::ServiceError> {
    let mut merged = service.list_tools(request.clone()).await?;
    // Taken, not read: the cursor is the pipe's to follow, and handing it
    // back would offer the client a second page of tools it already has.
    let mut cursor = merged.next_cursor.take();
    let mut seen: std::collections::HashSet<Cursor> = std::collections::HashSet::new();
    let mut pages = 1_usize;

    while let Some(next) = cursor {
        // A cursor that has already been handed out means the server is
        // paging in a circle, which is an unbounded walk however high the
        // page ceiling is.
        if !seen.insert(next.clone()) {
            eprintln!(
                "{}",
                stopped_merging(
                    upstream,
                    &format!("it handed back the cursor {next:?} twice")
                )
            );
            break;
        }
        if pages >= MAX_TOOL_PAGES {
            eprintln!(
                "{}",
                stopped_merging(
                    upstream,
                    &format!("its list runs past {MAX_TOOL_PAGES} pages")
                )
            );
            break;
        }
        if merged.tools.len() >= MAX_MERGED_TOOLS {
            eprintln!(
                "{}",
                stopped_merging(
                    upstream,
                    &format!("its list runs past {MAX_MERGED_TOOLS} tools")
                )
            );
            break;
        }
        // The client's own params carry forward — only the cursor is the
        // pipe's — so a request's `_meta` reaches every page of the walk.
        let params = request.clone().unwrap_or_default().with_cursor(Some(next));
        let mut page = service.list_tools(Some(params)).await?;
        merged.tools.append(&mut page.tools);
        cursor = page.next_cursor;
        pages += 1;
    }
    Ok(merged)
}

/// The one wording for a `tools/list` walk that had to stop early. Names the
/// server, because with several upstreams behind one gateway "which one is
/// doing this" is the whole of what the operator can act on, and says the
/// list is partial, because that is what the user will otherwise notice as a
/// missing tool and blame on the client.
fn stopped_merging(upstream: &str, reason: &str) -> String {
    format!(
        "warning: stopped merging tools/list for upstream {upstream:?} because {reason}; \
         clients will see a partial tool list"
    )
}

impl ServerHandler for Gateway {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = self.capabilities();
        info.server_info = self.identity();
        info
    }

    /// Starts relaying the upstream's list-changed notifications to this
    /// session (issue #140).
    ///
    /// Only a session with a handshake reaches here — 2026-07-28 has no
    /// `initialize` and therefore no `notifications/initialized`, and asks
    /// for the same events through `subscriptions/listen` instead.
    ///
    /// Nothing is spawned for a session that was promised nothing: before the
    /// gateway has ever reached the upstream it advertises tools only, and a
    /// server that never announces a change gets no relay either. The task
    /// ends with the session; see [`relay`].
    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        let promised = promised(&self.capabilities());
        if promised.is_empty() {
            return;
        }
        let Some(changes) = self.manager.subscribe(&self.upstream) else {
            return;
        };
        tokio::spawn(relay(context.peer.clone(), changes, promised));
    }

    /// What of a `subscriptions/listen` filter this pipe will honour — the
    /// 2026-07-28 shape of the same forwarding (SEP-2568).
    ///
    /// rmcp narrows whatever is returned here by both the request and the
    /// capabilities [`Gateway::get_info`] advertises, so this only has to
    /// subtract what the pipe itself cannot carry: `resource_subscriptions`,
    /// which needs a per-URI subscription upstream that the pipe does not
    /// hold. `None` — which rmcp answers "method not found" to — is the
    /// honest reply when the server behind this endpoint promises no
    /// notifications at all.
    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        let mut accepted = requested.supported_by(&self.capabilities());
        accepted.resource_subscriptions = None;
        (!subscribed(&accepted).is_empty()).then_some(accepted)
    }

    /// Runs one `subscriptions/listen` stream until the client cancels it.
    ///
    /// The same upstream events [`relay`] forwards to a session peer, put on
    /// the request's own stream instead — which is where this revision says a
    /// server-initiated notification belongs, now that there is no session to
    /// hold one.
    async fn listen(&self, context: SubscriptionContext) -> Result<(), ErrorData> {
        let subscribed = subscribed(context.accepted());
        let Some(mut changes) = self.manager.subscribe(&self.upstream) else {
            // The server was removed from the config under a stream that was
            // already open. Nothing is coming, so hold it until the client
            // lets go rather than closing it as if it had been served.
            context.cancelled().await;
            return Ok(());
        };
        let sink = context.sink().clone();
        loop {
            let event = tokio::select! {
                () = context.cancelled() => return Ok(()),
                event = changes.recv() => event,
            };
            let due = match event {
                Ok(what) if subscribed.contains(&what) => vec![what],
                Ok(_) => continue,
                // See [`relay`]: a lap of the filter says the same thing the
                // dropped events would have.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => subscribed.clone(),
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    context.cancelled().await;
                    return Ok(());
                }
            };
            for what in due {
                if !push(&sink, what).await {
                    return Ok(());
                }
            }
        }
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        // A client that sends a cursor is one that *does* paginate, asking
        // for the page after a list that has no page after it. An empty list
        // ends its loop; the alternative the spec offers for a cursor a
        // server does not recognise is an error, which would fail a
        // `tools/list` for a client that did nothing wrong.
        if request
            .as_ref()
            .is_some_and(|params| params.cursor.is_some())
        {
            // Nothing recorded: capture is a log of upstream traffic, and
            // this answer never went upstream.
            return Ok(bridged(&context, ListToolsResult::default()));
        }
        let who = Self::attribution(&context);
        let upstream = self.upstream.clone();
        // One record for the whole walk rather than one per page: the client
        // made a single `tools/list`, and N rows against it would have
        // `mcpgw watch` reporting traffic nobody downstream generated. The
        // pages are the pipe's business, not the log's.
        let result = self
            .forward(
                &who,
                &self.upstream,
                Kind::List,
                None,
                |service| async move { merged_tools(&service, &upstream, request).await },
                |result| format!("{} tool(s)", result.tools.len()),
            )
            .await;
        Ok(bridged(&context, result?))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let who = Self::attribution(&context);
        let upstream = self.upstream.clone();
        // Captured before the request moves upstream.
        let tool = request.name.to_string();
        let args = request.arguments.clone().map(|args| {
            crate::capture::body(&serde_json::Value::Object(args.into_iter().collect()))
        });

        let started = Instant::now();
        let response = self
            .within_deadline(
                &upstream,
                // The `_once` form, for the same reason `read_resource` and
                // `get_prompt` use it: `call_tool` would drive the MRTR
                // rounds here, against this process's client handler, which
                // has no user to ask. An `input_required` answer belongs to
                // the client downstream — it is the one that can collect the
                // input and retry with `inputResponses` and `requestState`,
                // which this pipe forwards as part of the request.
                self.call_upstream(&upstream, |service| async move {
                    service.call_tool_once(request).await
                }),
            )
            .await;
        let elapsed = started.elapsed();

        self.record(&who, |session| {
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
        // Still paged, unlike `tools/list`: the cursor a pipe hands back came
        // from the one upstream that will be asked for the next page, so it
        // stays meaningful, and nothing is lost by keeping it. Merging is a
        // concession to clients that ignore `nextCursor`, and what they lose
        // by ignoring it is tools — a resource list can run to thousands of
        // entries, and collapsing that into one answer would trade a bug
        // nobody has reported for a reply nobody can hold.
        self.forward(
            &Self::attribution(&context),
            &self.upstream,
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
        self.forward(
            &Self::attribution(&context),
            &self.upstream,
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
        let uri = request.uri.clone();
        // The `_once` form forwards an `input_required` answer downstream
        // instead of trying to satisfy it here: the client on the other side
        // is the one that can ask a human, and a pipe must not swallow a
        // round it cannot complete.
        self.forward(
            &Self::attribution(&context),
            &self.upstream,
            Kind::ResourceRead,
            Some(uri),
            |service| async move { service.read_resource_once(request).await },
            |response| match response {
                ReadResourceResponse::Complete(result) => crate::capture::body(
                    &serde_json::to_value(result).unwrap_or_else(|_| format!("{result:?}").into()),
                ),
                // Untruncated: the capture writer redacts before it cuts.
                other => format!("{other:?}"),
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
        self.forward(
            &Self::attribution(&context),
            &self.upstream,
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
        let name = request.name.clone();
        self.forward(
            &Self::attribution(&context),
            &self.upstream,
            Kind::PromptGet,
            Some(name),
            |service| async move { service.get_prompt_once(request).await },
            |response| match response {
                GetPromptResponse::Complete(result) => crate::capture::body(
                    &serde_json::to_value(result).unwrap_or_else(|_| format!("{result:?}").into()),
                ),
                // Untruncated: the capture writer redacts before it cuts.
                other => format!("{other:?}"),
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
        let argument = request.argument.name.clone();
        self.forward(
            &Self::attribution(&context),
            &self.upstream,
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
///
/// Whole, not truncated: the writer redacts the body before it cuts it, and
/// cutting here would hand it half a credential (see [`crate::capture`]).
fn preview(response: &CallToolResponse) -> String {
    match response {
        CallToolResponse::Complete(result) => {
            serde_json::to_string(result).unwrap_or_else(|_| format!("{result:?}"))
        }
        // Elicitation and task responses carry no result body worth
        // serializing here; their debug form names the shape well enough.
        other => format!("{other:?}"),
    }
}

/// The gateway's own endpoint, served at `/mcp`. It fronts no server and
/// forwards nothing.
///
/// Something has to be there: `doctor` and `daemon status` ask the port
/// whether a gateway is answering, and a client that dials the base can ask
/// it who it is. What it must not be is a second way to reach the servers —
/// one client, one server, one endpoint, and that endpoint is `/s/<name>`.
#[derive(Clone)]
pub struct Base;

/// What the base answers a `tools/call` with. It names the fix rather than
/// the rule: whoever sent it has a client config pointing one path too high,
/// and where to point it instead is the whole of the help there is.
pub const NO_TOOLS_HERE: &str =
    "the gateway serves each server at /s/<name>; point the client at one";

impl ServerHandler for Base {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        // Tools declared and none served. The alternative — declaring
        // nothing — leaves a client with no reason to ask, and then no way
        // to find out that this endpoint holds nothing for it.
        info.capabilities = tools_only();
        info.server_info = Implementation::new("mcpgw", env!("CARGO_PKG_VERSION"));
        info
    }

    // Answered without awaiting anything: this face reaches no upstream, so
    // both replies are ready the moment the request arrives.
    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + Send + '_ {
        // Bridged like any other list: a 2026-07-28 client rejects a result
        // with no `resultType` and no caching fields, empty or not.
        std::future::ready(Ok(bridged(&context, ListToolsResult::default())))
    }

    fn call_tool(
        &self,
        _request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResponse, ErrorData>> + Send + '_ {
        // -32601 is JSON-RPC's own code and means the same thing in both
        // revisions this gateway speaks.
        std::future::ready(Err(ErrorData::new(
            ErrorCode::METHOD_NOT_FOUND,
            NO_TOOLS_HERE,
            None,
        )))
    }
}

/// Serves one server — `gateway`, at `/s/<name>` — next to the base
/// endpoint, on `listener` until `shutdown` resolves. The one-server shape of
/// [`serve_http_with`].
///
/// # Errors
///
/// Returns the underlying I/O error when the HTTP server fails.
pub async fn serve_http(
    name: String,
    gateway: Gateway,
    listener: tokio::net::TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let table = crate::endpoints::EndpointTable::new([(name, gateway)]);
    serve_http_with(crate::endpoints::Endpoints::new(table), listener, shutdown).await
}

/// Serves [`Base`] at `/mcp` and one per-server face under `/s/<name>` for
/// each of `endpoints`. They share the listener, the origin guard and —
/// because every gateway is built over the same [`UpstreamManager`] — the
/// upstream connections.
///
/// # Errors
///
/// Returns the underlying I/O error when the HTTP server fails.
pub async fn serve_http_with(
    endpoints: crate::endpoints::Endpoints,
    listener: tokio::net::TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
    };

    let service = StreamableHttpService::new(
        || Ok(Base),
        // Kept for the clients that still have sessions. 2026-07-28 removed
        // them (SEP-2567) and rmcp serves every request on that revision
        // statelessly whatever this manager says, so it allocates nothing for
        // a current client; a 2025-11-25 client still opens one at
        // `initialize` and needs it to exist. Dropping it would break the
        // older half of the matrix to tidy up the newer half.
        LocalSessionManager::default().into(),
        // The default also leaves rmcp's SEP-2243 checks on: a POST that
        // declares 2026-07-28 must carry `Mcp-Method`, and `Mcp-Name` when
        // the method names a subject, both matching the body. That is
        // validated before a request reaches this handler, so nothing here
        // has to re-derive it.
        StreamableHttpServerConfig::default(),
    );
    // Layered over the merged router, so the guard covers every face.
    let router = axum::Router::new()
        .nest_service("/mcp", service)
        .merge(crate::endpoints::router(endpoints))
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
    use super::{is_local_origin, stopped_merging};

    /// The line is the only trace a capped walk leaves, and it is read by
    /// someone wondering where their tools went.
    #[test]
    fn the_capped_walk_warning_names_the_server_and_the_consequence() {
        let line = stopped_merging("github", "it handed back the cursor \"c\" twice");
        assert!(line.contains("github"), "{line}");
        assert!(line.contains("tools/list"), "{line}");
        assert!(line.contains("cursor"), "{line}");
        assert!(line.contains("partial tool list"), "{line}");
    }

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
