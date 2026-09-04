//! Config hot reload against a running gateway: the endpoint table and the
//! upstream children both follow the config file, and nothing already in
//! flight is disturbed while they do.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use mcpgw_core::endpoints::{EndpointTable, Endpoints, endpoint_path};
use mcpgw_core::gateway::serve_http_with;
use mcpgw_core::reload::Reloader;
use mcpgw_core::upstream::{UpstreamManager, UpstreamStatus};
use rmcp_client_http::ServiceExt as _;
use rmcp_client_http::transport::StreamableHttpClientTransport;

type Client = rmcp_client_http::service::RunningService<rmcp_client_http::RoleClient, ()>;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mcpgw-test-server"))
}

/// A config naming one `[servers.<name>]` per entry, in the shape
/// `mcpgw add` writes.
fn config(servers: &[(&str, &str, bool)]) -> String {
    use std::fmt::Write as _;

    let mut text = "version = 1\n".to_owned();
    for (name, mode, enabled) in servers {
        let _ = write!(
            text,
            "\n[servers.{name}]\ntype = \"stdio\"\ncommand = '{}'\nargs = [\"{mode}\"]\nenabled = {enabled}\n",
            fixture().display()
        );
    }
    text
}

/// Writes `text` to `path` the way [`mcpgw_core::ConfigStore`] does: a temp
/// file renamed over the target. The rename is the point — it replaces the
/// inode, which is exactly what a naive file watch would fail to notice.
fn write(path: &Path, text: &str) {
    let temp = path.with_extension("toml.tmp");
    std::fs::write(&temp, text).unwrap();
    std::fs::rename(&temp, path).unwrap();
}

struct Harness {
    addr: std::net::SocketAddr,
    path: PathBuf,
    reloader: Reloader,
    manager: Arc<UpstreamManager>,
    // Held so the config file outlives the test.
    _dir: tempfile::TempDir,
}

impl Harness {
    /// Boots a gateway serving `servers`, through the same `apply` a reload
    /// goes through.
    async fn start(servers: &[(&str, &str, bool)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write(&path, &config(servers));

        let manager = Arc::new(
            UpstreamManager::new(std::collections::BTreeMap::new())
                .with_connect_timeout(Duration::from_secs(30))
                .with_backoff_base(Duration::from_millis(20)),
        );
        let endpoints = Endpoints::new(EndpointTable::new(Vec::new()));
        let reloader = Reloader::new(path.clone(), Arc::clone(&manager), endpoints.clone());
        reloader.reload().await.unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve_http_with(endpoints, listener, std::future::pending()));
        Self {
            addr,
            path,
            reloader,
            manager,
            _dir: dir,
        }
    }

    /// Rewrites the config file and reloads it, the way the watcher would
    /// once its poll noticed. Returns whatever the reload made of the file.
    async fn reload(&self, text: &str) -> Result<Vec<String>, mcpgw_core::Error> {
        write(&self.path, text);
        self.reloader.reload().await.map(|done| done.serving)
    }

    async fn set(&self, servers: &[(&str, &str, bool)]) -> Vec<String> {
        self.reload(&config(servers)).await.unwrap()
    }

    async fn client(&self, path: &str) -> Client {
        let url = format!("http://{}{path}", self.addr);
        ().serve(StreamableHttpClientTransport::from_uri(url))
            .await
            .unwrap()
    }

    async fn tools(&self, path: &str) -> Vec<String> {
        let client = self.client(path).await;
        let tools = client.list_all_tools().await.unwrap();
        let names = tools.iter().map(|t| t.name.to_string()).collect();
        client.cancel().await.unwrap();
        names
    }

    /// The raw response to a bare POST at `path`, headers and all.
    async fn raw_post(&self, path: &str) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\n\
             Accept: application/json, text/event-stream\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            self.addr,
            body.len()
        );
        let mut stream = tokio::net::TcpStream::connect(self.addr).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        String::from_utf8_lossy(&response).into_owned()
    }
}

