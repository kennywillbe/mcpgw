//! The first-run wizard: `mcpgw init`, and what a bare `mcpgw` on a
//! terminal does.
//!
//! Show and confirm. Every step announces what it found and what it would
//! write *before* it writes anything, and the wizard is over the moment a
//! user says stop. That promise is made in the opening two lines, so it is
//! the one thing a step may not quietly break.
//!
//! # Contract for the step modules
//!
//! **This file is final**, in the same sense [`mcpgw_core::daemon`] is.
//! [`detect`] is implemented; [`import`], [`daemon`] and [`sync`] ship as
//! one-line stubs, and each is filled in by exactly one later change
//! touching exactly one file (W2, W3 and W4 respectively). An implementor
//! writes the body of their own module and nothing else.
//!
//! A step module is a pair of free functions, and nothing more:
//!
//! ```ignore
//! pub fn pending(cx: &Ctx) -> bool;                      // work to do here?
//! pub fn run(cx: &mut Ctx) -> anyhow::Result<Outcome>;   // do it, asking first
//! ```
//!
//! `pending` is the delta-awareness. It is cheap, it never prompts and it
//! never writes, because the driver calls it for *every* step before the
//! first one runs — that is how a re-run on a finished machine prints a
//! status card instead of walking four steps that all have nothing to say.
//! A step whose `pending` is false is not `run`; the driver prints its
//! dimmed already-done line instead.
//!
//! `pending` is asked again immediately before the step runs, because the
//! steps are ordered and the earlier ones change the answer: `sync` has
//! nothing to push until `import` has put something in the config. A step
//! that writes must therefore call [`Ctx::refresh`] before it returns, so
//! the steps after it see the machine as it now is rather than as it was
//! when the wizard started.
//!
//! Everything a step needs to answer either question is already on [`Ctx`].
//! If a step needs a new fact about the machine, it belongs on `Ctx` for all
//! four rather than being read twice in two spellings.
//!
//! Under `--yes` a step must not prompt and must take its recommended
//! answer, and where there is no answer that is safe to assume it must fail
//! loudly with a command the user can run instead — [`Ctx::confirm`] and
//! [`Ctx::choose`] already behave that way, so a step that asks through them
//! gets it for free.

pub mod daemon;
pub mod detect;
pub mod import;
pub mod sync;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use mcpgw_core::daemon::{GatewayReach, PROBE_TIMEOUT};
use mcpgw_core::state::ManagedState;
use mcpgw_core::{ClientKind, Config, Detection, Error};
use owo_colors::OwoColorize as _;

use crate::ui;

#[derive(clap::Args)]
pub struct InitArgs {
    /// Never prompt: take the recommended answer at every step. The full
    /// plan is still printed
    #[arg(long)]
    pub yes: bool,
    /// Gateway URL the wizard expects your clients to reach
    #[arg(long, default_value = mcpgw_core::endpoints::DEFAULT_URL, value_name = "URL")]
    pub gateway_url: String,
}

/// What a step reports back to the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The step did its work, or offered it and the user declined this one.
    Handled,
    /// The user asked to stop the wizard. Nothing further runs.
    Stop,
}

/// Everything the steps read about this machine, gathered once and
/// re-gathered by [`Ctx::refresh`] after a step changes any of it.
pub struct Ctx {
    pub color: bool,
    /// `--yes`: no step may block on stdin.
    pub assume_yes: bool,
    pub config_path: PathBuf,
    pub config: Config,
    pub state: ManagedState,
    pub gateway_url: String,
    pub reach: GatewayReach,
    pub detections: Vec<(ClientKind, Detection)>,
}

impl Ctx {
    /// Asks a question whose recommended answer is yes.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`std::io::Error`] if the terminal cannot be
    /// read.
    pub fn confirm(&self, question: &str) -> anyhow::Result<bool> {
        if self.assume_yes {
            // Echoed rather than skipped: `--yes` still owes the reader a
            // transcript of what it agreed to on their behalf.
            println!("{question} [Y/n] y");
            return Ok(true);
        }
        ui::confirm_default_yes(question)
    }

