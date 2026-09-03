//! End-to-end coverage for `self-update` and the version notice, against a
//! canned release served from loopback. The real release host is never
//! touched: `MCPGW_UPDATE_BASE_URL` moves both the API and the download URLs
//! onto the tiny server below, which speaks just enough HTTP/1.1 to answer
//! the three GETs the binary makes.
//!
//! That seam only exists in debug builds, and the binary under test shares
//! this target's profile — so under `cargo test --release` the suite stands
//! down rather than letting the tests reach github.com for real.
#![cfg(debug_assertions)]

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Output;

use assert_cmd::Command;

mod util;

/// The triple this test binary — and therefore the mcpgw under test — was
/// built for, from the same build script the command reads it from.
// Only the unix-gated end-to-end tests name the triple; Windows builds the
// file without them and -D warnings rejects the dead const.
#[cfg(unix)]
const TARGET: &str = env!("MCPGW_TARGET");

#[cfg(unix)]
const SHIPPED_TARGETS: [&str; 4] = [
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "x86_64-pc-windows-msvc",
];

const NEWER: &str = "9.9.9";

fn current() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// A loopback release host. It stays up for the rest of the test binary's
/// life, which is shorter than any test that could still be talking to it.
struct ReleaseHost {
    base: String,
}

impl ReleaseHost {
    fn start(routes: HashMap<String, Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(&stream);
                let mut request = String::new();
                if reader.read_line(&mut request).is_err() {
                    continue;
                }
                // Headers are read to the blank line and thrown away: the
                // requests under test carry nothing this server acts on.
                let mut header = String::new();
                while reader.read_line(&mut header).is_ok_and(|n| n > 0) {
                    if header.trim().is_empty() {
                        break;
                    }
                    header.clear();
                }
                let path = request.split_whitespace().nth(1).unwrap_or("").to_owned();
                let mut stream = &stream;
                match routes.get(&path) {
                    Some(body) => {
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(head.as_bytes());
                        let _ = stream.write_all(body);
                    }
                    None => {
                        let _ = stream.write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                    }
                }
                let _ = stream.flush();
            }
        });
        Self { base }
    }

    /// A host publishing `version` as its latest release, with no assets.
    /// Enough for `--check` and for the notice, neither of which downloads.
    fn with_latest(version: &str) -> Self {
        Self::start(HashMap::from([(
            "/releases/latest".to_owned(),
            format!(r#"{{"tag_name": "v{version}"}}"#).into_bytes(),
        )]))
    }
}

#[cfg(unix)]
fn asset_name(version: &str) -> String {
    let extension = if TARGET.contains("windows") {
        "zip"
    } else {
        "tar.gz"
    };
    format!("mcpgw-{version}-{TARGET}.{extension}")
}

#[cfg(unix)]
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    use std::fmt::Write as _;
    sha2::Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Packages `binary` exactly the way release.yml does.
#[cfg(unix)]
fn release_archive(version: &str, binary: &[u8]) -> Vec<u8> {
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    let mut header = tar::Header::new_gnu();
    header.set_size(binary.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    builder
        .append_data(
            &mut header,
            format!("mcpgw-{version}-{TARGET}/mcpgw"),
            binary,
        )
        .unwrap();
    builder.into_inner().unwrap().finish().unwrap()
}

/// Runs the mcpgw at `exe` with the release host and state directory pinned
/// into the sandbox.
///
/// Several tests copy the binary into their tempdir and exec the copy, so
/// the spawn can lose the ETXTBSY race a sibling test's fork sets up —
/// waited out by [`util::retrying_while_busy`], which is where the whole
/// suite's account of that race now lives.
fn run(exe: &Path, base: &str, state: &Path, args: &[&str]) -> Output {
    util::retrying_while_busy(exe, || {
        Command::new(exe)
            .args(args)
            .env("MCPGW_UPDATE_BASE_URL", base)
            .env("MCPGW_STATE_DIR", state)
            .env_remove("MCPGW_NO_UPDATE_CHECK")
            .output()
    })
}

fn built_binary() -> PathBuf {
    assert_cmd::cargo::cargo_bin("mcpgw")
}

/// What the stand-in release binary prints, whatever it is asked.
#[cfg(unix)]
const STAND_IN_OUTPUT: &str = "the downloaded binary";

/// A few hundred kilobytes of real machine code to ship inside the fake
/// release, compiled here rather than taken from `target/`.
///
/// The payload used to be the binary under test, which read well — the
/// replacement was byte-identical to what it replaced — until the Linux
/// debug build grew past the 64 MiB cap `release::fetch` puts on a download
/// and every branch in flight started failing with "the response body is
/// larger than request limit". That cap is a real defence and a real
/// release archive is a stripped release build an order of magnitude under
/// it, so the fixture moves rather than the limit. Nothing in the path
/// under test reads the payload's contents; what matters is that the file
/// that lands is executable, and a hello-world proves that as well as a
/// 250 MB debug build while staying the same size forever.
#[cfg(unix)]
fn stand_in_binary(dir: &Path) -> Vec<u8> {
    let source = dir.join("stand-in.rs");
    std::fs::write(
        &source,
        format!("fn main() {{ println!(\"{STAND_IN_OUTPUT}\"); }}"),
    )
    .unwrap();

    // Whatever compiled this test is what compiles the stand-in: cargo sets
    // RUSTC for the session, and a session that got this far has one.
    let out = dir.join("stand-in");
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
    let result = std::process::Command::new(rustc)
        .args(["-O", "-C", "strip=symbols", "-o"])
        .arg(&out)
        .arg(&source)
        .output()
        .expect("rustc");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    std::fs::read(&out).unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).unwrap()
}

