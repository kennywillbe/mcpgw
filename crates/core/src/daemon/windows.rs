//! Windows service, through the service control manager.
//!
//! The Windows half of [`super::ServiceManager`] is shaped by two facts the
//! other two platforms do not have.
//!
//! **A service is not an ordinary process.** The SCM launches the registered
//! binary and then waits for it to hand back a control dispatcher; a process
//! that just starts working fails the start with error 1053. `mcpgw serve`
//! is an ordinary process and must stay one, so the registered command is
//! not `mcpgw serve` at all: it is the hidden [`RUN_SERVICE_COMMAND`], whose
//! whole job is to be a proper service and to run `mcpgw serve` as its child
//! with stdout and stderr redirected into [`super::LogPaths`]. Supervising a
//! child rather than serving in-process is what keeps `serve` unaware that
//! any of this exists — and it is also what makes the SCM's restart actions
//! work, because a gateway that dies takes the service's exit code with it.
//!
//! **Registering a service needs administrator rights.** Every operation
//! here therefore has two paths: do the work, or explain in advance why
//! Windows is about to ask, relaunch the same command elevated, and wait for
//! it. A user who declines is told that nothing happened and what the two
//! ways forward are. Nothing here fails silently, and nothing here prompts
//! before it has said why.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
    ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus as SystemStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager as Scm, ServiceManagerAccess};
use windows_service::{Error as ServiceApiError, service_dispatcher};

use super::{DaemonError, DaemonSpec, Installed, ServiceManager, ServiceStatus};

/// Service name, fixed here so uninstall can find what an older mcpgw
/// registered.
pub const SERVICE_NAME: &str = "mcpgw";

/// What the Services console and `sc query` show next to the name.
pub const DISPLAY_NAME: &str = "mcpgw gateway";

/// The hidden `mcpgw daemon` subcommand the SCM starts. Named here rather
/// than in the CLI so the registration and the entry point cannot drift.
pub const RUN_SERVICE_COMMAND: &str = "run-service";

/// The hidden `mcpgw daemon` subcommand the elevated relaunch of `install`
/// runs. It carries the whole spec — see [`spec_flags`].
pub const INSTALL_ELEVATED_COMMAND: &str = "install-elevated";

/// Where the registration lives. Windows has no unit file, and this key is
/// the closest thing a user can go and look at.
const REGISTRY_KEY: &str = r"HKLM\SYSTEM\CurrentControlSet\Services\mcpgw";

/// Reported in [`DaemonError::Service`], and identical to
/// [`WindowsService::name`] so a message reads the same either way.
const MANAGER: &str = "the Windows service manager";

const DESCRIPTION: &str = "Serves the MCP servers in your mcpgw config on one local endpoint, so \
                           clients have something to talk to before you open a terminal.";

/// Windows services are `SERVICE_WIN32_OWN_PROCESS`: one service, one
/// process, which is what a gateway is.
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

/// Restart delays, first failure to third. Short, because the gateway takes
/// milliseconds to come up and a user waiting on a tool call is not helped
/// by a polite pause; increasing, so a gateway failing on a taken port does
/// not spin.
const RESTART_DELAYS: [Duration; 3] = [
    Duration::from_secs(2),
    Duration::from_secs(10),
    Duration::from_secs(30),
];

/// How long a run has to last before the failure count starts over. An hour:
/// long enough that three quick failures are recognised as one broken
/// install rather than as three unrelated ones.
// Seconds rather than the `from_hours` clippy asks for, for the reason
// already written out over `INTERVAL` in the CLI's update notice: that
// constructor is far newer than anything else this workspace needs, and it
// would turn an otherwise fine toolchain into a bare "no function in
// `Duration`".
#[allow(clippy::duration_suboptimal_units)]
const FAILURE_RESET: Duration = Duration::from_secs(60 * 60);

/// How often the service checks on its child, and the granularity of its
/// response to a stop.
const SUPERVISE_POLL: Duration = Duration::from_millis(250);

/// Promised to the SCM when a stop begins, so it waits rather than reporting
/// the service hung.
const STOP_HINT: Duration = Duration::from_secs(10);

/// `ERROR_SERVICE_DOES_NOT_EXIST`.
const NO_SUCH_SERVICE: i32 = 1060;

/// `ERROR_SERVICE_EXISTS`.
const SERVICE_EXISTS: i32 = 1073;

/// `ERROR_SERVICE_ALREADY_RUNNING`.
const ALREADY_RUNNING: i32 = 1056;

/// The Windows service control manager.
#[derive(Debug, Clone, Copy)]
pub struct WindowsService {
    /// Whether this process may register and control services. A field so
    /// tests can take the elevation branch without being elevated.
    permitted: fn() -> bool,
    /// How the elevated half is run. A field for the same reason: no test
    /// may open a UAC dialog.
    elevate: fn(&Path, &[OsString]) -> Result<Elevation, DaemonError>,
}

