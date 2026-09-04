//! `mcpgw auth login|status|logout`: the half of OAuth that needs a human.
//!
//! The gateway can present a token and refresh it; it cannot open a browser,
//! and a daemon that could would be a daemon that pops a window on a server
//! nobody is sitting at. So the browser half lives here, in a command a person
//! runs, and the two halves meet at one file under the state directory.

use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context as _;
use mcpgw_core::auth::{self, Tokens};
use mcpgw_core::config::ServerAuth;
use mcpgw_core::probe::ProbeError;
use mcpgw_core::probe_state::{AuthObservation, ProbeState};
use mcpgw_core::{Config, ConfigStore, Error, Server, Transport};
use owo_colors::OwoColorize as _;

#[derive(clap::Args)]
pub struct AuthArgs {
    #[command(subcommand)]
    pub command: AuthCommand,
}

#[derive(clap::Subcommand)]
pub enum AuthCommand {
    /// Log in to a server that needs OAuth, through the browser
    Login(LoginArgs),
    /// Show which servers have a stored login, and how long it lasts
    Status(StatusArgs),
    /// Delete this machine's stored login for a server
    Logout(LogoutArgs),
}

#[derive(clap::Args)]
pub struct LoginArgs {
    /// Server name; omit to log in to every server that needs it
    pub name: Option<String>,
    /// Client id issued by the provider out of band, for the hosts that
    /// register clients by hand. Saved to the config for later refreshes
    // Tied to a named server: it is written into that server's entry, and
    // there is no entry to write it into when the command is logging in to
    // whatever happens to be waiting.
    #[arg(long, value_name = "ID", requires = "name")]
    pub client_id: Option<String>,
    /// Environment variable holding the secret paired with --client-id
    #[arg(long, value_name = "VAR", requires = "client_id")]
    pub client_secret_env: Option<String>,
    /// Scope to request (repeatable); default is whatever the server asks for
    #[arg(long = "scope", value_name = "SCOPE")]
    pub scopes: Vec<String>,
    /// Print the authorization URL instead of opening a browser
    #[arg(long)]
    pub no_browser: bool,
    /// How long to wait for the browser, in seconds
    #[arg(long, default_value_t = auth::LOGIN_TIMEOUT.as_secs(), value_name = "SECS")]
    pub timeout: u64,
}

#[derive(clap::Args)]
pub struct StatusArgs {
    /// Server name; omit for every server in the config
    pub name: Option<String>,
    /// Machine-readable output
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args)]
pub struct LogoutArgs {
    /// Server name
    pub name: String,
}

pub fn run(args: &AuthArgs, color: bool) -> anyhow::Result<u8> {
    match &args.command {
        AuthCommand::Login(login) => run_login(login, color),
        AuthCommand::Status(status) => run_status(status, color),
        AuthCommand::Logout(logout) => run_logout(logout),
    }
}

fn state_dir() -> anyhow::Result<PathBuf> {
    mcpgw_core::paths::state_dir()
        .context("cannot determine a home directory to resolve the state path")
}

/// The http url of `name`, or an error naming what it is instead.
fn http_url(name: &str, server: &Server) -> anyhow::Result<String> {
    match &server.transport {
        Transport::Http { url, .. } => Ok(url.clone()),
        Transport::Stdio { .. } => Err(auth::Error::NotHttp {
            name: name.to_owned(),
        }
        .into()),
    }
}

fn configured_auth(server: &Server) -> Option<&ServerAuth> {
    match &server.transport {
        Transport::Http { auth, .. } => auth.as_ref(),
        Transport::Stdio { .. } => None,
    }
}

/// What a server presents to its upstream, as far as the config says.
///
/// `status` reports on OAuth, and most http servers never do OAuth: one
/// carrying an `Authorization` header, or a `headers_command` that mints one,
/// is already authenticated and has nothing to log in to. Telling its owner
/// to run `auth login` — which is what a single "no login yet" line for every
/// http server amounted to — is advice that would break a working server.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Credential {
    /// An `[auth]` table, a stored login, or both.
    Oauth,
    /// A `headers_command`, which mints its own header at connect time.
    Command,
    /// A literal `headers` entry in the config.
    Header,
    /// Nothing the config knows about, so a login is still the open question.
    None,
}