#[test]
fn check_reports_a_newer_release_with_its_own_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let host = ReleaseHost::with_latest(NEWER);
    let out = run(
        &built_binary(),
        &host.base,
        dir.path(),
        &["self-update", "--check"],
    );
    assert_eq!(out.status.code(), Some(10), "{}", stderr(&out));
    assert_eq!(
        stdout(&out).trim(),
        format!("mcpgw {NEWER} is available (you have {})", current())
    );
}

#[test]
fn check_is_silent_and_zero_when_this_is_the_latest() {
    let dir = tempfile::tempdir().unwrap();
    let host = ReleaseHost::with_latest(current());
    let out = run(
        &built_binary(),
        &host.base,
        dir.path(),
        &["self-update", "--check"],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        stdout(&out).trim(),
        format!("mcpgw {} is the latest release", current())
    );
}

#[test]
fn an_unreachable_release_host_fails_the_command() {
    let dir = tempfile::tempdir().unwrap();
    // Port 1 on loopback: nothing listens there, and the refusal is
    // immediate rather than a timeout.
    let out = run(
        &built_binary(),
        "http://127.0.0.1:1",
        dir.path(),
        &["self-update", "--check"],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("cannot reach"), "{}", stderr(&out));
}

#[test]
fn a_cargo_install_is_sent_back_to_cargo() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join(".cargo/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let exe = bin.join(format!("mcpgw{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(built_binary(), &exe).unwrap();
    // No release host: the install method has to be decided before any
    // network use, so an unroutable base URL must not change the answer.
    let out = run(&exe, "http://127.0.0.1:1", dir.path(), &["self-update"]);
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert_eq!(
        stderr(&out).trim(),
        "mcpgw was installed via cargo — run: cargo install mcpgw"
    );
}

#[test]
fn a_homebrew_install_is_sent_back_to_brew() {
    let dir = tempfile::tempdir().unwrap();
    // A Cellar layout, not a directory called `homebrew`: the prefix
    // markers are anchored to absolute paths a tempdir cannot reproduce,
    // and only the Cellar segment travels.
    let bin = dir.path().join("Cellar/mcpgw/0.1.0/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let exe = bin.join(format!("mcpgw{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(built_binary(), &exe).unwrap();
    let out = run(&exe, "http://127.0.0.1:1", dir.path(), &["self-update"]);
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert_eq!(
        stderr(&out).trim(),
        "mcpgw was installed via Homebrew — run: brew upgrade mcpgw"
    );
}

#[test]
fn a_standalone_install_on_the_latest_release_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let host = ReleaseHost::with_latest(current());
    let out = run(&built_binary(), &host.base, dir.path(), &["self-update"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        stdout(&out).trim(),
        format!("already up to date ({})", current())
    );
}

/// The whole download-verify-replace path, against a "release" carrying a
/// real executable: the file left behind is run afterwards, and what it
/// prints is something only the downloaded bytes could print. Unix only —
/// replacing a running image on Windows is a different dance and CI is the
/// wrong place to discover its edge cases.
#[cfg(unix)]
#[test]
fn a_verified_archive_replaces_the_running_binary() {
    if !SHIPPED_TARGETS.contains(&TARGET) {
        eprintln!("skipped: no release archive is built for {TARGET}");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let exe = bin.join("mcpgw");
    std::fs::copy(built_binary(), &exe).unwrap();

    let payload = stand_in_binary(dir.path());
    let archive = release_archive(NEWER, &payload);
    let asset = asset_name(NEWER);
    let sums = format!(
        "{}  {asset}\n{}  mcpgw-installer.sh\n",
        sha256_hex(&archive),
        sha256_hex(b"unrelated"),
    );
    let host = ReleaseHost::start(HashMap::from([
        (
            "/releases/latest".to_owned(),
            format!(r#"{{"tag_name": "v{NEWER}"}}"#).into_bytes(),
        ),
        (format!("/releases/download/v{NEWER}/{asset}"), archive),
        (
            format!("/releases/download/v{NEWER}/SHA256SUMS"),
            sums.into_bytes(),
        ),
    ]));

    let out = run(&exe, &host.base, dir.path(), &["self-update"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        stdout(&out).contains(&format!("updated mcpgw {} -> {NEWER}", current())),
        "{}",
        stdout(&out)
    );

    // The replaced file is an executable that runs, and it is the one that
    // came down the wire — an assertion the old payload could not make,
    // being a copy of the binary that was already sitting there.
    let version = util::retrying_while_busy(&exe, || Command::new(&exe).arg("--version").output());
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        STAND_IN_OUTPUT
    );
    // Nothing was left staged beside it.
    let leftovers: Vec<_> = std::fs::read_dir(&bin)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name != "mcpgw")
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[cfg(unix)]
#[test]
fn a_tampered_archive_is_refused() {
    if !SHIPPED_TARGETS.contains(&TARGET) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let exe = bin.join("mcpgw");
    std::fs::copy(built_binary(), &exe).unwrap();
    let before = std::fs::read(&exe).unwrap();

    let asset = asset_name(NEWER);
    let archive = release_archive(NEWER, b"a binary that is not what was signed for");
    // The sums file describes different bytes than the ones served.
    let sums = format!("{}  {asset}\n", sha256_hex(b"the honest archive"));
    let host = ReleaseHost::start(HashMap::from([
        (
            "/releases/latest".to_owned(),
            format!(r#"{{"tag_name": "v{NEWER}"}}"#).into_bytes(),
        ),
        (format!("/releases/download/v{NEWER}/{asset}"), archive),
        (
            format!("/releases/download/v{NEWER}/SHA256SUMS"),
            sums.into_bytes(),
        ),
    ]));

    let out = run(&exe, &host.base, dir.path(), &["self-update"]);
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert!(
        stderr(&out).contains("checksum mismatch"),
        "{}",
        stderr(&out)
    );
    assert_eq!(std::fs::read(&exe).unwrap(), before);
}

#[test]
fn the_notice_goes_to_stderr_and_leaves_stdout_machine_readable() {
    let dir = tempfile::tempdir().unwrap();
    let host = ReleaseHost::with_latest(NEWER);
    let config = dir.path().join("config.toml");
    let out = Command::new(built_binary())
        .args(["list", "--json"])
        .env("MCPGW_CONFIG", &config)
        .env("MCPGW_UPDATE_BASE_URL", &host.base)
        .env("MCPGW_STATE_DIR", dir.path())
        .env_remove("MCPGW_NO_UPDATE_CHECK")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stderr(&out).trim(),
        format!(
            "mcpgw {NEWER} is available (you have {}) — run `mcpgw self-update`",
            current()
        )
    );
    serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap();

    // Throttled: the stamp the first run left says today's check happened.
    let again = Command::new(built_binary())
        .args(["list", "--json"])
        .env("MCPGW_CONFIG", &config)
        .env("MCPGW_UPDATE_BASE_URL", &host.base)
        .env("MCPGW_STATE_DIR", dir.path())
        .env_remove("MCPGW_NO_UPDATE_CHECK")
        .output()
        .unwrap();
    assert_eq!(stderr(&again), "");
    assert!(dir.path().join("update-check.json").exists());
}

#[test]
fn the_kill_switch_stops_the_check_before_it_starts() {
    let dir = tempfile::tempdir().unwrap();
    let host = ReleaseHost::with_latest(NEWER);
    let out = Command::new(built_binary())
        .args(["list", "--json"])
        .env("MCPGW_CONFIG", dir.path().join("config.toml"))
        .env("MCPGW_UPDATE_BASE_URL", &host.base)
        .env("MCPGW_STATE_DIR", dir.path())
        .env("MCPGW_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(stderr(&out), "");
    assert!(!dir.path().join("update-check.json").exists());
}

#[test]
fn an_offline_check_is_silent_rather_than_noisy() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(built_binary())
        .args(["list", "--json"])
        .env("MCPGW_CONFIG", dir.path().join("config.toml"))
        .env("MCPGW_UPDATE_BASE_URL", "http://127.0.0.1:1")
        .env("MCPGW_STATE_DIR", dir.path())
        .env_remove("MCPGW_NO_UPDATE_CHECK")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(stderr(&out), "");
    // The failed attempt still counts as today's, so an offline machine
    // retries once a day rather than on every command.
    assert!(dir.path().join("update-check.json").exists());
}

#[test]
fn a_failed_command_gets_no_notice() {
    let dir = tempfile::tempdir().unwrap();
    let host = ReleaseHost::with_latest(NEWER);
    let config = dir.path().join("config.toml");
    std::fs::write(&config, "this is not toml = = =").unwrap();
    let out = Command::new(built_binary())
        .args(["list"])
        .env("MCPGW_CONFIG", &config)
        .env("MCPGW_UPDATE_BASE_URL", &host.base)
        .env("MCPGW_STATE_DIR", dir.path())
        .env_remove("MCPGW_NO_UPDATE_CHECK")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!stderr(&out).contains("self-update"), "{}", stderr(&out));
}

/// A stamp of the shape today's check leaves behind, dated now so nothing
/// downstream of it is due.
fn stamp_seeing(dir: &Path, seen: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let path = dir.join("update-check.json");
    std::fs::write(
        &path,
        format!(r#"{{"last_check": {now}, "last_seen": "{seen}"}}"#),
    )
    .unwrap();
    path
}

fn notice_line() -> String {
    format!(
        "mcpgw {NEWER} is available (you have {}) — run `mcpgw self-update`",
        current()
    )
}

#[test]
fn a_gateway_that_will_not_answer_is_told_it_may_be_an_old_gateway() {
    let dir = tempfile::tempdir().unwrap();
    let stamp = stamp_seeing(dir.path(), NEWER);
    let before = std::fs::read(&stamp).unwrap();
    // Port 1 for both the gateway and the release host: status exits 1
    // because nothing answers, and any attempt to check for a release
    // rather than read the stamp would be a refused connection, not a
    // notice.
    let out = run(
        &built_binary(),
        "http://127.0.0.1:1",
        dir.path(),
        &["daemon", "status", "--url", "http://127.0.0.1:1/mcp"],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert!(stderr(&out).contains(&notice_line()), "{}", stderr(&out));
    // Read, never rewritten: a failing command must not spend today's check.
    assert_eq!(std::fs::read(&stamp).unwrap(), before);
}

#[test]
fn a_failing_status_on_the_latest_release_says_nothing() {
    let dir = tempfile::tempdir().unwrap();
    stamp_seeing(dir.path(), current());
    let out = run(
        &built_binary(),
        "http://127.0.0.1:1",
        dir.path(),
        &["daemon", "status", "--url", "http://127.0.0.1:1/mcp"],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert!(!stderr(&out).contains("self-update"), "{}", stderr(&out));
}

#[test]
fn the_kill_switch_stops_the_cached_notice_too() {
    let dir = tempfile::tempdir().unwrap();
    stamp_seeing(dir.path(), NEWER);
    let out = Command::new(built_binary())
        .args(["daemon", "status", "--url", "http://127.0.0.1:1/mcp"])
        .env("MCPGW_UPDATE_BASE_URL", "http://127.0.0.1:1")
        .env("MCPGW_STATE_DIR", dir.path())
        .env("MCPGW_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert!(!stderr(&out).contains("self-update"), "{}", stderr(&out));
}

#[test]
fn a_command_that_worked_stays_quiet_until_the_next_check_is_due() {
    let dir = tempfile::tempdir().unwrap();
    stamp_seeing(dir.path(), NEWER);
    // The cached line belongs to the failure path only: on a command that
    // worked the daily check still decides, and today's has happened.
    let out = Command::new(built_binary())
        .args(["list", "--json"])
        .env("MCPGW_CONFIG", dir.path().join("config.toml"))
        .env("MCPGW_UPDATE_BASE_URL", "http://127.0.0.1:1")
        .env("MCPGW_STATE_DIR", dir.path())
        .env_remove("MCPGW_NO_UPDATE_CHECK")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stderr(&out), "");
}
