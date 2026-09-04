//! Upstream lifecycle management for the gateway: lazy spawning, exponential
//! backoff with a failure latch, passive health via transport liveness, and
//! explicit status reporting. This layer is mcpgw's answer to the
//! reliability failures of per-session-process gateways: one connection per
//! server, multiplexed, and every state visible — never a silent empty list.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use http::{HeaderName, HeaderValue};
use rmcp::ServiceExt as _;
use rmcp::transport::TokioChildProcess;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use tokio::process::Command;
use tokio::sync::{Mutex, Notify};

use crate::config::{Server, Transport};

pub type UpstreamService = rmcp::service::RunningService<rmcp::RoleClient, UpstreamClient>;

/// Which of an upstream's lists it said had changed.
///
/// One value per `notifications/*/list_changed` the protocol has, and nothing
/// else: the notification carries no payload, so there is nothing to relay
/// but the fact that it happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListChanged {
    Tools,
    Resources,
    Prompts,
}

/// How many list-changed events one upstream keeps for a downstream session
/// that is not draining them.
///
/// Small on purpose. These events carry no data — a client's only reaction is
/// to re-read the list — so a receiver that falls behind has lost nothing but
/// the count, and the answer to overflow is one more notification rather than
/// a bigger buffer. See the `Lagged` arms downstream, which resend instead of
/// giving up.
const CHANGE_BACKLOG: usize = 16;

/// The rmcp client handler on every upstream connection: it exists so that a
/// server's list-changed notifications have somewhere to land.
///
/// It used to be `()`, which meant rmcp decoded each notification and dropped
/// it on the floor — the whole of issue #140. Everything else about a
/// connection is unchanged; this type answers no requests and holds no state
/// beyond the channel it publishes on.
#[derive(Clone, Debug)]
pub struct UpstreamClient {
    /// `None` for a connection nobody is listening to. The probe dials one
    /// per run, asks it what it can do and drops it, so wiring a channel to
    /// it would only give the events a queue to rot in.
    changes: Option<tokio::sync::broadcast::Sender<ListChanged>>,
}

impl UpstreamClient {
    /// A connection whose notifications go nowhere.
    #[must_use]
    pub fn detached() -> Self {
        Self { changes: None }
    }

    /// Publishes one event, if anything is listening. A send with no
    /// receivers fails and that is not an error: an upstream is allowed to
    /// announce a change while no client is connected.
    fn publish(&self, what: ListChanged) {
        if let Some(changes) = &self.changes {
            let _ = changes.send(what);
        }
    }
}

impl rmcp::ClientHandler for UpstreamClient {
    async fn on_tool_list_changed(
        &self,
        _context: rmcp::service::NotificationContext<rmcp::RoleClient>,
    ) {
        self.publish(ListChanged::Tools);
    }

    async fn on_resource_list_changed(
        &self,
        _context: rmcp::service::NotificationContext<rmcp::RoleClient>,
    ) {
        self.publish(ListChanged::Resources);
    }

    async fn on_prompt_list_changed(
        &self,
        _context: rmcp::service::NotificationContext<rmcp::RoleClient>,
    ) {
        self.publish(ListChanged::Prompts);
    }
}

// Explicit cancellation and transport death (child exit) are reported by
// different rmcp signals; either one means the slot is stale. Neither ever
// fires for a streamable-http upstream whose server vanished: rmcp hands the
// failed POST to the one request that made it and keeps its worker running,
// so an http slot can only be found stale by a request failing through it —
// which is what [`UpstreamManager::call`] is for.
fn dead(service: &UpstreamService) -> bool {
    service.is_closed() || service.is_transport_closed()
}

/// Whether `err` means the connection is gone rather than that the server
/// answered "no".
///
/// Only these two are the transport speaking. `McpError` is a JSON-RPC error
/// the server chose to send — an unknown tool, invalid params — and demoting
/// on it would tear down a perfectly healthy connection every time a client
/// mistyped a tool name. A request timeout or a cancellation says nothing
/// about whether the next request would land either, so both are left alone.
fn transport_failure(err: &rmcp::service::ServiceError) -> bool {
    matches!(
        err,
        rmcp::service::ServiceError::TransportSend(_)
            | rmcp::service::ServiceError::TransportClosed
    )
}

/// Whether a failed request was answered `401` by the remote.
///
/// The transport buries it: the 401 becomes an `AuthRequiredError` several
/// links down a `Box<dyn Error>` chain, and the accessor rmcp exposes for the
/// question ([`is_authorization_required`]) is on the *handshake* error only,
/// so a request that fails this way has to be asked by walking the chain.
/// Matching the type rather than the message for the same reason
/// [`connect_once`](UpstreamManager::connect_once) asks rather than matches:
/// the Display of the whole chain is a bare "Auth required".
///
/// A 403 is deliberately not this: rmcp reports it as a distinct
/// `InsufficientScopeError`, and a token that is real but too narrow is not
/// one a fresh run of the same command would widen.
///
/// [`is_authorization_required`]: rmcp::service::ClientInitializeError::is_authorization_required
fn unauthorized(err: &rmcp::service::ServiceError) -> bool {
    let rmcp::service::ServiceError::TransportSend(transport) = err else {
        return false;
    };
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(transport.error.as_ref());
    while let Some(current) = source {
        if current.is::<rmcp::transport::streamable_http_client::AuthRequiredError>() {
            return true;
        }
        source = current.source();
    }
    false
}

/// The `headers_command` of an upstream, or `None` for one whose headers are
/// literal (a stdio server included).
fn headers_command(server: &Server) -> Option<&[String]> {
    match &server.transport {
        Transport::Http {
            headers_command, ..
        } if !headers_command.is_empty() => Some(headers_command),
        Transport::Http { .. } | Transport::Stdio { .. } => None,
    }
}

/// One minute, the window every `calls_per_minute` is a count over.
const WINDOW: Duration = Duration::from_secs(60);

/// What a refused charge tells the caller: the ceiling that refused it, and
/// how long until the next call would be let through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverBudget {
    pub limit: u32,
    pub retry_in: Duration,
}

/// One server's `calls_per_minute` token bucket.
///
/// Held as the theoretical arrival time of the next call rather than as a
/// token count and a refill timestamp, which is the same bucket written
/// down differently (GCRA). It is the form that needs no floating point and
/// accumulates no rounding: a count has to be refilled by `elapsed × rate`
/// on every charge, and a burst of sub-millisecond calls then credits zero
/// each time and starves the bucket, while a deadline only ever moves
/// forward by whole emission intervals.
///
/// `None` is a bucket nothing has been charged against yet, which is a full
/// one — a fresh server owes nobody a wait. That is also why the state lives
/// on [`Upstream`] and not on the gateway pipe: a reload that only edits an
/// entry's metadata keeps the same `Upstream` and so keeps the bucket, and a
/// transport change installs a new one and so starts a new bucket, which is
/// the same rule the live connection underneath follows.
#[derive(Default)]
struct Budget(std::sync::Mutex<Option<Instant>>);

impl Budget {
    /// Charges one call at `now`, returning what stopped it if the bucket
    /// had nothing left.
    ///
    /// `limit` is passed in on every charge rather than stored, so an edit
    /// to `calls_per_minute` applies to the next call rather than to the
    /// next reconnect. A `limit` of 0 is no budget at all and never touches
    /// the state, so turning a budget off and on again does not hand a
    /// client a fresh burst.
    ///
    /// `now` is a parameter for the same reason it is a monotonic
    /// [`Instant`]: the suite has to be able to advance it, and a wall clock
    /// that steps backwards over an NTP correction would hand out a free
    /// burst.
    fn charge(&self, limit: u32, now: Instant) -> Option<OverBudget> {
        if limit == 0 {
            return None;
        }
        // The gap one call is worth, and how far ahead of `now` the deadline
        // may run before the bucket is empty. `limit - 1` because the call
        // being charged is itself the last of the burst.
        let interval = WINDOW / limit;
        let burst = interval.saturating_mul(limit - 1);
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A deadline already in the past is a bucket that sat idle long
        // enough to refill; clamping it to `now` is what stops an hour of
        // quiet from buying back more than the one burst it is worth.
        let deadline = state.unwrap_or(now).max(now);
        let ahead = deadline.duration_since(now);
        if ahead > burst {
            return Some(OverBudget {
                limit,
                retry_in: ahead.saturating_sub(burst),
            });
        }
        *state = Some(deadline + interval);
        None
    }
}