impl Default for WindowsService {
    fn default() -> Self {
        Self {
            permitted: may_manage_services,
            elevate: relaunch_elevated,
        }
    }
}

impl WindowsService {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Explains why Windows is about to ask, runs `args` elevated, waits.
    fn with_rights(&self, op: Op, exe: &Path, args: &[OsString]) -> Result<(), DaemonError> {
        // Printed from here, not from the CLI, because this is the only
        // place that knows a UAC dialog is a moment away — and a reason that
        // arrives after the dialog is a reason nobody read.
        eprintln!("{}", op.explanation());
        match (self.elevate)(exe, args)? {
            Elevation::Completed => Ok(()),
            Elevation::Declined => Err(refusal(op.declined())),
            Elevation::Failed(code) => Err(refusal(op.failed(code))),
        }
    }

    /// The elevated relaunch of a plain `mcpgw daemon <op>`, which re-enters
    /// this file with the rights it needs and takes the other branch.
    fn again_elevated(&self, op: Op, extra: &[&str]) -> Result<(), DaemonError> {
        let exe = std::env::current_exe().map_err(|source| DaemonError::Io {
            action: "locate the running binary of",
            path: PathBuf::from("mcpgw"),
            source,
        })?;
        let mut args = vec![OsString::from("daemon"), OsString::from(op.subcommand())];
        args.extend(extra.iter().map(OsString::from));
        self.with_rights(op, &exe, &args)
    }
}

impl ServiceManager for WindowsService {
    fn name(&self) -> &'static str {
        MANAGER
    }

    fn install(&self, spec: &DaemonSpec) -> Result<Installed, DaemonError> {
        if (self.permitted)() {
            return install_here(spec);
        }
        // The elevated half is handed the whole spec rather than left to
        // work one out again. Over-the-shoulder elevation runs as a
        // *different* user, and that user's profile resolves a different
        // config file and a different log directory from the one the person
        // at the keyboard is looking at.
        let mut args = vec![
            OsString::from("daemon"),
            OsString::from(INSTALL_ELEVATED_COMMAND),
        ];
        args.extend(spec_flags(spec));
        self.with_rights(Op::Install, &spec.exe, &args)?;
        // That process's console is gone, so what actually happened is read
        // back from the service database rather than assumed.
        let outcome = match self.query() {
            Ok(status) if status.running => "it is running now, and starts again at every boot",
            Ok(_) => {
                "it is registered but not running — `mcpgw daemon logs`, then `mcpgw daemon start`"
            }
            Err(_) => "`mcpgw daemon status` will say whether it is running",
        };
        Ok(report(spec, outcome.to_owned()))
    }

    fn uninstall(&self) -> Result<(), DaemonError> {
        if (self.permitted)() {
            return uninstall_here();
        }
        self.again_elevated(Op::Uninstall, &[])
    }

    fn start(&self, spec: &DaemonSpec) -> Result<(), DaemonError> {
        if (self.permitted)() {
            return start_here();
        }
        // `start` takes an address so it can be aimed at a service installed
        // on another port; passing it through keeps the elevated half aimed
        // at the same one.
        let port = spec.port.to_string();
        self.again_elevated(Op::Start, &["--bind", &spec.bind, "--port", &port])
    }

    fn stop(&self) -> Result<(), DaemonError> {
        if (self.permitted)() {
            return stop_here();
        }
        self.again_elevated(Op::Stop, &[])
    }

    fn query(&self) -> Result<ServiceStatus, DaemonError> {
        // Querying needs no right beyond `CONNECT`, which is why `status`
        // never prompts for anything.
        let scm = connect(ServiceManagerAccess::CONNECT)?;
        let access = ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG;
        let service = match scm.open_service(SERVICE_NAME, access) {
            Ok(service) => service,
            Err(err) if is_code(&err, NO_SUCH_SERVICE) => return Ok(ServiceStatus::default()),
            Err(err) => return Err(service_error(&err)),
        };
        let status = service.query_status().map_err(|err| service_error(&err))?;
        let running = status.current_state == ServiceState::Running;
        // The one line worth showing: a service that stopped by itself is
        // the case a user is trying to understand.
        let detail = match (running, failure_code(status.exit_code)) {
            (false, Some(code)) => Some(format!(
                "it stopped with exit code {code} — `mcpgw daemon logs` has the reason"
            )),
            _ => None,
        };
        Ok(ServiceStatus {
            installed: true,
            running,
            unit_path: Some(PathBuf::from(REGISTRY_KEY)),
            detail,
        })
    }
}

