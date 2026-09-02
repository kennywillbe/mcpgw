//! Where releases are read from, and the two GETs that read them.

use std::time::Duration;

use anyhow::Context as _;

/// The repository releases are published to.
const REPO: &str = "kennywillbe/mcpgw";

/// Points every release lookup at another host. This is a test seam: the
/// suite serves canned release JSON and archives from a loopback listener,
/// which is the only way to exercise the download path without reaching
/// github.com from CI.
const BASE_URL_ENV: &str = "MCPGW_UPDATE_BASE_URL";

/// GitHub rejects API requests without one, and a named agent is what shows
/// up in the repository's traffic if this ever needs debugging.
const USER_AGENT: &str = concat!("mcpgw/", env!("CARGO_PKG_VERSION"));

/// A release archive is a few megabytes; the cap only exists so a wrong URL
/// cannot stream unbounded data into memory.
const MAX_ASSET_BYTES: u64 = 64 * 1024 * 1024;

/// The two hosts a release lives on: the API that names the latest tag and
/// the download host that serves its assets.
pub struct Endpoints {
    api: String,
    download: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            api: format!("https://api.github.com/repos/{REPO}"),
            download: format!("https://github.com/{REPO}/releases/download"),
        }
    }
}

impl Endpoints {
    /// Reads the endpoints from the environment, honouring the test seam.
    ///
    /// An override collapses both hosts onto one base, laid out with the
    /// same paths GitHub uses, so the production URLs stay exactly what
    /// [`Endpoints::default`] builds.
    #[must_use]
    pub fn from_env() -> Self {
        match std::env::var(BASE_URL_ENV) {
            Ok(base) if !base.is_empty() => Self::with_base(&base),
            _ => Self::default(),
        }
    }

    fn with_base(base: &str) -> Self {
        let base = base.trim_end_matches('/').to_owned();
        Self {
            download: format!("{base}/releases/download"),
            api: base,
        }
    }

    #[must_use]
    pub fn latest_url(&self) -> String {
        format!("{}/releases/latest", self.api)
    }

    #[must_use]
    pub fn asset_url(&self, version: &str, name: &str) -> String {
        format!("{}/v{version}/{name}", self.download)
    }
}

/// A client with a hard total deadline, so no update check can ever hold a
/// command hostage to a hung connection.
#[must_use]
pub fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .user_agent(USER_AGENT)
        .build()
        .into()
}

/// Turns a request failure into something worth reading. A 404 is the one
/// status worth naming: it is the difference between "the network is down"
/// and "that release or asset was never published", which is the failure a
/// user can act on.
fn describe(err: ureq::Error, url: &str, missing: &str) -> anyhow::Error {
    match err {
        ureq::Error::StatusCode(404) => anyhow::anyhow!("{url} returned 404 — {missing}"),
        other => anyhow::Error::new(other).context(format!("cannot reach {url}")),
    }
}

/// The version of the latest published release, without the leading `v`.
///
/// # Errors
///
/// Any transport, status or shape failure; callers decide whether that is
/// worth reporting.
pub fn latest_version(agent: &ureq::Agent, endpoints: &Endpoints) -> anyhow::Result<String> {
    let url = endpoints.latest_url();
    let body = agent
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|err| describe(err, &url, "the repository has no published release"))?
        .body_mut()
        .read_to_string()
        .with_context(|| format!("cannot read the release list from {url}"))?;
    let release: serde_json::Value =
        serde_json::from_str(&body).with_context(|| format!("{url} did not return JSON"))?;
    let tag = release["tag_name"]
        .as_str()
        .with_context(|| format!("{url} returned a release without a tag_name"))?;
    Ok(tag.strip_prefix('v').unwrap_or(tag).to_owned())
}

/// Downloads one release asset in full.
///
/// # Errors
///
/// Any transport or status failure, and anything larger than the size cap.
pub fn fetch(agent: &ureq::Agent, url: &str) -> anyhow::Result<Vec<u8>> {
    agent
        .get(url)
        .call()
        .map_err(|err| describe(err, url, "the release does not carry that file"))?
        .body_mut()
        .with_config()
        .limit(MAX_ASSET_BYTES)
        .read_to_vec()
        .with_context(|| format!("cannot read {url}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_endpoints_are_the_real_release_urls() {
        let endpoints = Endpoints::default();
        assert_eq!(
            endpoints.latest_url(),
            "https://api.github.com/repos/kennywillbe/mcpgw/releases/latest"
        );
        assert_eq!(
            endpoints.asset_url("0.2.0", "mcpgw-0.2.0-x86_64-apple-darwin.tar.gz"),
            "https://github.com/kennywillbe/mcpgw/releases/download/v0.2.0/mcpgw-0.2.0-x86_64-apple-darwin.tar.gz"
        );
    }

    #[test]
    fn an_override_moves_both_urls_and_tolerates_a_trailing_slash() {
        for base in ["http://127.0.0.1:9", "http://127.0.0.1:9/"] {
            let endpoints = Endpoints::with_base(base);
            assert_eq!(endpoints.latest_url(), "http://127.0.0.1:9/releases/latest");
            assert_eq!(
                endpoints.asset_url("1.0.0", "SHA256SUMS"),
                "http://127.0.0.1:9/releases/download/v1.0.0/SHA256SUMS"
            );
        }
    }
}
