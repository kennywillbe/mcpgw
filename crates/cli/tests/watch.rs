//! End-to-end coverage for the shape of `mcpgw watch` that a script can hit
//! by accident: `--tui` with nowhere to draw.

use std::path::Path;
use std::process::Output;

use assert_cmd::Command;

mod util;

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

/// `--client` on the real binary, against a traffic file on disk: the filter
/// is what a user reaches for when two harnesses share a gateway, and the
/// field has to survive into `--json` for the `jq` half of the same job.
///
/// Spawned rather than run to completion, because `watch` is a follow: it
/// never exits on its own, so the test reads what it wants and ends it.
#[test]
fn the_client_filter_narrows_the_json_stream() {
    let state = tempfile::tempdir().unwrap();
    let traffic = state.path().join("traffic");
    std::fs::create_dir_all(&traffic).unwrap();
    // Today's file by the same rule the gateway names it, so `watch` follows
    // the one it is told to follow rather than one this test invented.
    let now = mcpgw_core::capture::now_millis();
    std::fs::write(
        mcpgw_core::capture::daily_path(&traffic, now),
        format!(
            "{}\n{}\n{}\n",
            line(now, Some("claude-code/2.1.3")),
            line(now, Some("cursor/0.48")),
            line(now, None),
        ),
    )
    .unwrap();

    let mut child = util::SpawnedBlocking::new(
        std::process::Command::new(assert_cmd::cargo::cargo_bin("mcpgw"))
            .args(["watch", "--json", "--client", "CURSOR"])
            .env("MCPGW_NO_UPDATE_CHECK", "1")
            .env("MCPGW_STATE_DIR", state.path())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    );

    let stdout = child.stdout.take().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufRead::lines(std::io::BufReader::new(stdout)) {
            if sender.send(line.unwrap()).is_err() {
                return;
            }
        }
    });

    let first = receiver
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("watch printed nothing");
    let record: serde_json::Value = serde_json::from_str(&first).unwrap();
    // Matched on a case-insensitive substring, and the field the filter is
    // about is on the line the filter let through.
    assert_eq!(record["client"], "cursor/0.48", "{first}");
    // The other two lines were in the same file and are not this client:
    // one is a different harness, one nobody attributed.
    let extra = receiver.recv_timeout(std::time::Duration::from_secs(2));
    assert!(extra.is_err(), "{extra:?}");
}

/// One captured line as a gateway writes it, optionally attributed.
fn line(ts: u64, client: Option<&str>) -> String {
    let client = client.map_or(String::new(), |client| format!(r#""client":"{client}","#));
    format!(r#"{{"ts":{ts},"session":"s3ss",{client}"endpoint":"s/github","server":"github","#)
        + r#""tool":"create_issue","kind":"call","duration_ms":87,"ok":true}"#
}