/// Registers (or re-registers) the service, assuming the caller may.
///
/// Public because the elevated half of `install` is a separate process that
/// re-enters here directly: it has already been told it is allowed, and
/// asking a second time would only give it a second chance to be wrong.
///
/// # Errors
///
/// [`DaemonError::Service`] when the service control manager refuses.
pub fn install_here(spec: &DaemonSpec) -> Result<Installed, DaemonError> {
    let scm = connect(ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?;
    let info = service_info(spec);
    let access = ServiceAccess::QUERY_CONFIG
        | ServiceAccess::CHANGE_CONFIG
        | ServiceAccess::QUERY_STATUS
        | ServiceAccess::START
        | ServiceAccess::STOP;
    let service = match scm.create_service(&info, access) {
        Ok(service) => service,
        // Re-running `install` after changing `--port` is a normal thing to
        // do, and `create_service` refuses a name that is taken — so an
        // existing registration is rewritten rather than reported as a
        // conflict the user has to go and resolve by hand.
        Err(err) if is_code(&err, SERVICE_EXISTS) => {
            let service = scm
                .open_service(SERVICE_NAME, access)
                .map_err(|err| service_error(&err))?;
            service
                .change_config(&info)
                .map_err(|err| service_error(&err))?;
            service
        }
        Err(err) => return Err(service_error(&err)),
    };

    service
        .set_description(DESCRIPTION)
        .map_err(|err| service_error(&err))?;
    service
        .update_failure_actions(failure_actions())
        .map_err(|err| service_error(&err))?;
    // Without this flag the recovery actions only fire when the process
    // *crashes*. A gateway exiting with a non-zero code is exactly the case
    // they exist for, and it is not a crash.
    service
        .set_failure_actions_on_non_crash_failures(true)
        .map_err(|err| service_error(&err))?;

    // Started here rather than left to the next boot: someone who typed
    // `install` wants a gateway, and the CLI's own next line says where it
    // will answer. A start that fails is a note, not a failed install — the
    // registration really did happen, and `uninstall` is how it goes away.
    let outcome = match service.start(NO_ARGUMENTS) {
        Ok(()) => {
            "it is running now, and starts again at every boot — before anyone logs in".to_owned()
        }
        Err(err) if is_code(&err, ALREADY_RUNNING) => {
            "it was already running — `mcpgw daemon stop` then `start` to pick up these settings"
                .to_owned()
        }
        Err(err) => format!(
            "it is registered but would not start ({err}) — `mcpgw daemon logs`, then \
             `mcpgw daemon start`"
        ),
    };
    Ok(report(spec, outcome))
}

/// Stops the service if it runs, then deletes the registration.
fn uninstall_here() -> Result<(), DaemonError> {
    let scm = connect(ServiceManagerAccess::CONNECT)?;
    let access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
    let service = match scm.open_service(SERVICE_NAME, access) {
        Ok(service) => service,
        // Removing what is not there is the end state that was asked for.
        Err(err) if is_code(&err, NO_SUCH_SERVICE) => return Ok(()),
        Err(err) => return Err(service_error(&err)),
    };
    if current_state(&service)? != ServiceState::Stopped {
        // A stop that fails does not block the removal: a service marked for
        // deletion is gone after the next boot regardless, and failing here
        // would leave the user believing it is still installed.
        let _ = service.stop();
    }
    service.delete().map_err(|err| service_error(&err))
}

fn start_here() -> Result<(), DaemonError> {
    let service = open_for(ServiceAccess::START | ServiceAccess::QUERY_STATUS)?;
    if current_state(&service)? == ServiceState::Running {
        return Ok(());
    }
    service
        .start(NO_ARGUMENTS)
        .map_err(|err| service_error(&err))
}

fn stop_here() -> Result<(), DaemonError> {
    let service = open_for(ServiceAccess::STOP | ServiceAccess::QUERY_STATUS)?;
    if current_state(&service)? == ServiceState::Stopped {
        return Ok(());
    }
    service
        .stop()
        .map(|_| ())
        .map_err(|err| service_error(&err))
}

fn current_state(service: &windows_service::service::Service) -> Result<ServiceState, DaemonError> {
    service
        .query_status()
        .map(|status| status.current_state)
        .map_err(|err| service_error(&err))
}

/// Opens the installed service, turning "it does not exist" into the
/// sentence that names the command which would create it.
fn open_for(access: ServiceAccess) -> Result<windows_service::service::Service, DaemonError> {
    let scm = connect(ServiceManagerAccess::CONNECT)?;
    scm.open_service(SERVICE_NAME, access).map_err(|err| {
        if is_code(&err, NO_SUCH_SERVICE) {
            refusal(
                "no mcpgw service is installed — `mcpgw daemon install` registers one, and \
                 `mcpgw serve` runs a gateway in this terminal without one"
                    .to_owned(),
            )
        } else {
            service_error(&err)
        }
    })
}

/// `Service::start` wants a slice of arguments; ours are baked into the
/// registration instead, so the SCM passes them on every start — including
/// the ones it makes by itself at boot and after a failure.
const NO_ARGUMENTS: &[&OsStr] = &[];

fn connect(access: ServiceManagerAccess) -> Result<Scm, DaemonError> {
    Scm::local_computer(None::<&str>, access).map_err(|err| service_error(&err))
}

/// The registration, built in one place so `create_service` and
/// `change_config` cannot disagree about what an installed mcpgw looks like.
fn service_info(spec: &DaemonSpec) -> ServiceInfo {
    let mut arguments = vec![
        OsString::from("daemon"),
        OsString::from(RUN_SERVICE_COMMAND),
    ];
    arguments.extend(spec_flags(spec));
    ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(DISPLAY_NAME),
        service_type: SERVICE_TYPE,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: spec.exe.clone(),
        launch_arguments: arguments,
        dependencies: Vec::new(),
        // LocalSystem. Running as the installing user would mean holding
        // their password, which nothing here is going to ask for — so the
        // gateway is pointed at that user's config and state explicitly (see
        // `spawn_gateway`), and the consequence is written into the install
        // notes rather than left to be discovered.
        account_name: None,
        account_password: None,
    }
}

