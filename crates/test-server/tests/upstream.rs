use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mcpgw_core::upstream::{UpstreamError, UpstreamManager, UpstreamStatus};
use mcpgw_core::{Server, Transport};

fn stdio_server(mode: &str) -> Server {
    Server {
        enabled: true,
        tags: Vec::new(),
        transport: Transport::Stdio {
            command: env!("CARGO_BIN_EXE_mcpgw-test-server").to_owned(),
            args: vec![mode.to_owned()],
            env: BTreeMap::new(),
        },
    }
}

fn http_server(url: &str, headers: &[(&str, &str)]) -> Server {
    Server {
        enabled: true,
        tags: Vec::new(),
        transport: Transport::Http {
            url: url.to_owned(),
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        },
    }
}

fn manager(entries: &[(&str, Server)]) -> UpstreamManager {
    let servers: BTreeMap<String, Server> = entries
        .iter()
        .map(|(name, server)| ((*name).to_owned(), server.clone()))
        .collect();
    UpstreamManager::new(servers)
        .with_connect_timeout(Duration::from_secs(5))
        // 50ms → 100ms ladder keeps the failure tests fast.
        .with_backoff_base(Duration::from_millis(50))
}

#[tokio::test]
async fn lazy_until_first_demand_then_reused() {
    let mgr = manager(&[("fx", stdio_server("healthy"))]);
    assert_eq!(mgr.status("fx").await, Some(UpstreamStatus::Idle));

    let first = mgr.ready("fx").await.unwrap();
    assert_eq!(mgr.status("fx").await, Some(UpstreamStatus::Ready));
    let second = mgr.ready("fx").await.unwrap();
    // Same live instance, not a second process.
    assert!(Arc::ptr_eq(&first, &second));
    mgr.shutdown().await;
    assert_eq!(mgr.status("fx").await, Some(UpstreamStatus::Idle));
}

#[tokio::test]
async fn crash_latches_failed_after_backoff_ladder() {
    let mgr = manager(&[("fx", stdio_server("exit"))]);
    let started = Instant::now();
    let err = mgr.ready("fx").await.unwrap_err();
    let elapsed = started.elapsed();

    let UpstreamError::Failed { attempts, .. } = &err else {
        panic!("expected Failed, got {err}");
    };
    assert_eq!(*attempts, 3);
    // Two backoff sleeps must have happened: 50ms + 100ms.
    assert!(elapsed >= Duration::from_millis(150), "elapsed {elapsed:?}");
    assert!(matches!(
        mgr.status("fx").await,
        Some(UpstreamStatus::Failed(_))
    ));
}

#[tokio::test]
async fn latched_failure_gets_one_fresh_chance_per_demand() {
    let mgr = manager(&[("fx", stdio_server("exit"))]);
    let _ = mgr.ready("fx").await.unwrap_err();

    let started = Instant::now();
    let err = mgr.ready("fx").await.unwrap_err();
    // Single attempt: no ladder sleeps.
    assert!(started.elapsed() < Duration::from_millis(100));
    let UpstreamError::Failed { attempts, .. } = &err else {
        panic!("expected Failed, got {err}");
    };
    assert_eq!(*attempts, 1);
}

#[tokio::test]
async fn death_after_ready_reconnects_on_next_demand() {
    let mgr = manager(&[("fx", stdio_server("die-on-tools"))]);
    let first = mgr.ready("fx").await.unwrap();
    // The fixture dies on this request; the call errors out.
    assert!(first.list_all_tools().await.is_err());
    // Give the transport task a moment to observe the child's death.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(mgr.status("fx").await, Some(UpstreamStatus::Idle));

    let second = mgr.ready("fx").await.unwrap();
    assert!(!Arc::ptr_eq(&first, &second), "must be a fresh instance");
}

