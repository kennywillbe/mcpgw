//! An in-process OAuth provider: a protected resource and the authorization
//! server that fronts it, on one origin and one port.
//!
//! One origin because that is what the real thing looks like from a client's
//! side and what rmcp insists on: it refuses a `resource_metadata` URL that is
//! not same-origin with the server that sent the challenge, and it refuses
//! authorization-server metadata on a loopback host unless the resource is on
//! one too.
//!
//! The user is not simulated with a browser. `/authorize` redirects to the
//! callback immediately, which is what a browser does after a consent screen
//! nobody has to click. Everything a login is actually asserted on — PKCE
//! `S256`, the `state` round trip, RFC 9207 `iss`, refresh rotation, which
//! client id was presented — happens on the wire either way.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::extract::{Form, Query, State};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;

/// What the provider advertises and how long its tokens last.
#[derive(Debug, Clone)]
pub struct Config {
    /// Advertise `client_id_metadata_document_supported`, which is what makes
    /// a client offer its metadata document rather than register.
    pub cimd: bool,
    /// Advertise a `registration_endpoint`, the Dynamic Client Registration
    /// fallback.
    pub dcr: bool,
    /// `expires_in` on every access token issued.
    pub access_ttl: u64,
    /// Hand out a new refresh token on every refresh, the way a provider that
    /// rotates them does. The old one stops working.
    pub rotate_refresh: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cimd: true,
            dcr: false,
            access_ttl: 3600,
            rotate_refresh: true,
        }
    }
}

/// What the provider has seen and what it currently accepts.
#[derive(Debug, Default)]
pub struct Recorder {
    pub authorizations: AtomicUsize,
    pub exchanges: AtomicUsize,
    pub refreshes: AtomicUsize,
    pub registrations: AtomicUsize,
    inner: std::sync::Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// The `client_id` presented at `/authorize`, which is the whole of what
    /// "which identity did the client use" means from this side.
    client_id: Option<String>,
    redirect_uri: Option<String>,
    scope: Option<String>,
    /// Issued codes → the PKCE challenge filed with them.
    codes: HashMap<String, String>,
    /// Access tokens the resource endpoint accepts right now.
    access: std::collections::HashSet<String>,
    /// Refresh tokens the token endpoint accepts right now.
    refresh: std::collections::HashSet<String>,
    issued: usize,
}

impl Recorder {
    /// The `client_id` of the last authorization request.
    #[must_use]
    pub fn client_id(&self) -> Option<String> {
        self.lock().client_id.clone()
    }

    /// The `redirect_uri` of the last authorization request, which is where a
    /// test reads the ephemeral loopback port the client picked.
    #[must_use]
    pub fn redirect_uri(&self) -> Option<String> {
        self.lock().redirect_uri.clone()
    }

    #[must_use]
    pub fn scope(&self) -> Option<String> {
        self.lock().scope.clone()
    }

    /// Whether the resource endpoint would accept this bearer token.
    #[must_use]
    pub fn accepts(&self, token: &str) -> bool {
        self.lock().access.contains(token)
    }

    /// Stops accepting every access token issued so far, leaving the refresh
    /// tokens alone — an access token that was cut short rather than a grant
    /// that was withdrawn.
    pub fn revoke_access(&self) {
        self.lock().access.clear();
    }

    /// Withdraws the grant: neither the access tokens nor the refresh tokens
    /// are accepted any more, which is what a user revoking mcpgw in a
    /// provider's settings does.
    pub fn revoke_grant(&self) {
        let mut inner = self.lock();
        inner.access.clear();
        inner.refresh.clear();
    }

    /// Poisoning cannot happen here — no test panics while holding this — and
    /// a fixture that unwraps says so in one place rather than at every call.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("fixture state is never poisoned")
    }
}

#[derive(Clone)]
struct AppState {
    config: Config,
    base: String,
    recorder: Arc<Recorder>,
}

/// A bound provider whose resource endpoint has not been decided yet.
///
/// The socket is bound first because every document the provider serves names
/// its own origin, and the origin is not known until the OS has handed out a
/// port.
pub struct Pending {
    listener: tokio::net::TcpListener,
    pub addr: SocketAddr,
    /// `http://127.0.0.1:<port>` — the issuer, and the prefix of every
    /// endpoint.
    pub base: String,
    pub recorder: Arc<Recorder>,
    router: Router,
}