/// Consecutive connect attempts before latching into `Failed`.
pub const ATTEMPTS: u32 = 3;

#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    #[error("unknown upstream {name:?}")]
    Unknown { name: String },

    #[error("upstream {name:?} is disabled")]
    Disabled { name: String },

    #[error("upstream {name:?} failed after {attempts} attempt(s): {message}")]
    Failed {
        name: String,
        attempts: u32,
        message: String,
    },

    /// The upstream answered 401. Its own error rather than a
    /// [`Failed`](Self::Failed) message because the fix is a login on this
    /// machine, not a retry, and every layer above has to be able to say so
    /// without reading error text.
    #[error("upstream {name:?} needs OAuth; run mcpgw auth login {name} on this machine")]
    AuthRequired {
        name: String,
        /// The `resource_metadata` URL from the server's `WWW-Authenticate`
        /// challenge, when it sent one. Reported, not acted on: the broker
        /// asks the server for its own challenge when a login starts rather
        /// than trusting one a gateway recorded at some earlier point.
        resource_metadata: Option<String>,
    },

    /// The upstream's `headers_command` did not produce headers. Its own
    /// error rather than a [`Failed`](Self::Failed) message for the same
    /// reason [`AuthRequired`](Self::AuthRequired) is: the connect ladder has
    /// nothing to offer it — a command that exits non-zero exits non-zero
    /// again half a second later — and the fix is on this machine, not on the
    /// server. `message` is the command's own, which names it and quotes a
    /// tail of its stderr; it never carries what the command printed.
    #[error("upstream {name:?} has no headers: {message}")]
    HeadersCommand { name: String, message: String },

    #[error("upstream {name:?} was shut down while connecting")]
    ShutDown { name: String },
}

/// What one [`UpstreamManager::call`] can fail at: reaching the upstream at
/// all, or the request once it was reached. Kept apart because callers say
/// different things about them — only the first is the gateway being unable
/// to serve, the second is the server's own answer.
#[derive(Debug, thiserror::Error)]
pub enum CallError {
    #[error(transparent)]
    Upstream(#[from] UpstreamError),
    #[error(transparent)]
    Service(#[from] rmcp::service::ServiceError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamStatus {
    /// Not started yet (lazy) or shut down.
    Idle,
    /// A connect ladder is running right now. Reported as its own state
    /// rather than folded into `Idle`: with a 30s timeout per attempt the
    /// ladder can run for a minute and a half, and "starting" is what a
    /// waiting user actually wants to see there.
    Connecting,
    Ready,
    /// Latched after exhausting attempts; the next request gets one chance.
    Failed(String),
    /// The server answered 401. Kept apart from `Failed` because the ladder
    /// is not the answer to it — a 401 will still be a 401 three attempts
    /// later — and because the fix a user is owed names a login, not a
    /// server that is down.
    AuthRequired {
        /// The `resource_metadata` URL from the challenge, if there was one.
        resource_metadata: Option<String>,
    },
}

enum Slot {
    Idle,
    /// A ladder owns this slot. The flag is that ladder's liveness witness:
    /// see [`ConnectGuard`] for why the claim has to be revocable without
    /// taking the lock.
    Connecting(Arc<AtomicBool>),
    Ready(Arc<UpstreamService>),
    Failed(String),
    AuthRequired(Option<String>),
}

impl Slot {
    /// Whether the slot is claimed by a ladder that is still running. An
    /// abandoned `Connecting` reads as `Idle` everywhere: it describes a
    /// future nobody is polling any more.
    fn connecting(&self) -> bool {
        matches!(self, Slot::Connecting(live) if live.load(Ordering::Acquire))
    }
}

struct Upstream {
    /// Swapped rather than owned so a config reload can update an entry's
    /// metadata — `enabled`, tags — without disturbing the live connection
    /// underneath it. The transport half never changes in place: a transport
    /// edit retires the whole upstream and installs a fresh one, because a
    /// running child cannot be re-pointed at a different command.
    server: arc_swap::ArcSwap<Server>,
    /// Set when a reload takes this upstream out of the map. Everything that
    /// could still reach it does so through an [`Arc`] captured before the
    /// swap, and this is how those late callers learn the entry is gone:
    /// without it a `ready()` that looked the name up a moment before the
    /// reload would spawn a child nothing owns any more.
    retired: std::sync::atomic::AtomicBool,
    slot: Mutex<Slot>,
    /// Notified every time the slot leaves `Connecting`, so demands that
    /// arrive mid-ladder can wait for its outcome without holding the lock.
    settled: Notify,
    /// What this upstream said about itself at its last successful connect.
    ///
    /// Kept next to the slot but deliberately outliving it: the downstream
    /// `initialize` that needs it is a synchronous handler which must not
    /// start a connect ladder, so the only capabilities it can report are
    /// remembered ones. Survives a disconnect for the same reason — a server
    /// that had prompts a second ago still has them while it restarts.
    info: arc_swap::ArcSwapOption<rmcp::model::ServerPeerInfo>,
    /// Where this upstream's list-changed notifications are published, and
    /// what every downstream session on it subscribes to.
    ///
    /// Broadcast rather than a per-session channel because one upstream can
    /// be serving several sessions at once and each of them needs the same
    /// event; kept on the entry rather than on the connection so a subscriber
    /// outlives a reconnect. A transport swap carries this sender across to
    /// the replacement entry — see [`UpstreamManager::apply`] — so a client
    /// that subscribed before a reload is still listening after it.
    changes: tokio::sync::broadcast::Sender<ListChanged>,
    /// This server's call budget, metered across every downstream session:
    /// the thing being protected is the upstream, and it cannot tell which
    /// client's loop is hammering it.
    budget: Budget,
}

impl Upstream {
    fn retired(&self) -> bool {
        self.retired.load(Ordering::Acquire)
    }

    /// Takes this upstream out of service: no new ladder may start on it, and
    /// the connection it holds is released.
    ///
    /// Released, not killed. Dropping the manager's `Arc` leaves any request
    /// that already took one holding the service alive; rmcp closes it when
    /// the last handle goes, which is after that request finishes. This is
    /// the whole reload invariant in three lines — a live connection is never
    /// mutated in place, it is unpublished and reaped by refcount — and it is
    /// why a `tools/call` running across a reload still gets its answer from
    /// the server it started on.
    ///
    /// A ladder in flight is not waited for: it finds the slot no longer its
    /// own (and the retired flag set) when it lands, and throws its
    /// connection away rather than installing it.
    async fn retire(&self) {
        self.retired.store(true, Ordering::Release);
        let mut slot = self.slot.lock().await;
        *slot = Slot::Idle;
        drop(slot);
        // Callers parked on a ladder this retirement just disowned: wake them
        // so they re-read the slot, see the flag and get an answer.
        self.settled.notify_waiters();
    }
}

/// Owns the `Connecting` claim for one run of the connect ladder.
///
/// The ladder runs with no lock held, so the only way it can end without a
/// state transition is the `ready()` future being dropped — which a gateway
/// client disconnecting mid-`tools/call` does routinely, with up to ~93s of
/// exposure at the default timeouts. Left alone, the slot would stay
/// `Connecting` for the life of the process: `status()` frozen there and
/// every later demand parked on `settled` with nobody left to wake it.
///
/// Revoking the claim through an atomic rather than the slot lock is what
/// makes this work at all — `Drop` is synchronous and the lock is async, so
/// there is no way to take it here.
struct ConnectGuard<'a> {
    upstream: &'a Upstream,
    live: Arc<AtomicBool>,
}

impl Drop for ConnectGuard<'_> {
    fn drop(&mut self) {
        // A ladder that settled cleared this itself; nothing left to undo.
        if !self.live.swap(false, Ordering::AcqRel) {
            return;
        }
        // Waiters register before they read the slot, so one that registered
        // earlier is woken here and one that arrives later reads a revoked
        // claim and takes the slot over instead of parking. Either way no
        // wakeup is lost.
        self.upstream.settled.notify_waiters();
    }
}

/// What one look at the slot decided, carried out of the lock's scope.
enum Claim {
    /// A live service to hand back.
    Live(Arc<UpstreamService>),
    /// Another caller owns the ladder; wait for its outcome.
    Wait,
    /// This caller now owns the slot and runs a ladder of N attempts.
    Own(u32, Arc<AtomicBool>),
}

