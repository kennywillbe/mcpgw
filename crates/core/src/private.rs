//! Owner-only filesystem helpers.
//!
//! Everything mcpgw keeps under the state dir is derived from client configs
//! — which carry API tokens and `Authorization` headers — so it inherits the
//! discipline capture files already had: directories 0700, files 0600. On
//! non-unix targets both are no-ops beyond ordinary creation, because the
//! mode bits have no equivalent there.
//!
//! The entry points are public because the state directory has writers
//! outside this crate — the CLI's update-check stamp and its client-config
//! writer are two — and the hardening is an invariant of the directory, not
//! of any one writer. The
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

/// Replaces `path` with `bytes`, atomically and owner-only.
///
/// The invariant, in the order it has to happen:
///
/// 1. the parent directory exists and is owner-only ([`create_dir_all`]);
/// 2. the new contents are staged in a temp file *in that same directory*,
///    so publishing them is a rename and never a cross-device copy;
/// 3. the temp file is narrowed to 0600 **before** the first byte is written
///    — a file created at the process umask and hardened after the rename is
///    world-readable for the length of the write, and some of these files
///    are bearer tokens;
/// 4. the bytes are fsynced, then renamed over `path`, so a crash yields the
///    old file or the new one and never a half-written one;
/// 5. the parent directory is fsynced, because syncing the bytes leaves the
///    rename that publishes them undurable.
///
/// Any failure drops the temp file, which unlinks it: a failed write leaves
/// neither a damaged destination nor a stray temp behind.
///
/// Every state file mcpgw writes goes through here. It is `pub` because the
/// state directory has writers outside this crate, and a writer that rolls
/// its own sequence is how the copies this replaced drifted apart.
///
/// # Errors
///
/// Whatever the underlying filesystem operation reports, unwrapped from
/// [`tempfile`]'s persist error.
pub fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    create_dir_all(parent)?;
    // Prefixed with the destination's own name so a temp left behind by a
    // killed process says which file it was replacing.
    let prefix = path.file_name().map_or_else(
        || ".mcpgw.".to_owned(),
        |name| format!(".{}.", name.to_string_lossy()),
    );
    let mut tmp = tempfile::Builder::new()
        .prefix(&prefix)
        .tempfile_in(parent)?;
    harden_file(tmp.path())?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|err| err.error)?;
    sync_dir(parent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_writes_the_bytes_and_creates_the_missing_parents() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("nested/deeper/state.json");
        write_atomically(&path, b"{}\n").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"{}\n");
    }

    #[test]
    fn it_replaces_an_existing_file_whole() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.json");
        write_atomically(&path, b"the long previous contents").unwrap();
        write_atomically(&path, b"short").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"short");
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_owner_only_and_so_is_the_directory_it_created() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("state");
        let path = dir.join("token");
        write_atomically(&path, b"secret\n").unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&dir), 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn a_hardened_destination_stays_hardened_when_the_umask_is_wide_open() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("token");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_atomically(&path, b"new").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the rename must publish the 0600 temp file");
    }

    #[test]
    fn a_failed_write_leaves_neither_a_temp_file_nor_a_damaged_destination() {
        let root = tempfile::tempdir().unwrap();
        // A directory cannot be replaced by a rename, so persist fails after
        // the temp file has already been written and synced.
        let path = root.path().join("occupied");
        std::fs::create_dir(&path).unwrap();
        write_atomically(&path, b"never published").unwrap_err();
        let left: Vec<_> = std::fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(left, vec![std::ffi::OsString::from("occupied")]);
        assert!(path.is_dir());
    }
}
