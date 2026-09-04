//! The OAuth broker: what `mcpgw auth login` runs, and what the gateway
//! presents afterwards.
//!
//! # Why the gateway logs in and the client does not
//!
//! A server behind OAuth answers `401` with a `WWW-Authenticate` challenge,
//! and every MCP client knows what to do with one. Behind the gateway none of
//! them ever sees it: relaying the challenge would have the client complete
//! the upstream's flow and then send the upstream's token through us, which is
//! the token passthrough the spec forbids at the resource server. So the
//! challenge stops here and the login happens here — once, on this machine,
//! for every client at the same time.
//!
//! # What is stored, and where
//!
//! One file per server at `<state>/auth/<name>.json`, 0600 inside a 0700
//! directory. It holds the access token, the refresh token and the issuer that
//! minted them. Nothing else in mcpgw reads it: the gateway hands it to rmcp's
//! [`CredentialStore`], which is also what writes a rotated refresh token back.
//!
//! Keyed by server name *and* issuer: the name is the file, and the issuer
//! inside it is what rmcp checks before reusing a token, so a provider that
//! moves to a different authorization server invalidates the tokens rather
//! than presenting them to a stranger.
//!
//! # Which identity is presented
//!
//! In the order the 2026-07-28 spec asks for, and chosen by rmcp from the
//! material handed to it:
//!
//! 1. a client id issued out of band (`--client-id`, or `[servers.x.auth]`),
//!    which is all Atlassian and GitHub accept;
//! 2. a Client ID Metadata Document — [`CLIENT_ID_URL`], an https URL that
//!    *is* the client id — when the authorization server advertises
//!    `client_id_metadata_document_supported`;
//! 3. Dynamic Client Registration, deprecated in 2026-07-28 but still the only
//!    thing Notion, Sentry and Cloudflare offer.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use oauth2::TokenResponse as _;
use rmcp::transport::auth::{
    AuthorizationRequest, CredentialRefreshGuard, CredentialStore, OAuthState, StoredCredentials,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

/// The Client ID Metadata Document mcpgw identifies itself with.
///
/// A single URL for every install, exactly as Claude Code ships one: the
/// document is static, public and says nothing about the machine reading it,
/// and an authorization server fetching it learns only what any user of mcpgw
/// already knows. The alternative — Dynamic Client Registration per install —
/// creates a client record per laptop on every provider, which is the thing
/// 2026-07-28 deprecated it for.
///
/// The document itself lives at `book/src/client.json`: mdbook copies every
/// non-markdown file under `src` into the published site, so the page that
/// documents this and the document it names are one deployment.
pub const CLIENT_ID_URL: &str = "https://kennywillbe.github.io/mcpgw/client.json";

/// The name an authorization server shows the user on its consent screen when
/// it registers us dynamically. The document at [`CLIENT_ID_URL`] carries the
/// same string for the CIMD path.
pub const CLIENT_NAME: &str = "mcpgw";

/// How long a login waits for the browser to come back.
///
/// Five minutes is long enough to find the right account, log into an SSO and
/// answer an MFA prompt, and short enough that a login nobody finished stops
/// holding a socket open on the loopback interface.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// Schema version of the token file. Bumped only for a change a previous
/// build could misread; a new optional field does not need one.
pub const TOKEN_VERSION: u32 = 1;

/// Where every server's tokens live.
#[must_use]
pub fn dir(state_dir: &Path) -> PathBuf {
    state_dir.join("auth")
}

/// The token file for one server.
///
/// Named after the server rather than the issuer: a name is what the user
/// types, what `doctor` prints and what the config already keys on, and two
/// entries pointing at one provider are still two logins as far as scopes and
/// revocation are concerned. The issuer is recorded *inside* the file, which
/// is where it does its job — see [`Tokens`].
#[must_use]
pub fn token_path(state_dir: &Path, server: &str) -> PathBuf {
    dir(state_dir).join(format!("{server}.json"))
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot read {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid token file {path} (delete it and run mcpgw auth login again)")]
    Parse {
        path: PathBuf,
        #[source]
        source: Box<serde_json::Error>,
    },

    /// Whatever rmcp's OAuth client reported: discovery, registration, the
    /// authorization request, the exchange, the `iss` check.
    #[error("OAuth failed: {0}")]
    OAuth(#[from] rmcp::transport::auth::AuthError),

    #[error("cannot listen on 127.0.0.1 for the OAuth redirect")]
    Listener(#[source] std::io::Error),

    #[error("no answer from the browser within {}s", LOGIN_TIMEOUT.as_secs())]
    Timeout,

    /// The authorization server redirected back with an error rather than a
    /// code, or with nothing this side can use. `message` is its own
    /// `error`/`error_description`, which is the only account of what the
    /// user was refused for.
    #[error("the authorization server refused the login: {message}")]
    Refused { message: String },

    #[error("server {name:?} is a stdio server; OAuth is for http servers")]
    NotHttp { name: String },

    #[error("{var} is not set, and server {name:?} names it as its client secret")]
    MissingSecret { name: String, var: String },
}

/// One server's stored OAuth credentials, as the file holds them.
///
/// The shape is deliberately thin: a version, the server the file belongs to,
/// and the credential record rmcp itself stores and refreshes. Keeping the
/// second half in rmcp's own type is what makes a refresh write back a file
/// this build can still read — a hand-copied projection of the token response
/// would have to be kept in step with the SDK's, and would drop whatever
/// vendor fields a provider chose to send.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tokens {
    pub version: u32,
    /// The server these belong to. Redundant against the filename and there
    /// on purpose: a file copied to the wrong name is a token presented to
    /// the wrong provider, and this is what lets a reader notice.
    pub server: String,
    pub credentials: StoredCredentials,
}

/// Which of the three identities the stored `client_id` came from.
///
/// Derived rather than recorded. A CIMD client id *is* an https URL — the
/// same test rmcp applies when it decides whether a client id survives an
/// issuer change — and a dynamically registered one never is, so the file
/// cannot disagree with itself about what it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identity {
    /// A client id the provider issued out of band and the config names.
    Preregistered,
    /// [`CLIENT_ID_URL`], or whatever the seam points it at.
    Cimd,
    /// Issued by the authorization server's `registration_endpoint`.
    Dcr,
}

impl std::fmt::Display for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Preregistered => "pre-registered client id",
            Self::Cimd => "client id metadata document",
            Self::Dcr => "dynamic client registration",
        })
    }
}

