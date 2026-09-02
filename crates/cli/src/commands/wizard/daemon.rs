//! Wizard step 3: get the gateway running, and keep it running.
//!
//! The one step that hands a machine something it will still be doing next
//! week, which is why it says the whole of what that means — the login item,
//! the file it writes, the command that removes it again — before it asks.
//! Everything here is optional: a user who says no, or a platform whose
//! service support has not shipped, leaves with a working setup and a
//! `mcpgw serve` to run in a terminal.
//!
//! See the contract in [`super`]: this module is `pending` + `run`.

use std::time::{Duration, Instant};

use anyhow::Context as _;
use mcpgw_core::daemon::{
    DaemonError, DaemonSpec, LogPaths, PROBE_TIMEOUT, ServiceManager, ServiceStatus,
};

use super::{Ctx, Outcome};
use crate::ui;

/// How long an install is given to become a gateway that answers. Generous
/// against a cold supervisor and a first-run gateway; short enough that a
/// service which is never coming up does not hold the wizard hostage.
const UP_DEADLINE: Duration = Duration::from_secs(15);

/// How long the port is watched after a user says they stopped their
/// foreground gateway. A terminated listener frees its port at once; this is
/// slack for the moment between Ctrl-C and the process actually leaving.
const RELEASE_DEADLINE: Duration = Duration::from_secs(5);

/// Gap between polls, in both waits above.
const POLL: Duration = Duration::from_millis(250);

const HEADING: &str = "Next: keep the gateway running.";

/// What the step offers instead of a service, on every path that ends without
/// one — a declined install, an unsupported platform, a failed install.
const FALLBACK: &str = "Run `mcpgw serve` in a terminal and leave it open; everything else in this setup works \
     exactly the same.";

/// True unless a gateway is already answering. Deliberately not "is a
/// service installed": a foreground `mcpgw serve` in another terminal is a
/// gateway, and offering to install a service on top of one would be the
/// wizard arguing with what the user can plainly see working.
pub fn pending(cx: &Ctx) -> bool {
    !cx.reach.is_up()
}

/// Explains the login service, asks, and installs it.
///
/// # Errors
///
/// Returns the underlying [`std::io::Error`] if the terminal cannot be read,
/// or a failure from [`Ctx::refresh`] after the service is installed. A
/// platform that cannot install, a port that is taken and a supervisor that
/// refuses are all outcomes rather than errors: none of them is a reason to
/// abandon a setup whose remaining steps work against a foreground gateway.
pub fn run(cx: &mut Ctx) -> anyhow::Result<Outcome> {
    let service = mcpgw_core::daemon::platform_service();
    let spec = match spec(cx) {
        Ok(spec) => spec,
        // No home directory, or a gateway URL with no port in it: the wizard
        // cannot describe a service, let alone install one.
        Err(err) => return Ok(no_service(cx, &format!("{err:#}"))),
    };

    // Asked before anything is announced, because a platform whose installer
    // has not shipped should never be told about login items it cannot get.
    let queried = service.query();
    if let Err(DaemonError::NotSupportedYet(message)) = &queried {
        return unsupported(cx, message);
    }

    // The port belongs to whoever holds it, and an install that would fail on
    // it has its own conversation rather than the general one.
    if let Err(err) = mcpgw_core::daemon::preflight(&spec) {
        return match err {
            DaemonError::PortInUse { .. } => port_taken(cx, &service, &spec),
            other => Ok(no_service(cx, &other.to_string())),
        };
    }

    announce(cx, &service, &spec, queried.ok().as_ref());
    if cx.confirm("\nInstall it now?")? {
        return install(cx, &service, spec);
    }
    println!("  {FALLBACK}");
    println!(
        "  {}",
        ui::dim(
            "`mcpgw daemon install` sets the service up later, whenever you want it.",
            cx.color,
        )
    );
    Ok(Outcome::Handled)
}

