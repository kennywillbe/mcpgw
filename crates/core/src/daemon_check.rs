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
//! Lives outside [`crate::daemon`] because that module is final by contract,
//! and outside [`crate::doctor`] because this reads the filesystem — doctor's
//! rules are pure by design and take their environment injected. It is one
//! module rather than one check per command so `daemon status`, `doctor` and
//! the wizard's status card cannot disagree about what "stale" means or
//! spell the fix three ways.

use std::path::{Path, PathBuf};

use crate::daemon::DaemonSpec;
use crate::doctor::{Finding, Severity};

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

#[cfg(test)]
mod tests {
    use super::*;

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
        // Windows needs a privilege for symlinks that CI does not grant; a
        // hard link resolves to the same file and proves the same thing.
        #[cfg(windows)]
        std::fs::hard_link(&real, &link).unwrap();

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
}
