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
    if let Some(line) = project_line() {
        bullets.push(ui::dim(&line, cx.color));
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

/// The one line about repo-local MCP configs, or nothing when the working
/// directory has none.
///
/// Said here because the wizard's first promise is that it will name what it
/// found, and a `.mcp.json` sitting in the repo is exactly the thing a user
/// would otherwise assume the sync step took care of. It is dim and it is one
/// line: nothing in the wizard acts on these files, so a bigger say than that
/// would be a promise it cannot keep.
fn project_line() -> Option<String> {
    let found = mcpgw_core::projects::discover_cwd();
    if found.is_empty() {
        return None;
    }
    let names: Vec<String> = found
        .iter()
        .map(|config| {
            config
                .path
                .strip_prefix(&config.dir)
                .unwrap_or(&config.path)
                .display()
                .to_string()
        })
        .collect();
    let servers: usize = found.iter().map(|config| config.read.servers.len()).sum();
    let plural = if servers == 1 { "server" } else { "servers" };
    Some(format!(
        "also found {} in this repo with {servers} {plural}; project files are not managed yet",
        names.join(", ")
    ))
}