/// The server map, keyed by name. Replaced whole on a reload.
type Upstreams = BTreeMap<String, Arc<Upstream>>;

/// What one [`UpstreamManager::apply`] changed. Empty for a reload that
/// found the same servers it was already running.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Changes {
    /// Names the config gained. Lazy: nothing is connected until demanded.
    pub added: Vec<String>,
    /// Names whose transport changed, so the old child was retired and a
    /// fresh upstream took its place.
    pub replaced: Vec<String>,
    /// Names the config lost.
    pub removed: Vec<String>,
    /// Names still configured but flipped to `enabled = false`; their
    /// connection is retired and further demands are refused.
    pub stopped: Vec<String>,
}

impl Changes {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.replaced.is_empty()
            && self.removed.is_empty()
            && self.stopped.is_empty()
    }
}

impl std::fmt::Display for Changes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts = Vec::new();
        for (label, names) in [
            ("added", &self.added),
            ("replaced", &self.replaced),
            ("removed", &self.removed),
            ("stopped", &self.stopped),
        ] {
            if !names.is_empty() {
                parts.push(format!("{label} {}", names.join(", ")));
            }
        }
        if parts.is_empty() {
            return f.write_str("no server changes");
        }
        f.write_str(&parts.join("; "))
    }
}

pub struct UpstreamManager {
    /// Read on every request and replaced whole by a reload, so readers are
    /// lock-free and can never queue behind one. Writers serialize on
    /// `reloading` instead: the swap is a read-modify-write over the old map
    /// and two concurrent ones would lose an entry.
    upstreams: arc_swap::ArcSwap<Upstreams>,
    reloading: Mutex<()>,
    connect_timeout: Duration,
    backoff_base: Duration,
    headers_timeout: Duration,
    /// Where `mcpgw auth login` left its tokens, when this manager is running
    /// somewhere that has a state directory at all.
    ///
    /// [`None`] means no upstream is given a credential — every test manager
    /// that never logs in, and any embedder without a home. It is not the
    /// same as "no tokens": a manager with a state directory and no file for
    /// a server dials that server bare, which is the state the `401` names.
    state_dir: Option<std::path::PathBuf>,
}

fn upstream(server: Server) -> Arc<Upstream> {
    upstream_publishing(server, tokio::sync::broadcast::Sender::new(CHANGE_BACKLOG))
}

/// A fresh entry that publishes on an existing channel: what a transport swap
/// builds, so the sessions already subscribed do not have to be told to
/// resubscribe to something they cannot see.
fn upstream_publishing(
    server: Server,
    changes: tokio::sync::broadcast::Sender<ListChanged>,
) -> Arc<Upstream> {
    Arc::new(Upstream {
        server: arc_swap::ArcSwap::from_pointee(server),
        retired: std::sync::atomic::AtomicBool::new(false),
        slot: Mutex::new(Slot::Idle),
        settled: Notify::new(),
        info: arc_swap::ArcSwapOption::empty(),
        changes,
        budget: Budget::default(),
    })
}

impl UpstreamManager {
    #[must_use]
    pub fn new(servers: BTreeMap<String, Server>) -> Self {
        let upstreams: Upstreams = servers
            .into_iter()
            .map(|(name, server)| (name, upstream(server)))
            .collect();
        Self {
            upstreams: arc_swap::ArcSwap::from_pointee(upstreams),
            reloading: Mutex::new(()),
            connect_timeout: Duration::from_secs(30),
            backoff_base: Duration::from_millis(500),
            headers_timeout: crate::headers::TIMEOUT,
            state_dir: None,
        }
    }

    /// Points the manager at the directory `mcpgw auth login` writes tokens
    /// into, so an upstream that has one connects with it.
    #[must_use]
    pub fn with_state_dir(mut self, state_dir: std::path::PathBuf) -> Self {
        self.state_dir = Some(state_dir);
        self
    }

    #[must_use]
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Test hook: scale the 500ms→1s→2s ladder down for fast suites.
    #[must_use]
    pub fn with_backoff_base(mut self, base: Duration) -> Self {
        self.backoff_base = base;
        self
    }

    /// Test hook: shorten the ceiling a `headers_command` runs under, so a
    /// suite can prove a hanging one is killed without waiting the ten
    /// seconds a real gateway gives it.
    #[must_use]
    pub fn with_headers_timeout(mut self, timeout: Duration) -> Self {
        self.headers_timeout = timeout;
        self
    }