impl Tokens {
    /// The tokens for `server`, or `None` when nobody has logged in yet.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] for read failures other than not-found, and
    /// [`Error::Parse`] for a file that is not a token record. A corrupt file
    /// is not treated as absent: "never logged in" and "logged in, and the
    /// file is unreadable" call for different advice.
    pub fn load(state_dir: &Path, server: &str) -> Result<Option<Self>, Error> {
        let path = token_path(state_dir, server);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(Error::Io { path, source }),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|source| Error::Parse {
                path,
                source: Box::new(source),
            })
    }

    /// Writes the tokens for `server`, owner-only, atomically.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] for any filesystem failure.
    pub fn save(&self, state_dir: &Path) -> Result<(), Error> {
        use std::io::Write as _;

        let path = token_path(state_dir, &self.server);
        let io_err = |p: &Path| {
            let p = p.to_owned();
            move |source| Error::Io { path: p, source }
        };
        let parent = dir(state_dir);
        crate::private::create_dir_all(&parent).map_err(io_err(&parent))?;
        let json = serde_json::to_vec_pretty(self)
            .map_err(std::io::Error::other)
            .map_err(io_err(&path))?;
        let mut tmp = tempfile::Builder::new()
            .prefix(".token.json.")
            .tempfile_in(&parent)
            .map_err(io_err(&parent))?;
        // Narrowed before a byte of it is written: a temp file created at the
        // process umask and hardened afterwards is world-readable for the
        // length of the write, and what is being written is a bearer token.
        crate::private::harden_file(tmp.path()).map_err(io_err(&path))?;
        tmp.write_all(&json).map_err(io_err(&path))?;
        tmp.as_file().sync_all().map_err(io_err(&path))?;
        tmp.persist(&path).map_err(|err| Error::Io {
            path: path.clone(),
            source: err.error,
        })?;
        crate::private::harden_file(&path).map_err(io_err(&path))?;
        crate::private::sync_dir(&parent).map_err(io_err(&parent))?;
        Ok(())
    }

    /// Deletes the tokens for `server`; `false` if there were none.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] for a delete that failed for any reason but the file
    /// already being gone.
    pub fn delete(state_dir: &Path, server: &str) -> Result<bool, Error> {
        let path = token_path(state_dir, server);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(Error::Io { path, source }),
        }
    }

    #[must_use]
    pub fn client_id(&self) -> &str {
        &self.credentials.client_id
    }

    #[must_use]
    pub fn issuer(&self) -> Option<&str> {
        self.credentials.issuer.as_deref()
    }

    #[must_use]
    pub fn scopes(&self) -> &[String] {
        &self.credentials.granted_scopes
    }

    /// See [`Identity`]. `configured` is the client id the config names for
    /// this server, which is the only thing that tells a pre-registered id
    /// apart from one the authorization server minted.
    #[must_use]
    pub fn identity(&self, configured: Option<&str>) -> Identity {
        if configured.is_some_and(|id| id == self.client_id()) {
            Identity::Preregistered
        } else if self.client_id().starts_with("https://") {
            Identity::Cimd
        } else {
            Identity::Dcr
        }
    }

    /// Unix seconds at which the access token stops being accepted, or `None`
    /// for a provider that sent no `expires_in` — which means "until it is
    /// refused", and there is nothing to count down.
    #[must_use]
    pub fn expires_at(&self) -> Option<u64> {
        let response = self.credentials.token_response.as_ref()?;
        let expires_in = response.expires_in()?;
        let received = self.credentials.token_received_at?;
        Some(received.saturating_add(expires_in.as_secs()))
    }

    /// Whether the access token's own clock has run out. Says nothing about
    /// whether the *login* has: a refresh token renews it without a browser,
    /// which is why this is not the question `auth status` leads with.
    #[must_use]
    pub fn expired(&self) -> bool {
        self.expires_at().is_some_and(|at| at <= now())
    }

    /// Whether the gateway can renew the access token on its own.
    #[must_use]
    pub fn renewable(&self) -> bool {
        self.credentials
            .token_response
            .as_ref()
            .is_some_and(|response| response.refresh_token().is_some())
    }

    /// What a report should say about this login in one word.
    #[must_use]
    pub fn state(&self) -> TokenState {
        if !self.expired() {
            TokenState::Valid
        } else if self.renewable() {
            TokenState::Renewable
        } else {
            TokenState::Expired
        }
    }
}