impl Credential {
    fn label(self) -> &'static str {
        match self {
            Self::Oauth => "oauth",
            Self::Command => "command",
            Self::Header => "header",
            Self::None => "none",
        }
    }
}

/// `logged_in` is whether a token file is on this machine, which counts as
/// OAuth even where the config says nothing: it is a login that happened.
fn credential(server: &Server, logged_in: bool) -> Credential {
    if logged_in || configured_auth(server).is_some() {
        return Credential::Oauth;
    }
    match &server.transport {
        // A command wins over a literal header for the same reason the
        // upstream merges it over one: it is the value that ends up on the
        // wire.
        Transport::Http {
            headers_command, ..
        } if !headers_command.is_empty() => Credential::Command,
        Transport::Http { headers, .. } if !headers.is_empty() => Credential::Header,
        _ => Credential::None,
    }
}

/// What the last probe said about a server nothing in the config
/// authenticates — the difference between a server that has never needed a
/// login and one that has never been asked.
///
/// From the config alone the two look identical, and printing the login hint
/// for both is what sent people to `auth login` for servers that answer
/// happily with no credential at all. The answer comes from
/// [`mcpgw_core::probe_state`], which `doctor --probe` and `auth login` fill
/// in when they dial.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Observed {
    /// A probe reached the server while presenting nothing.
    NoAuthNeeded,
    /// A probe was answered 401 with OAuth discovery metadata.
    LoginRequired,
    /// Nothing has probed this server yet, so there is genuinely nothing to
    /// go on — which is a thing to say, not a reason to guess.
    NotChecked,
}

impl Observed {
    fn of(name: &str, state: &ProbeState) -> Self {
        match state.get(name).map(|seen| seen.auth) {
            Some(AuthObservation::NoAuthNeeded) => Self::NoAuthNeeded,
            Some(AuthObservation::LoginRequired) => Self::LoginRequired,
            None => Self::NotChecked,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::NoAuthNeeded => "no_auth_needed",
            Self::LoginRequired => "login_required",
            Self::NotChecked => "not_checked",
        }
    }
}

fn run_login(args: &LoginArgs, color: bool) -> anyhow::Result<u8> {
    let path = super::canonical_config_path()?;
    let config = Config::load(&path).with_context(|| format!("cannot load {}", path.display()))?;
    let state_dir = state_dir()?;
    let runtime = tokio::runtime::Runtime::new()?;

    let names = if let Some(name) = &args.name {
        if !config.servers.contains_key(name) {
            return Err(Error::UnknownServer {
                name: name.clone(),
                available: config.servers.keys().cloned().collect(),
            }
            .into());
        }
        vec![name.clone()]
    } else {
        // Every server that says it needs a login, asked one at a time. The
        // alternative — logging in to whatever has an `[auth]` table — would
        // miss exactly the servers this is for, since a server nobody has
        // configured anything for is the common case.
        let found = runtime.block_on(needing_login(&config, &state_dir));
        if found.is_empty() {
            println!("no server is waiting on a login");
            return Ok(0);
        }
        found
    };

    let mut failures = 0;
    let mut logged_in = Vec::new();
    for name in &names {
        // The name came out of this map a moment ago and the config is not
        // reloaded in between, so the miss is unreachable — skipped rather
        // than unwrapped all the same.
        let Some(server) = config.servers.get(name) else {
            continue;
        };
        let presented = args
            .client_id
            .as_deref()
            .or_else(|| configured_client_id(server));
        match login_one(&runtime, args, &state_dir, name, server, color) {
            Ok(tokens) => {
                report_login(name, &tokens, presented, color);
                logged_in.push(name.clone());
            }
            Err(err) => {
                failures += 1;
                // Reported and carried on with: a batch login into three
                // providers must not stop at the first one that is down.
                eprintln!("{name}: {err:#}");
            }
        }
    }
    // Only after a login that worked, and only once the browser is closed: a
    // client id the provider rejected is not one to write into the config,
    // and a config write while a flow is still open would be a write nobody
    // asked for.
    if let (Some(client_id), Some(name)) = (&args.client_id, logged_in.first()) {
        persist_identity(&path, name, client_id, args)?;
    }
    Ok(u8::from(failures > 0))
}