/// Calls the `pid` fixture's one tool and returns the process id it reports.
/// A server that was restarted answers with a different one.
async fn pid(client: &Client, tool: &str) -> String {
    let request = rmcp_client_http::model::CallToolRequestParams::new(tool.to_owned());
    let result = client.call_tool(request).await.unwrap();
    let text = format!("{result:?}");
    let digits: String = text
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect();
    assert!(!digits.is_empty(), "no process id in {text}");
    digits
}

/// The whole point of the milestone: `mcpgw add` while `serve` runs.
#[tokio::test]
async fn a_server_added_to_the_config_is_served_without_a_restart() {
    let gateway = Harness::start(&[("fx1", "healthy", true)]).await;
    assert!(
        gateway
            .raw_post(&endpoint_path("fx2"))
            .await
            .contains("404")
    );

    let serving = gateway
        .set(&[("fx1", "healthy", true), ("fx2", "healthy", true)])
        .await;
    assert_eq!(serving, ["fx1", "fx2"]);

    // The endpoint route is registered once, for the whole process: a server
    // that was not in the config when it was built answering on it is what
    // proves the swap reaches the running router rather than a fresh one.
    assert_eq!(
        gateway.tools(&endpoint_path("fx2")).await,
        ["echo", "reverse"]
    );
    // The base endpoint is not part of any of this and says so before and
    // after: it fronts no server, so a reload cannot change what it serves.
    assert!(gateway.tools("/mcp").await.is_empty());
    gateway.manager.shutdown().await;
}

#[tokio::test]
async fn a_server_removed_from_the_config_404s_and_names_what_is_left() {
    let gateway = Harness::start(&[("fx1", "healthy", true), ("fx2", "healthy", true)]).await;
    assert_eq!(
        gateway.tools(&endpoint_path("fx2")).await,
        ["echo", "reverse"]
    );

    gateway.set(&[("fx1", "healthy", true)]).await;

    let response = gateway.raw_post(&endpoint_path("fx2")).await;
    assert!(response.contains("404"), "{response}");
    // The 404 has to list the *new* table: a stale client config is the
    // usual cause, and the answer to it is the names actually served now.
    assert!(response.contains("known endpoints: /s/fx1"), "{response}");
    assert!(!response.contains("/s/fx2"), "{response}");
    gateway.manager.shutdown().await;
}

#[tokio::test]
async fn a_disabled_server_drops_out() {
    let gateway = Harness::start(&[("fx1", "healthy", true), ("fx2", "healthy", true)]).await;
    let serving = gateway
        .set(&[("fx1", "healthy", true), ("fx2", "healthy", false)])
        .await;
    assert_eq!(serving, ["fx1"]);

    assert!(
        gateway
            .raw_post(&endpoint_path("fx2"))
            .await
            .contains("404")
    );
    // Still configured, just not served: the manager knows the name and
    // refuses it, rather than reporting it as unknown.
    assert_eq!(
        gateway.manager.status("fx2").await,
        Some(UpstreamStatus::Idle)
    );
    gateway.manager.shutdown().await;
}

/// The reload's central promise to everything already running: a server the
/// edit did not mention keeps the child process it had. Proven by process id,
/// because "still Ready" would also be true of a silent restart.
#[tokio::test]
async fn an_untouched_server_keeps_its_child_across_a_reload() {
    let gateway = Harness::start(&[("keep", "pid", true)]).await;
    let client = gateway.client(&endpoint_path("keep")).await;
    let before = pid(&client, "pid").await;

    gateway
        .set(&[("keep", "pid", true), ("sibling", "healthy", true)])
        .await;

    let after = pid(&client, "pid").await;
    assert_eq!(before, after, "the untouched server was restarted");
    assert_eq!(
        gateway.tools(&endpoint_path("sibling")).await,
        ["echo", "reverse"]
    );
    client.cancel().await.unwrap();
    gateway.manager.shutdown().await;
}

