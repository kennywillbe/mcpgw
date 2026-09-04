//! The install token every client presents to the gateway.
//!
//! Until 0.5 the whole trust boundary was "loopback only": the gateway held
//! nothing a process running as you could not read straight out of
//! `~/.cursor/mcp.json`, so a socket on 127.0.0.1 was the same door those
//! files already were. `mcpgw auth login` ended that. The gateway now holds
//! OAuth refresh tokens for remote servers, and those live nowhere else —
//! reaching the port is worth strictly more than reading every client config
//! on the machine.
//!
//! So the port gets a credential of its own: 32 bytes from the OS random
//! source, base64url, generated once per install and kept at
//! `<state>/gateway.token` mode 0600. `sync` writes it into every managed
//! client entry as `Authorization: Bearer <token>`, `connect` reads it back
//! out of the same file, and the gateway checks it on every `/s/<name>`
//! request.
//!
//! What it is not: a bearer token in a file readable by your uid does not
//! defend against a process running as you, which can simply read it. It
//! defends the *port* — a listener past loopback, a container sharing the
//! host's network, another user on a multi-user box — which is exactly the
//! ground `--bind` beyond 127.0.0.1 was refused over.

use std::path::{Path, PathBuf};

use crate::error::Error;

/// Filename under the state dir. A single flat file rather than something
/// under `auth/`: that directory is one file per *upstream* login, and this
/// is the gateway's own front door.
pub const FILE: &str = "gateway.token";

/// Bytes of entropy behind a token, before encoding.
pub const BYTES: u32 = 32;

/// The scheme the token is presented under, and the whole of the challenge
/// sent back when it is missing. Deliberately bare: RFC 6750 allows a `realm`
/// and MCP's own challenge carries `resource_metadata` pointing at an
/// authorization server, and this is a static string with neither. A client
/// that read a discovery URL here would start an OAuth flow against something
/// that has never issued a token in its life.
pub const CHALLENGE: &str = "Bearer";

/// One install's gateway token.
///
/// Compared rather than printed: [`Debug`] is implemented by hand so a token
/// cannot reach a log through a `{:?}` on some struct that happens to hold
/// one.
#[derive(Clone, PartialEq, Eq)]
pub struct GatewayToken(String);

impl std::fmt::Debug for GatewayToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GatewayToken(***)")
    }
}

impl GatewayToken {
    /// A fresh token from the OS random source.
    #[must_use]
    pub fn generate() -> Self {
        // oauth2's own random-token constructor, which is the one already
        // minting this process's PKCE verifiers and CSRF states: 32 bytes out
        // of a CSPRNG, base64url with no padding. A second generator here
        // would be a second thing to get wrong.
        Self(oauth2::CsrfToken::new_random_len(BYTES).into_secret())
    }

    /// Wraps a token that came from somewhere else — the file, or a test.
    #[must_use]
    pub fn from_secret(secret: impl Into<String>) -> Self {
        Self(secret.into())
    }

    /// Where the token lives under `state_dir`.
    #[must_use]
    pub fn path(state_dir: &Path) -> PathBuf {
        state_dir.join(FILE)
    }

    /// The token itself. Named like the secret it is, so a call site that
    /// puts one somewhere durable reads as the decision it is.
    #[must_use]
    pub fn secret(&self) -> &str {
        &self.0
    }

    /// The header value a client entry carries.
    #[must_use]
    pub fn header_value(&self) -> String {
        format!("Bearer {}", self.0)
    }

    /// Enough of the token to tell two apart in a terminal, and not enough to
    /// use. What `token show` and `sync --dry-run` print.
    #[must_use]
    pub fn masked(&self) -> String {
        // The prefix alone: a suffix as well would make a shoulder-surfed
        // screenshot plus a guess at the middle meaningfully easier, and
        // there is only ever one token to recognise.
        format!("{}…", self.0.chars().take(6).collect::<String>())
    }

    /// Whether `presented` is this token.
    ///
    /// Non-short-circuiting: every byte is folded in whatever the ones before
    /// it said, so the time the comparison takes carries no information about
    /// how long a shared prefix was. The length is compared first and leaks
    /// as it always did — every token this install ever mints is 43
    /// characters, so there is nothing there to learn.
    #[must_use]
    pub fn matches(&self, presented: &str) -> bool {
        let (want, got) = (self.0.as_bytes(), presented.as_bytes());
        want.len() == got.len()
            && want
                .iter()
                .zip(got)
                .fold(0u8, |differs, (a, b)| differs | (a ^ b))
                == 0
    }

    /// Reads the token, or [`None`] when this install has none yet.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] for a read failure other than not-found.
    pub fn load(state_dir: &Path) -> Result<Option<Self>, Error> {
        let path = Self::path(state_dir);
        match std::fs::read_to_string(&path) {
            // Trimmed: the file is one line, and an editor that added a
            // newline must not silently lock everybody out.
            Ok(text) => Ok(Some(Self(text.trim().to_owned()))),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(Error::Io { path, source }),
        }
    }

    /// Reads the token, minting and storing one if there is none. The flag
    /// says whether this call is what created it, which is the one moment
    /// worth a line of output.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the token cannot be read or written.
    pub fn load_or_create(state_dir: &Path) -> Result<(Self, bool), Error> {
        if let Some(token) = Self::load(state_dir)? {
            return Ok((token, false));
        }
        let token = Self::generate();
        token.save(state_dir)?;
        Ok((token, true))
    }

    /// Mints a new token over whatever was there. Every client entry holding
    /// the old one stops working until `sync` runs, which is the point.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the token cannot be written.
    pub fn rotate(state_dir: &Path) -> Result<Self, Error> {
        let token = Self::generate();
        token.save(state_dir)?;
        Ok(token)
    }