/// The show half of show-and-confirm: what will be installed, where it will
/// live, and every consequence a user would otherwise meet as a surprise.
fn announce(
    cx: &Ctx,
    service: &impl ServiceManager,
    spec: &DaemonSpec,
    status: Option<&ServiceStatus>,
) {
    let mut bullets = vec![
        format!(
            "I'll install a {} service so the gateway starts when you log in and comes back if \
             it crashes.",
            service.name()
        ),
        format!(
            "It serves {} from {}.",
            spec.url(),
            spec.config_path.display()
        ),
    ];
    bullets.extend(platform_specifics(service.name(), status));
    ui::step(HEADING, &bullets, cx.color);
}

/// The per-platform half of the announcement — the parts a user only finds
/// out about afterwards otherwise.
///
/// These sentences are also in the install notes, which are printed as a
/// receipt once the service exists. The receipt is not the warning: by the
/// time launchd has bootstrapped the job, macOS has already shown its
/// notification and the user is reading about something that happened.
fn platform_specifics(manager: &str, status: Option<&ServiceStatus>) -> Vec<String> {
    let mut bullets = Vec::new();
    // The one thing that visibly changes about the machine, said before
    // anything can make it happen.
    #[cfg(target_os = "macos")]
    bullets.push(
        "macOS will show a \"Background Items Added\" notification and list mcpgw under System \
         Settings › General › Login Items & Extensions — that's this, and leaving it enabled is \
         what keeps the gateway coming back."
            .to_owned(),
    );
    bullets.push(service_file_line(manager));

    // systemd without lingering stops every user service at logout, which
    // turns "starts at login" into something much smaller than it sounds.
    // The platform is the one that knows, so its own words are surfaced.
    if let Some(detail) = status.and_then(|status| status.detail.as_deref())
        && detail.to_lowercase().contains("linger")
    {
        bullets.push(detail.to_owned());
        bullets.push(
            "That means the gateway stops when you log out. `loginctl enable-linger` changes it \
             — your call, I won't run it for you."
                .to_owned(),
        );
    }
    bullets
}

/// Where the service definition goes, named as a file when the platform can
/// say so — "one file you can look at and delete" is what makes a login item
/// feel like something a user still owns.
fn service_file_line(manager: &str) -> String {
    #[cfg(target_os = "macos")]
    if let Ok(path) = mcpgw_core::daemon::launchd::plist_path() {
        return format!(
            "It's one file, {} — `mcpgw daemon uninstall` stops it and removes it completely.",
            path.display()
        );
    }
    format!("It's a single {manager} service — `mcpgw daemon uninstall` removes it completely.")
}

/// Installs, then waits for the two things that have to be true before the
/// wizard may claim the gateway is up: the supervisor holds the job, and
/// something at the URL actually answers.
fn install(
    cx: &mut Ctx,
    service: &impl ServiceManager,
    mut spec: DaemonSpec,
) -> anyhow::Result<Outcome> {
    // The ordering the platforms are allowed to assume (see the contract in
    // `mcpgw_core::daemon`): logs exist and preflight has passed before
    // `install` is called. Preflight runs again here rather than only before
    // the question, because the conversation above took time and the port may
    // have been taken during it.
    match mcpgw_core::daemon::prepare_logs(&spec.state_dir) {
        Ok(logs) => spec.logs = logs,
        Err(err) => return Ok(no_service(cx, &err.to_string())),
    }
    if let Err(err) = mcpgw_core::daemon::preflight(&spec) {
        return Ok(no_service(cx, &err.to_string()));
    }

    let installed = match service.install(&spec) {
        Ok(installed) => installed,
        // A platform whose `query` works but whose installer has not shipped.
        Err(DaemonError::NotSupportedYet(message)) => return unsupported(cx, message),
        Err(err) => {
            println!("  the service could not be installed: {err}");
            println!("  `mcpgw daemon logs` has whatever it managed to write.");
            println!("  {FALLBACK}");
            return Ok(Outcome::Handled);
        }
    };

    println!("  installed at {}", installed.unit_path.display());
    for note in &installed.notes {
        println!("  {}", ui::dim(note, cx.color));
    }

    let up = wait_until_up(service, &spec);
    // The world changed either way: the steps after this one ask whether a
    // gateway answers, and they must not be told what was true before.
    cx.refresh()?;
    if up {
        println!(
            "  the gateway is answering at {} — and it will be there at your next login too.",
            spec.url()
        );
    } else {
        println!(
            "  installed, but nothing answers at {} yet — `mcpgw daemon status` and \
             `mcpgw daemon logs` say why.",
            spec.url()
        );
        println!("  {FALLBACK}");
    }
    Ok(Outcome::Handled)
}

