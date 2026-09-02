//! Wizard step 4: point the clients at the gateway, and check they arrive.
//!
//! **Stub.** `pending` is real — the driver and the status card need it from
//! day one — and `run` names the command that does the job today. W4 fills
//! in the body and touches no other file; see the contract in [`super`].

use super::{Ctx, Outcome};
use crate::ui;

/// True when there is something to push and nowhere it has been pushed yet.
/// Both halves matter: with an empty config there is nothing to point a
/// client at, and once mcpgw's own record shows entries it wrote, keeping
/// them current is `mcpgw sync`'s job rather than the wizard's.
pub fn pending(cx: &Ctx) -> bool {
    cx.enabled_servers() > 0 && cx.synced_clients().is_empty()
}

/// # Errors
///
/// Infallible today; the signature is the one W4 needs.
// The `Result` is the step contract's, not this body's: the driver calls
// every step through one fn pointer type.
#[allow(clippy::unnecessary_wraps)]
pub fn run(cx: &mut Ctx) -> anyhow::Result<Outcome> {
    ui::step(
        "Pointing your clients at the gateway.",
        &[ui::dim(
            "coming in the next release of this wizard — run `mcpgw sync` \
             yourself for now",
            cx.color,
        )],
        cx.color,
    );
    Ok(Outcome::Handled)
}