/// The three things a stored login can be, from a reader's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenState {
    /// The access token has not run out yet.
    Valid,
    /// It has, but a refresh token is stored, so the next connect renews it
    /// without anybody opening a browser. Not a problem to report.
    Renewable,
    /// It has, and there is nothing to renew it with: this one needs a login.
    Expired,
}

impl TokenState {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Valid => "valid",
            Self::Renewable => "expired, renews itself",
            Self::Expired => "expired",
        }
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// rmcp's credential store, backed by one server's token file.
///
/// This is the whole of the gateway's side of OAuth. rmcp asks it for the
/// credentials before a request, refreshes them when they are nearly out and
/// hands the result straight back here — including a rotated refresh token,
/// which is why the file is written by the SDK's own record type rather than
/// by anything in this crate.
#[derive(Debug, Clone)]
pub struct FileStore {
    state_dir: PathBuf,
    server: String,
}

impl FileStore {
    #[must_use]
    pub fn new(state_dir: &Path, server: &str) -> Self {
        Self {
            state_dir: state_dir.to_owned(),
            server: server.to_owned(),
        }
    }

    fn store_err(err: &Error) -> rmcp::transport::auth::AuthError {
        rmcp::transport::auth::AuthError::CredentialStoreError(err.to_string())
    }
}

