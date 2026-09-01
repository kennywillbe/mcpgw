//! Helpers shared by the CLI's end-to-end tests.

use std::path::PathBuf;

/// The scripted fixture server lives in a sibling package, so `CARGO_BIN_EXE`
/// cannot name it here; it sits next to this test executable's parent
/// (`target/<profile>/`), which holds for every cargo layout the suite runs
/// under and CI always builds the whole workspace.
pub fn fixture_binary() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap();
    let path = dir.join(format!("mcpgw-test-server{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.exists(),
        "fixture binary missing at {} — build the workspace first",
        path.display()
    );
    path
}