    /// The configured names, in path order. Returns owned strings rather
    /// than borrows because the map behind them can be replaced by a reload
    /// at any moment.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.upstreams.load().keys().cloned().collect()
    }

    /// The entry for `name` as of right now. The [`Arc`] keeps it alive for
    /// the caller even if a reload removes it a microsecond later — see
    /// [`Upstream::retired`] for what that caller is then obliged to check.
    fn get(&self, name: &str) -> Option<Arc<Upstream>> {
        self.upstreams.load().get(name).cloned()
    }

    /// Current status without touching the upstream. The slot lock is never
    /// held across a connect, so this answers immediately even while an
    /// upstream is halfway through its ladder.
    pub async fn status(&self, name: &str) -> Option<UpstreamStatus> {
        let upstream = self.get(name)?;
        let slot = upstream.slot.lock().await;
        Some(match &*slot {
            // A Ready slot whose transport died counts as Idle-with-history;
            // report Ready only while actually alive.
            Slot::Ready(service) if !dead(service) => UpstreamStatus::Ready,
            Slot::Connecting(_) if slot.connecting() => UpstreamStatus::Connecting,
            // An abandoned claim included: nothing is running.
            Slot::Idle | Slot::Ready(_) | Slot::Connecting(_) => UpstreamStatus::Idle,
            Slot::Failed(message) => UpstreamStatus::Failed(message.clone()),
            Slot::AuthRequired(resource_metadata) => UpstreamStatus::AuthRequired {
                resource_metadata: resource_metadata.clone(),
            },
        })
    }

    /// The config entry `name` is served under as of right now.
    ///
    /// Live rather than captured at startup: a reload that changes nothing
    /// about a server's transport stores the new entry here in place, so a
    /// caller reading a per-server setting through this sees the edited one
    /// without anything reconnecting.
    #[must_use]
    pub fn server(&self, name: &str) -> Option<Arc<Server>> {
        Some(self.get(name)?.server.load_full())
    }

    /// Charges one `tools/call` against `name`'s budget, returning what
    /// refused it when the server is over.
    ///
    /// The limit is read here rather than by the caller, and read on every
    /// charge, so a reload that raises or lowers `calls_per_minute` applies
    /// to the next call without anything reconnecting — the same liveness
    /// [`server`](Self::server) gives the tool rules. An unknown name is
    /// unmetered: whatever refuses it, it is not this.
    #[must_use]
    pub fn charge(&self, name: &str) -> Option<OverBudget> {
        let upstream = self.get(name)?;
        let limit = upstream.server.load().calls_per_minute;
        upstream.budget.charge(limit, Instant::now())
    }

    /// What `name` reported at its last successful connect, or `None` if it
    /// has never been reached in this process.
    ///
    /// Synchronous and lock-free on purpose: its one caller is the gateway's
    /// `initialize` handler, which rmcp calls synchronously and which must
    /// never be the thing that starts an upstream.
    #[must_use]
    pub fn last_server_info(&self, name: &str) -> Option<Arc<rmcp::model::ServerPeerInfo>> {
        self.get(name)?.info.load_full()
    }

    /// A stream of `name`'s list-changed notifications, or `None` for a name
    /// this manager does not serve.
    ///
    /// Subscribing does not connect anything: a session can be listening
    /// before the upstream behind it has ever been reached, which is the
    /// ordinary case for a client that dials the gateway at boot. The
    /// subscription survives reconnects and transport swaps; what it does not
    /// survive is the server being removed from the config, which takes its
    /// endpoint with it.
    #[must_use]
    pub fn subscribe(&self, name: &str) -> Option<tokio::sync::broadcast::Receiver<ListChanged>> {
        Some(self.get(name)?.changes.subscribe())
    }

    /// Returns a live service for `name`, spawning it on first demand.
    ///
    /// Concurrent callers still coalesce — exactly one connect cycle runs and
    /// everyone else waits for its outcome — but the slot lock is only ever
    /// held for the state transitions around the ladder, never across it.
    /// The ladder can take three connect timeouts plus its backoff sleeps
    /// (a minute and a half at the defaults), and holding the lock for that
    /// long blocked [`status`](Self::status), [`shutdown`](Self::shutdown)
    /// and every unrelated `tools/call` on the same upstream behind it.
    ///
    /// A `Failed` upstream gets a single fresh attempt per call (no backoff
    /// ladder).
    ///
    /// Cancel-safe: dropping this future mid-ladder (a downstream client
    /// hanging up during `tools/call`) releases the slot instead of wedging
    /// the upstream — see [`ConnectGuard`].
    ///
    /// # Errors
    ///
    /// Returns [`UpstreamError`] for unknown/disabled upstreams, exhausted
    /// connect attempts, and a shutdown that lands mid-connect.
    pub async fn ready(&self, name: &str) -> Result<Arc<UpstreamService>, UpstreamError> {
        let upstream = self.get(name).ok_or_else(|| UpstreamError::Unknown {
            name: name.to_owned(),
        })?;
        let upstream = &*upstream;
        let server = upstream.server.load_full();
        if !server.enabled {
            return Err(UpstreamError::Disabled {
                name: name.to_owned(),
            });
        }

        let (attempts, live) = loop {
            // Registered before the slot is read, so an outcome that lands
            // between the read and the wait below still wakes this caller.
            // `enable` is what registers; dropping an enabled `Notified`
            // that was never awaited simply unregisters it.
            let settled = upstream.settled.notified();
            tokio::pin!(settled);
            settled.as_mut().enable();

            let claim = {
                let mut slot = upstream.slot.lock().await;
                // Checked under the slot lock, which is also what `retire`
                // takes: either this caller claims the slot and `retire`
                // then finds a `Connecting` claim to revoke, or it reads the
                // flag `retire` already set. There is no ordering in which a
                // ladder starts on an upstream nobody owns any more.
                if upstream.retired() {
                    return Err(UpstreamError::ShutDown {
                        name: name.to_owned(),
                    });
                }
                match &*slot {
                    Slot::Ready(service) if !dead(service) => Claim::Live(Arc::clone(service)),
                    // Someone else owns the ladder; wait for its outcome
                    // instead of starting a second one.
                    Slot::Connecting(_) if slot.connecting() => Claim::Wait,
                    // Dead-after-ready is a new crash episode, and an
                    // abandoned claim is a slot nobody owns: full ladder.
                    Slot::Ready(_) | Slot::Idle | Slot::Connecting(_) => {
                        let live = Arc::new(AtomicBool::new(true));
                        *slot = Slot::Connecting(Arc::clone(&live));
                        Claim::Own(ATTEMPTS, live)
                    }
                    // Latched: one fresh chance per demand. A 401 slot is
                    // given the same single attempt — the credential may
                    // have arrived since — but never the ladder.
                    Slot::Failed(_) | Slot::AuthRequired(_) => {
                        let live = Arc::new(AtomicBool::new(true));
                        *slot = Slot::Connecting(Arc::clone(&live));
                        Claim::Own(1, live)
                    }
                }
            };
            match claim {
                Claim::Live(service) => return Ok(service),
                Claim::Wait => settled.await,
                Claim::Own(attempts, live) => break (attempts, live),
            }
        };

        // Armed for the whole ladder: if this future is dropped before the
        // outcome below is published, the guard hands the slot back.
        let guard = ConnectGuard {
            upstream,
            live: Arc::clone(&live),
        };
        let outcome = self
            .connect_with_backoff(name, &server, attempts, &upstream.changes)
            .await;

        let mut slot = upstream.slot.lock().await;
        // A shutdown during the ladder left the slot Idle, and a demand that
        // found the claim revoked may have started a ladder of its own.
        // Either way this connection must not be installed behind their
        // backs — discard it instead. A reload that retired the upstream
        // mid-ladder counts the same way: installing here would leave a
        // child running under an entry no longer in the map.
        let abandoned = upstream.retired()
            || !matches!(&*slot, Slot::Connecting(owner) if Arc::ptr_eq(owner, &live));
        let result = match outcome {
            Ok(service) => {
                let service = Arc::new(service);
                if abandoned {
                    service.cancellation_token().cancel();
                    Err(UpstreamError::ShutDown {
                        name: name.to_owned(),
                    })
                } else {
                    // Snapshot before the slot goes live, so the first
                    // request through it already sees the real capabilities.
                    // A handshake that somehow reported nothing leaves the
                    // previous snapshot alone rather than erasing it.
                    if let Some(info) = service.peer_info() {
                        upstream.info.store(Some(info));
                    }
                    *slot = Slot::Ready(Arc::clone(&service));
                    Ok(service)
                }
            }
            Err(err) => {
                if !abandoned {
                    *slot = match &err {
                        UpstreamError::AuthRequired {
                            resource_metadata, ..
                        } => Slot::AuthRequired(resource_metadata.clone()),
                        _ => Slot::Failed(err.to_string()),
                    };
                }
                Err(err)
            }
        };
        drop(slot);
        // The ladder is over and its outcome is published, so the guard has
        // nothing left to undo: clearing the flag here is what disarms it.
        live.store(false, Ordering::Release);
        drop(guard);
        upstream.settled.notify_waiters();
        result
    }

    /// Runs one request against `name`, connecting it first if needed, and
    /// demotes the slot when the failure is the transport rather than the
    /// server.
    ///
    /// This is the only path a request may take to an upstream. Passive
    /// liveness alone cannot see an http server that went away — see
    /// [`dead`] — so the request that fails is the one signal there is, and
    /// it has to be acted on in exactly one place: the manager owns slot
    /// state, and a copy of this per request family would be a copy that
    /// drifts.
    ///
    /// # Errors
    ///
    /// Returns [`CallError::Upstream`] when the upstream could not be
    /// reached at all and [`CallError::Service`] when the request itself
    /// failed.
    pub async fn call<T, F>(
        &self,
        name: &str,
        call: impl FnOnce(Arc<UpstreamService>) -> F,
    ) -> Result<T, CallError>
    where
        F: Future<Output = Result<T, rmcp::service::ServiceError>>,
    {
        let service = self.ready(name).await?;
        match call(Arc::clone(&service)).await {
            Ok(value) => Ok(value),
            Err(err) => {
                // A service whose own liveness already reports the death —
                // a stdio child that exited — needs nothing from here: the
                // slot reads that as Idle-with-history and the next demand
                // gets the full backoff ladder, which is what a crashed
                // child has always been given. Demotion is for the failure
                // rmcp cannot see at all: a transport that still looks alive
                // because its worker never noticed the far end leave.
                //
                // Ahead of both: a 401 against an upstream whose headers come
                // from a command is neither. The connection is healthy and
                // the credential it was opened with is not, so the slot is
                // released rather than demoted — the next demand then rebuilds
                // the connection, which reruns the command. Falling through to
                // `demote` would have latched `Failed` over a server that is
                // working and made the next demand spend its one attempt on
                // the same expired token.
                if unauthorized(&err) && self.expects_rotation(name) {
                    self.release(name, &service).await;
                } else if transport_failure(&err) && !dead(&service) {
                    self.demote(name, &service, &err).await;
                }
                Err(err.into())
            }
        }
    }

    /// Takes a connection that just failed at the transport out of its slot,
    /// so the next demand runs the ladder and [`status`](Self::status) stops
    /// claiming `Ready`.
    ///
    /// Released rather than killed, exactly as in [`Upstream::retire`]: other
    /// requests may still be in flight on this service, and rmcp closes it
    /// when the last of them drops its handle. The identity check is what
    /// makes concurrent failures safe — the first one demotes, and a second
    /// arriving after some other caller already reconnected finds a slot that
    /// is no longer the one it was talking to and leaves it alone.
    async fn demote(
        &self,
        name: &str,
        stale: &Arc<UpstreamService>,
        err: &rmcp::service::ServiceError,
    ) {
        let Some(upstream) = self.get(name) else {
            return;
        };
        let mut slot = upstream.slot.lock().await;
        if matches!(&*slot, Slot::Ready(live) if Arc::ptr_eq(live, stale)) {
            *slot = Slot::Failed(format!("transport failure: {err}"));
        }
    }

    /// Whether `name` gets its headers from a command, and so has a
    /// credential that can go stale under a live connection.
    fn expects_rotation(&self, name: &str) -> bool {
        self.get(name)
            .is_some_and(|upstream| headers_command(&upstream.server.load()).is_some())
    }

    /// Puts a connection whose credential expired back to `Idle`, so the next
    /// demand builds a new one with a fresh run of the command.
    ///
    /// `Idle` rather than the `Failed` [`demote`](Self::demote) writes: the
    /// server is not down and this is not a failure to report, so the next
    /// demand is owed the full ladder rather than the single latched attempt.
    /// The identity check is the same one, and there for the same reason —
    /// two requests failing at once must not undo a reconnection the first
    /// one already made.
    async fn release(&self, name: &str, stale: &Arc<UpstreamService>) {
        let Some(upstream) = self.get(name) else {
            return;
        };
        let mut slot = upstream.slot.lock().await;
        if matches!(&*slot, Slot::Ready(live) if Arc::ptr_eq(live, stale)) {
            *slot = Slot::Idle;
        }
    }

    /// Makes `servers` the live set, keeping every upstream that did not
    /// change — same entry, same slot, same child process.
    ///
    /// The rules, in the order a reload cares about them:
    /// - an entry whose [`Server`] is byte-identical is not touched at all;
    /// - one whose metadata changed but whose transport did not keeps its
    ///   connection and gets the new [`Server`] swapped in, because
    ///   `enabled` and tags say nothing about the process that is running;
    /// - one whose transport changed is retired and replaced by a fresh,
    ///   unconnected entry: a running child cannot be re-pointed;
    /// - one that vanished, or that just went `enabled = false`, is retired.
    ///
    /// Retiring never kills a connection out from under a request — see
    /// [`Upstream::retire`].
    pub async fn apply(&self, servers: BTreeMap<String, Server>) -> Changes {
        // One reload at a time: the map swap below is a read-modify-write and
        // two of them racing would drop whichever entries the loser added.
        let _reloading = self.reloading.lock().await;

        let current = self.upstreams.load_full();
        let mut next = Upstreams::new();
        let mut changes = Changes::default();
        let mut retire = Vec::new();
        // The channels of the entries a transport swap replaced. Whatever the
        // new connection lists first is a different list by definition — a
        // different command, or a different URL — so every session already on
        // this name is owed the news, and the sender it is subscribed to is
        // the one carried into the replacement.
        let mut announce = Vec::new();

        for (name, server) in servers {
            let Some(existing) = current.get(&name) else {
                changes.added.push(name.clone());
                next.insert(name, upstream(server));
                continue;
            };
            let old = existing.server.load_full();
            if *old == server {
                next.insert(name, Arc::clone(existing));
                continue;
            }
            if old.transport == server.transport {
                let stopped = old.enabled && !server.enabled;
                existing.server.store(Arc::new(server));
                if stopped {
                    changes.stopped.push(name.clone());
                    retire.push(Arc::clone(existing));
                }
                next.insert(name, Arc::clone(existing));
                continue;
            }
            changes.replaced.push(name.clone());
            retire.push(Arc::clone(existing));
            let published = existing.changes.clone();
            announce.push(published.clone());
            next.insert(name, upstream_publishing(server, published));
        }
        for (name, existing) in current.iter() {
            if !next.contains_key(name) {
                changes.removed.push(name.clone());
                retire.push(Arc::clone(existing));
            }
        }

        // Published before anything is retired, so the window in which a
        // demand could reach a doomed entry through the map is closed first.
        self.upstreams.store(Arc::new(next));
        for upstream in retire {
            upstream.retire().await;
        }
        // Announced after the swap, so a client that re-reads a list on the
        // news reaches the entry that replaced the one it was told about.
        // Every family, because the pipe cannot know which of them the new
        // transport serves differently — and each session filters this down
        // to what it was actually promised at `initialize`.
        for published in announce {
            for what in [
                ListChanged::Tools,
                ListChanged::Resources,
                ListChanged::Prompts,
            ] {
                let _ = published.send(what);
            }
        }
        changes
    }

    /// Stops every running upstream (children die with their transports).
    /// Never waits for a connect in flight: that ladder finds the slot no
    /// longer its own when it lands and throws its connection away.
    pub async fn shutdown(&self) {
        let upstreams = self.upstreams.load_full();
        for upstream in upstreams.values() {
            let mut slot = upstream.slot.lock().await;
            if let Slot::Ready(service) = &*slot {
                service.cancellation_token().cancel();
            }
            *slot = Slot::Idle;
            drop(slot);
            // Backstop for callers already parked on a ladder this shutdown
            // just disowned: they wake, find the slot Idle and get an answer
            // of their own instead of waiting for a settle that never comes.
            upstream.settled.notify_waiters();
        }
    }

    async fn connect_with_backoff(
        &self,
        name: &str,
        server: &Server,
        attempts: u32,
        changes: &tokio::sync::broadcast::Sender<ListChanged>,
    ) -> Result<UpstreamService, UpstreamError> {
        let mut last = String::new();
        for attempt in 0..attempts {
            if attempt > 0 {
                // 500ms → 1s → 2s with the default base.
                tokio::time::sleep(self.backoff_base * 2u32.pow(attempt - 1)).await;
            }
            // Boxed: this future carries a whole handshake — two of them for
            // a server that turns out to speak the newer lifecycle — and it
            // is reached from every downstream request handler, whose frames
            // would all have to be big enough to hold it. One allocation per
            // connect attempt buys that back.
            match Box::pin(self.connect_attempt(name, server, changes)).await {
                Ok(service) => return Ok(service),
                // Retrying a 401 only asks the same question again, more
                // slowly: the credential this connection lacks cannot appear
                // between two attempts of one ladder. A `headers_command`
                // upstream has already had the one extra attempt that could
                // change the answer — see `connect_attempt`.
                //
                // A `headers_command` that failed is the same shape of
                // answer: it fails again on the next rung, and its message
                // already says what to fix.
                Err(
                    err @ (UpstreamError::AuthRequired { .. }
                    | UpstreamError::HeadersCommand { .. }),
                ) => return Err(err),
                Err(err) => last = err.to_string(),
            }
        }
        Err(UpstreamError::Failed {
            name: name.to_owned(),
            attempts,
            message: last,
        })
    }

    /// One rung of the ladder, plus the single retry a rotating credential
    /// needs.
    ///
    /// A `headers_command` upstream that is answered `401` gets exactly one
    /// more connect, which runs the command again. That is the whole of how
    /// a token that expired since the last connect is replaced without a
    /// restart: the first attempt presents what the command last minted, the
    /// server refuses it, and the second presents whatever the command says
    /// now. Nothing else is retried here — for a literal credential the
    /// second answer is the first one — and the retry happens once, so a
    /// server that is simply protected cannot turn one demand into a loop.
    async fn connect_attempt(
        &self,
        name: &str,
        server: &Server,
        changes: &tokio::sync::broadcast::Sender<ListChanged>,
    ) -> Result<UpstreamService, UpstreamError> {
        match self.connect_once(name, server, changes).await {
            Err(UpstreamError::AuthRequired { .. }) if headers_command(server).is_some() => {
                self.connect_once(name, server, changes).await
            }
            outcome => outcome,
        }
    }

    async fn connect_once(
        &self,
        name: &str,
        server: &Server,
        changes: &tokio::sync::broadcast::Sender<ListChanged>,
    ) -> Result<UpstreamService, UpstreamError> {
        // Every rung of the ladder dials with the same handler, so a
        // connection that replaces a dead one publishes to the receivers the
        // dead one had.
        let client = UpstreamClient {
            changes: Some(changes.clone()),
        };
        let failed = |message: String| UpstreamError::Failed {
            name: name.to_owned(),
            attempts: 1,
            message,
        };
        match &server.transport {
            Transport::Stdio { command, args, env } => {
                stdio_ladder(command, args, env, Some(self.connect_timeout), client)
                    .await
                    .map_err(|err| match err {
                        LadderError::Transport(err) => failed(format!("spawn failed: {err}")),
                        LadderError::Dial(err) => failed(err.message),
                    })
            }
            Transport::Http {
                url,
                headers_command,
                headers,
                ..
            } => {
                self.connect_http(name, url, headers_command, headers, client)
                    .await
            }
        }
    }

    /// The http half of [`connect_once`](Self::connect_once), lifted out so
    /// each transport's ladder reads on its own.
    async fn connect_http(
        &self,
        name: &str,
        url: &str,
        headers_command: &[String],
        headers: &std::collections::BTreeMap<String, String>,
        client: UpstreamClient,
    ) -> Result<UpstreamService, UpstreamError> {
        let failed = |message: String| UpstreamError::Failed {
            name: name.to_owned(),
            attempts: 1,
            message,
        };
        // Run before anything is dialed, and run again on every connect: the
        // whole point of the field is that what it prints last time is not
        // what it prints now.
        let resolved;
        let headers = if headers_command.is_empty() {
            headers
        } else {
            resolved = crate::headers::resolve(headers_command, headers, self.headers_timeout)
                .await
                .map_err(|err| UpstreamError::HeadersCommand {
                    name: name.to_owned(),
                    message: err.to_string(),
                })?;
            &resolved
        };
        let config = http_config(url, headers).map_err(failed)?;
        // The stored login, when there is one. Built per connect rather than
        // kept on the manager: it holds the discovered authorization-server
        // metadata, and a gateway that runs for weeks must not be pinning a
        // provider's endpoints from the first time it saw them.
        let credentials = match &self.state_dir {
            Some(state_dir) => crate::auth::client(state_dir, name, url)
                .await
                .map_err(|err| failed(err.to_string()))?,
            None => None,
        };
        // Nothing is dialed until the handshake, so an unreachable host
        // surfaces here as a normal connect failure and feeds the same backoff
        // ladder as a stdio spawn failure.
        let outcome = http_ladder(&config, credentials, Some(self.connect_timeout), client).await;
        outcome.map_err(|err| {
            if err.auth_required {
                // The token file is deliberately left alone. A 401 that
                // survived rmcp's own refresh-and-retry says the login has to
                // be redone, and deleting the evidence would leave `auth
                // status` unable to tell a login that expired from one that
                // never happened — which are two different sentences to a
                // user.
                UpstreamError::AuthRequired {
                    name: name.to_owned(),
                    resource_metadata: err.resource_metadata,
                }
            } else {
                failed(err.message)
            }
        })
    }
}