#[async_trait::async_trait]
impl CredentialStore for FileStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, rmcp::transport::auth::AuthError> {
        Tokens::load(&self.state_dir, &self.server)
            .map(|tokens| tokens.map(|tokens| tokens.credentials))
            .map_err(|err| Self::store_err(&err))
    }

    async fn save(
        &self,
        credentials: StoredCredentials,
    ) -> Result<(), rmcp::transport::auth::AuthError> {
        Tokens {
            version: TOKEN_VERSION,
            server: self.server.clone(),
            credentials,
        }
        .save(&self.state_dir)
        .map_err(|err| Self::store_err(&err))
    }

    async fn clear(&self) -> Result<(), rmcp::transport::auth::AuthError> {
        Tokens::delete(&self.state_dir, &self.server)
            .map(|_| ())
            .map_err(|err| Self::store_err(&err))
    }

    /// A sidecar lock file held across the whole load-refresh-save.
    ///
    /// The gateway multiplexes every client onto one connection per server,
    /// so two requests arriving as the token expires would otherwise both
    /// refresh — and a provider that rotates refresh tokens invalidates the
    /// first rotation with the second, logging the user out of a session they
    /// were in the middle of. A sidecar rather than the token file itself for
    /// the reason every other lock here is one: [`Tokens::save`] renames a new
    /// inode over the old, which would strand a lock held on it.
    async fn acquire_refresh_guard(
        &self,
    ) -> Result<Option<CredentialRefreshGuard>, rmcp::transport::auth::AuthError> {
        let path = token_path(&self.state_dir, &self.server);
        let parent = dir(&self.state_dir);
        // Blocking file locking on the runtime's worker: the wait is bounded
        // by one other process's token request, but a blocked worker is a
        // blocked gateway, so it goes where blocking calls go.
        let file = tokio::task::spawn_blocking(move || {
            crate::private::create_dir_all(&parent)?;
            let lock = crate::store::lock_path(&path);
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&lock)?;
            file.lock()?;
            Ok::<_, std::io::Error>(file)
        })
        .await
        .map_err(|err| {
            rmcp::transport::auth::AuthError::CredentialStoreError(format!(
                "refresh lock task failed: {err}"
            ))
        })?
        .map_err(|err| {
            rmcp::transport::auth::AuthError::CredentialStoreError(format!(
                "cannot lock the token file: {err}"
            ))
        })?;
        Ok(Some(CredentialRefreshGuard::new(file)))
    }
}

/// Everything one `mcpgw auth login` needs to know.
pub struct Login<'a> {
    /// The canonical server name; also the token file's name.
    pub server: &'a str,
    /// The MCP endpoint. Discovery starts here.
    pub url: &'a str,
    pub state_dir: &'a Path,
    /// A pre-registered client id, from `--client-id` or the config.
    pub client_id: Option<&'a str>,
    /// Its secret, already read out of the environment by the caller — this
    /// module never looks a secret up itself.
    pub client_secret: Option<&'a str>,
    /// Scopes to ask for. Empty lets the provider's own metadata decide,
    /// which is what almost every one of them expects.
    pub scopes: &'a [String],
    /// The `WWW-Authenticate` value from a 401 this machine already saw, when
    /// there is one. It seeds discovery from the challenge instead of probing
    /// the server again, and carries the provider's own scope hint.
    pub challenge: Option<&'a str>,
    pub timeout: Duration,
}