fn configured_client_id(server: &Server) -> Option<&str> {
    configured_auth(server).and_then(|auth| auth.client_id.as_deref())
}

fn login_one(
    runtime: &tokio::runtime::Runtime,
    args: &LoginArgs,
    state_dir: &std::path::Path,
    name: &str,
    server: &Server,
    color: bool,
) -> anyhow::Result<Tokens> {
    let url = http_url(name, server)?;
    let configured = configured_auth(server);
    let client_id = args
        .client_id
        .as_deref()
        .or_else(|| configured.and_then(|auth| auth.client_id.as_deref()));
    let secret_var = args
        .client_secret_env
        .as_deref()
        .or_else(|| configured.and_then(|auth| auth.client_secret_env.as_deref()));
    let secret = match secret_var {
        Some(var) => Some(std::env::var(var).map_err(|_| auth::Error::MissingSecret {
            name: name.to_owned(),
            var: var.to_owned(),
        })?),
        None => None,
    };
    let scopes: Vec<String> = if args.scopes.is_empty() {
        configured
            .map(|auth| auth.scopes.clone())
            .unwrap_or_default()
    } else {
        args.scopes.clone()
    };

    let timeout = Duration::from_secs(args.timeout);
    let no_browser = args.no_browser;
    runtime
        .block_on(async {
            // Asked of the server itself rather than replayed from something a
            // gateway recorded earlier: a provider that moved its metadata since
            // then would send us to the old place.
            let challenge = auth::challenge(server, timeout).await;
            let request = auth::Login {
                server: name,
                url: &url,
                state_dir,
                client_id,
                client_secret: secret.as_deref(),
                scopes: &scopes,
                challenge: challenge.as_deref(),
                timeout,
            };
            auth::login(&request, |url| announce(name, url, no_browser, color)).await
        })
        .map_err(Into::into)
}

/// Puts the authorization URL in front of the user, and opens it where that
/// is possible.
///
/// The URL is printed either way. `--no-browser` is for a headless box, but a
/// browser that opened on the wrong profile, a remote session and an X display
/// that is not there all end the same way — with a user who needs the link —
/// and a URL nobody needed costs one line.
fn announce(name: &str, url: &str, no_browser: bool, color: bool) {
    let opened = !no_browser && auth::open_browser(url);
    if opened {
        println!("{name}: opening the browser to finish the login");
    } else {
        println!("{name}: open this URL to finish the login");
    }
    println!("  {}", crate::ui::dim(url, color));
    // Flushed by hand: what follows is a wait of up to five minutes, and a
    // line still sitting in a pipe's buffer is a URL the user cannot click.
    let _ = std::io::stdout().flush();
}

/// `presented` is the client id this login was told to use, which is the only
/// thing that tells a pre-registered identity apart from one the provider
/// minted — and it is not in the config yet at this point, because the config
/// is only written once the login has worked.
fn report_login(name: &str, tokens: &Tokens, presented: Option<&str>, color: bool) {
    let identity = tokens.identity(presented);
    let issuer = tokens.issuer().unwrap_or("an unnamed issuer");
    let line = format!("logged in to {name} at {issuer} ({identity})");
    if color {
        println!("{} {line}", "✓".green());
    } else {
        println!("✓ {line}");
    }
}

