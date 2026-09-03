//! The platform-agnostic half of `mcpgw daemon`: the refusals every
//! supervisor inherits, the log discipline, and the stub each per-OS
//! milestone will replace.

use std::path::PathBuf;

use mcpgw_core::daemon::{
    DaemonError, DaemonSpec, GatewayReach, LogPaths, PROBE_TIMEOUT, PortPolicy, ServiceStatus,
    port_policy, preflight, prepare_logs, probe_gateway,
};

fn spec(bind: &str, port: u16, state_dir: &std::path::Path) -> DaemonSpec {
    DaemonSpec {
        exe: PathBuf::from("/usr/local/bin/mcpgw"),
        config_path: PathBuf::from("/home/u/.config/mcpgw/config.toml"),
        state_dir: state_dir.to_owned(),
        bind: bind.to_owned(),
        port,
        logs: LogPaths::under_state_dir(state_dir),
    }
}

/// The decided security property: an unattended gateway may only be reached
/// from the machine it runs on.
#[test]
fn a_non_loopback_bind_is_refused_with_the_reason_spelled_out() {
    let dir = tempfile::tempdir().unwrap();
    for bind in ["0.0.0.0", "192.168.1.10", "::", "gateway.internal"] {
        let err = preflight(&spec(bind, 0, dir.path()), PortPolicy::MustBeFree).unwrap_err();
        assert!(
            matches!(&err, DaemonError::NonLoopbackBind { bind: got } if got == bind),
            "{bind}: {err}"
        );
        let text = err.to_string();
        assert!(text.contains("no authentication"), "{text}");
        assert!(text.contains("logfile nobody reads"), "{text}");
    }
}

#[test]
fn every_loopback_spelling_passes_the_bind_check() {
    let dir = tempfile::tempdir().unwrap();
    // Port 0 never conflicts, so only the bind check can fail here.
    for bind in ["127.0.0.1", "127.0.0.53", "::1", "localhost"] {
        assert!(
            preflight(&spec(bind, 0, dir.path()), PortPolicy::MustBeFree).is_ok(),
            "{bind}"
        );
    }
}

#[test]
fn a_taken_port_is_refused_by_name_and_points_at_status() {
    let dir = tempfile::tempdir().unwrap();
    // Held for the whole test, so the conflict is real rather than a
    // just-vacated port another process could grab in between.
    let held = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = held.local_addr().unwrap().port();

    let err = preflight(&spec("127.0.0.1", port, dir.path()), PortPolicy::MustBeFree).unwrap_err();
    let text = err.to_string();
    assert!(matches!(err, DaemonError::PortInUse { .. }), "{text}");
    assert!(text.contains(&format!("127.0.0.1:{port}")), "{text}");
    assert!(text.contains("mcpgw daemon status"), "{text}");
    drop(held);
}

#[test]
fn the_logs_land_under_the_state_dir_and_are_created_empty() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let paths = prepare_logs(&state).unwrap();

    assert_eq!(paths, LogPaths::under_state_dir(&state));
    assert!(paths.stdout.starts_with(state.join("logs")));
    for path in [&paths.stdout, &paths.stderr] {
        assert_eq!(std::fs::read(path).unwrap(), Vec::<u8>::new());
    }
    // Idempotent: preparing again must not truncate what the daemon wrote.
    std::fs::write(&paths.stdout, "listening\n").unwrap();
    prepare_logs(&state).unwrap();
    assert_eq!(
        std::fs::read_to_string(&paths.stdout).unwrap(),
        "listening\n"
    );
}

/// Same discipline as the traffic log: the gateway's output can carry the
/// header values it was configured with.
#[cfg(unix)]
#[test]
fn the_log_dir_is_0700_and_the_log_files_0600() {
    use std::os::unix::fs::PermissionsExt as _;

    let mode =
        |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;

    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let paths = prepare_logs(&state).unwrap();

    assert_eq!(mode(&state), 0o700, "{:o}", mode(&state));
    let logs = state.join("logs");
    assert_eq!(mode(&logs), 0o700, "{:o}", mode(&logs));
    for path in [&paths.stdout, &paths.stderr] {
        assert_eq!(mode(path), 0o600, "{} is {:o}", path.display(), mode(path));
    }

    // A file an older build (or a supervisor's own redirect) left readable
    // is narrowed, not left as found.
    std::fs::set_permissions(&paths.stdout, std::fs::Permissions::from_mode(0o644)).unwrap();
    prepare_logs(&state).unwrap();
    assert_eq!(mode(&paths.stdout), 0o600, "{:o}", mode(&paths.stdout));
}

