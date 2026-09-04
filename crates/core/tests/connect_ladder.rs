//! One connect ladder, checked from both ends.
//!
//! `doctor --probe` promises that a server answers *the way mcpgw would
//! reach it*, which is only true while the gateway and the probe dial
//! through the same code. They did once by hand, in two files that happened
//! to agree; these tests are what makes the agreement enforced instead of
//! observed.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use mcpgw_core::config::{Server, Transport};
use mcpgw_core::probe::probe_server;
use mcpgw_core::upstream::UpstreamManager;

/// A server whose MCP endpoint answers every request with a redirect, and
/// which counts the requests that arrive at the target.
///
/// Redirects are the visible half of the shared http client: it is built with
/// `redirect(Policy::none())` so custom headers — an `Authorization` among
/// them — can never be replayed to a host the config did not name. A caller
/// that built its own client would follow the redirect and be counted.
async fn redirecting_server() -> (String, Arc<AtomicUsize>) {
    let followed = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&followed);
    let app = axum::Router::new()
        .route(
            "/mcp",
            axum::routing::any(|| async {
                (
                    axum::http::StatusCode::TEMPORARY_REDIRECT,
                    [(axum::http::header::LOCATION, "/followed")],
                )
            }),
        )
        .route(
            "/followed",
            axum::routing::any(move || {
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    "not an MCP server"
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://127.0.0.1:{port}/mcp"), followed)
}

fn http_server(url: &str) -> Server {
    Server {
        enabled: true,
        tags: Vec::new(),
        calls_per_minute: 0,
        tools: None,
        transport: Transport::Http {
            url: url.to_owned(),
            headers_command: Vec::new(),
            headers: BTreeMap::new(),
            auth: None,
        },
    }
}

/// The behavioural half: both paths refuse the redirect, because both build
/// their client with `upstream::http_client`. Either one growing a client of
/// its own would follow it and fail this.
#[tokio::test]
async fn neither_connect_path_follows_a_redirect() {
    let (url, followed) = redirecting_server().await;
    let server = http_server(&url);

    let probe = probe_server("redirecting", &server, None, Duration::from_secs(10)).await;
    assert!(
        probe.is_err(),
        "a redirect is not a handshake, so the probe must fail"
    );
    assert_eq!(
        followed.load(Ordering::SeqCst),
        0,
        "`doctor --probe` followed the redirect"
    );

    let manager = UpstreamManager::new(
        [("redirecting".to_owned(), server)]
            .into_iter()
            .collect::<BTreeMap<_, _>>(),
    )
    .with_connect_timeout(Duration::from_secs(10))
    .with_backoff_base(Duration::from_millis(1));
    assert!(
        manager.ready("redirecting").await.is_err(),
        "a redirect is not a handshake, so the connect must fail"
    );
    assert_eq!(
        followed.load(Ordering::SeqCst),
        0,
        "the gateway followed the redirect"
    );
}

/// The structural half, and the one that catches a split before it can drift:
/// the probe reaches an upstream only through `upstream`'s ladder, so it
/// names none of the pieces a ladder is made of. Spelled against the source
/// because the ladder is crate-private — a test that could call it could not
/// prove nobody else rebuilt it.
#[test]
fn the_probe_has_no_connect_ladder_of_its_own() {
    let source = include_str!("../src/probe.rs");
    for piece in [
        "StreamableHttpClientTransport",
        "reqwest::Client",
        "TokioChildProcess",
        "Lifecycle::",
    ] {
        assert!(
            !source.contains(piece),
            "probe.rs builds `{piece}` itself; the connect ladder belongs in upstream.rs, \
             or `doctor --probe` stops proving what the gateway does"
        );
    }
}