/// A transport edit is the one change that cannot be applied in place, so it
/// is the one that must restart the child.
#[tokio::test]
async fn a_changed_transport_restarts_the_child() {
    let gateway = Harness::start(&[("fx", "pid", true)]).await;
    let client = gateway.client(&endpoint_path("fx")).await;
    let before = pid(&client, "pid").await;
    client.cancel().await.unwrap();

    // Same command, different argv: a different server as far as the process
    // is concerned.
    let text = config(&[("fx", "pid", true)]).replace("[\"pid\"]", "[\"pid\", \"--v2\"]");
    gateway.reload(&text).await.unwrap();

    let client = gateway.client(&endpoint_path("fx")).await;
    let after = pid(&client, "pid").await;
    assert_ne!(before, after, "the transport changed but the child did not");
    client.cancel().await.unwrap();
    gateway.manager.shutdown().await;
}

/// Fat-fingered TOML must never be able to take a working gateway down.
#[tokio::test]
async fn a_config_that_does_not_parse_keeps_the_old_table_serving() {
    let gateway = Harness::start(&[("fx1", "healthy", true)]).await;

    let err = gateway
        .reload("version = 1\n[servers.fx1\ntype = \"stdio\"\n")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("config.toml"), "{err}");

    assert_eq!(
        gateway.tools(&endpoint_path("fx1")).await,
        ["echo", "reverse"]
    );

    // And the gateway is not wedged: fixing the file still reloads.
    let serving = gateway
        .set(&[("fx1", "healthy", true), ("fx2", "healthy", true)])
        .await;
    assert_eq!(serving, ["fx1", "fx2"]);
    gateway.manager.shutdown().await;
}

/// THE invariant: a reload swaps handles, it does not mutate live
/// connections. A call already forwarded upstream when the table is replaced
/// — here by a reload that removes the very server it is talking to — still
/// gets its answer from the process it started on.
#[tokio::test]
async fn a_call_in_flight_completes_against_the_old_service() {
    let gateway = Harness::start(&[("going", "pid", true), ("staying", "pid", true)]).await;
    let client = gateway.client(&endpoint_path("going")).await;
    // Connects the upstream, so the call below is upstream-bound rather than
    // spending its time in a connect ladder.
    let before = pid(&client, "pid").await;

    let request = rmcp_client_http::model::CallToolRequestParams::new("pid".to_owned());
    let in_flight = tokio::spawn(async move {
        let result = client.call_tool(request).await;
        (client, result)
    });
    // The fixture holds a `pid` call for half a second; a tenth of that is
    // enough to be inside it without racing the answer.
    tokio::time::sleep(Duration::from_millis(50)).await;
    gateway.set(&[("staying", "pid", true)]).await;

    let (client, result) = in_flight.await.unwrap();
    let text = format!("{:?}", result.unwrap());
    assert!(
        text.contains(&before),
        "the in-flight call should have been answered by the process it \
         started on ({before}), got {text}"
    );

    // The endpoint is gone for anything that starts *after* the swap.
    assert!(
        gateway
            .raw_post(&endpoint_path("going"))
            .await
            .contains("404")
    );
    client.cancel().await.unwrap();
    gateway.manager.shutdown().await;
}

/// A reload that changes nothing must be a no-op — no restarts, no
/// reconnects, nothing for the poll to churn every two seconds.
#[tokio::test]
async fn reloading_an_unchanged_config_changes_nothing() {
    let gateway = Harness::start(&[("fx", "healthy", true)]).await;
    assert_eq!(
        gateway.tools(&endpoint_path("fx")).await,
        ["echo", "reverse"]
    );
    assert_eq!(
        gateway.manager.status("fx").await,
        Some(UpstreamStatus::Ready)
    );

    let done = gateway.reloader.reload().await.unwrap();
    assert!(done.changes.is_empty(), "{:?}", done.changes);
    assert_eq!(
        gateway.manager.status("fx").await,
        Some(UpstreamStatus::Ready)
    );
    gateway.manager.shutdown().await;
}