/// Records `--client-id` in the config so refreshes and later logins present
/// the same identity. See [`ConfigStore::set_auth`].
fn persist_identity(
    path: &std::path::Path,
    name: &str,
    client_id: &str,
    args: &LoginArgs,
) -> anyhow::Result<()> {
    let mut store = ConfigStore::edit(path)?;
    store.set_auth(
        name,
        &ServerAuth {
            client_id: Some(client_id.to_owned()),
            client_secret_env: args.client_secret_env.clone(),
            scopes: args.scopes.clone(),
        },
    )?;
    store
        .save()
        .with_context(|| format!("cannot write {}", path.display()))
}

/// The enabled http servers that answer 401 right now.
///
/// What it sees is also written down: this is a probe pass like
/// `doctor --probe`, and a later `auth status` — which opens no socket — can
/// only tell "never needed a login" from "never asked" by reading back what
/// a pass like this one saw.
async fn needing_login(config: &Config, state_dir: &std::path::Path) -> Vec<String> {
    let timeout = Duration::from_secs(10);
    let mut found = Vec::new();
    let mut observed = Vec::new();
    for (name, server) in &config.servers {
        if !server.enabled || !matches!(server.transport, Transport::Http { .. }) {
            continue;
        }
        // With the stored login attached, so a server that is already logged
        // in is not offered again.
        let outcome = mcpgw_core::probe::probe_server(name, server, Some(state_dir), timeout).await;
        // A handshake that presented a credential proves nothing about a
        // server that would take a caller without one, so only a probe that
        // carried nothing records a clean bill.
        let bare = matches!(
            credential(server, token_exists(state_dir, name)),
            Credential::None
        );
        match &outcome {
            Ok(_) if bare => observed.push((name.clone(), AuthObservation::NoAuthNeeded)),
            Err(ProbeError::AuthRequired) => {
                observed.push((name.clone(), AuthObservation::LoginRequired));
                found.push(name.clone());
            }
            _ => {}
        }
    }
    // Best effort: a state directory that will not take a write costs a
    // sharper `auth status` line later and nothing about this login.
    drop(ProbeState::record(state_dir, observed));
    found
}

fn token_exists(state_dir: &std::path::Path, name: &str) -> bool {
    Tokens::load(state_dir, name).ok().flatten().is_some()
}

/// One line of `auth status`, gathered before anything is printed so the
/// table can be padded to its widest name.
struct Row {
    name: String,
    /// The client id the config names, for the identity line.
    configured: Option<String>,
    tokens: Option<Tokens>,
    credential: Credential,
    observed: Observed,
}

fn run_status(args: &StatusArgs, color: bool) -> anyhow::Result<u8> {
    let path = super::canonical_config_path()?;
    let config = Config::load(&path).with_context(|| format!("cannot load {}", path.display()))?;
    let state_dir = state_dir()?;

    let names: Vec<String> = match &args.name {
        Some(name) => vec![name.clone()],
        None => config
            .servers
            .iter()
            .filter(|(_, server)| matches!(server.transport, Transport::Http { .. }))
            .map(|(name, _)| name.clone())
            .collect(),
    };

    // Read once for the whole table: it is the same file for every row.
    let probed = ProbeState::load(&state_dir);
    let mut rows = Vec::new();
    for name in names {
        let server = config.servers.get(&name);
        let configured = server.and_then(configured_client_id);
        let tokens = Tokens::load(&state_dir, &name)?;
        let credential = server.map_or(Credential::None, |server| {
            credential(server, tokens.is_some())
        });
        let observed = Observed::of(&name, &probed);
        rows.push(Row {
            name,
            configured: configured.map(str::to_owned),
            tokens,
            credential,
            observed,
        });
    }

    if args.json {
        // `observed_auth` is added beside the fields that were always here,
        // never in place of one: a reader keyed on `logged_in` or
        // `credential` keeps reading exactly what it read before.
        let entries: Vec<serde_json::Value> = rows
            .iter()
            .map(|row| match &row.tokens {
                None => serde_json::json!({
                    "server": row.name,
                    "logged_in": false,
                    "credential": row.credential.label(),
                    "observed_auth": row.observed.label(),
                }),
                Some(tokens) => serde_json::json!({
                    "server": row.name,
                    "logged_in": true,
                    "credential": row.credential.label(),
                    "observed_auth": row.observed.label(),
                    "state": tokens.state().label(),
                    "expires_at": tokens.expires_at(),
                    "renewable": tokens.renewable(),
                    "issuer": tokens.issuer(),
                    "client_id": tokens.client_id(),
                    "identity": tokens.identity(row.configured.as_deref()).to_string(),
                    "scopes": tokens.scopes(),
                }),
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "servers": entries }))?
        );
        return Ok(0);
    }

    if rows.is_empty() {
        println!("no server needs a login");
        return Ok(0);
    }
    // Padded by hand for the same reason every other table here is: ANSI
    // escapes skew `format!` widths.
    let width = rows
        .iter()
        .map(|row| row.name.chars().count())
        .max()
        .unwrap_or(0);
    for row in &rows {
        let pad = " ".repeat(width - row.name.chars().count());
        println!("  {}{pad}  {}", row.name, detail(row, color));
    }
    Ok(0)
}