/// The `resource_metadata` URL out of a `WWW-Authenticate` challenge.
///
/// Parsed here rather than through rmcp's own `WWWAuthenticateParams`
/// because that type lives behind the `auth` feature, which this workspace
/// does not enable — and turning it on to read one quoted parameter would
/// pull a whole OAuth client in.
fn resource_metadata(challenge: &str) -> Option<String> {
    let at = challenge.find("resource_metadata=")? + "resource_metadata=".len();
    let rest = challenge[at..].trim_start();
    let value = match rest.strip_prefix('"') {
        Some(quoted) => quoted.split('"').next()?,
        // Unquoted is not what RFC 9728 shows, but a parameter list ends at
        // the comma either way.
        None => rest.split(',').next()?.trim(),
    };
    (!value.is_empty()).then(|| value.to_owned())
}

/// Which MCP lifecycle a handshake attempt uses.
#[derive(Clone, Copy)]
pub(crate) enum Lifecycle {
    /// `initialize` plus `notifications/initialized` — every revision up to
    /// and including 2025-11-25.
    Legacy,
    /// `server/discover` plus self-contained per-request `_meta` — 2026-07-28
    /// (SEP-2575), which has no handshake at all.
    Modern,
}

/// A failed handshake, classified into the two things a caller can act on.
pub(crate) struct DialError {
    pub(crate) message: String,
    /// The server answered the handshake with a JSON-RPC error, so it is
    /// reachable and speaking — a 2026-07-28-only server refusing a method it
    /// no longer has. A transport error, a timeout or a closed connection
    /// says nothing of the sort and must not cost a second attempt.
    pub(crate) refused_initialize: bool,
    /// The server answered 401. Kept apart from the message because the fix
    /// is a login on this machine, not a retry, and every layer above has to
    /// be able to say so without reading error text.
    pub(crate) auth_required: bool,
    /// The `resource_metadata` URL from that 401's `WWW-Authenticate`
    /// challenge, when it sent one.
    pub(crate) resource_metadata: Option<String>,
    /// That challenge, whole. The broker hands it back to rmcp, which seeds
    /// discovery from it — the provider's own `resource_metadata` URL and its
    /// scope hint — instead of guessing the well-known path. Kept apart from
    /// `resource_metadata` because the two have different readers: the
    /// gateway records one URL for a report, the broker replays the header.
    pub(crate) challenge: Option<String>,
}

