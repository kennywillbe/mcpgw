//! The OAuth broker against a real, in-process authorization server.
//!
//! No browser and no network: `/authorize` redirects straight back to the
//! loopback listener the login bound, which is what a browser does once the
//! consent screen is answered. Everything the flow is actually judged on
//! travels the wire either way — PKCE `S256`, the `state` round trip, RFC 9207
//! `iss`, refresh rotation, and which of the three client identities was
//! presented.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use mcpgw_core::auth::{self, Tokens};
use mcpgw_core::upstream::{UpstreamError, UpstreamManager};
use mcpgw_core::{Server, Transport};
use mcpgw_test_server::oauth;

/// The whole of what the login needs from the caller: somewhere to put the
/// tokens, and a server to log in to.
struct Fixture {
    provider: oauth::Provider,
    state: tempfile::TempDir,
}

impl Fixture {
    async fn start(config: oauth::Config) -> Self {
        let pending = oauth::bind(config).await;
        let challenge = pending.challenge();
        Self {
            provider: pending.serve(oauth::refusing_resource(challenge)),
            state: tempfile::tempdir().expect("a scratch state dir"),
        }
    }

    fn state_dir(&self) -> &std::path::Path {
        self.state.path()
    }

    async fn login(&self, client_id: Option<&str>) -> Result<Tokens, auth::Error> {
        let url = self.provider.mcp_url();
        let request = auth::Login {
            server: "linear",
            url: &url,
            state_dir: self.state_dir(),
            client_id,
            client_secret: None,
            scopes: &[],
            challenge: None,
            timeout: Duration::from_secs(30),
        };
        // The "browser": one plain GET at the authorization URL, following the
        // redirect into the loopback listener. A real browser does the same
        // two requests with a consent screen in between.
        auth::login(&request, follow).await
    }
}

/// Walks the authorization URL the way a browser would, in a thread of its
/// own so the login is already waiting on its listener when the redirect
/// lands.
fn follow(url: &str) {
    let url = url.to_owned();
    std::thread::spawn(move || {
        // Errors are deliberately swallowed: a login that never receives its
        // callback fails on its own timeout with a message about the browser,
        // which is the failure a test wants to read.
        let _ = ureq::get(&url).call();
    });
}

#[tokio::test]
async fn a_login_presents_the_published_client_id_metadata_document() {
    let fx = Fixture::start(oauth::Config::default()).await;
    let tokens = fx.login(None).await.expect("the login completes");

    // The identity a provider that supports CIMD is shown: an https URL it
    // can fetch, not a client record it had to mint.
    assert_eq!(
        fx.provider.recorder.client_id().as_deref(),
        Some(auth::CLIENT_ID_URL)
    );
    assert_eq!(tokens.identity(None), auth::Identity::Cimd);
    assert_eq!(tokens.issuer(), Some(fx.provider.base.as_str()));
    assert_eq!(fx.provider.recorder.exchanges.load(SEQ), 1);
    // The loopback literal, on a port the OS picked — never `localhost`, and
    // never a fixed port something else could already hold.
    let redirect = fx.provider.recorder.redirect_uri().expect("a redirect uri");
    assert!(redirect.starts_with("http://127.0.0.1:"), "{redirect}");
    assert!(redirect.ends_with("/callback"), "{redirect}");
    fx.provider.stop();
}

/// The fallback, and the reason it still exists: 2026-07-28 deprecated
/// registration, and Notion, Sentry and Cloudflare still only offer it.
#[tokio::test]
async fn registration_is_the_fallback_when_the_document_is_not_supported() {
    let fx = Fixture::start(oauth::Config {
        cimd: false,
        dcr: true,
        ..oauth::Config::default()
    })
    .await;
    let tokens = fx.login(None).await.expect("the login completes");

    assert_eq!(fx.provider.recorder.registrations.load(SEQ), 1);
    let client_id = fx.provider.recorder.client_id().expect("a client id");
    assert!(client_id.starts_with("dcr-client-"), "{client_id}");
    assert_eq!(tokens.identity(None), auth::Identity::Dcr);
    fx.provider.stop();
}

/// A provider that offers both is still shown the id it issued by hand:
/// pre-registration wins, which is what makes Atlassian and GitHub work.
#[tokio::test]
async fn a_preregistered_client_id_beats_the_document_and_registration() {
    let fx = Fixture::start(oauth::Config {
        cimd: true,
        dcr: true,
        ..oauth::Config::default()
    })
    .await;
    let tokens = fx.login(Some("issued-by-hand")).await.expect("logs in");

    assert_eq!(
        fx.provider.recorder.client_id().as_deref(),
        Some("issued-by-hand")
    );
    assert_eq!(fx.provider.recorder.registrations.load(SEQ), 0);
    assert_eq!(
        tokens.identity(Some("issued-by-hand")),
        auth::Identity::Preregistered
    );
    fx.provider.stop();
}

