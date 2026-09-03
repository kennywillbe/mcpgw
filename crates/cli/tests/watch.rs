//! End-to-end coverage for the shape of `mcpgw watch` that a script can hit
//! by accident: `--tui` with nowhere to draw.

use std::path::Path;
use std::process::Output;

use assert_cmd::Command;

fn run_watch(home: &Path, args: &[&str]) -> Output {
    Command::cargo_bin("mcpgw")
        .unwrap()
        // Hermetic: no test may phone home for a version notice.
        .env("MCPGW_NO_UPDATE_CHECK", "1")
        .arg("watch")
        .args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_DATA_HOME", home.join("data"))
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .unwrap()
}

#[test]
fn the_tui_refuses_a_pipe_and_names_the_stream_instead() {
    // `output()` gives the child a pipe, which is exactly the situation the
    // refusal exists for: a TUI written to a pipe is a screenful of escape
    // sequences, and whoever piped it wanted the line stream.
    let home = tempfile::tempdir().unwrap();
    let output = run_watch(home.path(), &["--tui"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("needs a terminal"), "{stderr}");
    assert!(stderr.contains("mcpgw watch --json"), "{stderr}");
    assert!(output.stdout.is_empty(), "{:?}", output.stdout);
}

#[test]
fn the_tui_and_the_json_stream_are_not_the_same_run() {
    let home = tempfile::tempdir().unwrap();
    let output = run_watch(home.path(), &["--tui", "--json"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cannot be used with"), "{stderr}");
}
