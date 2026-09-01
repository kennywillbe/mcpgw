use std::collections::BTreeMap;
use std::time::Duration;

use mcpgw_core::probe::{ProbeError, probe_stdio};

fn fixture_server() -> String {
    // Same-package binaries get this env var from cargo at test build time.
    env!("CARGO_BIN_EXE_mcpgw-test-server").to_owned()
}

async fn probe_mode(
    mode: &str,
    timeout_ms: u64,
) -> Result<mcpgw_core::probe::ProbeSuccess, ProbeError> {
    probe_stdio(
        &fixture_server(),
        &[mode.to_owned()],
        &BTreeMap::new(),
        Duration::from_millis(timeout_ms),
    )
    .await
}

#[tokio::test]
async fn healthy_server_reports_identity_and_tools() {
    let success = probe_mode("healthy", 5000).await.unwrap();
    assert_eq!(success.server_name, "mcpgw-test-server");
    assert_eq!(success.server_version, "9.9.9");
    assert_eq!(success.tool_count, 2);
}

#[tokio::test]
async fn unresponsive_server_times_out() {
    let err = probe_mode("slow", 300).await.unwrap_err();
    assert!(matches!(err, ProbeError::Timeout { .. }), "got: {err}");
}

#[tokio::test]
async fn garbage_output_fails_the_handshake() {
    let err = probe_mode("garbage", 3000).await.unwrap_err();
    // Non-JSON output either breaks the handshake outright or starves it
    // into the timeout, depending on how the transport treats bad frames.
    assert!(
        matches!(
            err,
            ProbeError::Handshake { .. } | ProbeError::Timeout { .. }
        ),
        "got: {err}"
    );
}

#[tokio::test]
async fn immediate_exit_fails_the_handshake() {
    let err = probe_mode("exit", 3000).await.unwrap_err();
    assert!(matches!(err, ProbeError::Handshake { .. }), "got: {err}");
}

#[tokio::test]
async fn missing_binary_is_a_spawn_error() {
    let err = probe_stdio(
        "/nonexistent/mcpgw-no-such-binary",
        &[],
        &BTreeMap::new(),
        Duration::from_millis(1000),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, ProbeError::Spawn { .. } | ProbeError::Handshake { .. }),
        "got: {err}"
    );
}
