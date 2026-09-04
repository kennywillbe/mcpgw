use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::response::IntoResponse as _;
use mcpgw_core::upstream::{CallError, UpstreamError, UpstreamManager, UpstreamStatus};
use mcpgw_core::{Server, Transport};

fn stdio_server(mode: &str) -> Server {
    Server {
        enabled: true,
        tags: Vec::new(),
        calls_per_minute: 0,
        tools: None,
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
        calls_per_minute: 0,
        tools: None,
        transport: Transport::Http {
            url: url.to_owned(),
            headers_command: Vec::new(),
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            auth: None,
        },
    }
}

fn manager(entries: &[(&str, Server)]) -> UpstreamManager {
    let servers: BTreeMap<String, Server> = entries
        .iter()
        .map(|(name, server)| ((*name).to_owned(), server.clone()))
        .collect();
    UpstreamManager::new(servers)
        .with_connect_timeout(Duration::from_secs(30))
        // 50ms → 100ms ladder keeps the failure tests fast.
        .with_backoff_base(Duration::from_millis(50))
}

/// Waits for `name` to reach `want`, polling rather than sleeping a guessed
/// interval: how long a child takes to spawn is a property of the machine,
/// and a sleep tuned to an idle one is a flake on a loaded one.
async fn wait_for_status(mgr: &UpstreamManager, name: &str, want: UpstreamStatus) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let status = mgr.status(name).await;
        if status.as_ref() == Some(&want) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{name} never reached {want:?}, last seen {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
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

    let err = mgr.ready("fx").await.unwrap_err();
    let UpstreamError::Failed { attempts, .. } = &err else {
        panic!("expected Failed, got {err}");
    };
    // The attempt count is what "one fresh chance, no ladder" means. The
    // clock said the same thing only on an idle machine: under parallel load
    // a single process spawn can outlast any ceiling tight enough to tell the
    // two apart, so the count is the assertion and the stopwatch goes.
    assert_eq!(*attempts, 1);
}

#[tokio::test]
async fn death_after_ready_reconnects_on_next_demand() {
    let mgr = manager(&[("fx", stdio_server("die-on-tools"))]);
    let first = mgr.ready("fx").await.unwrap();
    // The fixture dies on this request; the call errors out.
    assert!(first.list_all_tools().await.is_err());
    // The transport task has to observe the child's death first.
    wait_for_status(&mgr, "fx", UpstreamStatus::Idle).await;

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
    wait_for_status(&mgr, "fx", UpstreamStatus::Connecting).await;

    let started = Instant::now();
    mgr.shutdown().await;
    assert_eq!(mgr.status("fx").await, Some(UpstreamStatus::Idle));
    // The bug this guards was a 30s wait, so the ceiling only has to be far
    // below that — not close to how fast an unloaded machine answers.
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "blocked for {:?} behind the connect",
        started.elapsed()
    );

    // Dropping the task kills the parked child with its transport.
    connecting.abort();
}

/// A `ready()` future dropped mid-ladder used to leave the slot `Connecting`
/// with nobody left to settle it, wedging the upstream for the life of the
/// process. The obvious trigger is a downstream client hanging up during a
/// `tools/call`, which drops the whole request future.
#[tokio::test]
async fn a_dropped_connect_releases_the_upstream_instead_of_wedging_it() {
    let mgr = manager(&[
        ("fx", stdio_server("slow")),
        ("ok", stdio_server("healthy")),
    ]);

    // The `slow` fixture never answers, so the timeout expiring is exactly
    // the "future dropped mid-connect" case.
    let dropped = tokio::time::timeout(Duration::from_millis(300), mgr.ready("fx")).await;
    assert!(
        dropped.is_err(),
        "the fixture answered, so nothing was cancelled"
    );

    // The abandoned claim must not read as an in-flight connect any more.
    assert_eq!(mgr.status("fx").await, Some(UpstreamStatus::Idle));
    // And the manager as a whole still works.
    mgr.ready("ok").await.unwrap();
    assert_eq!(mgr.status("ok").await, Some(UpstreamStatus::Ready));

    // A second demand on the wedged upstream starts its own ladder rather
    // than parking forever: it gets to run and time out on its own terms.
    let retried = tokio::time::timeout(Duration::from_millis(300), mgr.ready("fx")).await;
    assert!(retried.is_err());
    assert_eq!(mgr.status("fx").await, Some(UpstreamStatus::Idle));
    mgr.shutdown().await;
}

