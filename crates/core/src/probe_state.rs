//! What the last probe learned about a server's authentication, kept across
//! runs so a command that does not probe can still report it.
//!
//! `mcpgw auth status` is a config read: it opens no socket, and from the
//! config alone a server that has never needed a credential looks exactly
//! like one that has never been asked. `doctor --probe` and `auth login`
//! already ask — a clean `initialize` against a server carrying nothing is
//! proof no login is wanted, a 401 with OAuth metadata is proof one is — so
//! they leave the answer here and `auth status` reads it back.
//!
//! Losing this file is safe: every server reads back as "not checked yet",
//! which is what it was before anything probed. Nothing in it is secret —
//! a server name and what it said about authentication — but it lives in the
//! state directory beside the tokens and gets the same owner-only treatment
//! rather than a second rule to remember.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, io_err};

/// What a probe observed about a server's need for a login.
///
/// Only the two outcomes that say something about authentication are here. A
/// timeout, a spawn failure or a handshake error says nothing about it, so a
/// probe that ends in one leaves whatever was already recorded alone rather
/// than overwriting knowledge with a network hiccup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthObservation {
    /// The probe completed the handshake while presenting no credential at
    /// all: no stored token, no `[auth]` table, no headers.
    NoAuthNeeded,
    /// The probe was answered 401 with OAuth discovery metadata.
    LoginRequired,
}

impl AuthObservation {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::NoAuthNeeded => "no_auth_needed",
            Self::LoginRequired => "login_required",
        }
    }
}

/// One server's last recorded observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub auth: AuthObservation,
    /// Unix seconds, for a reader that wants to say how old the answer is.
    #[serde(default)]
    pub at: u64,
}

/// The whole file: what every probed server last said.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeState {
    /// Keyed by canonical server name.
    #[serde(default)]
    pub servers: BTreeMap<String, Observation>,
}

/// The file this record lives in, under the state directory.
#[must_use]
pub fn path(state_dir: &Path) -> PathBuf {
    state_dir.join("probes.json")
}

impl ProbeState {
    /// Reads the record, or an empty one when there is nothing to read.
    ///
    /// A file that will not parse reads as empty too: it is a cache of
    /// observations, and refusing to print `auth status` because a cache went
    /// bad would be the worse failure.
    #[must_use]
    pub fn load(state_dir: &Path) -> Self {
        std::fs::read_to_string(path(state_dir))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<Observation> {
        self.servers.get(name).copied()
    }

    /// Merges `seen` into the stored record and writes it back.
    ///
    /// Merged rather than replaced because a probe pass covers whatever the
    /// run happened to target: `doctor --probe` on a config of one server
    /// must not erase what the other servers last said.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] for filesystem failures.
    pub fn record(
        state_dir: &Path,
        seen: impl IntoIterator<Item = (String, AuthObservation)>,
    ) -> Result<(), Error> {
        let mut state = Self::load(state_dir);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut changed = false;
        for (name, auth) in seen {
            state.servers.insert(name, Observation { auth, at: now });
            changed = true;
        }
        if !changed {
            return Ok(());
        }
        state.save(state_dir)
    }

    /// Atomically writes the record, creating the state directory as needed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] for filesystem failures.
    pub fn save(&self, state_dir: &Path) -> Result<(), Error> {
        let file = path(state_dir);
        let text = serde_json::to_string_pretty(self)
            .map_err(std::io::Error::other)
            .map_err(io_err(&file))?;
        crate::private::write_atomically(&file, text.as_bytes()).map_err(io_err(&file))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_file_reads_as_nothing_known() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(ProbeState::load(dir.path()), ProbeState::default());
    }

    #[test]
    fn a_record_survives_a_round_trip_and_later_passes_merge_into_it() {
        let dir = tempfile::tempdir().unwrap();
        ProbeState::record(
            dir.path(),
            [("github".to_owned(), AuthObservation::LoginRequired)],
        )
        .unwrap();
        ProbeState::record(
            dir.path(),
            [("deepwiki".to_owned(), AuthObservation::NoAuthNeeded)],
        )
        .unwrap();

        let state = ProbeState::load(dir.path());
        assert_eq!(
            state.get("github").map(|seen| seen.auth),
            Some(AuthObservation::LoginRequired)
        );
        assert_eq!(
            state.get("deepwiki").map(|seen| seen.auth),
            Some(AuthObservation::NoAuthNeeded)
        );
        assert!(state.get("mslearn").is_none());
    }

    #[test]
    fn a_later_pass_overwrites_what_the_same_server_said_before() {
        let dir = tempfile::tempdir().unwrap();
        ProbeState::record(
            dir.path(),
            [("github".to_owned(), AuthObservation::LoginRequired)],
        )
        .unwrap();
        ProbeState::record(
            dir.path(),
            [("github".to_owned(), AuthObservation::NoAuthNeeded)],
        )
        .unwrap();
        assert_eq!(
            ProbeState::load(dir.path()).get("github").map(|s| s.auth),
            Some(AuthObservation::NoAuthNeeded)
        );
    }

    #[test]
    fn a_file_that_will_not_parse_reads_as_nothing_known() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(path(dir.path()), "{not json").unwrap();
        assert_eq!(ProbeState::load(dir.path()), ProbeState::default());
    }
}