/// Runs the whole authorization-code flow and stores the result.
///
/// `announce` is called once with the authorization URL, before anything
/// waits: it is where the caller opens a browser, prints the URL, or both. It
/// is deliberately not this module's job — a daemon must never open a browser,
/// and the way to guarantee that is for the code the daemon links to have no
/// way of doing it.
///
/// The listener is bound before the URL exists, on `127.0.0.1:0`: the loopback
/// literal rather than `localhost` so nothing can resolve us onto another
/// interface (RFC 8252 §7.3), and port zero because the same section obliges
/// the authorization server to accept whichever ephemeral port the OS hands
/// out.
///
/// # Errors
///
/// [`Error::Listener`] when the loopback socket cannot be bound,
/// [`Error::OAuth`] for anything rmcp's client refuses — discovery,
/// registration, the PKCE exchange, an `iss` that does not match —
/// [`Error::Timeout`] when the browser never comes back, [`Error::Refused`]
/// when the authorization server redirects with an error, and [`Error::Io`]
/// when the tokens cannot be written.
pub async fn login(request: &Login<'_>, announce: impl FnOnce(&str)) -> Result<Tokens, Error> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(Error::Listener)?;
    let port = listener.local_addr().map_err(Error::Listener)?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    let mut state = OAuthState::new(request.url, None).await?;
    // Set before the flow starts, so the tokens land in the file the moment
    // the exchange succeeds rather than being carried back through memory and
    // written by a second, forgettable step.
    let OAuthState::Unauthorized(manager) = &mut state else {
        return Err(Error::OAuth(
            rmcp::transport::auth::AuthError::InternalError(
                "a fresh OAuth state is not unauthorized".to_owned(),
            ),
        ));
    };
    manager.set_credential_store(FileStore::new(request.state_dir, request.server));

    let mut authorization = AuthorizationRequest::new(redirect_uri)
        .with_client_name(CLIENT_NAME)
        // SEP-837. mcpgw is a CLI on the user's own machine with a loopback
        // redirect, which is what OIDC calls a native client; omitting this
        // defaults to `web` and gets the redirect rejected by every
        // OIDC-backed provider.
        .with_application_type("native")
        .with_client_metadata_url(CLIENT_ID_URL);
    if let Some(client_id) = request.client_id {
        authorization = authorization.with_preregistered_client(client_id);
        if let Some(secret) = request.client_secret {
            authorization = authorization.with_client_secret(secret);
        }
    }
    if !request.scopes.is_empty() {
        authorization = authorization.with_scopes(request.scopes.to_vec());
    }
    if let Some(challenge) = request.challenge {
        authorization = authorization.with_challenge(challenge);
    }
    state.start_authorization(authorization).await?;

    announce(&state.get_authorization_url().await?);

    let query = wait_for_callback(&listener, request.timeout).await?;
    state
        .handle_callback_url(&format!("http://127.0.0.1:{port}/callback?{query}"))
        .await?;

    // Read back rather than returned from memory: what the caller reports has
    // to be what the next connect will present, and the store is the only
    // thing that has seen both.
    Tokens::load(request.state_dir, request.server)?.ok_or_else(|| Error::Refused {
        message: "the authorization server issued no token".to_owned(),
    })
}

/// Waits for the browser's one redirect and returns its query string.
///
/// Requests for anything but the callback path are answered and ignored: a
/// browser asking for `/favicon.ico` first must not consume the one accept
/// the login gets. The `state` parameter is not checked here — it is checked
/// where it means something, against the PKCE verifier rmcp filed under it,
/// which is the check that actually binds the callback to this login.
async fn wait_for_callback(
    listener: &tokio::net::TcpListener,
    timeout: Duration,
) -> Result<String, Error> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let accepted = tokio::time::timeout_at(deadline, listener.accept()).await;
        let Ok(accepted) = accepted else {
            return Err(Error::Timeout);
        };
        let (stream, _) = accepted.map_err(Error::Listener)?;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        // Only the request line is read. A body is never sent to a redirect
        // target, and reading headers would mean a second thing to get wrong.
        if reader.read_line(&mut line).await.is_err() {
            continue;
        }
        let target = line.split_whitespace().nth(1).unwrap_or_default();
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let done = path == "/callback";
        let body = if done {
            "mcpgw is logged in. You can close this tab."
        } else {
            "mcpgw is waiting for the authorization redirect."
        };
        let response = format!(
            "HTTP/1.1 {} {}\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            if done { 200 } else { 404 },
            if done { "OK" } else { "Not Found" },
            body.len(),
        );
        let mut stream = reader.into_inner();
        // Best effort: the tokens matter, the browser's tab does not. A user
        // who closed the window mid-redirect still gets logged in.
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;
        if !done {
            continue;
        }
        if let Some(message) = refusal(query) {
            return Err(Error::Refused { message });
        }
        return Ok(query.to_owned());
    }
}