/// The other half of the same bug: a caller already parked behind the ladder
/// when it is abandoned has to be woken, not left on `settled` forever.
#[tokio::test]
async fn a_parked_caller_recovers_when_the_ladder_it_waits_on_is_dropped() {
    // A ladder short enough that the woken caller can run a whole one of its
    // own inside the test's patience.
    let mgr = Arc::new(
        UpstreamManager::new(
            [("fx".to_owned(), stdio_server("slow"))]
                .into_iter()
                .collect(),
        )
        .with_connect_timeout(Duration::from_millis(400))
        .with_backoff_base(Duration::from_millis(20)),
    );

    let owner = tokio::spawn({
        let mgr = Arc::clone(&mgr);
        async move { tokio::time::timeout(Duration::from_millis(300), mgr.ready("fx")).await }
    });
    // Let the owner claim the slot before the second caller looks at it.
    wait_for_status(&mgr, "fx", UpstreamStatus::Connecting).await;

    let parked = tokio::spawn({
        let mgr = Arc::clone(&mgr);
        async move { tokio::time::timeout(Duration::from_secs(5), mgr.ready("fx")).await }
    });

    assert!(
        owner.await.unwrap().is_err(),
        "the owner should have timed out"
    );
    // Woken by the abandoned claim, the parked caller runs its own ladder
    // against the same never-answering fixture and fails on the connect
    // timeout — the point is that it is not still parked.
    assert!(
        matches!(parked.await.unwrap(), Ok(Err(UpstreamError::Failed { .. }))),
        "the parked caller never recovered"
    );
    mgr.shutdown().await;
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

/// The demotion an http upstream needs must not reach a stdio child, whose
/// death rmcp reports on its own. That path is unchanged: the slot reads
/// Idle-with-history and the next demand gets the whole ladder, not the
/// single latched attempt a `Failed` slot would have been given.
#[tokio::test]
async fn a_dead_child_stays_on_the_ladder_instead_of_latching_failed() {
    let mgr = manager(&[("fx", stdio_server("die-on-tools"))]);
    let first = mgr.ready("fx").await.unwrap();

    // The fixture dies on this request, so the call fails on a transport
    // that is on its way down.
    let err = mgr
        .call(
            "fx",
            |service| async move { service.list_all_tools().await },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, CallError::Service(_)),
        "the upstream was reached, so this is the request failing: {err}"
    );
    assert!(
        !matches!(mgr.status("fx").await, Some(UpstreamStatus::Failed(_))),
        "a dead child must not latch the slot"
    );

    // The transport task has to observe the child's death first.
    wait_for_status(&mgr, "fx", UpstreamStatus::Idle).await;
    let second = mgr.ready("fx").await.unwrap();
    assert!(!Arc::ptr_eq(&first, &second), "must be a fresh instance");
    mgr.shutdown().await;
}

/// A bare http server that answers every request `401` with an RFC 9728
/// challenge, plus the count of requests it has seen. It speaks no MCP at
/// all, which is exactly what an OAuth-protected server does to a client
/// with no token.
fn unauthorized_server() -> (std::net::SocketAddr, Arc<std::sync::atomic::AtomicUsize>) {
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    let app = axum::Router::new().fallback(move || {
        let counter = Arc::clone(&counter);
        async move {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            (
                axum::http::StatusCode::UNAUTHORIZED,
                [(
                    axum::http::header::WWW_AUTHENTICATE,
                    "Bearer resource_metadata=\"https://auth.example.com/.well-known/\
                     oauth-protected-resource\"",
                )],
            )
        }
    });
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        axum::serve(listener, app).await.unwrap();
    });
    (addr, hits)
}

