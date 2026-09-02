//! systemd user unit. **Stub** — see the contract in [`super`]: filling this
//! file in is the whole of the systemd milestone, and nothing outside it
//! should need to change.

use super::{DaemonError, DaemonSpec, Installed, ServiceManager, ServiceStatus};

/// Unit name, fixed here so uninstall can find what an older mcpgw wrote.
pub const UNIT: &str = "mcpgw.service";

const NOT_YET: &str = "the systemd --user unit is not in this release yet — \
    run `mcpgw serve` in a terminal (or under a supervisor of your own) for \
    now, and `mcpgw daemon status` will still report on it";

/// systemd in user mode, through `systemctl --user`.
#[derive(Debug, Default, Clone, Copy)]
pub struct Systemd {
    _private: (),
}

impl Systemd {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl ServiceManager for Systemd {
    fn name(&self) -> &'static str {
        "systemd --user"
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