/// The authorization server's own account of a refusal, out of the redirect's
/// `error`/`error_description`. `None` for a redirect that carries neither,
/// which is the ordinary success case.
fn refusal(query: &str) -> Option<String> {
    let mut error = None;
    let mut description = None;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "error" => error = Some(value.into_owned()),
            "error_description" => description = Some(value.into_owned()),
            _ => {}
        }
    }
    let error = error?;
    Some(match description {
        Some(description) => format!("{error} ({description})"),
        None => error,
    })
}

/// Builds the credential-carrying http client for `server`, or `None` when
/// nobody has logged in for it.
///
/// `None` is not a failure and must not be reported as one: an http server
/// with no token file is either a server that needs no OAuth at all, or one
/// that does and has not been logged into — and the second answers `401`,
/// which is already the state that names the login command.
///
/// # Errors
///
/// [`Error::Io`] or [`Error::Parse`] when the token file exists and cannot be
/// read, and [`Error::OAuth`] when discovery fails. A discovery failure here
/// is a real failure: a token was stored for this server, so the provider
/// published metadata at least once.
pub async fn client(
    state_dir: &Path,
    server: &str,
    url: &str,
) -> Result<Option<rmcp::transport::auth::AuthClient<reqwest::Client>>, Error> {
    if Tokens::load(state_dir, server)?.is_none() {
        return Ok(None);
    }
    let mut manager = rmcp::transport::auth::AuthorizationManager::new(url).await?;
    manager.set_credential_store(FileStore::new(state_dir, server));
    // Re-reads the file through the store and re-runs discovery, which is
    // also where rmcp discards a token minted by an authorization server the
    // provider has since moved off. `false` means there is nothing usable to
    // present; the connect then goes out bare and comes back 401, which is
    // the state that tells the user to log in again.
    manager.initialize_from_store().await?;
    Ok(Some(rmcp::transport::auth::AuthClient::new(
        reqwest::Client::default(),
        manager,
    )))
}

/// Asks `server` for its `WWW-Authenticate` challenge by dialing it without
/// a credential.
///
/// `None` for a server that answers, one that cannot be reached, and one that
/// refuses without saying how — all three of which are the same thing to a
/// login: nothing to seed discovery with, so rmcp probes the well-known paths
/// instead. Deliberately not an error: the challenge is an optimisation and a
/// correctness aid for providers that publish their metadata off the
/// well-known path, never a precondition.
///
/// A stdio server has no challenge and is never dialed.
pub async fn challenge(server: &crate::config::Server, timeout: Duration) -> Option<String> {
    let crate::config::Transport::Http { url, headers, .. } = &server.transport else {
        return None;
    };
    let config = crate::upstream::http_config(url, headers).ok()?;
    let transport = rmcp::transport::StreamableHttpClientTransport::from_config(config);
    // One handshake on the legacy lifecycle only. A server that refuses
    // `initialize` because it speaks 2026-07-28 refuses it with a JSON-RPC
    // error, not a 401, and a second round trip would learn nothing about the
    // credential this is asking after.
    // Detached: this connection exists to read one challenge header and is
    // dropped, so a notification arriving on it has nobody to reach.
    match crate::upstream::dial(
        transport,
        crate::upstream::Lifecycle::Legacy,
        Some(timeout),
        crate::upstream::UpstreamClient::detached(),
    )
    .await
    {
        Err(err) if err.auth_required => err.challenge,
        _ => None,
    }
}