/// One handshake against `transport` on `lifecycle`, under `timeout` when
/// the caller has one. `None` is for a caller that already races the whole
/// connection against a deadline of its own.
///
/// The order the caller uses is legacy first, modern only if the server
/// answered an error, rather than rmcp's own `Auto` mode, which probes with
/// `server/discover` and waits ten seconds for a server that simply ignores
/// methods it does not know. Most upstreams are on the handshake to this day,
/// and none of them should pay that wait on every connect.
pub(crate) async fn dial<T, E, A>(
    transport: T,
    lifecycle: Lifecycle,
    timeout: Option<Duration>,
    client: UpstreamClient,
) -> Result<UpstreamService, DialError>
where
    T: rmcp::transport::IntoTransport<rmcp::RoleClient, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    use rmcp::ClientServiceExt as _;

    let handshake = async {
        match lifecycle {
            Lifecycle::Legacy => client.serve(transport).await,
            Lifecycle::Modern => {
                client
                    .serve_with_lifecycle(
                        transport,
                        rmcp::ClientLifecycleMode::Discover {
                            preferred_versions: vec![rmcp::model::ProtocolVersion::V_2026_07_28],
                        },
                    )
                    .await
            }
        }
    };
    let outcome = match timeout {
        Some(timeout) => tokio::time::timeout(timeout, handshake)
            .await
            .map_err(|_| format!("no handshake within {timeout:?}")),
        None => Ok(handshake.await),
    };
    match outcome {
        Ok(Ok(service)) => Ok(service),
        Ok(Err(err)) => Err(DialError {
            refused_initialize: matches!(
                (lifecycle, &err),
                (
                    Lifecycle::Legacy,
                    rmcp::service::ClientInitializeError::JsonRpcError(_)
                )
            ),
            // Asked of the error rather than matched on its text: rmcp buries
            // the 401 in a `Box<dyn Error>` chain whose Display is a bare
            // "Auth required", and this is the accessor it exposes for exactly
            // this question. A 403 (a token that is real but too narrow) is
            // deliberately not this state — nothing on this machine minted
            // that token, so there is nothing here to widen.
            auth_required: err.is_authorization_required(),
            resource_metadata: err.auth_challenge().and_then(resource_metadata),
            challenge: err.auth_challenge().map(str::to_owned),
            message: format!("handshake failed: {err}"),
        }),
        Err(message) => Err(DialError {
            message,
            refused_initialize: false,
            auth_required: false,
            resource_metadata: None,
            challenge: None,
        }),
    }
}

/// A ladder that ended before a handshake did, kept apart from one that
/// ended in a refusal: a transport that could not be built at all is a
/// failure on this machine, and every caller words it differently.
pub(crate) enum LadderError<T> {
    /// The transport could never be built — a stdio child that would not
    /// spawn.
    Transport(T),
    /// A handshake was attempted and failed.
    Dial(DialError),
}

impl LadderError<std::convert::Infallible> {
    /// The dial half, for a ladder whose transport factory cannot fail.
    fn into_dial(self) -> DialError {
        match self {
            Self::Transport(never) => match never {},
            Self::Dial(err) => err,
        }
    }
}

/// The connect ladder: one handshake on the legacy lifecycle, and a second
/// on the modern one only if the server answered the first with an error.
///
/// `transport` is called once per rung rather than handed in built, because
/// a rung consumes what it dials: a stdio child that failed the handshake
/// still owns its pipes, and an http transport keeps the session id the
/// first attempt was given.
///
/// This is the single ladder both the gateway's upstream manager and
/// `doctor --probe` climb. `--probe`'s contract is that a server answers
/// *the way mcpgw would reach it*, which only holds while there is one
/// ladder to change.
async fn dial_ladder<T, E, A, X>(
    mut transport: impl FnMut() -> Result<T, X>,
    timeout: Option<Duration>,
    client: UpstreamClient,
) -> Result<UpstreamService, LadderError<X>>
where
    T: rmcp::transport::IntoTransport<rmcp::RoleClient, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    let first = transport().map_err(LadderError::Transport)?;
    match dial(first, Lifecycle::Legacy, timeout, client.clone()).await {
        // A 401 is not "this server has no `initialize`": it is a server that
        // will not answer anything without a credential, so asking it again
        // over the other lifecycle only spends a second round trip on the
        // same refusal. `refused_initialize` is false for it for that reason.
        Err(err) if err.refused_initialize => {
            let second = transport().map_err(LadderError::Transport)?;
            dial(second, Lifecycle::Modern, timeout, client).await
        }
        other => other,
    }
    .map_err(LadderError::Dial)
}