/// Restart, restart, restart. The third entry is what the SCM repeats for
/// every failure after the second.
fn failure_actions() -> ServiceFailureActions {
    ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(FAILURE_RESET),
        reboot_msg: None,
        command: None,
        actions: Some(
            RESTART_DELAYS
                .iter()
                .map(|delay| ServiceAction {
                    action_type: ServiceActionType::Restart,
                    delay: *delay,
                })
                .collect(),
        ),
    }
}

/// What `install` reports back, with the thing about a Windows service that
/// a user has to know before their first surprise.
fn report(spec: &DaemonSpec, outcome: String) -> Installed {
    Installed {
        unit_path: PathBuf::from(REGISTRY_KEY),
        notes: vec![
            outcome,
            format!(
                "it runs as LocalSystem, so it was pointed at your config ({}) and your logs \
                 explicitly — but the MCP servers it launches run as SYSTEM too, and one that \
                 needs something only your account has (a PATH entry, a credential in your \
                 profile) will not find it",
                spec.config_path.display()
            ),
            "if it dies the service manager restarts it, three times in an hour before it gives \
             up"
            .to_owned(),
        ],
    }
}

/// The whole [`DaemonSpec`] as command-line flags.
///
/// Both hidden entry points take these, so the service the SCM starts and
/// the install an elevated process performs use the spec that was computed
/// once, in the user's own session, rather than one re-derived from an
/// environment neither of those processes shares.
#[must_use]
pub fn spec_flags(spec: &DaemonSpec) -> Vec<OsString> {
    let mut flags = vec![
        OsString::from("--bind"),
        OsString::from(&spec.bind),
        OsString::from("--port"),
        OsString::from(spec.port.to_string()),
    ];
    for (flag, path) in [
        ("--config", &spec.config_path),
        ("--state-dir", &spec.state_dir),
        ("--stdout", &spec.logs.stdout),
        ("--stderr", &spec.logs.stderr),
    ] {
        flags.push(OsString::from(flag));
        flags.push(path.clone().into_os_string());
    }
    flags
}

// --------------------------------------------------------------------------
// Being the service
// --------------------------------------------------------------------------

windows_service::define_windows_service!(ffi_service_main, service_main);

/// The spec [`service_main`] picks up. A `static` because the SCM calls that
/// function on a thread of its own with nothing but the service name in
/// hand, so there is nowhere else to hand it anything.
static SPEC: OnceLock<DaemonSpec> = OnceLock::new();

/// Runs this process *as* the service: hands the SCM a control dispatcher
/// and blocks until the service is stopped.
///
/// Reached only through the hidden [`RUN_SERVICE_COMMAND`], which is only
/// ever spelled by [`service_info`]. Run from a terminal it fails with the
/// SCM's "cannot connect to the service controller", which is the truth.
///
/// # Errors
///
/// [`DaemonError::Service`] when the SCM will not accept the dispatcher.
pub fn run_service(spec: &DaemonSpec) -> Result<(), DaemonError> {
    // `set` fails only if this ran twice in one process, and the second
    // caller would be handing over the same spec anyway.
    let _ = SPEC.set(spec.clone());
    service_dispatcher::start(SERVICE_NAME, ffi_service_main).map_err(|err| service_error(&err))
}

fn service_main(_arguments: Vec<OsString>) {
    let Some(spec) = SPEC.get() else {
        return;
    };
    if let Err(err) = supervise(spec) {
        // The last place left to say anything: there is no console, and the
        // SCM takes a number rather than a sentence.
        append_line(&spec.logs.stderr, &format!("mcpgw service: {err}"));
    }
}

