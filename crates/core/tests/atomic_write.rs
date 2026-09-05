//! One atomic writer, enforced rather than agreed.
//!
//! The temp-harden-fsync-persist-fsync-dir sequence used to be written out by
//! hand once per state file, and the copies drifted: some hardened the temp
//! file before writing it, some hardened the destination after the rename,
//! one hardened neither. They all call `private::write_atomically` now, and
//! this test is what keeps the eighth copy from being written — a reviewer
//! noticing is not a mechanism.

use std::path::{Path, PathBuf};

/// Every `.rs` file in the workspace's two library crates, except the one
/// file that is allowed to know how an atomic write is spelled.
fn sources() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut todo = vec![root.join("core/src"), root.join("cli/src")];
    let mut files = Vec::new();
    while let Some(dir) = todo.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                todo.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs")
                && path.file_name().is_some_and(|name| name != "private.rs")
            {
                files.push(path);
            }
        }
    }
    assert!(files.len() > 20, "the source walk found almost nothing");
    files
}

#[test]
fn nothing_outside_private_rs_hand_rolls_an_atomic_write() {
    let mut offenders = Vec::new();
    for path in sources() {
        let text = std::fs::read_to_string(&path).unwrap();
        for (line, text) in text.lines().enumerate() {
            // `self_update` stages a 0755 binary next to the one it replaces
            // and hands the temp file off with `keep`, which is a different
            // job from publishing an owner-only state file; it is caught by
            // neither needle.
            if text.contains("NamedTempFile") || text.contains(".persist(") {
                offenders.push(format!("{}:{}", path.display(), line + 1));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "these write files by hand instead of calling private::write_atomically:\n{}",
        offenders.join("\n")
    );
}
