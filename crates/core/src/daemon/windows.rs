//! Windows service. **Stub** — see the contract in [`super`]: filling this
//! file in is the whole of the Windows milestone, and nothing outside it
//! should need to change.

use super::{DaemonError, DaemonSpec, Installed, ServiceManager, ServiceStatus};

/// Service name, fixed here so uninstall can find what an older mcpgw
/// registered.
pub const SERVICE_NAME: &str = "mcpgw";

const NOT_YET: &str = "the Windows service is not in this release yet — \
    run `mcpgw serve` in a terminal for now, and `mcpgw daemon status` will \
    still report on it";

/// The Windows service control manager.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsService {
    _private: (),
}

impl WindowsService {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl ServiceManager for WindowsService {
    fn name(&self) -> &'static str {
        "the Windows service manager"
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
