use std::time::Duration;

use mcpgw_core::capture::{
    CaptureRecord, CaptureWriter, Kind, MAX_BODY_BYTES, TRUNCATION_MARKER, daily_path,
    session_fingerprint, truncate,
};

fn record() -> CaptureRecord {
    let mut record = CaptureRecord::new("s3ss10n", "github", Kind::Call, Duration::from_millis(42))
        .with_tool("create_issue")
        .with_args(r#"{"title":"bug"}"#.to_owned())
        .with_response(r#"{"content":[{"type":"text","text":"ok"}]}"#.to_owned());
    // Pinned so the snapshot does not move with the clock.
    record.ts = 1_767_225_600_123;
    record
}

#[test]
fn record_json_shape_is_stable() {
    insta::assert_snapshot!(serde_json::to_string(&record()).unwrap());
}

#[test]
fn failed_record_carries_the_error_and_drops_ok() {
    let mut record = record().with_error("upstream \"github\" failed");
    record.ts = 1_767_225_600_123;
    assert!(!record.ok);
    insta::assert_snapshot!(serde_json::to_string(&record).unwrap());
}

#[test]
fn records_round_trip_through_json() {
    let line = serde_json::to_string(&record()).unwrap();
    assert_eq!(
        serde_json::from_str::<CaptureRecord>(&line).unwrap(),
        record()
    );
}

/// A line exactly as the gateway wrote it before the `endpoint` field
/// existed, pasted verbatim rather than generated: the point of the test is
/// that a file on someone's disk today keeps parsing after they upgrade, and
/// a fixture built from the current struct could never prove that.
const PRE_N13_LINE: &str = r#"{"ts":1767225600123,"session":"1a2b3c4d","server":"github","tool":"create_issue","kind":"call","duration_ms":42,"ok":true,"args":"{\"title\":\"bug\"}","response":"{\"content\":[]}"}"#;

#[test]
fn a_line_written_before_endpoints_still_parses() {
    let record: CaptureRecord = serde_json::from_str(PRE_N13_LINE).unwrap();
    assert_eq!(record.session, "1a2b3c4d");
    assert_eq!(record.server, "github");
    assert_eq!(record.tool.as_deref(), Some("create_issue"));
    assert_eq!(record.kind, Kind::Call);
    // Absent, not empty: nothing about an old line says which face took it.
    assert_eq!(record.endpoint, None);
    // And it round-trips back to the same bytes, so re-emitting a parsed old
    // line (which `watch --json` does) neither invents nor drops a field.
    assert_eq!(serde_json::to_string(&record).unwrap(), PRE_N13_LINE);
}

#[test]
fn an_endpoint_survives_the_round_trip() {
    let record = record().with_endpoint("s/github");
    let line = serde_json::to_string(&record).unwrap();
    assert!(line.contains(r#""endpoint":"s/github""#), "{line}");
    assert_eq!(
        serde_json::from_str::<CaptureRecord>(&line).unwrap(),
        record
    );
}

#[test]
fn a_session_fingerprint_is_stable_and_hides_its_input() {
    let raw = "9a3f1c88-4d0e-4a6b-9f2e-1c5d7b3a0e64";
    let digest = session_fingerprint(raw);
    assert_eq!(digest.len(), 8);
    assert!(digest.chars().all(|c| c.is_ascii_hexdigit()), "{digest}");
    // Same session, same id — that is the whole of attribution.
    assert_eq!(digest, session_fingerprint(raw));
    // Different sessions do not collide, and the credential never appears.
    assert_ne!(
        digest,
        session_fingerprint("9a3f1c88-4d0e-4a6b-9f2e-1c5d7b3a0e65")
    );
    assert!(!raw.contains(&digest));
}

#[test]
fn short_bodies_are_left_alone() {
    assert_eq!(truncate("hello"), "hello");
    let exact = "x".repeat(MAX_BODY_BYTES);
    assert_eq!(truncate(&exact), exact);
}

#[test]
fn truncation_never_splits_a_codepoint() {
    // 3-byte characters straddle the cut: 2048 is not a multiple of 3, so a
    // naive byte slice at MAX_BODY_BYTES would land inside one.
    let text = "☃".repeat(MAX_BODY_BYTES);
    let cut = truncate(&text);
    assert!(cut.ends_with(TRUNCATION_MARKER), "{cut}");
    let kept = cut.strip_suffix(TRUNCATION_MARKER).unwrap();
    assert!(kept.len() <= MAX_BODY_BYTES);
    assert!(kept.len() > MAX_BODY_BYTES - 3);
    assert!(kept.chars().all(|c| c == '☃'));
}

#[test]
fn daily_files_are_named_by_utc_day() {
    let dir = std::path::Path::new("/traffic");
    // 2026-01-01T00:00:00Z and one millisecond before it.
    assert_eq!(
        daily_path(dir, 1_767_225_600_000),
        dir.join("2026-01-01.jsonl")
    );
    assert_eq!(
        daily_path(dir, 1_767_225_599_999),
        dir.join("2025-12-31.jsonl")
    );
    assert_eq!(daily_path(dir, 0), dir.join("1970-01-01.jsonl"));
    // A leap day, to prove the era arithmetic is not an approximation.
    assert_eq!(
        daily_path(dir, 1_709_164_800_000),
        dir.join("2024-02-29.jsonl")
    );
}

#[test]
fn writer_appends_jsonl_to_a_dated_file_it_creates() {
    let dir = tempfile::tempdir().unwrap();
    let traffic = dir.path().join("traffic");
    let writer = CaptureWriter::new(&traffic);
    assert!(!writer.session().is_empty());

    let first = record();
    let second = record().with_tool("list_issues");
    writer.append(&first).unwrap();
    writer.append(&second).unwrap();

    let path = daily_path(&traffic, first.ts);
    let text = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    let parsed: Vec<CaptureRecord> = lines
        .iter()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(parsed, vec![first, second]);
}

#[test]
fn writer_uses_the_session_of_the_gateway_run() {
    let dir = tempfile::tempdir().unwrap();
    let writer = CaptureWriter::under_state_dir(dir.path());
    assert_eq!(writer.dir(), dir.path().join("traffic"));

    let record = CaptureRecord::new(writer.session(), "fx", Kind::List, Duration::ZERO);
    writer.append(&record).unwrap();
    let text = std::fs::read_to_string(daily_path(writer.dir(), record.ts)).unwrap();
    assert!(text.contains(writer.session()), "{text}");
    // tools/list names no tool, so the field stays out of the line entirely.
    assert!(!text.contains("\"tool\""), "{text}");
}

#[cfg(unix)]
#[test]
fn traffic_files_are_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let writer = CaptureWriter::under_state_dir(dir.path());
    let record = CaptureRecord::new(writer.session(), "fx", Kind::List, Duration::ZERO);
    writer.append(&record).unwrap();

    let path = daily_path(writer.dir(), record.ts);
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "{mode:o}");
}
