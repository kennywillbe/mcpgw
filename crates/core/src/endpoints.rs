//! Per-server HTTP endpoints: one Streamable HTTP service per server, served
//! at `/s/<name>`, every one of them a [`Gateway`] pipe over the same shared
//! [`UpstreamManager`](crate::upstream::UpstreamManager). A client dialing an
//! endpoint sees that server's tools under their own names, with no aggregate
//! prefix to strip.
//!
//! Why a table plus a hand-written dispatch handler instead of one service
//! that reads the path: rmcp builds the handler through a factory closure
//! that is handed no request context, so a single service cannot know which
//! endpoint was dialed. The choice has to be made before the service is
//! entered, which means one pre-built service per server and a router step
//! that picks between them.
//!
//! The table lives behind an [`ArcSwap`] so a later config reload can publish
//! a whole new set of endpoints atomically, without pausing in-flight
//! requests or rebuilding the axum router.

use std::collections::BTreeMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::extract::{Path, Request, State};
use axum::response::{IntoResponse as _, Response};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tower_service::Service;

use crate::gateway::Gateway;

/// Path prefix every per-server endpoint lives under. Short on purpose: it
/// ends up pasted into a dozen client config files per machine.
pub const PREFIX: &str = "/s";

/// The endpoint path for `name`, e.g. `/s/github`.
#[must_use]
pub fn endpoint_path(name: &str) -> String {
    format!("{PREFIX}/{name}")
}

/// Rewrites `base`'s path to `name`'s endpoint, keeping scheme, host and
/// port. Used to turn the default gateway URL into a per-server one.
///
/// # Errors
///
/// Returns the parse error when `base` is not an absolute URL.
pub fn per_server_url(base: &str, name: &str) -> Result<String, url::ParseError> {
    let mut url = url::Url::parse(base)?;
    url.set_path(&endpoint_path(name));
    Ok(url.into())
}

/// One server's ready-to-serve HTTP face.
type ServerService = StreamableHttpService<Gateway, LocalSessionManager>;

/// The set of servers reachable under [`PREFIX`], keyed by server name.
pub struct EndpointTable {
    services: BTreeMap<String, ServerService>,
}

impl EndpointTable {
    /// Builds one endpoint per `(name, gateway)` pair. The caller owns how
    /// each gateway is configured — capture, deadlines, hints — because the
    /// deployment, not this table, knows what those should be.
    #[must_use]
    pub fn new(gateways: impl IntoIterator<Item = (String, Gateway)>) -> Self {
        let services = gateways
            .into_iter()
            .map(|(name, gateway)| {
                let service = StreamableHttpService::new(
                    move || Ok(gateway.clone()),
                    LocalSessionManager::default().into(),
                    // rmcp's own rebinding guard validates `Host` and, only
                    // when a list is configured, `Origin`. Its default leaves
                    // the origin list empty, so it never double-rejects what
                    // `guard_origin` already screens; the `Host` half is
                    // complementary and stays on. This mirrors the `/mcp`
                    // service exactly — one config for both faces.
                    StreamableHttpServerConfig::default(),
                );
                (name, service)
            })
            .collect();
        Self { services }
    }

    /// The server names this table serves, in path order.
    pub fn names(&self) -> impl ExactSizeIterator<Item = &str> {
        self.services.keys().map(String::as_str)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.services.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.services.is_empty()
    }

    fn get(&self, name: &str) -> Option<&ServerService> {
        self.services.get(name)
    }
}

/// A shared, atomically replaceable [`EndpointTable`]. Cloning shares the
/// same cell, so every router clone sees a swap immediately.
#[derive(Clone)]
pub struct Endpoints(Arc<ArcSwap<EndpointTable>>);

impl Endpoints {
    #[must_use]
    pub fn new(table: EndpointTable) -> Self {
        Self(Arc::new(ArcSwap::from_pointee(table)))
    }

    /// Publishes `table` in place of the current one. Requests that already
    /// picked a service keep running against it; the next dispatch sees the
    /// new table.
    pub fn store(&self, table: EndpointTable) {
        self.0.store(Arc::new(table));
    }

    /// The table as of right now.
    #[must_use]
    pub fn load(&self) -> arc_swap::Guard<Arc<EndpointTable>> {
        self.0.load()
    }
}

/// The routes serving `endpoints`. Merged into the gateway router next to
/// `/mcp`; the origin guard is applied by the caller, over both.
pub fn router(endpoints: Endpoints) -> axum::Router {
    axum::Router::new()
        // A single wildcard rather than `/s/{name}` plus a nested route:
        // Streamable HTTP treats the endpoint as a base URL, so anything
        // below it belongs to the same server and has to reach the same
        // service.
        .route(&format!("{PREFIX}/{{*rest}}"), axum::routing::any(dispatch))
        .with_state(endpoints)
}