/// The file is a bearer token on disk, and it is the only place one is
/// written.
#[tokio::test]
async fn the_token_file_is_owner_only_and_says_what_it_holds() {
    let fx = Fixture::start(oauth::Config::default()).await;
    fx.login(None).await.expect("the login completes");

    let path = auth::token_path(fx.state_dir(), "linear");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        let dir = std::fs::metadata(auth::dir(fx.state_dir()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir & 0o777, 0o700, "{dir:o}");
    }
    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(raw["version"], 1);
    assert_eq!(raw["server"], "linear");
    assert_eq!(raw["credentials"]["issuer"], fx.provider.base);
    assert_eq!(raw["credentials"]["token_response"]["access_token"], "at-2");
    assert!(raw["credentials"]["token_response"]["refresh_token"].is_string());

    // And it reads back as the record the gateway will present.
    let tokens = Tokens::load(fx.state_dir(), "linear").unwrap().unwrap();
    assert_eq!(tokens.state(), auth::TokenState::Valid);
    assert!(tokens.renewable());
    assert_eq!(tokens.scopes(), ["read"]);
    fx.provider.stop();
}

/// The login owns the whole wait: nothing arrives at the callback, and it
/// gives up rather than holding a loopback socket for the life of the
/// process.
#[tokio::test]
async fn a_login_nobody_finishes_times_out() {
    let fx = Fixture::start(oauth::Config::default()).await;
    let url = fx.provider.mcp_url();
    let request = auth::Login {
        server: "linear",
        url: &url,
        state_dir: fx.state_dir(),
        client_id: None,
        client_secret: None,
        scopes: &[],
        challenge: None,
        timeout: Duration::from_millis(200),
    };
    // The announcement prints the URL and no browser opens it, which is the
    // `--no-browser` shape on a machine where nobody clicks.
    let err = auth::login(&request, |_| ()).await.unwrap_err();
    assert!(matches!(err, auth::Error::Timeout), "{err}");
    assert!(Tokens::load(fx.state_dir(), "linear").unwrap().is_none());
    fx.provider.stop();
}

#[tokio::test]
async fn logout_deletes_the_stored_login_and_says_when_there_was_none() {
    let fx = Fixture::start(oauth::Config::default()).await;
    fx.login(None).await.expect("the login completes");
    assert!(Tokens::delete(fx.state_dir(), "linear").unwrap());
    assert!(Tokens::load(fx.state_dir(), "linear").unwrap().is_none());
    assert!(!Tokens::delete(fx.state_dir(), "linear").unwrap());
    fx.provider.stop();
}

const SEQ: std::sync::atomic::Ordering = std::sync::atomic::Ordering::SeqCst;

// ---------------------------------------------------------------------------
// The gateway half: a real MCP server behind the provider's bearer check.
// ---------------------------------------------------------------------------

/// A provider whose `/mcp` is an actual MCP server, served only to a request
/// carrying an access token the provider issued.
struct Protected {
    provider: oauth::Provider,
    inner: Arc<UpstreamManager>,
    state: tempfile::TempDir,
}

impl Protected {
    async fn start(config: oauth::Config) -> Self {
        use rmcp_client_http::transport::streamable_http_server::session::local::LocalSessionManager;
        use rmcp_client_http::transport::streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService,
        };

        let pending = oauth::bind(config).await;
        let challenge = pending.challenge();
        let recorder = Arc::clone(&pending.recorder);

        let inner = Arc::new(UpstreamManager::new(BTreeMap::from([(
            "fx".to_owned(),
            Server {
                enabled: true,
                tags: Vec::new(),
                transport: Transport::Stdio {
                    command: env!("CARGO_BIN_EXE_mcpgw-test-server").to_owned(),
                    args: vec!["healthy".to_owned()],
                    env: BTreeMap::new(),
                },
                tools: None,
                calls_per_minute: 0,
            },
        )])));
        let gateway = mcpgw_core::gateway::Gateway::new(Arc::clone(&inner), "fx".to_owned());
        let service = StreamableHttpService::new(
            move || Ok(gateway.clone()),
            LocalSessionManager::default().into(),
            StreamableHttpServerConfig::default(),
        );
        let resource =
            axum::Router::new()
                .nest_service("/mcp", service)
                .layer(axum::middleware::from_fn(
                    move |request: axum::extract::Request, next: axum::middleware::Next| {
                        let recorder = Arc::clone(&recorder);
                        let challenge = challenge.clone();
                        async move {
                            let presented = request
                                .headers()
                                .get(axum::http::header::AUTHORIZATION)
                                .and_then(|value| value.to_str().ok())
                                .and_then(|value| value.strip_prefix("Bearer "))
                                .unwrap_or_default()
                                .to_owned();
                            if recorder.accepts(&presented) {
                                return next.run(request).await;
                            }
                            // rmcp only reads a 401 as an authorization failure
                            // when the challenge header is there.
                            axum::response::IntoResponse::into_response((
                                axum::http::StatusCode::UNAUTHORIZED,
                                [(axum::http::header::WWW_AUTHENTICATE, challenge)],
                            ))
                        }
                    },
                ));
        Self {
            provider: pending.serve(resource),
            inner,
            state: tempfile::tempdir().expect("a scratch state dir"),
        }
    }

    fn manager(&self) -> UpstreamManager {
        UpstreamManager::new(BTreeMap::from([(
            "linear".to_owned(),
            Server {
                enabled: true,
                tags: Vec::new(),
                transport: Transport::Http {
                    url: self.provider.mcp_url(),
                    headers_command: Vec::new(),
                    headers: BTreeMap::new(),
                    auth: None,
                },
                tools: None,
                calls_per_minute: 0,
            },
        )]))
        .with_backoff_base(Duration::from_millis(50))
        .with_state_dir(self.state.path().to_owned())
    }

    async fn login(&self) {
        let url = self.provider.mcp_url();
        let request = auth::Login {
            server: "linear",
            url: &url,
            state_dir: self.state.path(),
            client_id: None,
            client_secret: None,
            scopes: &[],
            challenge: None,
            timeout: Duration::from_secs(30),
        };
        auth::login(&request, follow)
            .await
            .expect("the login completes");
    }

    async fn stop(self) {
        self.provider.stop();
        self.inner.shutdown().await;
    }
}

