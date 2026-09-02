//! macOS launch agent. **Stub** — see the contract in [`super`]: filling
//! this file in is the whole of the launchd milestone, and nothing outside
//! it should need to change.

use super::{DaemonError, DaemonSpec, Installed, ServiceManager, ServiceStatus};

/// The reverse-DNS label a launch agent is addressed by. Fixed here rather
/// than in the implementation because uninstall has to find what a previous
/// version of mcpgw installed.
pub const LABEL: &str = "io.mcpgw.gateway";

/// Every operation, in one sentence, because a user who runs into this wants
/// the workaround more than the explanation.
const NOT_YET: &str = "the macOS launch agent is not in this release yet — \
    run `mcpgw serve` in a terminal for now, and `mcpgw daemon status` will \
    still report on it";

/// launchd, through `launchctl bootstrap gui/<uid>`.
#[derive(Debug, Default, Clone, Copy)]
pub struct Launchd {
    // Private so the struct can gain fields without breaking construction.
    _private: (),
}

impl Launchd {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl ServiceManager for Launchd {
    fn name(&self) -> &'static str {
        "launchd"
    }

    fn install(&self, _spec: &DaemonSpec) -> Result<Installed, DaemonError> {
        Err(DaemonError::NotSupportedYet(NOT_YET))
    }

    fn uninstall(&self) -> Result<(), DaemonError> {
        Err(DaemonError::NotSupportedYet(NOT_YET))
    }

    fn start(&self, _spec: &DaemonSpec) -> Result<(), DaemonError> {
        Err(DaemonError::NotSupportedYet(NOT_YET))
    }

    fn stop(&self) -> Result<(), DaemonError> {
        Err(DaemonError::NotSupportedYet(NOT_YET))
    }

    fn query(&self) -> Result<ServiceStatus, DaemonError> {
        Err(DaemonError::NotSupportedYet(NOT_YET))
    }
}
