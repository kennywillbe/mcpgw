//! Whether the installed service still runs the mcpgw you are running.
//!
//! A service definition names the binary it was installed from by absolute
//! path (see [`DaemonSpec::exe`]), and nothing re-points it when the way
//! mcpgw is installed changes underneath — `cargo uninstall` then Homebrew
//! leaves a plist aimed at a path that is gone, and installing a second copy
//! leaves the supervisor happily running the first one while every upgrade
//! lands on the other. Both states are silent: the supervisor reports a
//! healthy job and the gateway answers.
//!
//! The same service is also worth an answer about *which build* it is
//! serving: launchd and systemd keep executing the binary they were handed
//! at start, so `brew upgrade` leaves yesterday's gateway answering on 8137
//! with nothing on the wire admitting it. That half reads the record the
//! running gateway publishes ([`crate::runtime`]) rather than the service
//! definition, and is only ever believed together with a live probe.
//!
//! Lives outside [`crate::daemon`] because that module is final by contract,
//! and outside [`crate::doctor`] because this reads the filesystem — doctor's
//! rules are pure by design and take their environment injected. It is one
//! module rather than one check per command so `daemon status`, `doctor` and
//! the wizard's status card cannot disagree about what "stale" means or
//! spell the fix three ways.

use std::path::{Path, PathBuf};

use crate::daemon::{DaemonSpec, GatewayReach};
use crate::doctor::{Finding, Severity};
use crate::runtime;

/// How the binary the service was installed from relates to the one running
/// this process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceExe {
    /// Same file, however it is spelled. Nothing to report.
    Matches,
    /// The recorded binary is no longer there at all.
    Missing { recorded: PathBuf, running: PathBuf },
    /// Both exist and they are different files.
    Differs { recorded: PathBuf, running: PathBuf },
}

impl ServiceExe {
    /// The one sentence every command says about a stale service, or `None`
    /// when there is nothing to say.
    ///
    /// Both halves name `mcpgw daemon install`, which since 0.4 reinstalls
    /// over its own running service: one command, no stop to remember.
    #[must_use]
    pub fn advice(&self) -> Option<String> {
        match self {
            ServiceExe::Matches => None,
            ServiceExe::Missing { recorded, running } => Some(format!(
                "installed from {}, which is gone — run `mcpgw daemon install` to point it at {}",
                recorded.display(),
                running.display()
            )),
            ServiceExe::Differs { recorded, running } => Some(format!(
                "runs {}; you are running {} — run `mcpgw daemon install` to switch it",
                recorded.display(),
                running.display()
            )),
        }
    }

    /// The same fact as a doctor finding.
    ///
    /// A warning, not an error: the service may be serving every client
    /// perfectly on the old binary, and the cost of ignoring this is an
    /// upgrade that quietly does not take effect — exactly the "works but
    /// stale" case [`Severity::Warning`] is for.
    #[must_use]
    pub fn finding(&self) -> Option<Finding> {
        self.advice().map(|advice| Finding {
            client: None,
            server: None,
            severity: Severity::Warning,
            message: format!("the gateway service {advice}"),
            code: None,
        })
    }
}

/// Compares the recorded binary with `running`.
///
/// Both sides are canonicalized before they are compared, because the same
/// file legitimately has several names: Homebrew's `/opt/homebrew/bin/mcpgw`
/// is a symlink into the Cellar, and reporting that as a different binary
/// would send everyone who is perfectly set up to reinstall. A recorded path
/// that cannot be resolved is [`ServiceExe::Missing`] — the interesting case
/// is a deleted binary, and a path we are not allowed to look at is no more
/// runnable by the supervisor than one that is gone.
///
/// The paths in the result are the ones passed in, not their canonical
/// forms: the user recognises the path they installed from, not the Cellar
/// path behind it.
#[must_use]
pub fn check_service_exe(recorded: &Path, running: &Path) -> ServiceExe {
    let Ok(real_recorded) = std::fs::canonicalize(recorded) else {
        return ServiceExe::Missing {
            recorded: recorded.to_owned(),
            running: running.to_owned(),
        };
    };
    // A running binary that cannot be canonicalized (deleted out from under
    // the process, which is how this bug is usually met on macOS) is still
    // compared, as given: it is either equal to the recorded path or it is
    // the difference worth reporting.
    let real_running = std::fs::canonicalize(running).unwrap_or_else(|_| running.to_owned());
    if real_recorded == real_running {
        ServiceExe::Matches
    } else {
        ServiceExe::Differs {
            recorded: recorded.to_owned(),
            running: running.to_owned(),
        }
    }
}

