//! The record a running gateway publishes about itself.

use std::path::PathBuf;

use mcpgw_core::Error;
use mcpgw_core::runtime::{GatewayRecord, read_record, record_path, remove_record, write_record};
use mcpgw_core::upgrade::{ExeStamp, UpgradeRestart};

fn record(port: u16) -> GatewayRecord {
    GatewayRecord {
        version: "0.4.0".to_owned(),
        pid: 4242,
        exe: PathBuf::from("/usr/local/bin/mcpgw"),
        bind: "127.0.0.1".to_owned(),
        port,
        started_at: 1_700_000_000,
        last_upgrade_restart: None,
    }
}

#[test]
fn a_written_record_reads_back_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");

    write_record(&state, &record(8137)).unwrap();

    assert_eq!(read_record(&state, 8137).unwrap(), Some(record(8137)));
}

#[test]
fn no_record_is_not_an_error() {
    let dir = tempfile::tempdir().unwrap();

    assert_eq!(read_record(dir.path(), 8137).unwrap(), None);
}

/// The whole point of keying by port: a foreground `serve --port 9000` and
/// the installed service on 8137 are two gateways, and each has to be able
/// to say what it is without erasing the other.
#[test]
fn two_gateways_on_two_ports_keep_two_records() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");

    write_record(&state, &record(8137)).unwrap();
    write_record(&state, &record(9000)).unwrap();

    assert_eq!(read_record(&state, 8137).unwrap().unwrap().port, 8137);
    assert_eq!(read_record(&state, 9000).unwrap().unwrap().port, 9000);

    remove_record(&state, 9000);

    assert_eq!(read_record(&state, 9000).unwrap(), None);
    assert!(read_record(&state, 8137).unwrap().is_some());
}

/// Removing what is not there is the state that was asked for, and it runs
/// on the way out of a process that has nobody left to tell.
#[test]
fn removing_a_record_that_is_not_there_is_silent() {
    let dir = tempfile::tempdir().unwrap();

    remove_record(dir.path(), 8137);
}

#[test]
fn a_corrupt_record_names_the_file_to_delete() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(record_path(dir.path(), 8137), "{not json").unwrap();

    let err = read_record(dir.path(), 8137).unwrap_err();

    assert!(
        matches!(&err, Error::RecordParse { path, .. } if path == &record_path(dir.path(), 8137)),
        "{err:?}"
    );
    assert!(
        err.to_string().contains("gateway-8137.json"),
        "{err}: the message has to name the file a user is told to delete"
    );
}

/// A newer gateway writing a field this build has never heard of must not
/// turn an older `status` into a parse error.
#[test]
fn a_field_from_a_newer_gateway_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let mut json = serde_json::to_value(record(8137)).unwrap();
    json.as_object_mut()
        .unwrap()
        .insert("config_path".to_owned(), "/home/u/config.toml".into());
    std::fs::write(
        record_path(dir.path(), 8137),
        serde_json::to_vec(&json).unwrap(),
    )
    .unwrap();

    assert_eq!(read_record(dir.path(), 8137).unwrap(), Some(record(8137)));
}

/// Replacing a record is a rename over the old one, so a reader polling it
/// during a restart never sees half a file.
#[test]
fn a_second_write_replaces_the_first() {
    let dir = tempfile::tempdir().unwrap();
    let mut second = record(8137);
    second.pid = 5;
    second.version = "0.4.1".to_owned();

    write_record(dir.path(), &record(8137)).unwrap();
    write_record(dir.path(), &second).unwrap();

    assert_eq!(read_record(dir.path(), 8137).unwrap(), Some(second));
    // The temp file the rename published must not be left beside it.
    let strays: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name != "gateway-8137.json")
        .collect();
    assert!(strays.is_empty(), "{strays:?}");
}

/// The other direction of the same leniency: a record written before the
/// upgrade watcher existed is what every gateway on disk right now leaves,
/// and reading one must not need a field it never wrote.
#[test]
fn a_record_without_the_upgrade_field_still_reads() {
    let dir = tempfile::tempdir().unwrap();
    let mut json = serde_json::to_value(record(8137)).unwrap();
    json.as_object_mut().unwrap().remove("last_upgrade_restart");
    std::fs::write(
        record_path(dir.path(), 8137),
        serde_json::to_vec(&json).unwrap(),
    )
    .unwrap();

    assert_eq!(read_record(dir.path(), 8137).unwrap(), Some(record(8137)));
}

/// The restart guard only works if it survives the process that wrote it.
#[test]
fn the_restart_a_gateway_recorded_reaches_the_gateway_that_replaces_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut written = record(8137);
    written.last_upgrade_restart = Some(UpgradeRestart {
        stamp: ExeStamp {
            mtime: Some(1_700_000_500),
            len: 9_000_000,
        },
        at: 1_700_000_501,
    });

    write_record(dir.path(), &written).unwrap();

    assert_eq!(read_record(dir.path(), 8137).unwrap(), Some(written));
}