#[tokio::test]
async fn unknown_and_disabled_are_typed_errors() {
    let mut disabled = stdio_server("healthy");
    disabled.enabled = false;
    let mgr = manager(&[("off", disabled)]);

    assert!(matches!(
        mgr.ready("nope").await.unwrap_err(),
        UpstreamError::Unknown { .. }
    ));
    assert!(matches!(
        mgr.ready("off").await.unwrap_err(),
        UpstreamError::Disabled { .. }
    ));
}

#[tokio::test]
async fn unreachable_http_upstream_latches_failed_after_the_ladder() {
    // Port 1 on loopback refuses instantly, so the ladder is the only wait.
    let mgr = manager(&[("remote", http_server("http://127.0.0.1:1/mcp", &[]))]);
    let started = Instant::now();
    let err = mgr.ready("remote").await.unwrap_err();
    let elapsed = started.elapsed();

    let UpstreamError::Failed { attempts, .. } = &err else {
        panic!("expected Failed, got {err}");
    };
    assert_eq!(*attempts, 3);
    assert!(elapsed >= Duration::from_millis(150), "elapsed {elapsed:?}");
    assert!(matches!(
        mgr.status("remote").await,
        Some(UpstreamStatus::Failed(_))
    ));
}

#[tokio::test]
async fn configured_headers_reach_the_http_upstream() {
    // A bare http server that records what it is asked for: the handshake
    // fails, but the request it sent is what this test is about.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let app = axum::Router::new().fallback(move |headers: axum::http::HeaderMap| {
        let tx = tx.clone();
        async move {
            let _ = tx.send(headers);
            axum::http::StatusCode::NOT_FOUND
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let server = http_server(
        &format!("http://{addr}/mcp"),
        &[("Authorization", "Bearer t0ken"), ("X-Tenant", "acme")],
    );
    let mgr = manager(&[("remote", server)]);
    let _ = mgr.ready("remote").await.unwrap_err();

    let headers = rx.recv().await.expect("upstream was never contacted");
    assert_eq!(headers["authorization"], "Bearer t0ken");
    assert_eq!(headers["x-tenant"], "acme");
}

/// The connect ladder used to run under the per-upstream lock, so a hung
/// server made `status` and `shutdown` wait out three connect timeouts plus
/// the backoff sleeps — around 93 seconds at the defaults.
#[tokio::test]
async fn status_and_shutdown_answer_while_a_connect_is_in_flight() {
    // The `slow` fixture never answers the handshake, so the ladder is stuck
    // in its first attempt for the whole test.
    let mgr = Arc::new(
        UpstreamManager::new(
            [("fx".to_owned(), stdio_server("slow"))]
                .into_iter()
                .collect(),
        )
        .with_connect_timeout(Duration::from_secs(30)),
    );
    let connecting = tokio::spawn({
        let mgr = Arc::clone(&mgr);
        async move { mgr.ready("fx").await }
    });

    // Let the ladder get past spawning and into its wait.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let started = Instant::now();
    assert_eq!(mgr.status("fx").await, Some(UpstreamStatus::Connecting));
    mgr.shutdown().await;
    assert_eq!(mgr.status("fx").await, Some(UpstreamStatus::Idle));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "blocked for {:?} behind the connect",
        started.elapsed()
    );

    // Dropping the task kills the parked child with its transport.
    connecting.abort();
}

#[tokio::test]
async fn concurrent_demands_coalesce_into_one_instance() {
    let mgr = Arc::new(manager(&[("fx", stdio_server("healthy"))]));
    let a = tokio::spawn({
        let mgr = Arc::clone(&mgr);
        async move { mgr.ready("fx").await.unwrap() }
    });
    let b = tokio::spawn({
        let mgr = Arc::clone(&mgr);
        async move { mgr.ready("fx").await.unwrap() }
    });
    let (a, b) = (a.await.unwrap(), b.await.unwrap());
    assert!(Arc::ptr_eq(&a, &b));
    mgr.shutdown().await;
}