/// The port the service would bind is already taken. Whether that is a
/// problem depends entirely on what is holding it.
fn port_taken(
    cx: &mut Ctx,
    service: &impl ServiceManager,
    spec: &DaemonSpec,
) -> anyhow::Result<Outcome> {
    let url = spec.url();
    // Probed now rather than read off `cx.reach`: the step only runs because
    // no gateway was answering when the wizard started, and the interesting
    // case is precisely the one where that has since changed.
    if !super::probe(&url, PROBE_TIMEOUT).is_up() {
        ui::step(
            HEADING,
            &[
                format!(
                    "Something is already listening on {} and it isn't an mcpgw gateway, so a \
                     service installed here could never bind it.",
                    spec.authority()
                ),
                "`mcpgw daemon status` says what mcpgw can see of it; free that port, or install \
                 the service on another with `mcpgw daemon install --port`."
                    .to_owned(),
            ],
            cx.color,
        );
        println!("  {FALLBACK}");
        return Ok(Outcome::Handled);
    }

    ui::step(
        HEADING,
        &[
            format!(
                "A gateway is already answering at {url}, but nothing is installed under {} — \
                 that's an `mcpgw serve` running in a terminal, and it stops when that terminal \
                 does.",
                service.name()
            ),
            "A login service would serve the same address, so that one has to stop before this \
             one can start."
                .to_owned(),
        ],
        cx.color,
    );

    if cx.assume_yes {
        // Never on someone's behalf: the process holding that port is doing
        // useful work for a client right now, and --yes is not permission to
        // take it away.
        println!("  --yes: leaving the gateway you already have running, and installing nothing.");
        println!(
            "  {}",
            ui::dim(
                "stop it and run `mcpgw daemon install` when you want the service instead.",
                cx.color,
            )
        );
        return Ok(Outcome::Handled);
    }

    if !cx.confirm("\nSwap it for a service that starts at login?")? {
        println!(
            "  keeping the gateway you already have — `mcpgw daemon install` is there for later."
        );
        return Ok(Outcome::Handled);
    }
    if !wait_for_the_port(cx, spec)? {
        println!("  leaving it be — `mcpgw daemon install` is there once that port is free.");
        return Ok(Outcome::Handled);
    }
    install(cx, service, spec.clone())
}

/// Asks the user to stop their foreground gateway and waits for the port to
/// come free, re-asking for as long as they are willing to try.
fn wait_for_the_port(cx: &Ctx, spec: &DaemonSpec) -> anyhow::Result<bool> {
    loop {
        println!(
            "  press Ctrl-C in the terminal running `mcpgw serve`, then come back to this one."
        );
        if !cx.confirm("Stopped it?")? {
            return Ok(false);
        }
        let deadline = Instant::now() + RELEASE_DEADLINE;
        loop {
            if !mcpgw_core::daemon::port_in_use(&spec.bind, spec.port) {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(POLL);
        }
        println!("  {} is still busy.", spec.authority());
    }
}

/// Both halves of "it worked": the supervisor holds the job *and* the gateway
/// answers. Either alone is a service that will disappoint the next step —
/// launchd will happily report a job it is restarting in a loop.
fn wait_until_up(service: &impl ServiceManager, spec: &DaemonSpec) -> bool {
    let url = spec.url();
    let deadline = Instant::now() + UP_DEADLINE;
    loop {
        let running = service.query().is_ok_and(|status| status.running);
        if running && super::probe(&url, PROBE_TIMEOUT).is_up() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL);
    }
}

