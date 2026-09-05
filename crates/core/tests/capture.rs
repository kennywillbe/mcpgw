use std::time::Duration;

use mcpgw_core::capture::{
    Bodies, CapturePolicy, CaptureRecord, CaptureWriter, Kind, MAX_BODY_BYTES, RedactionRules,
    TRUNCATION_MARKER, daily_path, session_fingerprint, truncate,
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

/// The same for `client`, which arrived later still: an old line names no
/// client and comes back out of the parser byte for byte, so upgrading
/// neither invents an attribution nor rewrites a file somebody is tailing.
#[test]
fn a_line_written_before_client_attribution_still_parses() {
    let record: CaptureRecord = serde_json::from_str(PRE_N13_LINE).unwrap();
    assert_eq!(record.client, None);
    assert_eq!(serde_json::to_string(&record).unwrap(), PRE_N13_LINE);
}

#[test]
fn a_client_survives_the_round_trip() {
    let mut record = record();
    record.client = Some("claude-code/2.1.3".to_owned());
    let line = serde_json::to_string(&record).unwrap();
    assert!(line.contains(r#""client":"claude-code/2.1.3""#), "{line}");
    assert_eq!(
        serde_json::from_str::<CaptureRecord>(&line).unwrap(),
        record
    );
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
    // Under `full`, because what this test is about is appending and
    // rotation: a policy that rewrote the bodies would only obscure that.
    let writer = CaptureWriter::new(&traffic).with_policy(CapturePolicy::full());
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

/// A gateway writing under the default policy: the argument that carried a
/// credential is on disk with the credential gone and everything a reader
/// filters on still there.
#[test]
fn the_default_writer_redacts_before_anything_reaches_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let writer = CaptureWriter::under_state_dir(dir.path());
    let record = CaptureRecord::new(
        writer.session(),
        "github",
        Kind::Call,
        Duration::from_millis(9),
    )
    .with_tool("create_issue")
    .with_args(r#"{"title":"bug","api_key":"ghp_0123456789abcdefghij"}"#.to_owned())
    .with_error("GET https://api.example.com/mcp?access_token=s3cr3t3 failed");
    writer.append(&record).unwrap();

    let text = std::fs::read_to_string(daily_path(writer.dir(), record.ts)).unwrap();
    assert!(!text.contains("ghp_0123456789abcdefghij"), "{text}");
    assert!(!text.contains("s3cr3t3"), "{text}");
    assert!(text.contains("create_issue"), "{text}");
    assert!(text.contains("bug"), "{text}");

    let stored: CaptureRecord = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(stored.bodies, Bodies::Redacted);
    assert!(stored.args.as_deref().unwrap().contains("[redacted]"));
}

/// The whole reason redaction lives in the writer: a secret sitting past the
/// cap must not survive because the body was cut before anyone looked at it.
#[test]
fn a_secret_past_the_cap_is_still_redacted() {
    let dir = tempfile::tempdir().unwrap();
    let writer = CaptureWriter::under_state_dir(dir.path());
    let filler = "x".repeat(MAX_BODY_BYTES);
    let args = serde_json::json!({"filler": filler, "token": "ghp_0123456789abcdefghij"});
    let record = CaptureRecord::new(writer.session(), "fx", Kind::Call, Duration::ZERO)
        .with_args(mcpgw_core::capture::body(&args));
    writer.append(&record).unwrap();

    let text = std::fs::read_to_string(daily_path(writer.dir(), record.ts)).unwrap();
    assert!(!text.contains("ghp_0123456789abcdefghij"), "{text}");
    // And it is still truncated: redaction does not disable the cap.
    assert!(text.contains(TRUNCATION_MARKER), "{text}");
}

#[test]
fn an_off_writer_keeps_the_metadata_and_no_bodies() {
    let dir = tempfile::tempdir().unwrap();
    let writer = CaptureWriter::under_state_dir(dir.path())
        .with_policy(CapturePolicy::new(Bodies::Off, RedactionRules::builtin()));
    let record = CaptureRecord::new(writer.session(), "fx", Kind::Call, Duration::from_millis(4))
        .with_tool("echo")
        .with_args(r#"{"message":"anything at all"}"#.to_owned());
    writer.append(&record).unwrap();

    let text = std::fs::read_to_string(daily_path(writer.dir(), record.ts)).unwrap();
    assert!(!text.contains("anything at all"), "{text}");
    let stored: CaptureRecord = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(stored.bodies, Bodies::Off);
    assert_eq!(stored.args, None);
    assert_eq!(stored.tool.as_deref(), Some("echo"));
    assert_eq!(stored.duration_ms, 4);
}

#[test]
fn a_full_writer_is_byte_for_byte_what_it_always_was() {
    let dir = tempfile::tempdir().unwrap();
    let writer = CaptureWriter::under_state_dir(dir.path()).with_policy(CapturePolicy::full());
    let record = CaptureRecord::new(writer.session(), "fx", Kind::Call, Duration::ZERO)
        .with_args(r#"{"token":"ghp_0123456789abcdefghij"}"#.to_owned());
    writer.append(&record).unwrap();

    let text = std::fs::read_to_string(daily_path(writer.dir(), record.ts)).unwrap();
    assert!(text.contains("ghp_0123456789abcdefghij"), "{text}");
    // The mode is the default one for a parsed line, so it stays out of it.
    assert!(!text.contains("\"bodies\""), "{text}");
}

/// The field is additive in both directions: a line from an older gateway
/// reads as `full`, which is exactly what it was.
#[test]
fn a_line_without_the_bodies_field_reads_as_full() {
    let record: CaptureRecord = serde_json::from_str(PRE_N13_LINE).unwrap();
    assert_eq!(record.bodies, Bodies::Full);
    assert!(record.bodies.is_full());
}

/// The credential half of the OAuth broker meets the capture log here: the
/// token file's own shape, and the header the gateway builds from it, both
/// come out of the writer with nothing usable in them.
///
/// Pinned as its own test rather than left to the general rules because these
/// are the two exact spellings a stored login can take, and a rule that
/// stopped covering either would be a bearer token in a file the user is
/// invited to read and paste into a bug report.
#[test]
fn a_stored_oauth_login_never_reaches_the_capture_log() {
    let dir = tempfile::tempdir().unwrap();
    let writer = CaptureWriter::under_state_dir(dir.path());
    let record = CaptureRecord::new(writer.session(), "linear", Kind::Call, Duration::ZERO)
        .with_args(r#"{"headers":{"Authorization":"Bearer lin_oat_9f2e1c5d7b3a0e64"}}"#.to_owned())
        .with_response(
            r#"{"access_token":"lin_oat_9f2e1c5d7b3a0e64","refresh_token":"lin_ort_0badc0ffee"}"#
                .to_owned(),
        );
    writer.append(&record).unwrap();

    let text = std::fs::read_to_string(daily_path(writer.dir(), record.ts)).unwrap();
    assert!(!text.contains("lin_oat_9f2e1c5d7b3a0e64"), "{text}");
    assert!(!text.contains("lin_ort_0badc0ffee"), "{text}");
    assert!(text.contains("[redacted]"), "{text}");
}

/// Retention: the days outside the window go, the days inside it stay, and
/// anything that is not one of mcpgw's daily files is left alone.
const DAY: u64 = 86_400_000;

fn seed(dir: &std::path::Path, names: &[&str]) {
    std::fs::create_dir_all(dir).unwrap();
    for name in names {
        std::fs::write(dir.join(name), "{}\n").unwrap();
    }
}

fn names(dir: &std::path::Path) -> Vec<String> {
    let mut found: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    found.sort();
    found
}

#[test]
fn prune_keeps_the_window_and_deletes_what_fell_out_of_it() {
    let dir = tempfile::tempdir().unwrap();
    let traffic = dir.path().join("traffic");
    // 2026-01-15 as the "now" every name below is relative to.
    let now = 1_768_435_200_000;
    let day_of = |back: u64| mcpgw_core::capture::daily_name(now - back * DAY);
    seed(
        &traffic,
        &[&day_of(0), &day_of(2), &day_of(6), &day_of(7), &day_of(30)],
    );

    // retain_days counts today, so a window of 7 keeps today back to day 6.
    let removed = mcpgw_core::capture::prune(&traffic, 7, now).unwrap();
    assert_eq!(
        removed,
        vec![traffic.join(day_of(30)), traffic.join(day_of(7))],
    );
    assert_eq!(names(&traffic), vec![day_of(6), day_of(2), day_of(0)]);
}

#[test]
fn prune_never_touches_a_file_it_did_not_write() {
    let dir = tempfile::tempdir().unwrap();
    let traffic = dir.path().join("traffic");
    let now = 1_768_435_200_000;
    seed(
        &traffic,
        &[
            "2020-01-01.jsonl.gz",
            "2020-01-02.json",
            "notes.md",
            "2020-01-03.jsonl",
            "old.jsonl",
        ],
    );

    let removed = mcpgw_core::capture::prune(&traffic, 7, now).unwrap();
    assert_eq!(removed, vec![traffic.join("2020-01-03.jsonl")]);
    assert_eq!(
        names(&traffic),
        vec![
            "2020-01-01.jsonl.gz",
            "2020-01-02.json",
            "notes.md",
            "old.jsonl",
        ],
    );
}

#[test]
fn a_retention_of_zero_keeps_everything() {
    let dir = tempfile::tempdir().unwrap();
    let traffic = dir.path().join("traffic");
    seed(&traffic, &["2001-01-01.jsonl"]);

    assert!(
        mcpgw_core::capture::prune(&traffic, 0, 1_768_435_200_000)
            .unwrap()
            .is_empty()
    );
    assert_eq!(names(&traffic), vec!["2001-01-01.jsonl"]);
}

#[test]
fn a_traffic_directory_that_does_not_exist_yet_prunes_to_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let removed =
        mcpgw_core::capture::prune(&dir.path().join("traffic"), 14, 1_768_435_200_000).unwrap();
    assert!(removed.is_empty());
}

/// A writer nobody configured retains a finite number of days, and the
/// append that opens a new day is what drops the ones that aged out.
#[test]
fn appending_on_a_new_day_prunes_the_days_that_aged_out() {
    let dir = tempfile::tempdir().unwrap();
    let writer = CaptureWriter::under_state_dir(dir.path());
    assert_eq!(
        writer.retain_days(),
        mcpgw_core::capture::DEFAULT_RETAIN_DAYS
    );

    let now = 1_768_435_200_000;
    let day_of = |back: u64| mcpgw_core::capture::daily_name(now - back * DAY);
    seed(writer.dir(), &[&day_of(20), &day_of(3)]);

    let mut record = CaptureRecord::new(writer.session(), "fx", Kind::List, Duration::ZERO);
    record.ts = now;
    writer.append(&record).unwrap();

    assert_eq!(names(writer.dir()), vec![day_of(3), day_of(0)]);
}

#[test]
fn a_writer_told_to_keep_one_day_keeps_only_today() {
    let dir = tempfile::tempdir().unwrap();
    let writer = CaptureWriter::under_state_dir(dir.path()).with_retain_days(1);
    let now = 1_768_435_200_000;
    let day_of = |back: u64| mcpgw_core::capture::daily_name(now - back * DAY);
    seed(writer.dir(), &[&day_of(1)]);

    let mut record = CaptureRecord::new(writer.session(), "fx", Kind::List, Duration::ZERO);
    record.ts = now;
    writer.append(&record).unwrap();

    assert_eq!(names(writer.dir()), vec![day_of(0)]);
}

#[test]
fn usage_measures_the_daily_files_and_ignores_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let traffic = dir.path().join("traffic");
    std::fs::create_dir_all(&traffic).unwrap();
    std::fs::write(traffic.join("2026-01-01.jsonl"), "aaaa").unwrap();
    std::fs::write(traffic.join("2026-02-01.jsonl"), "bb").unwrap();
    std::fs::write(traffic.join("notes.md"), "ignored entirely").unwrap();

    let usage = mcpgw_core::capture::usage(&traffic).unwrap();
    assert_eq!(usage.files, 2);
    assert_eq!(usage.bytes, 6);
    assert_eq!(usage.oldest.as_deref(), Some("2026-01-01"));

    let empty = mcpgw_core::capture::usage(&dir.path().join("nothing")).unwrap();
    assert_eq!(empty, mcpgw_core::capture::Usage::default());
}
