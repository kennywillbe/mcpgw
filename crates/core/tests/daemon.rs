//! The platform-agnostic half of `mcpgw daemon`: the refusals every
//! supervisor inherits, the log discipline, and the stub each per-OS
//! milestone will replace.

use std::path::PathBuf;

use mcpgw_core::daemon::{
    DaemonError, DaemonSpec, GatewayReach, LogPaths, PROBE_TIMEOUT, ServiceManager as _,
    platform_service, preflight, prepare_logs, probe_gateway,
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
        let err = preflight(&spec(bind, 0, dir.path())).unwrap_err();
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
        assert!(preflight(&spec(bind, 0, dir.path())).is_ok(), "{bind}");
    }
}

#[test]
fn a_taken_port_is_refused_by_name_and_points_at_status() {
    let dir = tempfile::tempdir().unwrap();
    // Held for the whole test, so the conflict is real rather than a
    // just-vacated port another process could grab in between.
    let held = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = held.local_addr().unwrap().port();

    let err = preflight(&spec("127.0.0.1", port, dir.path())).unwrap_err();
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

/// The stub each per-OS milestone replaces. Cfg-gated because exactly one of
/// the three is compiled into any build, by design.
#[test]
fn the_platform_stub_names_its_supervisor_and_the_workaround() {
    let dir = tempfile::tempdir().unwrap();
    let service = platform_service();
    let target = spec("127.0.0.1", 8137, dir.path());

    let messages = [
        service.install(&target).unwrap_err().to_string(),
        service.uninstall().unwrap_err().to_string(),
        service.start(&target).unwrap_err().to_string(),
        service.stop().unwrap_err().to_string(),
        service.query().unwrap_err().to_string(),
    ];
    for message in &messages {
        assert!(message.contains("not in this release yet"), "{message}");
        assert!(message.contains("mcpgw serve"), "{message}");
    }

    #[cfg(target_os = "macos")]
    {
        assert_eq!(service.name(), "launchd");
        assert!(
            messages[0].contains("macOS launch agent"),
            "{}",
            messages[0]
        );
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        assert_eq!(service.name(), "systemd --user");
        assert!(
            messages[0].contains("systemd --user unit"),
            "{}",
            messages[0]
        );
    }
    #[cfg(windows)]
    {
        assert_eq!(service.name(), "the Windows service manager");
        assert!(messages[0].contains("Windows service"), "{}", messages[0]);
    }
}