/// The service's whole life: report running, run the gateway, report how it
/// ended.
fn supervise(spec: &DaemonSpec) -> Result<(), DaemonError> {
    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    let handler = move |control| match control {
        // Interrogate has to be answered even though there is nothing to do:
        // a service that returns "not implemented" here is reported as
        // unresponsive.
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        ServiceControl::Stop | ServiceControl::Shutdown => {
            // A closed channel is a stop too, so the send is best-effort.
            let _ = stop_tx.send(());
            ServiceControlHandlerResult::NoError
        }
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let handle = service_control_handler::register(SERVICE_NAME, handler)
        .map_err(|err| service_error(&err))?;

    handle
        .set_service_status(status(
            ServiceState::Running,
            ServiceExitCode::Win32(0),
            Duration::default(),
        ))
        .map_err(|err| service_error(&err))?;

    let ended = run_gateway(spec, &stop_rx, |state| {
        let _ = handle.set_service_status(status(state, ServiceExitCode::Win32(0), STOP_HINT));
    });
    let exit = match ended {
        Ok(exit) => exit,
        Err(err) => {
            append_line(&spec.logs.stderr, &format!("mcpgw service: {err}"));
            ServiceExitCode::Win32(1)
        }
    };
    handle
        .set_service_status(status(ServiceState::Stopped, exit, Duration::default()))
        .map_err(|err| service_error(&err))
}

/// Runs `mcpgw serve` as a child and watches both it and the stop channel.
///
/// The exit code returned is the one the SCM sees, and the one its restart
/// actions key off.
fn run_gateway(
    spec: &DaemonSpec,
    stop: &Receiver<()>,
    report_state: impl Fn(ServiceState),
) -> Result<ServiceExitCode, DaemonError> {
    let mut child = spawn_gateway(spec)?;
    loop {
        if let Some(exit) = child.try_wait().map_err(|source| DaemonError::Io {
            action: "wait for",
            path: spec.exe.clone(),
            source,
        })? {
            // A gateway that ended by itself has failed whatever it says on
            // the way out — the service exists to keep it up, and only a
            // non-zero code makes the SCM restart it.
            let code = exit.code().and_then(|code| u32::try_from(code).ok());
            return Ok(ServiceExitCode::Win32(match code {
                Some(0) | None => 1,
                Some(code) => code,
            }));
        }
        match stop.recv_timeout(SUPERVISE_POLL) {
            // Disconnected means the handler is gone, which is not a state
            // this process can go on serving from either.
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                report_state(ServiceState::StopPending);
                // The gateway holds nothing a signal would let it flush —
                // the traffic log is appended line by line — and Windows has
                // no way to ask a console-less child to stop politely.
                let _ = child.kill();
                let _ = child.wait();
                return Ok(ServiceExitCode::Win32(0));
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

fn spawn_gateway(spec: &DaemonSpec) -> Result<Child, DaemonError> {
    Command::new(&spec.exe)
        .args(spec.serve_args())
        // A service inherits no user profile: under LocalSystem the two
        // paths the gateway resolves from `%USERPROFILE%` land in system32's
        // profile directory rather than in the config the person who
        // installed it edits. So they are handed over rather than guessed.
        .env(crate::paths::CONFIG_ENV, &spec.config_path)
        .env(crate::paths::STATE_ENV, &spec.state_dir)
        // A service has no console to inherit, which is the whole reason
        // these three are set: without them the gateway's output goes
        // nowhere and `mcpgw daemon logs` has nothing to show.
        .stdin(Stdio::null())
        .stdout(log_sink(&spec.logs.stdout)?)
        .stderr(log_sink(&spec.logs.stderr)?)
        .spawn()
        .map_err(|source| DaemonError::Io {
            action: "start",
            path: spec.exe.clone(),
            source,
        })
}

/// One of the log files, opened for append. `prepare_logs` has already
/// created it with the right permissions (see the ordering contract in
/// [`super`]), so this only has to not lose what is already there.
fn log_sink(path: &Path) -> Result<Stdio, DaemonError> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| DaemonError::Io {
            action: "open",
            path: path.to_owned(),
            source,
        })?;
    Ok(Stdio::from(file))
}

/// Appends one line, best effort. Used only where the alternative is
/// silence.
fn append_line(path: &Path, line: &str) {
    use std::io::Write as _;

    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

fn status(state: ServiceState, exit_code: ServiceExitCode, wait_hint: Duration) -> SystemStatus {
    SystemStatus {
        service_type: SERVICE_TYPE,
        current_state: state,
        controls_accepted: match state {
            // Only a running service can be asked to stop; advertising it
            // while stopping or stopped invites a control the SCM then
            // reports as failed.
            ServiceState::Running => ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            _ => ServiceControlAccept::empty(),
        },
        exit_code,
        checkpoint: 0,
        wait_hint,
        process_id: None,
    }
}

// --------------------------------------------------------------------------
// Elevation
// --------------------------------------------------------------------------

/// How an elevated relaunch ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Elevation {
    /// It ran and exited zero.
    Completed,
    /// The user said no to the UAC prompt.
    Declined,
    /// It ran and exited non-zero.
    Failed(u32),
}

/// The four operations that need administrator rights, and the sentences
/// each of them owes the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Install,
    Uninstall,
    Start,
    Stop,
}

impl Op {
    /// The visible `mcpgw daemon` subcommand, which is also what the
    /// elevated relaunch runs for everything but `install`.
    fn subcommand(self) -> &'static str {
        match self {
            Op::Install => "install",
            Op::Uninstall => "uninstall",
            Op::Start => "start",
            Op::Stop => "stop",
        }
    }

    fn command(self) -> String {
        format!("mcpgw daemon {}", self.subcommand())
    }

    /// Completes "Windows needs administrator rights to …".
    fn need(self) -> &'static str {
        match self {
            Op::Install => "install a service",
            Op::Uninstall => "remove a service",
            Op::Start => "start a service",
            Op::Stop => "stop a service",
        }
    }

    /// Completes "…, so …".
    fn nothing(self) -> &'static str {
        match self {
            Op::Install => "nothing was installed and nothing was changed",
            Op::Uninstall => "nothing was removed and nothing was changed",
            Op::Start => "the service was not started and nothing was changed",
            Op::Stop => "the service was not stopped and nothing was changed",
        }
    }

    /// Said *before* the UAC dialog appears, which is the whole point of it.
    fn explanation(self) -> String {
        format!(
            "Windows needs administrator rights to {}. It is about to ask you to approve one \
             elevated `{}`, which does that and nothing else. If you say no, nothing changes.",
            self.need(),
            self.command()
        )
    }

    fn declined(self) -> String {
        format!(
            "Windows needs administrator rights to {}. You said no, so {}. Two ways forward: \
             open a terminal as administrator and run `{}` again, or skip the service and run \
             `mcpgw serve` in a terminal — same gateway, it just stops when the terminal does.",
            self.need(),
            self.nothing(),
            self.command()
        )
    }

    fn failed(self, code: u32) -> String {
        format!(
            "the elevated `{}` exited with code {code}, so {}. Its window closes with it and \
             takes the reason along, so run `{}` from a terminal you opened as administrator to \
             see what it says.",
            self.command(),
            self.nothing(),
            self.command()
        )
    }
}