/// [`check_service_exe`] against the binary this process is running.
///
/// `None` when the platform will not say what that is: a check that cannot
/// name what you are running has nothing to advise switching to.
#[must_use]
pub fn service_exe(spec: &DaemonSpec) -> Option<ServiceExe> {
    let running = std::env::current_exe().ok()?;
    Some(check_service_exe(&spec.exe, &running))
}

/// How the mcpgw the gateway is *running* relates to the one running this
/// process.
///
/// A sibling of [`ServiceExe`] and not a variant of it: which binary a
/// service is aimed at and which build is answering right now are two facts
/// that come apart in both directions — a reinstall fixes the first without
/// restarting anything, and an upgrade in place replaces the file under a
/// process that keeps serving the old code. Both can be true at once, and a
/// reader that only prints one of them would be hiding half the answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceVersion {
    /// The gateway that answered is this build. Nothing to report.
    Same,
    /// It is answering on a different build than the one you typed.
    Differs { running: String, current: String },
    /// Nothing trustworthy to say: no gateway answered, or none published a
    /// record. Deliberately not an error and deliberately not a guess.
    Unknown,
}

impl ServiceVersion {
    /// The one sentence every command says about a gateway on another
    /// build, or `None` when there is nothing to say.
    ///
    /// Names `mcpgw daemon install` for the same reason [`ServiceExe::advice`]
    /// does: since 0.4 it reinstalls over its own running service, which is
    /// the only thing that gets a supervised gateway onto a new binary.
    #[must_use]
    pub fn advice(&self) -> Option<String> {
        match self {
            ServiceVersion::Same | ServiceVersion::Unknown => None,
            ServiceVersion::Differs { running, current } => Some(format!(
                "runs mcpgw {running}; you are running {current} — run \
                 `mcpgw daemon install` to restart it on this build"
            )),
        }
    }

    /// The same fact as a doctor finding, subject-prefixed like
    /// [`ServiceExe::finding`] so the two read as one family.
    ///
    /// A warning for the same reason: the old build may be serving every
    /// client perfectly, and what is broken is only that the upgrade did not
    /// reach it.
    #[must_use]
    pub fn finding(&self) -> Option<Finding> {
        self.advice().map(|advice| Finding {
            client: None,
            server: None,
            severity: Severity::Warning,
            message: format!("the gateway service {advice}"),
            code: None,
        })
    }
}

/// Compares two version strings as written.
///
/// No semver ordering: any difference is worth a line, including a service
/// that is *newer* than the CLI asking — which is what a stale `cargo
/// install` binary run against a Homebrew service looks like, and is just as
/// much a surprise worth naming as the upgrade that did not take.
#[must_use]
pub fn check_service_version(running: &str, current: &str) -> ServiceVersion {
    if running == current {
        ServiceVersion::Same
    } else {
        ServiceVersion::Differs {
            running: running.to_owned(),
            current: current.to_owned(),
        }
    }
}

/// What the gateway on `port` is running, according to the record it
/// published, against this build.
///
/// `reach` is the caller's own probe of that port and is not optional: a
/// record outlives the process that wrote it (a crash, a `kill -9`, a lost
/// machine), so believing one without a live answer on the port would report
/// the version of a gateway that stopped running last week. Anything short
/// of an answer — a port nobody holds, a port held by something that is not
/// HTTP, no record, or a record nobody can parse — is
/// [`ServiceVersion::Unknown`]. A corrupt record is not an error here for
/// the same reason: three read-only commands would each have to decide what
/// to do about it, and "cannot say" is the honest answer for all of them.
#[must_use]
pub fn service_version(state_dir: &Path, port: u16, reach: GatewayReach) -> ServiceVersion {
    if !reach.is_up() {
        return ServiceVersion::Unknown;
    }
    let Ok(Some(record)) = runtime::read_record(state_dir, port) else {
        return ServiceVersion::Unknown;
    };
    // Compared against core's version rather than the caller's: the CLI
    // writes the record and both crates are released from this workspace at
    // one version, so there is one number here and it is this one.
    check_service_version(&record.version, env!("CARGO_PKG_VERSION"))
}

