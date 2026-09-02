//! Wizard step 2: adopt the servers the clients already have.
//!
//! **Stub.** `pending` is real — the driver and the status card need it from
//! day one — and `run` names the command that does the job today. W2 fills
//! in the body and touches no other file; see the contract in [`super`].

use mcpgw_core::Detection;

use super::{Ctx, Outcome};
use crate::ui;

/// True when some configured client holds a server the canonical config has
/// never heard of. A client whose file will not parse is not counted as
/// having something to import — `mcpgw doctor` is where a broken client
/// config gets explained, and the wizard would only be guessing.
pub fn pending(cx: &Ctx) -> bool {
    unimported(cx).is_some()
}

/// # Errors
///
/// Infallible today; the signature is the one W2 needs.
// The `Result` is the step contract's, not this body's: the driver calls
// every step through one fn pointer type.
#[allow(clippy::unnecessary_wraps)]
pub fn run(cx: &mut Ctx) -> anyhow::Result<Outcome> {
    let from = unimported(cx).unwrap_or("<client>");
    ui::step(
        "Importing what your clients already have.",
        &[ui::dim(
            &format!(
                "coming in the next release of this wizard — run \
                 `mcpgw import --from {from}` yourself for now"
            ),
            cx.color,
        )],
        cx.color,
    );
    Ok(Outcome::Handled)
}

/// The id of the first client holding an unknown server, for the suggested
/// command.
fn unimported(cx: &Ctx) -> Option<&'static str> {
    cx.detections.iter().find_map(|(kind, detection)| {
        let Detection::Configured(path) = detection else {
            return None;
        };
        let read = kind.load(path).ok()?;
        read.servers
            .keys()
            .any(|name| !cx.config.servers.contains_key(name))
            .then_some(kind.id())
    })
}