/// Whether this process may register and control services.
///
/// Asked by opening the service database with the right that `install`
/// needs, rather than by reading the token's elevation flag: the question is
/// "may I create a service", and the two answers part company for the
/// built-in Administrator account and for anyone whose rights arrive through
/// policy rather than through UAC.
fn may_manage_services() -> bool {
    Scm::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .is_ok()
}

/// Runs `exe args` elevated through the shell's `runas` verb, and waits.
///
/// `ShellExecuteExW` rather than `CreateProcess`: raising its own token is
/// not something a process may do, and `runas` is the documented way to ask
/// the user to do it instead.
fn relaunch_elevated(exe: &Path, args: &[OsString]) -> Result<Elevation, DaemonError> {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_CANCELLED};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, INFINITE, WaitForSingleObject,
    };
    use windows_sys::Win32::UI::Shell::{
        SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
    };

    /// `SW_SHOWNORMAL`. Written out rather than pulled in from
    /// `Win32_UI_WindowsAndMessaging`, which is a whole feature for one
    /// integer that has not moved since Windows 3.
    const SW_SHOWNORMAL: i32 = 1;

    let verb = wide(OsStr::new("runas"));
    let file = wide(exe.as_os_str());
    let parameters = command_line(args);

    // Zeroed rather than written out field by field: the struct has a dozen
    // members this call has no opinion about, and for every one of them zero
    // is the default.
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = u32::try_from(std::mem::size_of::<SHELLEXECUTEINFOW>()).unwrap_or(0);
    // NOCLOSEPROCESS to get a handle back to wait on; NOASYNC because this
    // thread is about to block, and the shell has to be finished with the
    // buffers below before it returns.
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = parameters.as_ptr();
    info.nShow = SW_SHOWNORMAL;

    // SAFETY: every buffer pointed into outlives the call, and `info` is a
    // correctly sized, zero-initialised `SHELLEXECUTEINFOW`.
    let started = unsafe { ShellExecuteExW(&raw mut info) };
    if started == 0 {
        let err = std::io::Error::last_os_error();
        // The one failure that is not one: this is a user saying no.
        if err.raw_os_error() == Some(ERROR_CANCELLED.cast_signed()) {
            return Ok(Elevation::Declined);
        }
        return Err(refusal(format!(
            "Windows would not ask for administrator rights: {err}"
        )));
    }
    if info.hProcess.is_null() {
        // Documented as possible even on success. There is nothing to wait
        // on, so the caller reads the result back out of the SCM instead.
        return Ok(Elevation::Completed);
    }

    // SAFETY: `hProcess` is a live process handle owned by this thread until
    // the `CloseHandle` below, which is the last use of it.
    let code = unsafe {
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code: u32 = 0;
        let queried = GetExitCodeProcess(info.hProcess, &raw mut code);
        CloseHandle(info.hProcess);
        (queried != 0).then_some(code)
    };
    match code {
        Some(0) => Ok(Elevation::Completed),
        Some(code) => Ok(Elevation::Failed(code)),
        None => Err(refusal(
            "the elevated mcpgw ran, but Windows would not say how it ended — \
             `mcpgw daemon status` has the truth"
                .to_owned(),
        )),
    }
}