/// The whole point of the state: a server behind OAuth is not a broken
/// server, and the ladder has nothing to offer it.
#[tokio::test]
async fn a_401_upstream_is_auth_required_and_never_laddered() {
    let (addr, hits) = unauthorized_server();
    let mgr = manager(&[("linear", http_server(&format!("http://{addr}/mcp"), &[]))]);

    let started = Instant::now();
    let err = mgr.ready("linear").await.unwrap_err();
    let elapsed = started.elapsed();

    let UpstreamError::AuthRequired {
        name,
        resource_metadata,
    } = &err
    else {
        panic!("expected AuthRequired, got {err}");
    };
    assert_eq!(name, "linear");
    assert_eq!(
        resource_metadata.as_deref(),
        Some("https://auth.example.com/.well-known/oauth-protected-resource")
    );
    // The text a client is shown has to name the command that fixes it.
    assert_eq!(
        err.to_string(),
        "upstream \"linear\" needs OAuth; run mcpgw auth login linear on this machine"
    );
    assert_eq!(
        mgr.status("linear").await,
        Some(UpstreamStatus::AuthRequired {
            resource_metadata: Some(
                "https://auth.example.com/.well-known/oauth-protected-resource".to_owned()
            )
        })
    );
    // No ladder: three attempts would have slept 50ms + 100ms between them,
    // and the server would have counted a second and a third handshake.
    assert!(elapsed < Duration::from_millis(150), "elapsed {elapsed:?}");
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Latched like `Failed`: one fresh attempt per new demand, still no
    // ladder, because the credential may have arrived in between.
    let err = mgr.ready("linear").await.unwrap_err();
    assert!(matches!(err, UpstreamError::AuthRequired { .. }), "{err}");
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
    mgr.shutdown().await;
}

/// The other half of the rule: only a 401 is a 401. A server that answers
/// and then fails, or one that never answers at all, is the failure the
/// ladder exists for and must not be reported as a missing login.
#[tokio::test]
async fn a_live_server_that_fails_is_never_auth_required() {
    let mgr = manager(&[
        // Answers the handshake, then dies on the first request.
        ("live", stdio_server("die-on-tools")),
        // Never answers at all.
        ("gone", http_server("http://127.0.0.1:1/mcp", &[])),
    ]);

    let service = mgr.ready("live").await.unwrap();
    let err = service.list_all_tools().await.unwrap_err();
    assert!(
        !matches!(
            mgr.status("live").await,
            Some(UpstreamStatus::AuthRequired { .. })
        ),
        "a JSON-RPC/transport failure is not a missing login: {err}"
    );

    let err = mgr.ready("gone").await.unwrap_err();
    assert!(
        matches!(err, UpstreamError::Failed { attempts: 3, .. }),
        "{err}"
    );
    assert!(matches!(
        mgr.status("gone").await,
        Some(UpstreamStatus::Failed(_))
    ));
    mgr.shutdown().await;
}

/// An http upstream whose headers come from `mcpgw-test-server headers`,
/// reading `token_file`.
fn helper_server(url: &str, mode: &str, arg: Option<&std::path::Path>) -> Server {
    let mut argv = vec![
        env!("CARGO_BIN_EXE_mcpgw-test-server").to_owned(),
        mode.to_owned(),
    ];
    if let Some(arg) = arg {
        argv.push(arg.display().to_string());
    }
    Server {
        enabled: true,
        tags: Vec::new(),
        calls_per_minute: 0,
        tools: None,
        transport: Transport::Http {
            url: url.to_owned(),
            headers_command: argv,
            headers: BTreeMap::new(),
            auth: None,
        },
    }
}