/// The point of the whole feature: after one login on this machine, every
/// client behind the gateway reaches the server.
#[tokio::test]
async fn the_gateway_reaches_a_protected_server_with_the_stored_login() {
    let remote = Protected::start(oauth::Config::default()).await;
    let manager = remote.manager();

    // Before the login there is nothing to present, and the server says so in
    // the one way that names the fix.
    let err = manager.ready("linear").await.unwrap_err();
    assert!(matches!(err, UpstreamError::AuthRequired { .. }), "{err}");

    remote.login().await;

    let tools = manager
        .call(
            "linear",
            |service| async move { service.list_all_tools().await },
        )
        .await
        .expect("the upstream answers once it has a credential");
    assert!(!tools.is_empty());
    manager.shutdown().await;
    remote.stop().await;
}

/// A refresh is the gateway's own business: no browser, no command, and one
/// token request. The provider rotates its refresh tokens, so the file has to
/// come out holding the new one — a gateway that kept the old one would work
/// once and never again.
#[tokio::test]
async fn an_expiring_token_is_refreshed_without_a_second_login() {
    let remote = Protected::start(oauth::Config {
        access_ttl: 60,
        rotate_refresh: true,
        ..oauth::Config::default()
    })
    .await;
    remote.login().await;
    let before = Tokens::load(remote.state.path(), "linear")
        .unwrap()
        .unwrap();

    // Wound back past rmcp's refresh buffer, which is what an access token
    // that has been sitting since yesterday looks like. Rewriting the clock
    // beats sleeping through a real expiry.
    let mut aged = before.clone();
    aged.credentials.token_received_at = Some(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 120,
    );
    aged.save(remote.state.path()).unwrap();
    assert_eq!(aged.state(), mcpgw_core::auth::TokenState::Renewable);

    let manager = remote.manager();
    manager
        .call(
            "linear",
            |service| async move { service.list_all_tools().await },
        )
        .await
        .expect("the refreshed token is accepted");

    assert_eq!(remote.provider.recorder.refreshes.load(SEQ), 1);
    let after = Tokens::load(remote.state.path(), "linear")
        .unwrap()
        .unwrap();
    assert_eq!(after.state(), mcpgw_core::auth::TokenState::Valid);
    assert_ne!(
        serde_json::to_string(&after.credentials).unwrap(),
        serde_json::to_string(&before.credentials).unwrap(),
        "a rotated refresh token has to reach the file"
    );
    manager.shutdown().await;
    remote.stop().await;
}

/// A grant revoked at the provider: the stored login stops working, and the
/// file stays where it is. Deleting it would leave `auth status` unable to
/// tell an expired login from one that never happened.
#[tokio::test]
async fn a_revoked_login_is_reported_again_and_the_file_is_kept() {
    let remote = Protected::start(oauth::Config::default()).await;
    remote.login().await;
    let manager = remote.manager();
    manager
        .call(
            "linear",
            |service| async move { service.list_all_tools().await },
        )
        .await
        .expect("the first call works");
    manager.shutdown().await;

    // Only the access token is cut short first: rmcp's own recovery is one
    // refresh and one retry, and a user must not be sent to a browser for a
    // token the gateway could have renewed itself.
    remote.provider.recorder.revoke_access();
    let manager = remote.manager();
    manager
        .call(
            "linear",
            |service| async move { service.list_all_tools().await },
        )
        .await
        .expect("a cut-short access token is renewed, not reported");
    manager.shutdown().await;

    remote.provider.recorder.revoke_grant();

    let manager = remote.manager();
    let err = manager.ready("linear").await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "upstream \"linear\" needs OAuth; run mcpgw auth login linear on this machine"
    );
    assert!(
        Tokens::load(remote.state.path(), "linear")
            .unwrap()
            .is_some(),
        "the login is still on disk, so a report can say it expired"
    );
    manager.shutdown().await;
    remote.stop().await;
}