/// The stdio rung of the ladder: spawn `command` and hand its pipes to a
/// handshake, once per lifecycle.
pub(crate) async fn stdio_ladder(
    command: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
    timeout: Option<Duration>,
    client: UpstreamClient,
) -> Result<UpstreamService, LadderError<std::io::Error>> {
    dial_ladder(
        || {
            let mut cmd = Command::new(command);
            cmd.args(args);
            for (key, value) in env {
                cmd.env(key, value);
            }
            // Server logs on stderr are noise to both callers: the gateway
            // reports status, doctor reports outcomes.
            cmd.stderr(std::process::Stdio::null());
            // Orphan prevention (learned the hard way in the probe
            // milestone): a dropped ladder must take its child with it.
            cmd.kill_on_drop(true);
            TokioChildProcess::new(cmd)
        },
        timeout,
        client,
    )
    .await
}

/// The http rung of the ladder, dialed with `credentials` when the caller
/// has a stored login for the server and bare otherwise.
///
/// The two branches are spelled out rather than shared behind a trait
/// object: rmcp's transport is generic over its client, and the authorized
/// one is a different type, not a configured one.
pub(crate) async fn http_ladder(
    config: &StreamableHttpClientTransportConfig,
    credentials: Option<rmcp::transport::auth::AuthClient<reqwest::Client>>,
    timeout: Option<Duration>,
    client: UpstreamClient,
) -> Result<UpstreamService, DialError> {
    if let Some(auth) = credentials {
        let auth = ForwardsParams(auth);
        dial_ladder(
            || {
                Ok::<_, std::convert::Infallible>(
                    rmcp::transport::StreamableHttpClientTransport::with_client(
                        auth.clone(),
                        config.clone(),
                    ),
                )
            },
            timeout,
            client,
        )
        .await
    } else {
        // `from_config` would build rmcp's own reqwest client and give no way
        // to wrap it, so the client is built here — see [`http_client`] for
        // the settings that have to be kept in step with it.
        let plain = ForwardsParams(http_client());
        dial_ladder(
            || {
                Ok::<_, std::convert::Infallible>(
                    rmcp::transport::StreamableHttpClientTransport::with_client(
                        plain.clone(),
                        config.clone(),
                    ),
                )
            },
            timeout,
            client,
        )
        .await
    }
    .map_err(LadderError::into_dial)
}

/// Builds the streamable-http client config for `url`, passing the canonical
/// `headers` (auth tokens and friends) on every request.
pub(crate) fn http_config(
    url: &str,
    headers: &BTreeMap<String, String>,
) -> Result<StreamableHttpClientTransportConfig, String> {
    let mut custom = std::collections::HashMap::with_capacity(headers.len());
    for (key, value) in headers {
        let name = HeaderName::try_from(key).map_err(|_| format!("invalid header name {key:?}"))?;
        let value = HeaderValue::try_from(value)
            .map_err(|_| format!("invalid value for header {key:?}"))?;
        custom.insert(name, value);
    }
    Ok(
        StreamableHttpClientTransportConfig::with_uri(url.to_owned())
            .custom_headers(custom)
            // Set rather than inherited: a remote that restarts keeps its port
            // but forgets our session, and this is what turns the 404 that
            // follows into a transparent re-handshake instead of a failed
            // request. It is the fast path in front of the connect ladder, so a
            // change to the dependency's default must not be able to remove it
            // quietly.
            .reinit_on_expired_session(true),
    )
}

/// The `Mcp-Param-*` headers one downstream request arrived with, on their
/// way to the upstream POST that answers it.
///
/// SEP-2243 mirrors the `tools/call` arguments a server annotated with
/// `x-mcp-header` into `Mcp-Param-{Name}` headers, and 2026-07-28 makes
/// forwarding them a MUST for an intermediary that does not recognise them —
/// a gateway that ate them would make an upstream behave differently through
/// mcpgw than it does direct. Nothing else the client sent travels:
/// `Authorization` and `Mcp-Session-Id` belong to the hop between the client
/// and this gateway, and `Mcp-Method`/`Mcp-Name` are rmcp's to derive from
/// the message it is actually sending.
///
/// These ride the outgoing request's rmcp extensions, which are process-local
/// and never serialized. A stdio upstream therefore drops them on its own:
/// its transport writes JSON to a pipe and has no headers to put them in,
/// which is the whole of the right answer there.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParamHeaders(Vec<(HeaderName, HeaderValue)>);

impl ParamHeaders {
    /// Every `Mcp-Param-*` header in `headers`, or `None` when there is none.
    ///
    /// The prefix is matched case-insensitively because RFC 9110 field names
    /// are: [`HeaderName`] has already lowercased what came off the wire, and
    /// the constant it is compared against is not lowercase.
    #[must_use]
    pub fn collect(headers: &http::HeaderMap) -> Option<Self> {
        const PREFIX: &str = rmcp::transport::common::http_header::HEADER_MCP_PARAM_PREFIX;

        let forwarded: Vec<(HeaderName, HeaderValue)> = headers
            .iter()
            .filter(|(name, _)| {
                name.as_str()
                    .get(..PREFIX.len())
                    .is_some_and(|head| head.eq_ignore_ascii_case(PREFIX))
            })
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        (!forwarded.is_empty()).then_some(Self(forwarded))
    }
}

/// An HTTP client that adds the [`ParamHeaders`] carried by the message it is
/// sending to that one POST.
///
/// The mechanism exists because rmcp's client has no per-request header hook:
/// `custom_headers` is transport-wide and fixed when the connection is built,
/// and one connection serves every downstream client of an upstream. The
/// message is the only thing that travels from the caller to the POST, so the
/// pipe attaches the headers to the request's extensions (see
/// [`ParamHeaders`]) and they are read back off it here, one layer above the
/// socket.
///
/// Headers already present win, and that is the spec's rule rather than an
/// implementation detail: rmcp builds `Mcp-Param-*` itself for arguments the
/// upstream's own schema annotated, and only the ones it did *not* recognise
/// are the ones an intermediary forwards.
#[derive(Clone)]
pub(crate) struct ForwardsParams<C>(C);

/// `base` plus whatever `message` is carrying that it does not already have.
fn with_params(
    message: &rmcp::model::ClientJsonRpcMessage,
    mut base: std::collections::HashMap<HeaderName, HeaderValue>,
) -> std::collections::HashMap<HeaderName, HeaderValue> {
    use rmcp::model::GetExtensions as _;

    let rmcp::model::ClientJsonRpcMessage::Request(request) = message else {
        return base;
    };
    let Some(ParamHeaders(forwarded)) = request.request.extensions().get::<ParamHeaders>() else {
        return base;
    };
    for (name, value) in forwarded {
        base.entry(name.clone()).or_insert_with(|| value.clone());
    }
    base
}