/// A running provider.
pub struct Provider {
    pub addr: SocketAddr,
    pub base: String,
    pub recorder: Arc<Recorder>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Provider {
    /// The MCP endpoint clients are pointed at.
    #[must_use]
    pub fn mcp_url(&self) -> String {
        format!("{}/mcp", self.base)
    }

    pub fn stop(self) {
        self.task.abort();
    }
}

/// Binds a provider on an ephemeral loopback port.
///
/// # Panics
///
/// If the loopback socket cannot be bound, which is a broken test machine
/// rather than a case worth threading a `Result` through a fixture for.
pub async fn bind(config: Config) -> Pending {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback port");
    let addr = listener.local_addr().expect("read the bound port");
    let base = format!("http://{addr}");
    let recorder = Arc::new(Recorder::default());
    let state = AppState {
        config,
        base: base.clone(),
        recorder: Arc::clone(&recorder),
    };
    let router = Router::new()
        // RFC 9728. Three paths because rmcp probes all three when it has no
        // challenge to go on, and one of them is what a real provider serves.
        .route("/.well-known/oauth-protected-resource", get(resource_meta))
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(resource_meta),
        )
        .route(
            "/mcp/.well-known/oauth-protected-resource",
            get(resource_meta),
        )
        // RFC 8414, plus the OIDC spelling rmcp also tries.
        .route("/.well-known/oauth-authorization-server", get(server_meta))
        .route("/.well-known/openid-configuration", get(server_meta))
        .route("/authorize", get(authorize))
        .route("/token", post(token))
        .route("/register", post(register))
        .with_state(state);
    Pending {
        listener,
        addr,
        base,
        recorder,
        router,
    }
}

impl Pending {
    /// Serves the provider with `resource` merged in as the protected
    /// resource — an MCP server for the tests that call one, a bare `401` for
    /// the tests that only care about the login.
    #[must_use]
    pub fn serve(self, resource: Router) -> Provider {
        let app = self.router.merge(resource);
        let task = tokio::spawn(axum::serve(self.listener, app).into_future());
        Provider {
            addr: self.addr,
            base: self.base,
            recorder: self.recorder,
            task,
        }
    }

    /// The `WWW-Authenticate` value a protected resource on this provider
    /// answers a credential-less request with.
    #[must_use]
    pub fn challenge(&self) -> String {
        format!(
            "Bearer realm=\"mcp\", resource_metadata=\"{}/.well-known/oauth-protected-resource\"",
            self.base
        )
    }
}

/// A `/mcp` that always refuses, with the challenge that names where the
/// metadata lives. Enough for every test about the login itself.
pub fn refusing_resource(challenge: String) -> Router {
    Router::new().route(
        "/mcp",
        axum::routing::any(move || {
            let challenge = challenge.clone();
            async move {
                (
                    axum::http::StatusCode::UNAUTHORIZED,
                    [(axum::http::header::WWW_AUTHENTICATE, challenge)],
                )
                    .into_response()
            }
        }),
    )
}

async fn resource_meta(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({
        "resource": format!("{}/mcp", state.base),
        "authorization_servers": [state.base],
        "scopes_supported": ["read"],
    }))
}

async fn server_meta(State(state): State<AppState>) -> Json<serde_json::Value> {
    let mut body = json!({
        "issuer": state.base,
        "authorization_endpoint": format!("{}/authorize", state.base),
        "token_endpoint": format!("{}/token", state.base),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "scopes_supported": ["read"],
        // RFC 9207: the client checks the `iss` this provider sends back.
        "authorization_response_iss_parameter_supported": true,
    });
    if state.config.cimd {
        body["client_id_metadata_document_supported"] = json!(true);
    }
    if state.config.dcr {
        body["registration_endpoint"] = json!(format!("{}/register", state.base));
    }
    Json(body)
}