/// Splits the wildcard capture into the server name and the path the inner
/// service should see. The remainder always starts with `/`, so a bare
/// `/s/<name>` reaches the service as `/` — the same thing `nest_service`
/// hands the `/mcp` service, which keeps both faces byte-identical from
/// rmcp's point of view.
fn split_endpoint(rest: &str) -> (&str, String) {
    match rest.split_once('/') {
        Some((name, tail)) => (name, format!("/{tail}")),
        None => (rest, "/".to_owned()),
    }
}

/// Replaces `uri`'s path with `path`, keeping any query string.
fn rewrite_path(uri: &http::Uri, path: &str) -> http::Uri {
    let target = match uri.query() {
        Some(query) => format!("{path}?{query}"),
        None => path.to_owned(),
    };
    let mut parts = uri.clone().into_parts();
    parts.path_and_query = match target.parse() {
        Ok(path_and_query) => Some(path_and_query),
        Err(_) => return uri.clone(),
    };
    http::Uri::from_parts(parts).unwrap_or_else(|_| uri.clone())
}

async fn dispatch(
    State(endpoints): State<Endpoints>,
    Path(rest): Path<String>,
    mut request: Request,
) -> Response {
    let (name, tail) = split_endpoint(&rest);

    // The guard is dropped before awaiting: holding it across the request
    // would pin the old table for as long as a session streams, which is
    // exactly the pause an atomic swap exists to avoid.
    let mut service = {
        let table = endpoints.load();
        match table.get(name) {
            // Cloning is how rmcp's service is meant to be used (it is a
            // handle over shared state), and readiness is tracked per clone,
            // so we must never poll the one the table holds.
            Some(service) => service.clone(),
            None => return unknown_endpoint(name, &table),
        }
    };

    let inner = rewrite_path(request.uri(), &tail);
    *request.uri_mut() = inner;

    // Tower's contract: ready the service we own before calling it. rmcp's
    // is unconditionally ready, but that is its answer to give, not our
    // assumption to make.
    // Spelled out because the service is generic over the request body: only
    // naming the body type says which `poll_ready` this is.
    let ready = std::future::poll_fn(|cx| Service::<Request>::poll_ready(&mut service, cx)).await;
    if ready.is_err() {
        return (
            http::StatusCode::SERVICE_UNAVAILABLE,
            format!("server endpoint {name:?} is not accepting requests\n"),
        )
            .into_response();
    }
    match service.call(request).await {
        Ok(response) => response.map(axum::body::Body::new),
        // The service's error type is uninhabited: this arm cannot be taken.
        Err(never) => match never {},
    }
}

/// 404 for a path under [`PREFIX`] that names no server. Lists what is
/// actually served: the usual cause is a typo or a stale client config, and
/// the answer to both is the real names.
fn unknown_endpoint(name: &str, table: &EndpointTable) -> Response {
    let known = if table.is_empty() {
        "no per-server endpoints are configured (start the gateway with \
         `mcpgw serve --per-server`)"
            .to_owned()
    } else {
        let paths: Vec<String> = table.names().map(endpoint_path).collect();
        format!("known endpoints: {}", paths.join(", "))
    };
    (
        http::StatusCode::NOT_FOUND,
        format!("no server endpoint named {name:?} — {known}\n"),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::{endpoint_path, per_server_url, rewrite_path, split_endpoint};

    #[test]
    fn the_endpoint_itself_reaches_the_service_as_root() {
        assert_eq!(split_endpoint("github"), ("github", "/".to_owned()));
        // Streamable HTTP may address anything below the endpoint; the tail
        // survives, the prefix does not.
        assert_eq!(split_endpoint("github/sse"), ("github", "/sse".to_owned()));
        assert_eq!(split_endpoint("github/a/b"), ("github", "/a/b".to_owned()));
    }

    #[test]
    fn rewriting_the_path_keeps_the_query() {
        let uri: http::Uri = "/s/github/x?sessionId=7".parse().unwrap();
        assert_eq!(rewrite_path(&uri, "/x").to_string(), "/x?sessionId=7");
        let uri: http::Uri = "/s/github".parse().unwrap();
        assert_eq!(rewrite_path(&uri, "/").to_string(), "/");
    }

    #[test]
    fn per_server_urls_replace_only_the_path() {
        assert_eq!(endpoint_path("github"), "/s/github");
        assert_eq!(
            per_server_url("http://127.0.0.1:8137/mcp", "github").unwrap(),
            "http://127.0.0.1:8137/s/github"
        );
        // A host-only base gets the endpoint just the same.
        assert_eq!(
            per_server_url("http://localhost:9000", "fx").unwrap(),
            "http://localhost:9000/s/fx"
        );
        assert!(per_server_url("not a url", "fx").is_err());
    }
}