/// The platform has no installer yet. Honest, and explicitly not a failure:
/// the wizard's remaining steps do not care how the gateway gets started.
fn unsupported(cx: &Ctx, message: &str) -> anyhow::Result<Outcome> {
    ui::step(
        HEADING,
        &[
            "I can't install a service on this system yet.".to_owned(),
            FALLBACK.to_owned(),
            ui::dim(message, cx.color),
        ],
        cx.color,
    );
    if cx.assume_yes {
        println!("  --yes: skipping the service, because there is nothing here to install.");
    }
    Ok(if cx.confirm("\nContinue without it?")? {
        Outcome::Handled
    } else {
        Outcome::Stop
    })
}

/// No service, for a reason that is nobody's decision to make — a refused
/// bind address, an unreadable state directory. Says why, offers the
/// fallback, and lets the wizard carry on.
fn no_service(cx: &Ctx, why: &str) -> Outcome {
    ui::step(
        HEADING,
        &[
            format!("I can't install a service here: {why}"),
            FALLBACK.to_owned(),
        ],
        cx.color,
    );
    Outcome::Handled
}

/// The service the wizard would install, pointed at the address it has been
/// telling the user about all along.
///
/// Built here rather than borrowed from `mcpgw daemon`: that command's
/// builder takes its address from its own flags and their defaults, and a
/// wizard run with `--gateway-url` must not quietly install a service on a
/// different port than the one it just wrote into everybody's client config.
fn spec(cx: &Ctx) -> anyhow::Result<DaemonSpec> {
    let (bind, port) = authority_of(&cx.gateway_url).with_context(|| {
        format!(
            "{} has no host and port for a service to listen on",
            cx.gateway_url
        )
    })?;
    let state_dir = mcpgw_core::paths::state_dir()
        .context("cannot determine a home directory to resolve the state directory")?;
    Ok(DaemonSpec {
        // Resolved now: the service has to name a binary that will still be
        // there long after the shell that installed it is gone.
        exe: std::env::current_exe().context("cannot locate the running mcpgw binary")?,
        config_path: cx.config_path.clone(),
        // Named without touching the disk — nothing is written until the
        // user says yes, so the files themselves are created in `install`.
        logs: LogPaths::under_state_dir(&state_dir),
        state_dir,
        bind,
        port,
    })
}

/// The host and port a URL points at, with an IPv6 literal's brackets
/// stripped back off — [`DaemonSpec`] holds a bind address, and puts the
/// brackets back itself when it needs a URL authority again.
///
/// Hand-parsed because the CLI carries no URL crate, and this is the whole of
/// what it would be used for.
fn authority_of(url: &str) -> Option<(String, u16)> {
    let scheme_relative = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = scheme_relative.split(['/', '?', '#']).next()?;
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        (host, tail.strip_prefix(':'))
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        (host, Some(port))
    } else {
        (authority, None)
    };
    if host.is_empty() {
        return None;
    }
    let port = match port {
        Some(port) => port.parse().ok()?,
        // A gateway URL with no port is not a shape mcpgw writes, but it is
        // one someone can type, and the scheme's default is what their client
        // would dial.
        None if url.starts_with("https://") => 443,
        None => 80,
    };
    Some((host.to_owned(), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gateway_url_yields_the_address_a_service_would_bind() {
        assert_eq!(
            authority_of("http://127.0.0.1:8137/mcp"),
            Some(("127.0.0.1".to_owned(), 8137))
        );
        // The brackets belong to the URL, not to the bind address.
        assert_eq!(
            authority_of("http://[::1]:8137/mcp"),
            Some(("::1".to_owned(), 8137))
        );
        assert_eq!(
            authority_of("https://localhost/mcp"),
            Some(("localhost".to_owned(), 443))
        );
        assert_eq!(authority_of("http://:8137/mcp"), None);
        assert_eq!(authority_of("http://127.0.0.1:port/mcp"), None);
    }
}