/// A nul-terminated UTF-16 copy, as every `W` entry point wants.
fn wide(text: &OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt as _;

    text.encode_wide().chain(std::iter::once(0)).collect()
}

/// The arguments as one nul-terminated command line.
///
/// `ShellExecuteExW` takes a string, not a vector, so the quoting
/// `CommandLineToArgvW` will undo at the other end has to be applied here —
/// a path under `C:\Program Files\…` is the everyday case that needs it.
fn command_line(args: &[OsString]) -> Vec<u16> {
    let mut line: Vec<u16> = Vec::new();
    for arg in args {
        if !line.is_empty() {
            line.push(u16::from(b' '));
        }
        line.extend(quote(arg));
    }
    line.push(0);
    line
}

/// One argument, quoted the way `CommandLineToArgvW` parses.
///
/// Always quoted rather than only when it contains a space: the rules for an
/// unquoted argument are a second set of rules, and having one set is how
/// this stays correct.
fn quote(arg: &OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt as _;

    const QUOTE: u16 = b'"' as u16;
    const BACKSLASH: u16 = b'\\' as u16;

    let mut out = vec![QUOTE];
    let mut backslashes = 0usize;
    for unit in arg.encode_wide() {
        match unit {
            BACKSLASH => {
                backslashes += 1;
                out.push(unit);
            }
            // A quote is escaped, and so is every backslash in front of it —
            // otherwise `\"` would close the argument instead of standing
            // for a quote inside it.
            QUOTE => {
                out.extend(std::iter::repeat_n(BACKSLASH, backslashes + 1));
                backslashes = 0;
                out.push(QUOTE);
            }
            _ => {
                backslashes = 0;
                out.push(unit);
            }
        }
    }
    // Trailing backslashes are doubled for the same reason: the closing
    // quote must not be escaped by the path's own separator.
    out.extend(std::iter::repeat_n(BACKSLASH, backslashes));
    out.push(QUOTE);
    out
}

// --------------------------------------------------------------------------
// Errors
// --------------------------------------------------------------------------

fn refusal(message: String) -> DaemonError {
    DaemonError::Service {
        manager: MANAGER,
        message,
    }
}

fn service_error(err: &ServiceApiError) -> DaemonError {
    refusal(err.to_string())
}

/// Whether a service control manager call failed with a given Win32 code.
fn is_code(err: &ServiceApiError, code: i32) -> bool {
    matches!(err, ServiceApiError::Winapi(io) if io.raw_os_error() == Some(code))
}

