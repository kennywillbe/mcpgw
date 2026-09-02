//! Upstream lifecycle management for the gateway: lazy spawning, exponential
//! backoff with a failure latch, passive health via transport liveness, and
//! explicit status reporting. This layer is mcpgw's answer to the
//! reliability failures of per-session-process gateways: one connection per
//! server, multiplexed, and every state visible — never a silent empty list.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use http::{HeaderName, HeaderValue};
use rmcp::ServiceExt as _;
use rmcp::transport::TokioChildProcess;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use tokio::process::Command;
use tokio::sync::{Mutex, Notify};

use crate::config::{Server, Transport};

pub type UpstreamService = rmcp::service::RunningService<rmcp::RoleClient, ()>;

// Explicit cancellation and transport death (child exit) are reported by
// different rmcp signals; either one means the slot is stale. For http
// upstreams the death signal only fires once the client worker gives up —
// a server that vanishes between requests stays "alive" here until a call
// fails, which the slot logic already handles by reconnecting on the next
// demand.
fn dead(service: &UpstreamService) -> bool {
    service.is_closed() || service.is_transport_closed()
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

    #[error("upstream {name:?} was shut down while connecting")]
    ShutDown { name: String },
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
}

enum Slot {
    Idle,
    /// A ladder owns this slot. The flag is that ladder's liveness witness:
    /// see [`ConnectGuard`] for why the claim has to be revocable without
    /// taking the lock.
    Connecting(Arc<AtomicBool>),
    Ready(Arc<UpstreamService>),
    Failed(String),
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
    server: Server,
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

pub struct UpstreamManager {
    upstreams: BTreeMap<String, Upstream>,
    connect_timeout: Duration,
    backoff_base: Duration,
}

impl UpstreamManager {
    #[must_use]
    pub fn new(servers: BTreeMap<String, Server>) -> Self {
        let upstreams = servers
            .into_iter()
            .map(|(name, server)| {
                (
                    name,
                    Upstream {
                        server,
                        slot: Mutex::new(Slot::Idle),
                        settled: Notify::new(),
                        info: arc_swap::ArcSwapOption::empty(),
                    },
                )
            })
            .collect();
        Self {
            upstreams,
            connect_timeout: Duration::from_secs(30),
            backoff_base: Duration::from_millis(500),
        }
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

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.upstreams.keys().map(String::as_str)
    }

    /// Current status without touching the upstream. The slot lock is never
    /// held across a connect, so this answers immediately even while an
    /// upstream is halfway through its ladder.
    pub async fn status(&self, name: &str) -> Option<UpstreamStatus> {
        let upstream = self.upstreams.get(name)?;
        let slot = upstream.slot.lock().await;
        Some(match &*slot {
            // A Ready slot whose transport died counts as Idle-with-history;
            // report Ready only while actually alive.
            Slot::Ready(service) if !dead(service) => UpstreamStatus::Ready,
            Slot::Connecting(_) if slot.connecting() => UpstreamStatus::Connecting,
            // An abandoned claim included: nothing is running.
            Slot::Idle | Slot::Ready(_) | Slot::Connecting(_) => UpstreamStatus::Idle,
            Slot::Failed(message) => UpstreamStatus::Failed(message.clone()),
        })
    }

    /// What `name` reported at its last successful connect, or `None` if it
    /// has never been reached in this process.
    ///
    /// Synchronous and lock-free on purpose: its one caller is the gateway's
    /// `initialize` handler, which rmcp calls synchronously and which must
    /// never be the thing that starts an upstream.
    #[must_use]
    pub fn last_server_info(&self, name: &str) -> Option<Arc<rmcp::model::ServerPeerInfo>> {
        self.upstreams.get(name)?.info.load_full()
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
        let upstream = self
            .upstreams
            .get(name)
            .ok_or_else(|| UpstreamError::Unknown {
                name: name.to_owned(),
            })?;
        if !upstream.server.enabled {
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
                    // Latched: one fresh chance per demand.
                    Slot::Failed(_) => {
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
            .connect_with_backoff(name, &upstream.server, attempts)
            .await;

        let mut slot = upstream.slot.lock().await;
        // A shutdown during the ladder left the slot Idle, and a demand that
        // found the claim revoked may have started a ladder of its own.
        // Either way this connection must not be installed behind their
        // backs — discard it instead.
        let abandoned = !matches!(&*slot, Slot::Connecting(owner) if Arc::ptr_eq(owner, &live));
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
                    *slot = Slot::Failed(err.to_string());
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

    /// Stops every running upstream (children die with their transports).
    /// Never waits for a connect in flight: that ladder finds the slot no
    /// longer its own when it lands and throws its connection away.
    pub async fn shutdown(&self) {
        for upstream in self.upstreams.values() {
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
    ) -> Result<UpstreamService, UpstreamError> {
        let mut last = String::new();
        for attempt in 0..attempts {
            if attempt > 0 {
                // 500ms → 1s → 2s with the default base.
                tokio::time::sleep(self.backoff_base * 2u32.pow(attempt - 1)).await;
            }
            match self.connect_once(name, server).await {
                Ok(service) => return Ok(service),
                Err(err) => last = err.to_string(),
            }
        }
        Err(UpstreamError::Failed {
            name: name.to_owned(),
            attempts,
            message: last,
        })
    }

    async fn connect_once(
        &self,
        name: &str,
        server: &Server,
    ) -> Result<UpstreamService, UpstreamError> {
        let failed = |message: String| UpstreamError::Failed {
            name: name.to_owned(),
            attempts: 1,
            message,
        };
        let handshake =
            |result: Result<Result<UpstreamService, _>, tokio::time::error::Elapsed>| {
                result
                    .map_err(|_| failed(format!("no handshake within {:?}", self.connect_timeout)))?
                    .map_err(|err| failed(format!("handshake failed: {err}")))
            };

        match &server.transport {
            Transport::Stdio { command, args, env } => {
                let mut cmd = Command::new(command);
                cmd.args(args);
                for (key, value) in env {
                    cmd.env(key, value);
                }
                cmd.stderr(std::process::Stdio::null());
                // Orphan prevention (learned the hard way in the probe milestone).
                cmd.kill_on_drop(true);

                let transport = TokioChildProcess::new(cmd)
                    .map_err(|err| failed(format!("spawn failed: {err}")))?;
                handshake(tokio::time::timeout(self.connect_timeout, ().serve(transport)).await)
            }
            Transport::Http { url, headers } => {
                let config = http_config(url, headers).map_err(failed)?;
                // Nothing is dialed until the handshake, so an unreachable
                // host surfaces here as a normal connect failure and feeds
                // the same backoff ladder as a stdio spawn failure.
                let transport = rmcp::transport::StreamableHttpClientTransport::from_config(config);
                handshake(tokio::time::timeout(self.connect_timeout, ().serve(transport)).await)
            }
        }
    }
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
    Ok(StreamableHttpClientTransportConfig::with_uri(url.to_owned()).custom_headers(custom))
}

#[cfg(test)]
mod tests {
    use super::http_config;

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

    #[test]
    fn broken_header_names_are_reported_not_dropped() {
        let headers = [("not a header".to_owned(), "x".to_owned())]
            .into_iter()
            .collect();
        let err = http_config("https://mcp.example.com/mcp", &headers).unwrap_err();
        assert!(err.contains("invalid header name"), "{err}");
    }
}