/// The port a reader probed, off the URL it probed.
///
/// Records are keyed by port and every reader holds a URL instead — the one
/// from `--url`, from `daemon.json`, or from the wizard's `--gateway-url`.
/// Parsing it here keeps the three of them from disagreeing about which file
/// answers for which address.
#[must_use]
pub fn url_port(url: &str) -> Option<u16> {
    url::Url::parse(url).ok()?.port_or_known_default()
}

/// The host a reader probed, off the URL it probed, with an IPv6 literal's
/// brackets removed.
///
/// Sits next to [`url_port`] because it answers the other half of the same
/// question — a reader holding a URL and needing the address behind it, to
/// ask [`crate::daemon::is_loopback`] about or to bind. Bracketless because
/// both of those want it that way, and a caller that needs the URL spelling
/// back already has the URL.
#[must_use]
pub fn url_host(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    Some(parsed.host_str()?.trim_matches(['[', ']']).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A published record for `port`, with only the version the test cares
    /// about spelled out.
    fn write_record(state_dir: &Path, port: u16, version: &str) {
        crate::runtime::write_record(
            state_dir,
            &crate::runtime::GatewayRecord {
                version: version.to_owned(),
                pid: 4321,
                exe: PathBuf::from("/usr/local/bin/mcpgw"),
                bind: "127.0.0.1".to_owned(),
                port,
                started_at: 0,
                last_upgrade_restart: None,
            },
        )
        .unwrap();
    }

    fn write_binary(path: &Path) {
        std::fs::write(path, b"not really a binary").unwrap();
    }

    #[test]
    fn a_recorded_binary_that_is_gone_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let running = dir.path().join("running");
        write_binary(&running);
        let gone = dir.path().join("uninstalled");

        assert_eq!(
            check_service_exe(&gone, &running),
            ServiceExe::Missing {
                recorded: gone,
                running,
            }
        );
    }

    #[test]
    fn a_symlink_to_the_running_binary_is_not_a_difference() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("mcpgw-real");
        write_binary(&real);
        let link = dir.path().join("mcpgw-link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // A hard link is not a stand-in for a symlink here: it is a second
        // directory entry for one file, and `canonicalize` resolves it to
        // itself rather than to the other name, so the assertions below can
        // never hold. The case worth guarding is Homebrew's symlink, so the
        // test creates one.
        #[cfg(windows)]
        if let Err(err) = std::os::windows::fs::symlink_file(&real, &link) {
            // Creating a symlink on Windows needs SeCreateSymbolicLink,
            // which CI's runners hold and a locked-down account does not.
            // That is a fact about the account, not about the code, so it
            // skips rather than fails; every other error still panics.
            const ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;
            if err.kind() == std::io::ErrorKind::PermissionDenied
                || err.raw_os_error() == Some(ERROR_PRIVILEGE_NOT_HELD)
            {
                eprintln!("skipped: symlinks need a privilege this account lacks");
                return;
            }
            panic!("could not create a symlink at {}: {err}", link.display());
        }

        assert_eq!(check_service_exe(&link, &real), ServiceExe::Matches);
        assert_eq!(check_service_exe(&real, &link), ServiceExe::Matches);
    }

    #[test]
    fn two_installed_copies_differ() {
        let dir = tempfile::tempdir().unwrap();
        let cargo = dir.path().join("cargo-mcpgw");
        let brew = dir.path().join("brew-mcpgw");
        write_binary(&cargo);
        write_binary(&brew);

        assert_eq!(
            check_service_exe(&cargo, &brew),
            ServiceExe::Differs {
                recorded: cargo,
                running: brew,
            }
        );
    }

    #[test]
    fn only_a_stale_service_has_anything_to_say() {
        let dir = tempfile::tempdir().unwrap();
        let recorded = dir.path().join("old");
        let running = dir.path().join("new");

        assert_eq!(ServiceExe::Matches.advice(), None);
        assert_eq!(ServiceExe::Matches.finding(), None);

        let missing = ServiceExe::Missing {
            recorded: recorded.clone(),
            running: running.clone(),
        };
        let advice = missing.advice().unwrap();
        assert!(advice.contains("which is gone"), "{advice}");
        assert!(advice.contains("`mcpgw daemon install`"), "{advice}");
        let finding = missing.finding().unwrap();
        assert_eq!(finding.severity, Severity::Warning);
        assert!(finding.message.ends_with(&advice), "{}", finding.message);

        let differs = ServiceExe::Differs { recorded, running };
        let advice = differs.advice().unwrap();
        assert!(advice.contains("you are running"), "{advice}");
        assert_eq!(differs.finding().unwrap().severity, Severity::Warning);
    }

    /// A record without a live gateway is a record about a process that is
    /// gone — the case a crashed 0.4 gateway leaves on every machine.
    #[test]
    fn a_version_is_only_read_off_a_port_that_answered() {
        let dir = tempfile::tempdir().unwrap();
        write_record(dir.path(), 8137, "0.0.1");

        for reach in [GatewayReach::Down, GatewayReach::NotHttp] {
            assert_eq!(
                service_version(dir.path(), 8137, reach),
                ServiceVersion::Unknown,
                "{reach:?}"
            );
        }
    }

    #[test]
    fn a_gateway_that_published_nothing_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            service_version(dir.path(), 8137, GatewayReach::Answering(405)),
            ServiceVersion::Unknown
        );

        // Neither is a record nobody can parse: it is a half-written file or
        // one from a future shape, and either way there is nothing to say.
        std::fs::write(crate::runtime::record_path(dir.path(), 8137), b"{").unwrap();
        assert_eq!(
            service_version(dir.path(), 8137, GatewayReach::Answering(405)),
            ServiceVersion::Unknown
        );
    }

    #[test]
    fn the_gateway_running_this_build_says_nothing() {
        let dir = tempfile::tempdir().unwrap();
        write_record(dir.path(), 8137, env!("CARGO_PKG_VERSION"));

        let version = service_version(dir.path(), 8137, GatewayReach::Answering(405));
        assert_eq!(version, ServiceVersion::Same);
        assert_eq!(version.advice(), None);
        assert_eq!(version.finding(), None);
    }

    /// Both directions: the service left behind by an upgrade, and the old
    /// `cargo install` binary asking a Homebrew service that is ahead of it.
    #[test]
    fn any_difference_is_reported_whichever_side_is_older() {
        let dir = tempfile::tempdir().unwrap();
        write_record(dir.path(), 8137, "0.0.1");

        let version = service_version(dir.path(), 8137, GatewayReach::Answering(405));
        assert_eq!(
            version,
            ServiceVersion::Differs {
                running: "0.0.1".to_owned(),
                current: env!("CARGO_PKG_VERSION").to_owned(),
            }
        );

        assert_eq!(
            check_service_version("9.9.9", "0.4.0"),
            ServiceVersion::Differs {
                running: "9.9.9".to_owned(),
                current: "0.4.0".to_owned(),
            }
        );
    }

    #[test]
    fn the_version_sentence_names_both_builds_and_the_fix() {
        let advice = check_service_version("0.4.0", "0.4.1").advice().unwrap();
        assert_eq!(
            advice,
            "runs mcpgw 0.4.0; you are running 0.4.1 — run `mcpgw daemon install` to restart it \
             on this build"
        );
        let finding = check_service_version("0.4.0", "0.4.1").finding().unwrap();
        assert_eq!(finding.severity, Severity::Warning);
        assert_eq!(finding.message, format!("the gateway service {advice}"));

        assert_eq!(ServiceVersion::Unknown.advice(), None);
        assert_eq!(ServiceVersion::Unknown.finding(), None);
    }

    #[test]
    fn the_port_a_reader_probed_comes_off_its_url() {
        assert_eq!(url_port("http://127.0.0.1:8137/mcp"), Some(8137));
        // The default the scheme implies, for a URL that left it out.
        assert_eq!(url_port("http://localhost/mcp"), Some(80));
        assert_eq!(url_port("not a url"), None);
    }

    #[test]
    fn the_host_comes_off_the_same_url_without_its_brackets() {
        assert_eq!(
            url_host("http://127.0.0.1:8137/mcp").as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(url_host("http://[::1]:8137/mcp").as_deref(), Some("::1"));
        assert_eq!(
            url_host("http://localhost/mcp").as_deref(),
            Some("localhost")
        );
        assert_eq!(url_host("not a url"), None);
    }
}
