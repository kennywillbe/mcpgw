//! Upstream lifecycle management for the gateway: lazy spawning, exponential
//! backoff with a failure latch, passive health via transport liveness, and
//! explicit status reporting. This layer is mcpgw's answer to the
//! reliability failures of per-session-process gateways: one connection per
//! server, multiplexed, and every state visible — never a silent empty list.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use rmcp::ServiceExt as _;
use rmcp::transport::TokioChildProcess;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::config::{Server, Transport};

pub type UpstreamService = rmcp::service::RunningService<rmcp::RoleClient, ()>;

// Explicit cancellation and transport death (child exit) are reported by
// different rmcp signals; either one means the slot is stale.
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

    #[error("upstream {name:?} uses http transport (supported from the gateway HTTP milestone)")]
    UnsupportedTransport { name: String },

    #[error("upstream {name:?} failed after {attempts} attempt(s): {message}")]
    Failed {
        name: String,
        attempts: u32,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamStatus {
    /// Not started yet (lazy) or shut down.
    Idle,
    Ready,
    /// Latched after exhausting attempts; the next request gets one chance.
    Failed(String),
}

enum Slot {
    Idle,
    Ready(Arc<UpstreamService>),
    Failed(String),
}

struct Upstream {
    server: Server,
    slot: Mutex<Slot>,
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

    /// Current status without touching the upstream.
    pub async fn status(&self, name: &str) -> Option<UpstreamStatus> {
        let upstream = self.upstreams.get(name)?;
        let slot = upstream.slot.lock().await;
        Some(match &*slot {
            // A Ready slot whose transport died counts as Idle-with-history;
            // report Ready only while actually alive.
            Slot::Ready(service) if !dead(service) => UpstreamStatus::Ready,
            Slot::Idle | Slot::Ready(_) => UpstreamStatus::Idle,
            Slot::Failed(message) => UpstreamStatus::Failed(message.clone()),
        })
    }

    /// Returns a live service for `name`, spawning it on first demand.
    ///
    /// Concurrent callers coalesce on the per-upstream lock: exactly one
    /// connect cycle runs, everyone else waits for its outcome. A `Failed`
    /// upstream gets a single fresh attempt per call (no backoff ladder).
    ///
    /// # Errors
    ///
    /// Returns [`UpstreamError`] for unknown/disabled/http upstreams and
    /// exhausted connect attempts.
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

        let mut slot = upstream.slot.lock().await;
        let attempts = match &*slot {
            Slot::Ready(service) if !dead(service) => {
                return Ok(Arc::clone(service));
            }
            // Dead-after-ready is a new crash episode: full ladder.
            Slot::Ready(_) | Slot::Idle => ATTEMPTS,
            // Latched: one fresh chance per demand.
            Slot::Failed(_) => 1,
        };

        match self
            .connect_with_backoff(name, &upstream.server, attempts)
            .await
        {
            Ok(service) => {
                let service = Arc::new(service);
                *slot = Slot::Ready(Arc::clone(&service));
                Ok(service)
            }
            Err(err) => {
                *slot = Slot::Failed(err.to_string());
                Err(err)
            }
        }
    }

    /// Stops every running upstream (children die with their transports).
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
                Err(UpstreamError::UnsupportedTransport { name }) => {
                    // Retrying cannot change the transport type.
                    return Err(UpstreamError::UnsupportedTransport { name });
                }
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
        let Transport::Stdio { command, args, env } = &server.transport else {
            return Err(UpstreamError::UnsupportedTransport {
                name: name.to_owned(),
            });
        };
        let failed = |message: String| UpstreamError::Failed {
            name: name.to_owned(),
            attempts: 1,
            message,
        };

        let mut cmd = Command::new(command);
        cmd.args(args);
        for (key, value) in env {
            cmd.env(key, value);
        }
        cmd.stderr(std::process::Stdio::null());
        // Orphan prevention (learned the hard way in the probe milestone).
        cmd.kill_on_drop(true);

        let transport =
            TokioChildProcess::new(cmd).map_err(|err| failed(format!("spawn failed: {err}")))?;
        tokio::time::timeout(self.connect_timeout, ().serve(transport))
            .await
            .map_err(|_| failed(format!("no handshake within {:?}", self.connect_timeout)))?
            .map_err(|err| failed(format!("handshake failed: {err}")))
    }
}