#[test]
fn a_spec_names_the_url_and_the_serve_arguments_a_service_will_run() {
    let dir = tempfile::tempdir().unwrap();
    let loopback = spec("127.0.0.1", 8137, dir.path());
    assert_eq!(loopback.url(), "http://127.0.0.1:8137/mcp");
    assert_eq!(loopback.authority(), "127.0.0.1:8137");
    assert_eq!(
        loopback.serve_args(),
        ["serve", "--bind", "127.0.0.1", "--port", "8137"]
    );
    // An IPv6 literal has to come back bracketed or the URL is unparseable.
    assert_eq!(spec("::1", 9000, dir.path()).url(), "http://[::1]:9000/mcp");
}

/// The record `status` reads to find out where the service it is asking
/// about actually listens.
#[test]
fn the_installed_spec_survives_a_round_trip_through_the_state_dir() {
    use mcpgw_core::daemon::{load_spec, remove_spec, save_spec, spec_path};

    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    // Nothing recorded is not a failure — it is every install made before
    // this file existed, and every machine with no service at all.
    assert_eq!(load_spec(&state), None);

    let installed = spec("127.0.0.1", 18137, &state);
    save_spec(&installed).unwrap();
    assert_eq!(load_spec(&state), Some(installed.clone()));
    assert_eq!(
        load_spec(&state).unwrap().url(),
        "http://127.0.0.1:18137/mcp"
    );

    // Half a file, or one from a future schema, must not stop `status` from
    // running — it falls back to the default exactly as if none were there.
    std::fs::write(spec_path(&state), b"{\"bind\":").unwrap();
    assert_eq!(load_spec(&state), None);

    save_spec(&installed).unwrap();
    remove_spec(&state).unwrap();
    assert!(!spec_path(&state).exists());
    // Removing what is not there is the end state that was asked for.
    remove_spec(&state).unwrap();
}

/// It names a config path and a home directory, so it gets the same 0600 the
/// rest of the state dir has.
#[cfg(unix)]
#[test]
fn the_installed_spec_is_written_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let path = mcpgw_core::daemon::spec_path(&state);
    let mode =
        |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;

    mcpgw_core::daemon::save_spec(&spec("127.0.0.1", 18137, &state)).unwrap();
    assert_eq!(mode(&path), 0o600, "{:o}", mode(&path));

    // A file an older, looser build left readable is narrowed on the next
    // write rather than left as found.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    mcpgw_core::daemon::save_spec(&spec("127.0.0.1", 18137, &state)).unwrap();
    assert_eq!(mode(&path), 0o600, "{:o}", mode(&path));
}

/// The folders a launch agent cannot read through, and where the silent dyld
/// hang comes from. Driven with an injected home so it runs the same on a CI
/// runner as on a developer's machine.
#[cfg(target_os = "macos")]
#[test]
fn a_binary_under_a_tcc_protected_folder_is_named_by_the_folder() {
    use mcpgw_core::daemon::tcc_protected_dir;

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    for folder in ["Desktop", "Documents", "Downloads"] {
        let exe = home.join(folder).join("clone/target/release/mcpgw");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"").unwrap();
        assert_eq!(tcc_protected_dir(&exe, home), Some(folder));
    }
    // The paths a real install uses, and a folder that merely starts like a
    // protected one.
    for exe in [
        home.join("Desktopish/mcpgw"),
        home.join(".cargo/bin/mcpgw"),
        PathBuf::from("/opt/homebrew/bin/mcpgw"),
    ] {
        assert_eq!(tcc_protected_dir(&exe, home), None, "{}", exe.display());
    }
    // A symlink out of an unprotected path into Desktop hangs just the same,
    // so both sides are compared canonicalized.
    let link = home.join("bin-mcpgw");
    std::os::unix::fs::symlink(home.join("Desktop/clone/target/release/mcpgw"), &link).unwrap();
    assert_eq!(tcc_protected_dir(&link, home), Some("Desktop"));
}

#[tokio::test]
async fn a_port_nobody_holds_probes_as_down() {
    // Bound and released: the port is known to have been free, and nothing
    // in this test depends on it staying that way beyond the probe.
    let port = {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.local_addr().unwrap().port()
    };
    let reach = probe_gateway(&format!("http://127.0.0.1:{port}/mcp"), PROBE_TIMEOUT).await;
    assert_eq!(reach, GatewayReach::Down);
    assert!(!reach.is_up());
}