/// The non-zero exit a stopped service reported, if it reported one.
fn failure_code(exit: ServiceExitCode) -> Option<u32> {
    match exit {
        ServiceExitCode::Win32(0) => None,
        ServiceExitCode::Win32(code) | ServiceExitCode::ServiceSpecific(code) => Some(code),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::daemon::LogPaths;

    fn spec() -> DaemonSpec {
        let state = PathBuf::from(r"C:\Users\Ada Lovelace\.local\share\mcpgw");
        DaemonSpec {
            exe: PathBuf::from(r"C:\Program Files\mcpgw\mcpgw.exe"),
            config_path: PathBuf::from(r"C:\Users\Ada Lovelace\.config\mcpgw\config.toml"),
            logs: LogPaths::under_state_dir(&state),
            state_dir: state,
            bind: "127.0.0.1".to_owned(),
            port: 8137,
        }
    }

    fn text(units: &[u16]) -> String {
        String::from_utf16_lossy(units.strip_suffix(&[0]).unwrap_or(units))
    }

    /// A service with its two dangerous edges stubbed: it may never touch
    /// the service database, and it never opens a dialog.
    fn unelevated(
        outcome: fn(&Path, &[OsString]) -> Result<Elevation, DaemonError>,
    ) -> WindowsService {
        WindowsService {
            permitted: || false,
            elevate: outcome,
        }
    }

    #[test]
    fn the_registration_runs_the_hidden_entry_point_with_the_whole_spec() {
        let info = service_info(&spec());
        assert_eq!(info.name, OsString::from("mcpgw"));
        assert_eq!(info.display_name, OsString::from("mcpgw gateway"));
        assert_eq!(info.start_type, ServiceStartType::AutoStart);
        assert_eq!(info.service_type, SERVICE_TYPE);
        // LocalSystem, which is what makes the explicit config and state
        // paths below load-bearing rather than decorative.
        assert!(info.account_name.is_none());

        let arguments: Vec<String> = info
            .launch_arguments
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(arguments[0], "daemon");
        assert_eq!(arguments[1], RUN_SERVICE_COMMAND);
        // `serve` is never named in the registration: the SCM starts a
        // service, and the service starts serve.
        assert!(!arguments.contains(&"serve".to_owned()));
        for flag in [
            "--bind",
            "--port",
            "--config",
            "--state-dir",
            "--stdout",
            "--stderr",
        ] {
            assert!(arguments.contains(&flag.to_owned()), "{flag}");
        }
        assert!(arguments.contains(&r"C:\Users\Ada Lovelace\.config\mcpgw\config.toml".to_owned()));
    }

    #[test]
    fn every_failure_restarts_the_service_after_a_short_wait() {
        let actions = failure_actions().actions.unwrap();
        assert_eq!(actions.len(), 3);
        for action in &actions {
            assert_eq!(action.action_type, ServiceActionType::Restart);
            assert!(action.delay <= Duration::from_secs(30));
        }
        assert!(actions[0].delay < actions[2].delay);
    }

    #[test]
    fn declining_the_prompt_changes_nothing_and_says_so() {
        let service = unelevated(|_, _| Ok(Elevation::Declined));
        let message = service.install(&spec()).unwrap_err().to_string();
        // The decided sentence, verbatim.
        assert!(
            message.contains(
                "Windows needs administrator rights to install a service. You said no, so \
                 nothing was installed and nothing was changed."
            ),
            "{message}"
        );
        // ...and both ways forward, because a refusal without one is a dead end.
        assert!(
            message.contains("open a terminal as administrator"),
            "{message}"
        );
        assert!(message.contains("mcpgw serve"), "{message}");
    }

    #[test]
    fn each_operation_names_itself_in_its_refusal() {
        for (op, needle) in [
            (Op::Uninstall, "nothing was removed"),
            (Op::Start, "the service was not started"),
            (Op::Stop, "the service was not stopped"),
        ] {
            let declined = op.declined();
            assert!(declined.contains(needle), "{declined}");
            assert!(declined.contains(&op.command()), "{declined}");
        }
    }

    #[test]
    fn the_reason_is_given_before_the_prompt_rather_than_after() {
        let explanation = Op::Install.explanation();
        assert!(explanation.contains("Windows needs administrator rights to install a service"));
        assert!(explanation.contains("about to ask"));
        assert!(explanation.contains("If you say no, nothing changes"));
    }

    #[test]
    fn an_elevated_half_that_fails_points_at_the_terminal_that_would_show_why() {
        let service = unelevated(|_, _| Ok(Elevation::Failed(2)));
        let message = service.uninstall().unwrap_err().to_string();
        assert!(message.contains("exited with code 2"), "{message}");
        assert!(message.contains("as administrator"), "{message}");
    }

    #[test]
    fn the_elevated_relaunch_carries_the_spec_it_was_given() {
        // Captured through a `static` because the injected elevator is a
        // plain fn pointer and has nothing else to write to.
        static SEEN: OnceLock<(PathBuf, Vec<OsString>)> = OnceLock::new();
        let service = unelevated(|exe, args| {
            let _ = SEEN.set((exe.to_owned(), args.to_vec()));
            Ok(Elevation::Declined)
        });
        let _ = service.install(&spec());

        let (exe, args) = SEEN.get().expect("the elevator ran");
        assert_eq!(exe, &spec().exe);
        assert_eq!(args[0], OsString::from("daemon"));
        assert_eq!(args[1], OsString::from(INSTALL_ELEVATED_COMMAND));
        assert!(args.contains(&OsString::from("8137")));
        assert!(args.contains(&spec().logs.stderr.into_os_string()));
    }

    #[test]
    fn a_path_with_spaces_survives_the_command_line_it_is_pasted_into() {
        let line = text(&command_line(&[
            OsString::from("daemon"),
            OsString::from(r"C:\Program Files\mcpgw\config.toml"),
        ]));
        assert_eq!(line, r#""daemon" "C:\Program Files\mcpgw\config.toml""#);
    }

    #[test]
    fn a_trailing_backslash_does_not_escape_the_closing_quote() {
        assert_eq!(text(&quote(OsStr::new(r"C:\dir\"))), r#""C:\dir\\""#);
        assert_eq!(text(&quote(OsStr::new(r#"a"b"#))), r#""a\"b""#);
        assert_eq!(text(&quote(OsStr::new(r#"a\"b"#))), r#""a\\\"b""#);
    }

    #[test]
    fn a_stopped_service_reports_the_code_it_stopped_with() {
        assert_eq!(failure_code(ServiceExitCode::Win32(0)), None);
        assert_eq!(failure_code(ServiceExitCode::Win32(1)), Some(1));
        assert_eq!(failure_code(ServiceExitCode::ServiceSpecific(7)), Some(7));
    }

    #[test]
    fn the_notes_warn_about_the_account_the_service_runs_as() {
        let notes = report(&spec(), "started".to_owned()).notes.join(" ");
        assert!(notes.contains("LocalSystem"));
        assert!(notes.contains("SYSTEM"));
        assert!(notes.contains(r"C:\Users\Ada Lovelace\.config\mcpgw\config.toml"));
    }
}