async fn authorize(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> axum::response::Response {
    let bad =
        |message: &str| (axum::http::StatusCode::BAD_REQUEST, message.to_owned()).into_response();
    let (Some(client_id), Some(redirect_uri), Some(csrf)) = (
        params.get("client_id"),
        params.get("redirect_uri"),
        params.get("state"),
    ) else {
        return bad("missing client_id, redirect_uri or state");
    };
    if params.get("response_type").map(String::as_str) != Some("code") {
        return bad("response_type must be code");
    }
    // PKCE is not optional here: a client that skipped it would otherwise
    // pass every test in the suite.
    if params.get("code_challenge_method").map(String::as_str) != Some("S256") {
        return bad("code_challenge_method must be S256");
    }
    let Some(challenge) = params.get("code_challenge") else {
        return bad("missing code_challenge");
    };

    state.recorder.authorizations.fetch_add(1, Ordering::SeqCst);
    let code = {
        let mut inner = state.recorder.lock();
        inner.client_id = Some(client_id.clone());
        inner.redirect_uri = Some(redirect_uri.clone());
        inner.scope = params.get("scope").cloned();
        inner.issued += 1;
        let code = format!("code-{}", inner.issued);
        inner.codes.insert(code.clone(), challenge.clone());
        code
    };
    let separator = if redirect_uri.contains('?') { '&' } else { '?' };
    let location = format!(
        "{redirect_uri}{separator}code={code}&state={}&iss={}",
        urlencode(csrf),
        urlencode(&state.base)
    );
    (
        axum::http::StatusCode::FOUND,
        [(axum::http::header::LOCATION, location)],
    )
        .into_response()
}

async fn token(
    State(state): State<AppState>,
    Form(form): Form<HashMap<String, String>>,
) -> axum::response::Response {
    let deny = |error: &str| {
        (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({ "error": error })),
        )
            .into_response()
    };
    match form.get("grant_type").map(String::as_str) {
        Some("authorization_code") => {
            let (Some(code), Some(verifier)) = (form.get("code"), form.get("code_verifier")) else {
                return deny("invalid_request");
            };
            let filed = state.recorder.lock().codes.remove(code);
            let Some(challenge) = filed else {
                return deny("invalid_grant");
            };
            // The whole point of S256: the verifier the client kept has to
            // hash to the challenge it sent before the browser ever ran.
            let computed = oauth2::PkceCodeChallenge::from_code_verifier_sha256(
                &oauth2::PkceCodeVerifier::new(verifier.clone()),
            );
            if computed.as_str() != challenge {
                return deny("invalid_grant");
            }
            state.recorder.exchanges.fetch_add(1, Ordering::SeqCst);
            issue(&state)
        }
        Some("refresh_token") => {
            let Some(presented) = form.get("refresh_token") else {
                return deny("invalid_request");
            };
            let known = if state.config.rotate_refresh {
                state.recorder.lock().refresh.remove(presented)
            } else {
                state.recorder.lock().refresh.contains(presented)
            };
            if !known {
                return deny("invalid_grant");
            }
            state.recorder.refreshes.fetch_add(1, Ordering::SeqCst);
            issue(&state)
        }
        _ => deny("unsupported_grant_type"),
    }
}

fn issue(state: &AppState) -> axum::response::Response {
    let (access, refresh) = {
        let mut inner = state.recorder.lock();
        inner.issued += 1;
        let n = inner.issued;
        let access = format!("at-{n}");
        let refresh = format!("rt-{n}");
        inner.access.insert(access.clone());
        inner.refresh.insert(refresh.clone());
        (access, refresh)
    };
    Json(json!({
        "access_token": access,
        "token_type": "Bearer",
        "expires_in": state.config.access_ttl,
        "refresh_token": refresh,
        "scope": "read",
    }))
    .into_response()
}

async fn register(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    if !state.config.dcr {
        return (
            axum::http::StatusCode::NOT_FOUND,
            Json(json!({ "error": "registration is not offered" })),
        )
            .into_response();
    }
    let n = state.recorder.registrations.fetch_add(1, Ordering::SeqCst) + 1;
    Json(json!({
        // Never a URL: that is what tells a reader — and the code under test
        // — that this id came from registration and not from a metadata
        // document.
        "client_id": format!("dcr-client-{n}"),
        "redirect_uris": body.get("redirect_uris").cloned().unwrap_or(json!([])),
        "token_endpoint_auth_method": "none",
    }))
    .into_response()
}

/// Percent-encodes the few characters a query value can carry here. Written
/// out rather than pulled in: the values are a CSRF token and an
/// `http://127.0.0.1:<port>` origin, and this is the whole of what either
/// needs.
fn urlencode(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte));
            }
            _ => {
                out.push('%');
                out.push(char::from(HEX[usize::from(byte >> 4)]));
                out.push(char::from(HEX[usize::from(byte & 0x0f)]));
            }
        }
    }
    out
}