/// A socket that accepts and then says nothing is the "some other program
/// owns 8137" state, and calling that a running gateway costs an hour.
#[tokio::test]
async fn a_silent_listener_probes_as_not_http() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepting = tokio::spawn(async move {
        // Accepted connections are parked, never answered, for as long as
        // the probe can possibly wait.
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });

    let reach = probe_gateway(&format!("http://127.0.0.1:{port}/mcp"), PROBE_TIMEOUT).await;
    accepting.abort();
    assert_eq!(reach, GatewayReach::NotHttp);
}

/// A live install is never attempted from the suite: it needs administrator
/// rights, it would leave a registered service behind on a developer's
/// machine, and on a CI runner — which is elevated — it would succeed.
/// Querying is the one operation that needs no rights and changes nothing,
/// so it is the one that can be run for real, and on a machine without the
/// service it has to answer "not installed" rather than fail.
#[cfg(windows)]
#[test]
fn windows_answers_a_query_without_rights_and_without_changing_anything() {
    use mcpgw_core::daemon::{ServiceManager as _, platform_service};

    let service = platform_service();
    assert_eq!(service.name(), "the Windows service manager");
    let status = service.query().expect("the service database can be read");
    if !status.installed {
        assert!(!status.running);
        assert!(status.unit_path.is_none());
    }
}

/// A stand-in for a gateway that is already up: it answers every connection
/// with a response line, which is the whole of what a probe reads. Kept
/// alive by the returned task, so the port stays held for the test.
async fn answering_gateway() -> (tokio::task::JoinHandle<()>, u16) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let serving = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buffer = [0u8; 512];
                let _ = stream.read(&mut buffer).await;
                let _ = stream
                    .write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n")
                    .await;
            });
        }
    });
    (serving, port)
}

fn service(installed: bool, running: bool) -> ServiceStatus {
    ServiceStatus {
        installed,
        running,
        unit_path: None,
        detail: None,
    }
}

/// The bug in #116: a service installed while mcpgw lived in `~/.cargo/bin`
/// could not be reinstalled to point at a Homebrew binary, because its own
/// listening socket was read as somebody else's.
#[tokio::test]
async fn our_own_running_service_is_reinstalled_over_rather_than_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (serving, port) = answering_gateway().await;
    let ours = spec("127.0.0.1", port, dir.path());

    let policy = port_policy(Some(&service(true, true)), &ours).await;
    assert_eq!(policy, PortPolicy::OwnServiceReinstall);
    assert!(preflight(&ours, policy).is_ok());
    // The bind refusal is not part of the bargain: a reinstall onto an
    // address the network can reach is still an unattended, unauthenticated
    // gateway.
    let exposed = spec("0.0.0.0", port, dir.path());
    assert!(matches!(
        preflight(&exposed, PortPolicy::OwnServiceReinstall).unwrap_err(),
        DaemonError::NonLoopbackBind { .. }
    ));

    serving.abort();
}

/// The refusal has to survive the fix: the port is only ours when the
/// supervisor holds a running job *and* a gateway answers on it.
#[tokio::test]
async fn a_port_that_is_not_our_running_service_is_still_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (serving, port) = answering_gateway().await;
    let answering = spec("127.0.0.1", port, dir.path());

    // A gateway answering with nothing installed is a foreground
    // `mcpgw serve`, which a service must never be installed on top of.
    for queried in [
        None,
        Some(service(false, false)),
        Some(service(true, false)),
    ] {
        let policy = port_policy(queried.as_ref(), &answering).await;
        assert_eq!(policy, PortPolicy::MustBeFree, "{queried:?}");
        assert!(
            matches!(
                preflight(&answering, policy).unwrap_err(),
                DaemonError::PortInUse { .. }
            ),
            "{queried:?}"
        );
    }
    serving.abort();

    // A running service and a port held by something that does not speak
    // HTTP: the job is ours, the socket is not.
    let held = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = held.local_addr().unwrap().port();
    let parking = tokio::spawn(async move {
        let mut open = Vec::new();
        while let Ok((stream, _)) = held.accept().await {
            open.push(stream);
        }
    });
    let silent = spec("127.0.0.1", port, dir.path());
    let policy = port_policy(Some(&service(true, true)), &silent).await;
    assert_eq!(policy, PortPolicy::MustBeFree);
    assert!(matches!(
        preflight(&silent, policy).unwrap_err(),
        DaemonError::PortInUse { .. }
    ));
    parking.abort();
}
