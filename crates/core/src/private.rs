//! Owner-only filesystem helpers.
//!
//! Everything mcpgw keeps under the state dir is derived from client configs
//! — which carry API tokens and `Authorization` headers — so it inherits the
//! discipline capture files already had: directories 0700, files 0600. On
//! non-unix targets both are no-ops beyond ordinary creation, because the
//! mode bits have no equivalent there.
//!
//! The two entry points are public because the state directory has writers
//! outside this crate — the CLI's update-check stamp is one — and the
//! hardening is an invariant of the directory, not of any one writer. The
//! mode of `~/.local/share/mcpgw` is decided by whoever creates it first,
//! so a single writer reaching for bare `std::fs` silently loosens it for
//! everything that comes later.

use std::path::Path;

/// Creates `dir` and its missing parents, owner-only where the platform has
/// the concept. An existing directory keeps its current mode.
///
/// # Errors
///
/// Whatever [`std::fs::DirBuilder::create`] reports.
pub fn create_dir_all(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// Fsyncs the directory holding a file that was just replaced by rename.
/// Syncing the temp file only makes its *contents* durable; without this the
/// rename that publishes them can still be lost to a power cut. Windows
/// exposes no directory handle to sync, so there this is a no-op.
// The signature stays uniform so callers never cfg their error handling;
// off unix the body has nothing left that can fail.
#[cfg_attr(not(unix), allow(clippy::unnecessary_wraps))]
pub(crate) fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Narrows an existing file to owner read/write.
///
/// # Errors
///
/// Whatever [`std::fs::set_permissions`] reports.
// Same reason as `sync_dir`: uniform signature, nothing fallible off unix.
#[cfg_attr(not(unix), allow(clippy::unnecessary_wraps))]
pub fn harden_file(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}
