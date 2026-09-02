//! Wizard step 3: get the gateway running, and keep it running.
//!
//! **Stub.** `pending` is real — the driver and the status card need it from
//! day one — and `run` names the command that does the job today. W3 fills
//! in the body and touches no other file; see the contract in [`super`].

use super::{Ctx, Outcome};
use crate::ui;

/// True unless a gateway is already answering. Deliberately not "is a
/// service installed": a foreground `mcpgw serve` in another terminal is a
/// gateway, and offering to install a service on top of one would be the
/// wizard arguing with what the user can plainly see working.
pub fn pending(cx: &Ctx) -> bool {
    !cx.reach.is_up()
}

/// # Errors
///
/// Infallible today; the signature is the one W3 needs.
// The `Result` is the step contract's, not this body's: the driver calls
// every step through one fn pointer type.
#[allow(clippy::unnecessary_wraps)]
pub fn run(cx: &mut Ctx) -> anyhow::Result<Outcome> {
    ui::step(
        "Running the gateway in the background.",
        &[ui::dim(
            "coming in the next release of this wizard — run \
             `mcpgw daemon install` yourself for now",
            cx.color,
        )],
        cx.color,
    );
    Ok(Outcome::Handled)
}