/// `serve --server a` is a filter over the config, not a second source of
/// truth: what it names still has to exist and be enabled there.
#[tokio::test]
async fn a_selection_keeps_filtering_across_reloads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    write(&path, &config(&[("fx1", "healthy", true)]));

    let manager = Arc::new(UpstreamManager::new(std::collections::BTreeMap::new()));
    let endpoints = Endpoints::new(EndpointTable::new(Vec::new()));
    let reloader = Reloader::new(path.clone(), Arc::clone(&manager), endpoints)
        .with_selection(vec!["fx1".to_owned()]);
    assert_eq!(reloader.reload().await.unwrap().serving, ["fx1"]);

    // A server added to the config is not served: the selection did not
    // ask for it.
    write(
        &path,
        &config(&[("fx1", "healthy", true), ("fx2", "healthy", true)]),
    );
    assert_eq!(reloader.reload().await.unwrap().serving, ["fx1"]);

    // A selected server that goes away drops out rather than taking the
    // gateway down with it.
    write(&path, &config(&[("fx2", "healthy", true)]));
    assert!(reloader.reload().await.unwrap().serving.is_empty());
}

/// The watcher's own contract: notice the file changed, on its own, without
/// anyone calling `reload`.
#[tokio::test]
async fn the_watcher_picks_up_a_rename_over_the_config() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    write(&path, &config(&[("fx1", "healthy", true)]));

    let manager = Arc::new(UpstreamManager::new(std::collections::BTreeMap::new()));
    let endpoints = Endpoints::new(EndpointTable::new(Vec::new()));
    let reloader = Reloader::new(path.clone(), Arc::clone(&manager), endpoints);
    reloader.reload().await.unwrap();

    let stop = Arc::new(tokio::sync::Notify::new());
    let watcher = tokio::spawn({
        let stop = Arc::clone(&stop);
        // A poll far shorter than the shipped 2s: this test is about the
        // watcher noticing, not about how often it looks.
        async move {
            reloader
                .watch(Duration::from_millis(20), async move {
                    stop.notified().await;
                })
                .await;
        }
    });

    write(
        &path,
        &config(&[("fx1", "healthy", true), ("fx2", "healthy", true)]),
    );

    // Poll with a deadline rather than sleeping for a guessed interval: a
    // loaded CI runner may take a while to get round to the watcher.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if manager.status("fx2").await.is_some() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the watcher never picked the new server up"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    stop.notify_one();
    watcher.await.unwrap();
    manager.shutdown().await;
}

/// SIGHUP is the impatient operator's reload. Proven with a poll interval so
/// long that only the signal can be what fired.
#[cfg(unix)]
#[tokio::test]
async fn a_sighup_reloads_without_waiting_for_the_poll() {
    // Registered before anything is raised: SIGHUP's default disposition is
    // to kill the process, and this handler is what replaces it.
    let _installed = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .expect("a SIGHUP handler");

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    write(&path, &config(&[("fx1", "healthy", true)]));

    let manager = Arc::new(UpstreamManager::new(std::collections::BTreeMap::new()));
    let endpoints = Endpoints::new(EndpointTable::new(Vec::new()));
    let reloader = Reloader::new(path.clone(), Arc::clone(&manager), endpoints);
    reloader.reload().await.unwrap();

    let stop = Arc::new(tokio::sync::Notify::new());
    let watcher = tokio::spawn({
        let stop = Arc::clone(&stop);
        async move {
            reloader
                .watch(Duration::from_secs(3600), async move {
                    stop.notified().await;
                })
                .await;
        }
    });

    write(
        &path,
        &config(&[("fx1", "healthy", true), ("fx2", "healthy", true)]),
    );

    // Re-sent rather than sent once: the watcher may not have installed its
    // own handler yet, and a signal delivered before it did is simply lost.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        std::process::Command::new("kill")
            .args(["-HUP", &std::process::id().to_string()])
            .status()
            .unwrap();
        if manager.status("fx2").await.is_some() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "SIGHUP never reloaded the config"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    stop.notify_one();
    watcher.await.unwrap();
    manager.shutdown().await;
}