    /// Writes the token 0600 inside the 0700 state dir, atomically.
    ///
    /// The mode is set on the temp file before it is published rather than on
    /// the destination afterwards: a rotate that hardened after the rename
    /// would leave the new token world-readable for the length of one
    /// syscall.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] for any filesystem failure.
    pub fn save(&self, state_dir: &Path) -> Result<(), Error> {
        use std::io::Write as _;

        let path = Self::path(state_dir);
        let io_err = |p: &Path| {
            let p = p.to_owned();
            move |source| Error::Io { path: p, source }
        };
        crate::private::create_dir_all(state_dir).map_err(io_err(state_dir))?;
        let mut tmp = tempfile::Builder::new()
            .prefix(".gateway.token.")
            .tempfile_in(state_dir)
            .map_err(io_err(state_dir))?;
        crate::private::harden_file(tmp.path()).map_err(io_err(&path))?;
        writeln!(tmp, "{}", self.0).map_err(io_err(&path))?;
        tmp.as_file().sync_all().map_err(io_err(&path))?;
        tmp.persist(&path).map_err(|err| Error::Io {
            path: path.clone(),
            source: err.error,
        })?;
        crate::private::sync_dir(state_dir).map_err(io_err(state_dir))?;
        Ok(())
    }
}

/// The token off an `Authorization` header value, or [`None`] when the header
/// is absent or carries some other scheme.
///
/// The scheme is matched case-insensitively because RFC 7235 says it is
/// case-insensitive, and at least one client in the matrix writes `bearer`.
#[must_use]
pub fn presented(headers: &http::HeaderMap) -> Option<&str> {
    let value = headers.get(http::header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|token| !token.is_empty())
}

/// Which bind addresses a supervised gateway may be installed on.
///
/// Lives here rather than in `daemon` because it is a property of the token,
/// not of any supervisor: the reason `--bind 0.0.0.0` was refused outright
/// was that the port had no credential, and the reason it can be allowed is
/// that it now does. `daemon::preflight` asks this one question and stays the
/// single place the refusal is spelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BindPolicy {
    /// The historical rule, and still the default: loopback or nothing.
    #[default]
    LoopbackOnly,
    /// A token exists and `[gateway] require_token` is on, so every request
    /// past loopback has to carry it. Any address is then a decision the
    /// operator is allowed to make.
    Authenticated,
}

impl BindPolicy {
    /// What a gateway with this token and this config setting may bind.
    ///
    /// Both halves are required. A token with the grace period still running
    /// is not a boundary — an unauthenticated loopback request still passes —
    /// and `require_token` without a token file is a rule with nothing to
    /// enforce.
    #[must_use]
    pub fn new(require_token: bool, token: Option<&GatewayToken>) -> Self {
        if require_token && token.is_some() {
            Self::Authenticated
        } else {
            Self::LoopbackOnly
        }
    }

    /// Whether `bind` is an address this policy allows.
    #[must_use]
    pub fn permits(self, bind: &str) -> bool {
        self == Self::Authenticated || crate::daemon::is_loopback(bind)
    }
}

#[cfg(test)]
mod tests {
    use super::{BindPolicy, GatewayToken, presented};

    #[test]
    fn a_generated_token_is_thirty_two_bytes_of_base64url() {
        let token = GatewayToken::generate();
        // 32 bytes base64url with no padding is 43 characters, and the
        // alphabet has no `+`, `/` or `=` — which is what makes it safe to
        // paste into a JSON string and a TOML one alike.
        assert_eq!(token.secret().len(), 43);
        assert!(
            token
                .secret()
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "{}",
            token.secret()
        );
        assert_ne!(token.secret(), GatewayToken::generate().secret());
    }

    #[test]
    fn a_token_never_prints_itself() {
        let token = GatewayToken::from_secret("supersecretvalue");
        assert_eq!(format!("{token:?}"), "GatewayToken(***)");
        assert_eq!(token.masked(), "supers…");
        assert!(!token.masked().contains("value"));
    }

    #[test]
    fn only_the_exact_token_matches() {
        let token = GatewayToken::from_secret("abc123");
        assert!(token.matches("abc123"));
        assert!(!token.matches("abc124"));
        // A prefix and an extension both fail, which is the whole of what
        // the length check buys.
        assert!(!token.matches("abc"));
        assert!(!token.matches("abc1234"));
        assert!(!token.matches(""));
        assert!(!GatewayToken::from_secret("").matches("x"));
    }

    #[test]
    fn the_header_is_read_by_scheme_and_nothing_else() {
        let header = |value: &str| {
            let mut headers = http::HeaderMap::new();
            headers.insert(http::header::AUTHORIZATION, value.parse().unwrap());
            headers
        };
        assert_eq!(presented(&header("Bearer abc")), Some("abc"));
        assert_eq!(presented(&header("bearer abc")), Some("abc"));
        assert_eq!(presented(&header("Basic abc")), None);
        assert_eq!(presented(&header("Bearer ")), None);
        assert_eq!(presented(&header("abc")), None);
        assert_eq!(presented(&http::HeaderMap::new()), None);
    }

    #[test]
    fn a_bind_past_loopback_needs_both_a_token_and_the_switch() {
        let token = GatewayToken::generate();
        let permits = |require, token| BindPolicy::new(require, token).permits("0.0.0.0");
        assert!(permits(true, Some(&token)));
        // A token alone is the grace period, where an unauthenticated
        // loopback request still passes — no boundary to open the bind on.
        assert!(!permits(false, Some(&token)));
        assert!(!permits(true, None));
        // Loopback never needed either of them.
        assert!(BindPolicy::default().permits("127.0.0.1"));
        assert!(BindPolicy::default().permits("localhost"));
    }
}