/// One in-process MCP server behind a bearer token the test can change under
/// a live connection — a server whose credential rotates, which is the whole
/// situation `headers_command` exists for.
struct Guarded {
    addr: std::net::SocketAddr,
    /// What the guard accepts right now. Swapping it is how a test expires a
    /// token without touching the connection holding it.
    expected: Arc<std::sync::Mutex<String>>,
    inner: Arc<UpstreamManager>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Guarded {
    async fn start(token: &str) -> Self {
        use rmcp_client_http::transport::streamable_http_server::session::local::LocalSessionManager;
        use rmcp_client_http::transport::streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService,
        };

        // `legacy` rather than `healthy`: the healthy fixture marks its
        // tools/list cacheable for four seconds and the pipe in front of it
        // forwards that faithfully, so a second list would be answered out of
        // the client's own cache — without the request whose credential these
        // tests are about.
        let inner = Arc::new(manager(&[("fx", stdio_server("legacy"))]));
        let gateway = mcpgw_core::gateway::Gateway::new(Arc::clone(&inner), "fx".to_owned());
        let service = StreamableHttpService::new(
            move || Ok(gateway.clone()),
            LocalSessionManager::default().into(),
            StreamableHttpServerConfig::default(),
        );
        let expected = Arc::new(std::sync::Mutex::new(token.to_owned()));
        let guard = Arc::clone(&expected);
        let app =
            axum::Router::new()
                .nest_service("/mcp", service)
                .layer(axum::middleware::from_fn(
                    move |request: axum::extract::Request, next: axum::middleware::Next| {
                        let guard = Arc::clone(&guard);
                        async move {
                            let want = format!("Bearer {}", guard.lock().unwrap());
                            let got = request
                                .headers()
                                .get(axum::http::header::AUTHORIZATION)
                                .and_then(|value| value.to_str().ok())
                                .unwrap_or_default()
                                .to_owned();
                            if got == want {
                                return next.run(request).await;
                            }
                            // The challenge header is not decoration: rmcp only
                            // reports a 401 as an authorization failure when the
                            // server sent one.
                            (
                                axum::http::StatusCode::UNAUTHORIZED,
                                [(
                                    axum::http::header::WWW_AUTHENTICATE,
                                    "Bearer realm=\"corp\"",
                                )],
                            )
                                .into_response()
                        }
                    },
                ));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(axum::serve(listener, app).into_future());
        Self {
            addr,
            expected,
            inner,
            task,
        }
    }

    fn url(&self) -> String {
        format!("http://{}/mcp", self.addr)
    }

    /// Expires the credential the connection was opened with.
    fn rotate(&self, token: &str) {
        token.clone_into(&mut self.expected.lock().unwrap());
    }

    async fn stop(self) {
        self.task.abort();
        self.inner.shutdown().await;
    }
}

/// A token file holding `tokens`, one per line, in the order the command
/// hands them out.
fn token_file(dir: &tempfile::TempDir, tokens: &[&str]) -> std::path::PathBuf {
    let path = dir.path().join("tokens");
    std::fs::write(&path, tokens.join("\n")).unwrap();
    path
}

async fn list_tools(mgr: &UpstreamManager, name: &str) -> Result<usize, CallError> {
    mgr.call(
        name,
        |service| async move { service.list_all_tools().await },
    )
    .await
    .map(|tools| tools.len())
}

/// The base case: the connection is opened with what the command printed,
/// not with anything written in the config.
#[tokio::test]
async fn the_first_connect_uses_the_headers_the_command_printed() {
    let remote = Guarded::start("alpha").await;
    let dir = tempfile::tempdir().unwrap();
    let tokens = token_file(&dir, &["alpha"]);
    let mgr = manager(&[(
        "corp",
        helper_server(&remote.url(), "headers", Some(&tokens)),
    )]);

    assert!(list_tools(&mgr, "corp").await.unwrap() > 0);
    assert_eq!(mgr.status("corp").await, Some(UpstreamStatus::Ready));
    mgr.shutdown().await;
    remote.stop().await;
}

/// The point of the feature. A token that expires under a live connection
/// costs one failed call; the next one reruns the command, reconnects with
/// what it prints now, and works — no restart, and nothing latched.
#[tokio::test]
async fn a_rotated_token_is_picked_up_on_the_next_call() {
    let remote = Guarded::start("alpha").await;
    let dir = tempfile::tempdir().unwrap();
    let tokens = token_file(&dir, &["alpha", "beta"]);
    let mgr = manager(&[(
        "corp",
        helper_server(&remote.url(), "headers", Some(&tokens)),
    )]);

    assert!(list_tools(&mgr, "corp").await.unwrap() > 0);

    // The server rotates; the connection mcpgw is holding still presents the
    // credential it was opened with.
    remote.rotate("beta");
    let err = list_tools(&mgr, "corp").await.unwrap_err();
    assert!(matches!(err, CallError::Service(_)), "{err}");
    // Released rather than latched: the server is not down, so the next
    // demand is owed a fresh connection and not a single grudging attempt.
    assert_eq!(mgr.status("corp").await, Some(UpstreamStatus::Idle));

    assert!(
        list_tools(&mgr, "corp").await.unwrap() > 0,
        "the second call should have rerun the command"
    );
    assert_eq!(mgr.status("corp").await, Some(UpstreamStatus::Ready));
    mgr.shutdown().await;
    remote.stop().await;
}