    /// Offers a numbered choice. `default` is the recommended answer;
    /// `None` means there is no answer that can be taken on someone's
    /// behalf, which under `--yes` is a hard stop rather than a silent skip.
    /// `escape_hatch` is the command the user should run instead.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`std::io::Error`] if the terminal cannot be
    /// read, or an actionable failure when `--yes` meets a `None` default.
    pub fn choose(
        &self,
        prompt: &str,
        options: &[String],
        default: Option<usize>,
        escape_hatch: &str,
    ) -> anyhow::Result<usize> {
        if self.assume_yes {
            let Some(default) = default else {
                anyhow::bail!(
                    "--yes cannot answer this: {prompt}\n\
                     There is no answer that is safe to assume here, so nothing was \
                     written. Run `{escape_hatch}` and decide, or run `mcpgw init` \
                     without --yes."
                );
            };
            println!("{prompt} [{}] {}", default + 1, options[default]);
            return Ok(default);
        }
        ui::choose(prompt, options, default.unwrap_or(0))
    }

    /// Re-reads everything a step may have changed.
    ///
    /// # Errors
    ///
    /// Returns a failure only if the config exists but cannot be read; a
    /// missing config stays the empty config.
    pub fn refresh(&mut self) -> anyhow::Result<()> {
        self.config = load_config(&self.config_path)?;
        self.state = load_state();
        self.reach = probe(&self.gateway_url, PROBE_TIMEOUT);
        self.detections = detect_clients();
        Ok(())
    }

    /// Servers the gateway would publish an endpoint for.
    #[must_use]
    pub fn enabled_servers(&self) -> usize {
        self.config.servers.values().filter(|s| s.enabled).count()
    }

    /// Clients mcpgw has written entries into, by display name.
    #[must_use]
    pub fn synced_clients(&self) -> Vec<&'static str> {
        ClientKind::ALL
            .iter()
            .filter(|kind| {
                self.state
                    .clients
                    .get(kind.id())
                    .is_some_and(|names| !names.is_empty())
            })
            .map(|kind| kind.display_name())
            .collect()
    }
}

type Pending = fn(&Ctx) -> bool;
type Run = fn(&mut Ctx) -> anyhow::Result<Outcome>;

/// The four steps, in the order a first run walks them. The second string is
/// the dimmed line the driver prints when a step reports nothing to do.
const STEPS: [(Pending, Run, &str); 4] = [
    (
        detect::pending,
        detect::run,
        "your config already lists servers — skipping the survey",
    ),
    (
        import::pending,
        import::run,
        "nothing left to import — no client has a server your config is missing",
    ),
    (
        daemon::pending,
        daemon::run,
        "the gateway is already answering",
    ),
    (
        sync::pending,
        sync::run,
        "nothing to push — your clients already have what your config holds",
    ),
];

/// Runs the wizard, returning the process exit code.
///
/// # Errors
///
/// Returns a failure if the machine cannot be read, or if `--yes` runs into
/// a decision it must not make on the user's behalf.
pub fn run(args: &InitArgs, color: bool) -> anyhow::Result<u8> {
    let mut cx = context(args, color)?;

    // A machine where all four steps have nothing to say does not want to
    // be walked through setup again — it wants to be told where it stands.
    if STEPS.iter().all(|(pending, _, _)| !pending(&cx)) {
        status_card(&cx);
        return Ok(0);
    }

    opening(color);
    for (pending, step, done_line) in STEPS {
        if pending(&cx) {
            if step(&mut cx)? == Outcome::Stop {
                println!();
                ui::already_done(
                    "Stopped. Nothing was written — run `mcpgw init` again whenever you like.",
                    color,
                );
                return Ok(0);
            }
        } else {
            ui::already_done(&format!("· {done_line}"), color);
        }
        println!();
    }
    ui::already_done("Run `mcpgw doctor` any time to see where you stand.", color);
    Ok(0)
}

