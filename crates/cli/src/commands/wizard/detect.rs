//! Wizard step 1: take stock of the machine.
//!
//! The only step that writes nothing at all. It exists because the two
//! questions a first-time user actually has — "does it even know about my
//! editors?" and "is it going to touch something I did not expect?" — are
//! both answered by showing the list before anything else happens.
//!
//! See the contract in [`super`]: this module is `pending` + `run`.

use mcpgw_core::{ClientKind, Detection};

use super::{Ctx, Outcome};
use crate::ui;

/// Nothing to survey once the canonical config has servers in it: at that
/// point the user has already taken stock, and re-printing the machine's
/// inventory on every run is noise.
pub fn pending(cx: &Ctx) -> bool {
    cx.config.servers.is_empty()
}

/// Shows what is installed and what each client already has, then asks
/// whether to carry on.
///
/// # Errors
///
/// Returns the underlying [`std::io::Error`] if the terminal cannot be read.
pub fn run(cx: &mut Ctx) -> anyhow::Result<Outcome> {
    let mut rows: Vec<(String, String, String)> = Vec::new();
    let mut absent = 0usize;

    for (kind, detection) in &cx.detections {
        match detection {
            Detection::Configured(path) => rows.push((
                kind.display_name().to_owned(),
                servers_cell(*kind, path),
                path.display().to_string(),
            )),
            Detection::Installed => rows.push((
                kind.display_name().to_owned(),
                String::new(),
                "installed, no MCP config yet".to_owned(),
            )),
            // Thirteen "not installed" lines would bury the two clients the
            // user actually has, so the absent ones are one number.
            Detection::NotInstalled => absent += 1,
        }
    }

    let name_width = rows
        .iter()
        .map(|(n, ..)| n.chars().count())
        .max()
        .unwrap_or(0);
    let count_width = rows
        .iter()
        .map(|(_, c, _)| c.chars().count())
        .max()
        .unwrap_or(0);
    let mut bullets: Vec<String> = rows
        .iter()
        .map(|(name, count, detail)| {
            format!(
                "{name:name_width$}  {count:>count_width$}  {}",
                ui::dim(detail, cx.color)
            )
        })
        .collect();
    if absent > 0 {
        bullets.push(ui::dim(
            &format!("{absent} other supported clients are not installed here"),
            cx.color,
        ));
    }

    let heading = if rows.is_empty() {
        "Looking around — no MCP client is installed on this machine yet.".to_owned()
    } else {
        format!("Looking around — {} MCP clients found.", rows.len())
    };
    ui::step(&heading, &bullets, cx.color);

    Ok(if cx.confirm("\nContinue?")? {
        Outcome::Handled
    } else {
        Outcome::Stop
    })
}

/// How many servers this client already has, or why we cannot say. A file
/// we fail to read is reported here rather than aborting the wizard: the
/// remaining clients are still worth showing, and `mcpgw doctor` is the
/// command that explains a broken client config properly.
fn servers_cell(kind: ClientKind, path: &std::path::Path) -> String {
    match kind.load(path) {
        Ok(read) => match read.servers.len() {
            1 => "1 server".to_owned(),
            n => format!("{n} servers"),
        },
        Err(_) => "unreadable".to_owned(),
    }
}