impl<C> rmcp::transport::streamable_http_client::StreamableHttpClient for ForwardsParams<C>
where
    C: rmcp::transport::streamable_http_client::StreamableHttpClient,
{
    type Error = C::Error;

    fn post_message(
        &self,
        uri: Arc<str>,
        message: rmcp::model::ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: std::collections::HashMap<HeaderName, HeaderValue>,
    ) -> impl Future<
        Output = Result<
            rmcp::transport::streamable_http_client::StreamableHttpPostResponse,
            rmcp::transport::streamable_http_client::StreamableHttpError<Self::Error>,
        >,
    > + Send
    + '_ {
        let custom_headers = with_params(&message, custom_headers);
        self.0
            .post_message(uri, message, session_id, auth_header, custom_headers)
    }

    // Overridden as well as `post_message`: the worker calls this one, and
    // the trait's default would route it back through `post_message` on the
    // *inner* client, dropping whatever SSE size limit that client enforces.
    fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: rmcp::model::ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: std::collections::HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> impl Future<
        Output = Result<
            rmcp::transport::streamable_http_client::StreamableHttpPostResponse,
            rmcp::transport::streamable_http_client::StreamableHttpError<Self::Error>,
        >,
    > + Send
    + '_ {
        let custom_headers = with_params(&message, custom_headers);
        self.0.post_message_with_max_sse_event_size(
            uri,
            message,
            session_id,
            auth_header,
            custom_headers,
            max_sse_event_size,
        )
    }

    // The rest carry no request of their own — a session teardown and a
    // stream open — so there is nothing per-request to forward on them.
    fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: std::collections::HashMap<HeaderName, HeaderValue>,
    ) -> impl Future<
        Output = Result<
            (),
            rmcp::transport::streamable_http_client::StreamableHttpError<Self::Error>,
        >,
    > + Send
    + '_ {
        self.0
            .delete_session(uri, session_id, auth_header, custom_headers)
    }

    fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: std::collections::HashMap<HeaderName, HeaderValue>,
    ) -> impl Future<
        Output = Result<
            rmcp::transport::common::client_side_sse::BoxedSseResponse,
            rmcp::transport::streamable_http_client::StreamableHttpError<Self::Error>,
        >,
    > + Send
    + '_ {
        self.0
            .get_stream(uri, session_id, last_event_id, auth_header, custom_headers)
    }

    fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: std::collections::HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> impl Future<
        Output = Result<
            rmcp::transport::common::client_side_sse::BoxedSseResponse,
            rmcp::transport::streamable_http_client::StreamableHttpError<Self::Error>,
        >,
    > + Send
    + '_ {
        self.0.get_stream_with_max_sse_event_size(
            uri,
            session_id,
            last_event_id,
            auth_header,
            custom_headers,
            max_sse_event_size,
        )
    }
}

/// The client every unauthenticated http dial goes through — the gateway's
/// and `doctor --probe`'s alike.
///
/// A copy of rmcp's own default, which is private: idle pooling off (a reused
/// connection whose previous body was not drained stalls ~40ms on Linux's
/// delayed ACK) and redirects off (so custom headers cannot be replayed to a
/// redirect target). Both are behaviour, not taste — if rmcp changes them,
/// this has to follow, and this is the one place that has to be changed.
pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("failed to build the default http client")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

    use super::{Budget, http_config};

    /// A burst of `limit` gets through and the next call does not, and the
    /// wait it is told to take is exactly one emission interval.
    #[test]
    fn a_burst_of_the_limit_passes_and_the_next_one_does_not() {
        let budget = Budget::default();
        let now = Instant::now();
        for call in 0..2 {
            assert!(budget.charge(2, now).is_none(), "call {call} was refused");
        }
        let over = budget.charge(2, now).expect("the third call is over");
        assert_eq!(over.limit, 2);
        assert_eq!(over.retry_in, Duration::from_secs(30));
    }

    /// One call's worth of room comes back after 60/N seconds, and not
    /// before.
    #[test]
    fn a_refused_call_is_let_through_once_the_bucket_refills() {
        let budget = Budget::default();
        let now = Instant::now();
        for _ in 0..2 {
            assert!(budget.charge(2, now).is_none());
        }
        assert!(budget.charge(2, now + Duration::from_secs(29)).is_some());
        assert!(budget.charge(2, now + Duration::from_secs(30)).is_none());
        // And only one call's worth: the window did not reopen wholesale.
        assert!(budget.charge(2, now + Duration::from_secs(30)).is_some());
    }

    /// An idle server refills to a full burst and no further, however long
    /// it sat there.
    #[test]
    fn a_long_idle_period_buys_back_one_burst_and_no_more() {
        let budget = Budget::default();
        let now = Instant::now();
        assert!(budget.charge(2, now).is_none());
        let later = now + Duration::from_secs(3600);
        assert!(budget.charge(2, later).is_none());
        assert!(budget.charge(2, later).is_none());
        assert!(budget.charge(2, later).is_some());
    }

    /// The limit is the caller's on every charge, so a reload that raises it
    /// is felt on the very next call: the same emptied bucket now refills
    /// sixty times faster, and the wait shrinks to match.
    #[test]
    fn a_raised_limit_applies_to_the_next_call() {
        let budget = Budget::default();
        let now = Instant::now();
        for _ in 0..2 {
            assert!(budget.charge(2, now).is_none());
        }
        assert_eq!(
            budget.charge(2, now).map(|over| over.retry_in),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            budget.charge(60, now).map(|over| over.retry_in),
            Some(Duration::from_secs(1))
        );
        assert!(budget.charge(60, now + Duration::from_secs(1)).is_none());
    }

    /// No budget means no metering, and it does not spend the bucket either:
    /// turning one off and back on is not a way to buy a fresh burst.
    #[test]
    fn a_limit_of_zero_meters_nothing_and_spends_nothing() {
        let budget = Budget::default();
        let now = Instant::now();
        for _ in 0..50 {
            assert!(budget.charge(0, now).is_none());
        }
        for _ in 0..2 {
            assert!(budget.charge(2, now).is_none());
        }
        assert!(budget.charge(2, now).is_some());
    }

    #[test]
    fn headers_land_in_the_transport_config() {
        let headers = [("Authorization".to_owned(), "Bearer t0ken".to_owned())]
            .into_iter()
            .collect();
        let config = http_config("https://mcp.example.com/mcp", &headers).unwrap();
        assert_eq!(&*config.uri, "https://mcp.example.com/mcp");
        let name = http::HeaderName::from_static("authorization");
        assert_eq!(config.custom_headers[&name], "Bearer t0ken");
    }

    /// The recovery a restarted remote depends on, pinned against a change
    /// of heart in the transport's defaults.
    #[test]
    fn expired_sessions_are_reinitialized() {
        let config = http_config("https://mcp.example.com/mcp", &BTreeMap::new()).unwrap();
        assert!(config.reinit_on_expired_session);
    }

    /// The one thing worth keeping out of a `WWW-Authenticate` challenge:
    /// where the protected-resource metadata lives, for the broker that will
    /// need it. Everything else in the header is the upstream's business.
    #[test]
    fn the_resource_metadata_url_is_read_off_the_challenge() {
        use super::resource_metadata;

        assert_eq!(
            resource_metadata(
                r#"Bearer realm="mcp", resource_metadata="https://a.example/.well-known/x", error="invalid_token""#
            )
            .as_deref(),
            Some("https://a.example/.well-known/x")
        );
        // Unquoted is not what RFC 9728 shows, but the parameter list ends
        // at the comma either way.
        assert_eq!(
            resource_metadata("Bearer resource_metadata=https://a.example/x, scope=\"read\"")
                .as_deref(),
            Some("https://a.example/x")
        );
        assert_eq!(resource_metadata("Bearer realm=\"mcp\""), None);
        assert_eq!(resource_metadata("Bearer resource_metadata=\"\""), None);
    }

    #[test]
    fn broken_header_names_are_reported_not_dropped() {
        let headers = [("not a header".to_owned(), "x".to_owned())]
            .into_iter()
            .collect();
        let err = http_config("https://mcp.example.com/mcp", &headers).unwrap_err();
        assert!(err.contains("invalid header name"), "{err}");
    }

    /// The whole of what a request is allowed to carry to an upstream: the
    /// `Mcp-Param-*` family, whatever case it arrived in, and nothing else.
    #[test]
    fn only_the_param_family_is_collected() {
        let mut headers = http::HeaderMap::new();
        for (name, value) in [
            ("MCP-PARAM-Region", "eu"),
            ("mcp-param-tenant", "acme"),
            ("authorization", "Bearer t0ken"),
            ("mcp-session-id", "s1"),
            ("mcp-method", "tools/call"),
            ("mcp-name", "deploy"),
        ] {
            headers.insert(
                http::HeaderName::try_from(name).unwrap(),
                http::HeaderValue::from_static(value),
            );
        }

        let super::ParamHeaders(collected) = super::ParamHeaders::collect(&headers).unwrap();
        let mut names: Vec<&str> = collected.iter().map(|(name, _)| name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["mcp-param-region", "mcp-param-tenant"]);
    }

    #[test]
    fn a_request_carrying_none_collects_nothing() {
        let mut headers = http::HeaderMap::new();
        headers.insert("authorization", http::HeaderValue::from_static("Bearer x"));
        assert_eq!(super::ParamHeaders::collect(&headers), None);
    }
}
