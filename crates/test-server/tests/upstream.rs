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
async fn unknown_disabled_and_http_are_typed_errors() {
    let mut disabled = stdio_server("healthy");
    disabled.enabled = false;
    let http = Server {
        enabled: true,
        tags: Vec::new(),
        transport: Transport::Http {
            url: "https://mcp.example.com/mcp".to_owned(),
            headers: BTreeMap::new(),
        },
    };
    let mgr = manager(&[("off", disabled), ("remote", http)]);

    assert!(matches!(
        mgr.ready("nope").await.unwrap_err(),
        UpstreamError::Unknown { .. }
    ));
    assert!(matches!(
        mgr.ready("off").await.unwrap_err(),
        UpstreamError::Disabled { .. }
    ));
    assert!(matches!(
        mgr.ready("remote").await.unwrap_err(),
        UpstreamError::UnsupportedTransport { .. }
    ));
    // Unsupported transport must not burn the backoff ladder.
    assert!(matches!(
        mgr.ready("remote").await.unwrap_err(),
        UpstreamError::UnsupportedTransport { .. }
    ));
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
