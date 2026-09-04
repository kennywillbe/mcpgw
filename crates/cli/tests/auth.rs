//! `mcpgw auth login|status|logout` end to end, against an in-process
//! authorization server.
//!
//! `--no-browser` throughout: the test is the browser. It reads the URL the
//! command printed, walks it with a plain HTTP client, and the redirect lands
//! on the loopback listener the command is sitting on — which is exactly the
//! sequence a person produces, minus the consent screen.

mod util;

use std::io::{BufRead as _, BufReader};
use std::process::Stdio;

use mcpgw_test_server::oauth;
use util::mcpgw;

/// Runs an OAuth provider on its own runtime and thread, so a blocking CLI
/// test can talk to it without becoming an async test.
struct Provider {
    base: String,
    recorder: std::sync::Arc<oauth::Recorder>,
    // Dropped last; the runtime owns the server task, and dropping it is what
    // stops the provider.
    _runtime: tokio::runtime::Runtime,
}

impl Provider {
    fn start(config: oauth::Config) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (base, recorder) = runtime.block_on(async {
            let pending = oauth::bind(config).await;
            let challenge = pending.challenge();
            let base = pending.base.clone();
            let provider = pending.serve(oauth::refusing_resource(challenge));
            let recorder = std::sync::Arc::clone(&provider.recorder);
            // Leaked on purpose: the provider lives as long as its runtime,
            // and the runtime is dropped with this struct.
            std::mem::forget(provider);
            (base, recorder)
        });
        Self {
            base,
            recorder,
            _runtime: runtime,
        }
    }

    fn config(&self) -> String {
        format!(
            "version = 1\n\n[servers.linear]\ntype = \"http\"\nurl = \"{}/mcp\"\n",
            self.base
        )
    }
}

/// Runs `mcpgw auth login --no-browser`, walks the URL it prints, and returns
/// its whole stdout.
fn login(home: &std::path::Path, extra: &[&str]) -> (bool, String) {
    let mut child = mcpgw(home)
        .args(["auth", "login", "linear", "--no-browser"])
        .args(extra)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let mut lines = Vec::new();
    let mut reader = BufReader::new(stdout);
    // Read to the end, walking the URL the moment it appears: by the time the
    // command has printed one it is already sitting on its loopback listener,
    // and the lines after it are the ones that say how the login went.
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 {
            break;
        }
        let trimmed = line.trim().to_owned();
        if trimmed.starts_with("http://") {
            let url = trimmed.clone();
            std::thread::spawn(move || drop(ureq::get(&url).call()));
        }
        lines.push(trimmed);
    }
    let status = child.wait().unwrap();
    let mut text = lines.join("\n");
    text.push('\n');
    std::io::Read::read_to_string(&mut stderr, &mut text).unwrap();
    (status.success(), text)
}

#[test]
fn login_prints_the_url_walks_the_callback_and_stores_the_tokens() {
    let home = tempfile::tempdir().unwrap();
    let provider = Provider::start(oauth::Config::default());
    std::fs::write(home.path().join("config.toml"), provider.config()).unwrap();

    let (ok, text) = login(home.path(), &[]);
    assert!(ok, "{text}");
    // The URL is printed whether or not a browser was going to open, because
    // a browser that opened the wrong profile leaves the user with nothing.
    assert!(text.contains("open this URL to finish the login"), "{text}");
    assert!(
        text.contains(&format!("{}/authorize", provider.base)),
        "{text}"
    );
    assert!(text.contains("logged in to linear at"), "{text}");
    assert_eq!(provider.recorder.exchanges.load(SEQ), 1);

    // The tokens are where the gateway will look for them, and nothing that
    // reached the terminal carries one.
    let path = home.path().join("state/auth/linear.json");
    assert!(path.exists(), "{}", path.display());
    let stored: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let token = stored["credentials"]["token_response"]["access_token"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(!token.is_empty());
    assert!(
        !text.contains(&token),
        "a token reached the terminal: {text}"
    );

    // `status` reports it without printing it.
    let out = mcpgw(home.path())
        .args(["auth", "status", "--json"])
        .output()
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let row = &value["servers"][0];
    assert_eq!(row["server"], "linear");
    assert_eq!(row["logged_in"], true);
    assert_eq!(row["state"], "valid");
    assert_eq!(row["issuer"], provider.base);
    assert_eq!(row["client_id"], mcpgw_core::auth::CLIENT_ID_URL);
    assert_eq!(row["identity"], "client id metadata document");
    let printed = String::from_utf8_lossy(&out.stdout);
    assert!(
        !printed.contains(&token),
        "a token reached a report: {printed}"
    );

    // And `logout` takes it away.
    let out = mcpgw(home.path())
        .args(["auth", "logout", "linear"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("logged out of \"linear\""),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(!path.exists());
}

/// `--client-id` is not a one-off: a refresh runs in the daemon, where nobody
/// can pass a flag, so the id has to survive in the config.
#[test]
fn a_preregistered_client_id_is_presented_and_kept_in_the_config() {
    let home = tempfile::tempdir().unwrap();
    let provider = Provider::start(oauth::Config {
        cimd: true,
        dcr: true,
        ..oauth::Config::default()
    });
    let config = home.path().join("config.toml");
    std::fs::write(&config, provider.config()).unwrap();

    let (ok, text) = login(home.path(), &["--client-id", "issued-by-hand"]);
    assert!(ok, "{text}");
    assert_eq!(
        provider.recorder.client_id().as_deref(),
        Some("issued-by-hand")
    );
    assert_eq!(provider.recorder.registrations.load(SEQ), 0);

    let written = std::fs::read_to_string(&config).unwrap();
    assert!(written.contains("issued-by-hand"), "{written}");
    // The hand-written entry above kept its shape; only the table was added.
    assert!(written.contains("type = \"http\""), "{written}");

    let out = mcpgw(home.path())
        .args(["auth", "status", "linear", "--json"])
        .output()
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["servers"][0]["identity"], "pre-registered client id");
}

#[test]
fn status_says_which_servers_have_never_been_logged_into() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        "version = 1\n\n[servers.linear]\ntype = \"http\"\nurl = \"https://mcp.linear.app/mcp\"\n",
    )
    .unwrap();

    let out = mcpgw(home.path())
        .args(["auth", "status"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("linear  no login yet — run mcpgw auth login linear"),
        "{text}"
    );
}

/// Two answers to one header is a config mistake, and it is named where it is
/// made rather than at the next connect.
#[test]
fn a_server_cannot_have_both_a_headers_command_and_an_auth_table() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        "version = 1\n\n[servers.linear]\ntype = \"http\"\n\
         url = \"https://mcp.linear.app/mcp\"\n\
         headers_command = [\"corp-auth\"]\n\
         auth = { client_id = \"abc\" }\n",
    )
    .unwrap();

    let out = mcpgw(home.path())
        .args(["auth", "status"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(
        text.contains("sets both headers_command and [auth]"),
        "{text}"
    );
}

const SEQ: std::sync::atomic::Ordering = std::sync::atomic::Ordering::SeqCst;