/// The right-hand side of one `auth status` line: a report of the credential
/// this server holds, and a command to run only where there is one to run.
fn detail(row: &Row, color: bool) -> String {
    let Row {
        name,
        configured,
        tokens,
        credential,
        observed,
    } = row;
    match tokens {
        // Not a login prompt for a server that already holds a
        // credential: it is a report of the one it holds.
        None => match credential {
            Credential::Command => crate::ui::dim("headers from command", color),
            Credential::Header => crate::ui::dim("static header", color),
            // An `[auth]` table is the config saying this server does
            // OAuth, which is evidence enough on its own.
            Credential::Oauth => format!("no login yet — run mcpgw auth login {name}"),
            // Nothing in the config authenticates this one, so the only
            // evidence is what a probe saw — and the login hint goes to
            // the server that actually asked for a login.
            Credential::None => match observed {
                Observed::LoginRequired => {
                    format!("no login yet — run mcpgw auth login {name}")
                }
                Observed::NoAuthNeeded => crate::ui::dim(
                    "no auth needed (last probe succeeded without credentials)",
                    color,
                ),
                Observed::NotChecked => {
                    crate::ui::dim("not checked yet — run mcpgw doctor --probe", color)
                }
            },
        },
        Some(tokens) => {
            let identity = tokens.identity(configured.as_deref()).to_string();
            let issuer = tokens.issuer().unwrap_or("unnamed issuer");
            // The one state a user has to act on gets the command. Every
            // other line is a report, and a report does not tell people
            // to run things they do not have to.
            let suffix = if tokens.state() == mcpgw_core::auth::TokenState::Expired {
                format!(" — run mcpgw auth login {name}")
            } else {
                String::new()
            };
            format!(
                "{}{}  {issuer}  {}{suffix}",
                tokens.state().label(),
                remaining(tokens),
                crate::ui::dim(&identity, color),
            )
        }
    }
}

/// `" (42m left)"` for a token with a clock on it, nothing for one without.
fn remaining(tokens: &Tokens) -> String {
    let Some(at) = tokens.expires_at() else {
        return String::new();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let Some(left) = at.checked_sub(now).filter(|left| *left > 0) else {
        return String::new();
    };
    if left < 3600 {
        format!(" ({}m left)", left / 60)
    } else {
        format!(" ({}h left)", left / 3600)
    }
}

fn run_logout(args: &LogoutArgs) -> anyhow::Result<u8> {
    let state_dir = state_dir()?;
    if Tokens::delete(&state_dir, &args.name)? {
        println!("logged out of {:?} on this machine", args.name);
        // Said every time rather than only when it matters, because from here
        // there is no way to tell whether it matters: nothing in the token
        // file says whether the provider offers revocation, and mcpgw has
        // just deleted the only copy of the token it could have revoked with.
        println!(
            "  the provider may still hold the grant — revoke it there if this machine is at risk"
        );
    } else {
        println!("no stored login for {:?}", args.name);
    }
    Ok(0)
}