/// Hands `url` to the platform's browser. `false` when there was nothing to
/// hand it to, which is the headless case and not an error — the caller has
/// already printed the URL.
///
/// Deliberately not called from anywhere the daemon runs: see [`login`].
#[must_use]
pub fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    let (program, args): (&str, &[&str]) = ("open", &[]);
    #[cfg(target_os = "windows")]
    // `start` is a cmd builtin, not a program, and its first quoted argument
    // is taken as the window title — hence the empty one before the URL.
    let (program, args): (&str, &[&str]) = ("cmd", &["/C", "start", ""]);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let (program, args): (&str, &[&str]) = ("xdg-open", &[]);

    std::process::Command::new(program)
        .args(args)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(expires_in: Option<u64>, refresh: bool, client_id: &str) -> Tokens {
        let mut response = rmcp::transport::auth::OAuthTokenResponse::new(
            oauth2::AccessToken::new("at-secret".to_owned()),
            oauth2::basic::BasicTokenType::Bearer,
            rmcp::transport::auth::VendorExtraTokenFields::default(),
        );
        if let Some(seconds) = expires_in {
            response.set_expires_in(Some(&Duration::from_secs(seconds)));
        }
        if refresh {
            response.set_refresh_token(Some(oauth2::RefreshToken::new("rt-secret".to_owned())));
        }
        Tokens {
            version: TOKEN_VERSION,
            server: "linear".to_owned(),
            credentials: StoredCredentials::new(
                client_id.to_owned(),
                Some(response),
                vec!["read".to_owned()],
                Some(now()),
            )
            .with_issuer(Some("https://auth.example.com".to_owned())),
        }
    }

    /// The three states a report distinguishes, and the one that matters: an
    /// access token past its expiry with a refresh token behind it is not a
    /// login anybody has to redo.
    #[test]
    fn a_refreshable_token_is_not_a_login_to_redo() {
        assert_eq!(tokens(Some(3600), true, "id").state(), TokenState::Valid);
        let mut stale = tokens(Some(1), true, "id");
        stale.credentials.token_received_at = Some(now() - 60);
        assert_eq!(stale.state(), TokenState::Renewable);
        let mut dead = tokens(Some(1), false, "id");
        dead.credentials.token_received_at = Some(now() - 60);
        assert_eq!(dead.state(), TokenState::Expired);
        // A provider that sends no expiry has no clock to run out.
        assert_eq!(tokens(None, false, "id").state(), TokenState::Valid);
    }

    /// The file cannot disagree with itself about which identity it holds,
    /// because nothing records it — see [`Identity`].
    #[test]
    fn the_identity_is_read_off_the_client_id() {
        let cimd = tokens(
            Some(60),
            true,
            "https://kennywillbe.github.io/mcpgw/client.json",
        );
        assert_eq!(cimd.identity(None), Identity::Cimd);
        let dcr = tokens(Some(60), true, "s6BhdRkqt3");
        assert_eq!(dcr.identity(None), Identity::Dcr);
        let pre = tokens(Some(60), true, "atlassian-issued");
        assert_eq!(
            pre.identity(Some("atlassian-issued")),
            Identity::Preregistered
        );
        // Configured for a different id than the file holds: whatever this
        // is, it is not the one the config names.
        assert_eq!(pre.identity(Some("something-else")), Identity::Dcr);
    }

    #[test]
    fn a_refused_redirect_is_read_as_the_refusal_it_is() {
        assert_eq!(
            refusal("error=access_denied&error_description=user%20said%20no").as_deref(),
            Some("access_denied (user said no)")
        );
        assert_eq!(
            refusal("error=invalid_scope").as_deref(),
            Some("invalid_scope")
        );
        assert_eq!(refusal("code=abc&state=xyz"), None);
    }

    /// The constant and the document it names are one deployment, and the
    /// CIMD draft requires them to agree exactly: an authorization server
    /// fetches the URL and refuses the login if the `client_id` inside does
    /// not match it. Nothing but this test connects the two files.
    #[test]
    fn the_published_document_is_the_client_id_it_claims() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../book/src/client.json");
        let text = std::fs::read_to_string(&path).expect("book/src/client.json is published");
        let document: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(document["client_id"], CLIENT_ID_URL);
        assert_eq!(document["client_name"], CLIENT_NAME);
        // The three the spec requires, plus the two that decide whether a
        // native client's loopback redirect is accepted at all.
        assert!(document["redirect_uris"].is_array());
        assert_eq!(document["redirect_uris"][0], "http://127.0.0.1/callback");
        assert_eq!(document["application_type"], "native");
        assert_eq!(document["token_endpoint_auth_method"], "none");
        // https with a path component, which is what makes a URL usable as a
        // client id under the CIMD draft.
        assert!(CLIENT_ID_URL.starts_with("https://"));
        assert!(CLIENT_ID_URL.trim_start_matches("https://").contains('/'));
    }
}