/// A 401 at connect time is worth exactly one more run of the command: the
/// token minted a moment ago may already be the stale one.
#[tokio::test]
async fn a_401_at_connect_gets_one_more_run_of_the_command() {
    let remote = Guarded::start("fresh").await;
    let dir = tempfile::tempdir().unwrap();
    let tokens = token_file(&dir, &["stale", "fresh"]);
    let mgr = manager(&[(
        "corp",
        helper_server(&remote.url(), "headers", Some(&tokens)),
    )]);

    assert!(list_tools(&mgr, "corp").await.unwrap() > 0);
    // Both tokens were handed out, so the connect that succeeded is the
    // second one — the first was refused and the command run again.
    assert_eq!(std::fs::read_to_string(&tokens).unwrap().trim(), "fresh");
    mgr.shutdown().await;
    remote.stop().await;
}

/// A command that cannot produce headers is its own outcome: named, not
/// laddered, and carrying the command's stderr rather than its stdout.
#[tokio::test]
async fn a_failing_headers_command_is_reported_without_a_ladder() {
    let remote = Guarded::start("alpha").await;
    let mgr = manager(&[("corp", helper_server(&remote.url(), "headers-fail", None))]);

    let started = Instant::now();
    let err = mgr.ready("corp").await.unwrap_err();
    let elapsed = started.elapsed();

    let UpstreamError::HeadersCommand { name, message } = &err else {
        panic!("expected HeadersCommand, got {err}");
    };
    assert_eq!(name, "corp");
    assert!(message.contains("mcpgw-test-server"), "{message}");
    // The command's own diagnosis, which is what a user needs and is not
    // where the token is.
    assert!(message.contains("no vault session"), "{message}");
    // One run, not three: the ladder has nothing to offer a command that
    // exits non-zero.
    assert!(elapsed < Duration::from_secs(5), "elapsed {elapsed:?}");
    assert!(matches!(
        mgr.status("corp").await,
        Some(UpstreamStatus::Failed(_))
    ));
    mgr.shutdown().await;
    remote.stop().await;
}

/// A helper that blocks — on a login prompt, on a network call nobody will
/// answer — is killed at the ceiling rather than hanging every connect
/// behind it.
#[tokio::test]
async fn a_hanging_headers_command_is_killed_at_the_timeout() {
    let remote = Guarded::start("alpha").await;
    let mgr = manager(&[("corp", helper_server(&remote.url(), "headers-hang", None))])
        .with_headers_timeout(Duration::from_millis(200));

    let started = Instant::now();
    let err = mgr.ready("corp").await.unwrap_err();
    let elapsed = started.elapsed();

    assert!(
        matches!(&err, UpstreamError::HeadersCommand { message, .. } if message.contains("did not finish")),
        "{err}"
    );
    assert!(elapsed < Duration::from_secs(5), "elapsed {elapsed:?}");
    mgr.shutdown().await;
    remote.stop().await;
}

/// Output that is not a JSON object is a refusal, and the refusal does not
/// quote what was printed — a half-written token is still a token.
#[tokio::test]
async fn unparseable_output_is_refused_without_quoting_it() {
    let remote = Guarded::start("alpha").await;
    let mgr = manager(&[(
        "corp",
        helper_server(&remote.url(), "headers-garbage", None),
    )]);

    let err = mgr.ready("corp").await.unwrap_err();
    let message = err.to_string();
    assert!(message.contains("JSON object of header names"), "{message}");
    assert!(!message.contains("not-json"), "{message}");
    mgr.shutdown().await;
    remote.stop().await;
}
