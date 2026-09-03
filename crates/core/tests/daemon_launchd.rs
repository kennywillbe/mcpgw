//! The macOS launch agent: the exact plist launchd is handed, and the
//! read-only half of the manager that is safe to run on any Mac.
//!
//! The install/start/stop cycle is not exercised here — it bootstraps a real
//! job into the running user's launchd domain, so it lives in the CLI's
//! `daemon_launchd` test behind `MCPGW_DAEMON_LIVE=1` and never runs in CI.

#![cfg(target_os = "macos")]

use std::path::PathBuf;

use mcpgw_core::daemon::launchd::{LABEL, Launchd, plist_path, render_plist};
use mcpgw_core::daemon::{DaemonSpec, LogPaths, ServiceManager as _, ServiceStatus};

fn spec() -> DaemonSpec {
    let state_dir = PathBuf::from("/Users/u/.local/share/mcpgw");
    DaemonSpec {
        exe: PathBuf::from("/usr/local/bin/mcpgw"),
        config_path: PathBuf::from("/Users/u/.config/mcpgw/config.toml"),
        logs: LogPaths::under_state_dir(&state_dir),
        state_dir,
        bind: "127.0.0.1".to_owned(),
        port: 8137,
    }
}

/// The whole file, byte for byte: a plist is read by launchd and by the user
/// debugging it, and every key in here was a decision.
#[test]
fn the_plist_is_the_one_launchd_is_handed() {
    let rendered = render_plist(&spec(), Some("/opt/homebrew/bin:/usr/bin:/bin"));
    assert_eq!(rendered, EXPECTED_PLIST, "\n--- rendered ---\n{rendered}");
}

const EXPECTED_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>io.mcpgw.gateway</string>
	<key>ProgramArguments</key>
	<array>
		<string>/usr/local/bin/mcpgw</string>
		<string>serve</string>
		<string>--bind</string>
		<string>127.0.0.1</string>
		<string>--port</string>
		<string>8137</string>
		<string>--supervised</string>
	</array>
	<key>EnvironmentVariables</key>
	<dict>
		<key>MCPGW_CONFIG</key>
		<string>/Users/u/.config/mcpgw/config.toml</string>
		<key>MCPGW_STATE_DIR</key>
		<string>/Users/u/.local/share/mcpgw</string>
		<key>PATH</key>
		<string>/opt/homebrew/bin:/usr/bin:/bin</string>
	</dict>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>
	<key>StandardOutPath</key>
	<string>/Users/u/.local/share/mcpgw/logs/daemon.out.log</string>
	<key>StandardErrorPath</key>
	<string>/Users/u/.local/share/mcpgw/logs/daemon.err.log</string>
</dict>
</plist>
"#;

/// `KeepAlive` is the reason this plist is hand-written rather than generated:
/// as a bare `<true/>` it would restart the gateway after every deliberate
/// `mcpgw daemon stop`.
#[test]
fn keep_alive_restarts_a_crash_and_only_a_crash() {
    let rendered = render_plist(&spec(), None);
    assert!(
        rendered.contains(
            "\t<key>KeepAlive</key>\n\t<dict>\n\t\t<key>SuccessfulExit</key>\n\t\t<false/>\n"
        ),
        "{rendered}"
    );
    assert!(
        !rendered.contains("<key>KeepAlive</key>\n\t<true/>"),
        "{rendered}"
    );
}

/// The gateway is only worth supervising if it serves the config the install
/// was pointed at, and a launch agent starts with none of the environment the
/// installing shell had.
#[test]
fn the_agent_is_told_which_config_and_state_dir_to_use() {
    let rendered = render_plist(&spec(), None);
    assert!(rendered.contains("<key>MCPGW_CONFIG</key>"), "{rendered}");
    assert!(
        rendered.contains("<string>/Users/u/.config/mcpgw/config.toml</string>"),
        "{rendered}"
    );
    assert!(
        rendered.contains("<key>MCPGW_STATE_DIR</key>"),
        "{rendered}"
    );
    // No PATH to inherit means no PATH key, rather than an empty one that
    // would leave stdio servers with nothing to search at all.
    assert!(!rendered.contains("<key>PATH</key>"), "{rendered}");
}

/// One `&` in a home directory is enough for launchd to reject the whole file
/// with a parse error it does not explain.
#[test]
fn a_path_with_xml_metacharacters_survives_as_itself() {
    let mut spec = spec();
    spec.exe = PathBuf::from("/Users/a&b/<bin>/mcpgw");
    let rendered = render_plist(&spec, None);
    assert!(
        rendered.contains("<string>/Users/a&amp;b/&lt;bin&gt;/mcpgw</string>"),
        "{rendered}"
    );
    assert!(!rendered.contains("a&b"), "{rendered}");
}

/// The port and bind the install was given, spelled out in the arguments, so
/// a later change to mcpgw's defaults cannot move an installed service.
#[test]
fn the_program_arguments_are_the_specs_serve_arguments() {
    let mut spec = spec();
    spec.bind = "::1".to_owned();
    spec.port = 9137;
    let rendered = render_plist(&spec, None);
    for arg in spec.serve_args() {
        assert!(
            rendered.contains(&format!("<string>{arg}</string>")),
            "{arg}"
        );
    }
}

#[test]
fn the_agent_goes_where_launchd_looks_for_per_user_agents() {
    let path = plist_path().unwrap();
    assert!(
        path.ends_with(format!("Library/LaunchAgents/{LABEL}.plist")),
        "{}",
        path.display()
    );
    assert!(path.is_absolute(), "{}", path.display());
}

/// Read-only, so it is safe on a developer's machine whether or not they have
/// the agent installed: the plist on disk is what "installed" means, and the
/// answer has to match it either way.
#[test]
fn a_query_answers_for_the_plist_that_is_actually_there() {
    let status = Launchd::new().query().unwrap();
    let path = plist_path().unwrap();

    assert_eq!(status.installed, path.exists(), "{}", path.display());
    if status.installed {
        assert_eq!(status.unit_path.as_deref(), Some(path.as_path()));
    } else {
        assert_eq!(status, ServiceStatus::default());
        // Nothing installed is a status, not an error, and `daemon status`
        // prints it as "not installed under launchd".
        assert!(!status.running);
    }
}
