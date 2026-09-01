//! Upstream lifecycle management for the gateway: lazy spawning, exponential
//! backoff with a failure latch, passive health via transport liveness, and
//! explicit status reporting. This layer is mcpgw's answer to the
//! reliability failures of per-session-process gateways: one connection per
//! server, multiplexed, and every state visible — never a silent empty list.

use std::collections::BTreeMap;
use std::sync::Arc;
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
    Connecting,
    Ready(Arc<UpstreamService>),
    Failed(String),
}

struct Upstream {
    server: Server,
    slot: Mutex<Slot>,
    /// Notified every time the slot leaves `Connecting`, so demands that
    /// arrive mid-ladder can wait for its outcome without holding the lock.
    settled: Notify,
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
            Slot::Idle | Slot::Ready(_) => UpstreamStatus::Idle,
            Slot::Connecting => UpstreamStatus::Connecting,
            Slot::Failed(message) => UpstreamStatus::Failed(message.clone()),
        })
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

        let attempts = loop {
            let mut slot = upstream.slot.lock().await;
            match &*slot {
                Slot::Ready(service) if !dead(service) => {
                    return Ok(Arc::clone(service));
                }
                Slot::Connecting => {
                    // Someone else owns the ladder. Wait for its outcome
                    // instead of starting a second one, and do it without
                    // the lock. `enable` registers this waiter before the
                    // lock goes, so a connect that settles in between
                    // cannot slip past unheard.
                    let settled = upstream.settled.notified();
                    tokio::pin!(settled);
                    settled.as_mut().enable();
                    drop(slot);
                    settled.await;
                }
                // Dead-after-ready is a new crash episode: full ladder.
                Slot::Ready(_) | Slot::Idle => {
                    *slot = Slot::Connecting;
                    break ATTEMPTS;
                }
                // Latched: one fresh chance per demand.
                Slot::Failed(_) => {
                    *slot = Slot::Connecting;
                    break 1;
                }
            }
        };

        let outcome = self
            .connect_with_backoff(name, &upstream.server, attempts)
            .await;

        let mut slot = upstream.slot.lock().await;
        // A shutdown during the ladder left the slot Idle. It could not see
        // this connection to cancel it, so the connection must not be
        // installed behind its back — discard it instead.
        let abandoned = !matches!(*slot, Slot::Connecting);
        let result = match outcome {
            Ok(service) => {
                let service = Arc::new(service);
                if abandoned {
                    service.cancellation_token().cancel();
                    Err(UpstreamError::ShutDown {
                        name: name.to_owned(),
                    })
                } else {
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