/// A downstream client that records the list-changed notifications it is
/// sent — see the same helper in `gateway.rs`, kept per-suite because the
/// two harnesses share no test crate.
#[derive(Clone)]
struct Listening(tokio::sync::mpsc::UnboundedSender<&'static str>);

impl rmcp_client_http::ClientHandler for Listening {
    async fn on_tool_list_changed(
        &self,
        _context: rmcp_client_http::service::NotificationContext<rmcp_client_http::RoleClient>,
    ) {
        let _ = self.0.send("tools");
    }
}

/// A transport swap retires the child and dials a fresh one, so whatever the
/// new command lists is a different list by definition. A client sitting on
/// the endpoint has to be told, or the whole reason hot reload exists — not
/// having to restart the editor — is lost one layer down (issue #140).
#[tokio::test]
async fn swapping_a_servers_transport_tells_the_connected_client() {
    let gateway = Harness::start(&[("fx", "bump", true)]).await;
    // The endpoint advertises what it last heard, so the capability the
    // session listens on only exists once the server has been reached.
    assert!(
        gateway
            .tools(&endpoint_path("fx"))
            .await
            .contains(&"bump".to_owned())
    );

    let (heard, mut queue) = tokio::sync::mpsc::unbounded_channel();
    let url = format!("http://{}{}", gateway.addr, endpoint_path("fx"));
    let client = Listening(heard)
        .serve(StreamableHttpClientTransport::from_uri(url))
        .await
        .unwrap();

    // The stream a session's notifications ride is a second connection the
    // client opens on its own, after `initialize` returns, and it hands back
    // nothing to await on. This is the pause that lets it get there, and it
    // is a property of the test client rather than of the gateway.
    tokio::time::sleep(Duration::from_millis(500)).await;
    // A different fixture mode is a different `args`, which is a transport
    // change: the old child is retired and a new one takes its place.
    let replaced = gateway.set(&[("fx", "healthy", true)]).await;
    assert_eq!(replaced, ["fx"]);

    let what = tokio::time::timeout(Duration::from_secs(10), queue.recv())
        .await
        .expect("the reload told nobody")
        .unwrap();
    assert_eq!(what, "tools");

    let tools = client.list_all_tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(names, ["echo", "reverse"]);

    client.cancel().await.unwrap();
    gateway.manager.shutdown().await;
}

/// A tool list edited under a running gateway takes effect on the next
/// request — including on a session that was opened before the edit — and
/// without the server behind it being restarted, which is what would make an
/// allowlist change cost every client its connection.
#[tokio::test]
async fn a_new_tool_list_applies_without_reconnecting_anything() {
    let harness = Harness::start(&[("fx", "healthy", true)]).await;
    let path = endpoint_path("fx");
    // Opened before the edit and kept across it: what the filter does to an
    // already-connected client is the half a fresh client cannot show.
    let client = harness.client(&path).await;
    assert_eq!(harness.tools(&path).await, ["echo", "reverse"]);

    let mut text = config(&[("fx", "healthy", true)]);
    text.push_str("\n[servers.fx.tools]\nallow = [\"echo\"]\n");
    write(&harness.path, &text);
    let reloaded = harness.reloader.reload().await.unwrap();
    // Nothing added, removed, replaced or stopped: the transport did not
    // change, so the child process behind the endpoint is the same one.
    assert!(reloaded.changes.is_empty(), "{:?}", reloaded.changes);
    assert_eq!(reloaded.serving, ["fx"]);
    assert_eq!(harness.tools(&path).await, ["echo"]);

    // The session that predates the edit is filtered too. Asserted on a call
    // rather than on a list: an MCP client is entitled to reuse the listing
    // it already fetched, and this one does.
    let err = client
        .call_tool(rmcp_client_http::model::CallToolRequestParams::new(
            "reverse".to_owned(),
        ))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("is not allowed"), "{err}");

    // And back again, with nothing restarted either way.
    write(&harness.path, &config(&[("fx", "healthy", true)]));
    let reloaded = harness.reloader.reload().await.unwrap();
    assert!(reloaded.changes.is_empty(), "{:?}", reloaded.changes);
    assert_eq!(harness.tools(&path).await, ["echo", "reverse"]);
    client
        .call_tool(rmcp_client_http::model::CallToolRequestParams::new(
            "reverse".to_owned(),
        ))
        .await
        .unwrap();

    client.cancel().await.unwrap();
    harness.manager.shutdown().await;
}