fn opening(color: bool) {
    let lead = "mcpgw — let's get your MCP servers running through one gateway.";
    if color {
        println!("{}", lead.bold());
    } else {
        println!("{lead}");
    }
    println!("I'll ask before every change. Nothing is written until you say yes.");
    println!();
}

/// Reads the machine once: config, mcpgw's own record of what it wrote,
/// whether a gateway answers, and which clients are installed.
fn context(args: &InitArgs, color: bool) -> anyhow::Result<Ctx> {
    let config_path = super::canonical_config_path()?;
    Ok(Ctx {
        color,
        assume_yes: args.yes,
        config: load_config(&config_path)?,
        config_path,
        state: load_state(),
        reach: probe(&args.gateway_url, PROBE_TIMEOUT),
        gateway_url: args.gateway_url.clone(),
        detections: detect_clients(),
    })
}

fn load_config(path: &Path) -> anyhow::Result<Config> {
    match Config::load(path) {
        Ok(config) => Ok(config),
        // A missing file is the state the wizard exists for.
        Err(Error::NotFound { .. }) => Ok(Config::empty()),
        Err(err) => Err(err).with_context(|| format!("cannot load {}", path.display())),
    }
}

/// A state file that cannot be read counts as "mcpgw has written nothing",
/// which is what `doctor` and `sync` already assume — the wizard offering to
/// re-sync is the recovery, not an error to stop on.
fn load_state() -> ManagedState {
    mcpgw_core::paths::state_dir()
        .map(|dir| dir.join("managed.json"))
        .and_then(|path| ManagedState::load(&path).ok())
        .unwrap_or_default()
}

fn detect_clients() -> Vec<(ClientKind, Detection)> {
    ClientKind::ALL
        .iter()
        .map(|kind| (*kind, kind.detect()))
        .collect()
}

/// One loopback request, on a runtime built for it and torn down again —
/// the wizard is otherwise entirely synchronous, and carrying a runtime
/// through four steps for one probe would put every step in async colour.
fn probe(url: &str, timeout: Duration) -> GatewayReach {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return GatewayReach::Down;
    };
    runtime.block_on(mcpgw_core::daemon::probe_gateway(url, timeout))
}

/// Where you stand, on a machine that is already set up.
fn status_card(cx: &Ctx) {
    let heading = "mcpgw — everything is set up.";
    if cx.color {
        println!("{}", heading.bold());
    } else {
        println!("{heading}");
    }
    println!();

    let enabled = cx.enabled_servers();
    println!(
        "  servers   {} configured, {enabled} enabled",
        cx.config.servers.len()
    );
    println!("  gateway   {}", describe_gateway(cx, enabled));
    let clients = cx.synced_clients();
    println!(
        "  clients   {}",
        if clients.is_empty() {
            "none yet".to_owned()
        } else {
            clients.join(", ")
        }
    );
    println!();

    for (command, what) in [
        ("mcpgw list", "your servers, as the gateway sees them"),
        ("mcpgw watch", "the traffic going through it, live"),
        ("mcpgw doctor --probe", "every server reached for real"),
    ] {
        println!("  {command:<22}{}", ui::dim(what, cx.color));
    }
}

fn describe_gateway(cx: &Ctx, enabled: usize) -> String {
    match cx.reach {
        // Every enabled server gets its own endpoint beside the aggregate,
        // so what a client can dial is one more than the server count.
        GatewayReach::Answering(_) => {
            let endpoints = enabled + 1;
            format!("running at {} — {endpoints} endpoints", cx.gateway_url)
        }
        GatewayReach::NotHttp => {
            format!("something holds {} but does not speak HTTP", cx.gateway_url)
        }
        GatewayReach::Down => "not running — start it with `mcpgw daemon start`".to_owned(),
    }
}
