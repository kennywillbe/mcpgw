//! Everything mcpgw derives from client configs carries their secrets, so
//! the state dir, the backups in it and the state file are owner-only. Unix
//! only: the mode bits have no equivalent elsewhere.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use mcpgw_core::backup::backup_file;
use mcpgw_core::runtime::GatewayRecord;
use mcpgw_core::state::ManagedState;

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn backups_and_the_dirs_holding_them_are_owner_only() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let live = dir.path().join("mcp.json");
    std::fs::write(
        &live,
        r#"{"mcpServers":{"x":{"headers":{"Authorization":"Bearer t0ken"}}}}"#,
    )
    .unwrap();
    // A world-readable client config is the normal case, and `fs::copy`
    // carries the source mode into the backup.
    std::fs::set_permissions(&live, std::fs::Permissions::from_mode(0o644)).unwrap();

    let backup = backup_file(&state, "cursor", &live).unwrap();

    assert_eq!(mode(&backup), 0o600, "{:o}", mode(&backup));
    for dir in [
        state.as_path(),
        &state.join("backups"),
        &state.join("backups").join("cursor"),
    ] {
        assert_eq!(mode(dir), 0o700, "{} is {:o}", dir.display(), mode(dir));
    }
}

#[test]
fn the_state_file_is_owner_only_under_an_owner_only_dir() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let path = state.join("managed.json");

    let mut managed = ManagedState::default();
    managed
        .clients
        .entry("cursor".to_owned())
        .or_default()
        .insert("github".to_owned());
    managed.save(&path).unwrap();

    assert_eq!(mode(&path), 0o600, "{:o}", mode(&path));
    assert_eq!(mode(&state), 0o700, "{:o}", mode(&state));
    assert_eq!(ManagedState::load(&path).unwrap(), managed);
}

/// The gateway's runtime record names the binary and the path it runs from,
/// so it gets the same treatment as everything else under the state dir —
/// including when the gateway is the first thing to create that directory.
#[test]
fn the_gateway_record_is_owner_only_under_an_owner_only_dir() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let record = GatewayRecord {
        version: "0.4.0".to_owned(),
        pid: 4242,
        exe: std::path::PathBuf::from("/usr/local/bin/mcpgw"),
        bind: "127.0.0.1".to_owned(),
        port: 8137,
        started_at: 1_700_000_000,
        last_upgrade_restart: None,
    };

    mcpgw_core::runtime::write_record(&state, &record).unwrap();

    let path = mcpgw_core::runtime::record_path(&state, 8137);
    assert_eq!(mode(&path), 0o600, "{:o}", mode(&path));
    assert_eq!(mode(&state), 0o700, "{:o}", mode(&state));
}
