use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;

use anyhow::{Context as _, bail};
use mcpgw_core::sync::{apply_plan, plan_sync};
use mcpgw_core::{ClientKind, Config, Detection, Error, backup, paths, state::ManagedState};
use owo_colors::OwoColorize as _;

#[derive(clap::Args)]
pub struct SyncArgs {
    /// Only sync these clients (repeatable; ids: claude-desktop, claude-code, cursor, vscode)
    #[arg(long = "client", value_name = "ID")]
    pub clients: Vec<String>,
    /// Show what would change without writing anything
    #[arg(long)]
    pub dry_run: bool,
    /// Restore each selected client's config from its most recent backup
    #[arg(long, conflicts_with = "dry_run")]
    pub rollback: bool,
}

pub fn run(args: &SyncArgs, color: bool) -> anyhow::Result<()> {
    let targets = super::select_clients(&args.clients)?;
    let state_dir =
        paths::state_dir().context("cannot determine a home directory for the state dir")?;

    if args.rollback {
        return rollback(&targets, &state_dir);
    }

    let config_path = super::canonical_config_path()?;
    let canonical = match Config::load(&config_path) {
        Ok(config) => config.servers,
        // An absent canonical config means "manage nothing": previously
        // managed entries get removed, everything else is untouched.
        Err(Error::NotFound { .. }) => BTreeMap::new(),
        Err(err) => return Err(err.into()),
    };

    let state_path = state_dir.join("managed.json");
    let mut state = ManagedState::load(&state_path)?;

    for kind in targets {
        let heading = |text: &str| {
            if color {
                println!("{}", format!("{} — {text}", kind.display_name()).bold());
            } else {
                println!("{} — {text}", kind.display_name());
            }
        };
        let (path, exists) = match kind.detect() {
            Detection::NotInstalled => {
                heading("not found, skipped");
                continue;
            }
            Detection::Installed => {
                let Some(path) = kind.config_path() else {
                    heading("cannot resolve config path, skipped");
                    continue;
                };
                (path, false)
            }
            Detection::Configured(path) => (path, true),
        };

        let mut root = if exists {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("cannot read {}", path.display()))?;
            match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(value) => value,
                // JSONC or hand-broken JSON: refuse to rewrite what we
                // cannot faithfully parse.
                Err(err) => {
                    heading(&format!(
                        "skipped: {} is not strict JSON ({err}); fix or sync it manually",
                        path.display()
                    ));
                    continue;
                }
            }
        } else {
            serde_json::json!({})
        };

        let empty = serde_json::Map::new();
        let current = root
            .get(kind.root_key())
            .and_then(serde_json::Value::as_object)
            .unwrap_or(&empty);
        let managed = state.clients.get(kind.id()).cloned().unwrap_or_default();
        let plan = plan_sync(kind, current, &canonical, &managed);

        heading(&describe(&plan));
        print_plan_lines(&plan, color);

        if !plan.has_changes() {
            continue;
        }
        if args.dry_run {
            continue;
        }

        if exists {
            backup::backup_file(&state_dir, kind.id(), &path)?;
        }
        apply_plan(kind, &mut root, &plan);
        write_json(&path, &root)?;
        state
            .clients
            .insert(kind.id().to_owned(), plan.managed_after());
        state.save(&state_path)?;
    }
    Ok(())
}

fn describe(plan: &mcpgw_core::sync::SyncPlan) -> String {
    if plan.has_changes() {
        format!(
            "{} to add, {} to update, {} to remove",
            plan.adds.len(),
            plan.updates.len(),
            plan.removes.len()
        )
    } else {
        "no changes".to_owned()
    }
}

fn print_plan_lines(plan: &mcpgw_core::sync::SyncPlan, color: bool) {
    let line = |mark: &str, name: &str, note: &str, colored: fn(&str) -> String| {
        if color {
            println!("  {} {name}{note}", colored(mark));
        } else {
            println!("  {mark} {name}{note}");
        }
    };
    for name in &plan.adds {
        line("+", name, "", |m| m.green().to_string());
    }
    for name in &plan.updates {
        line("~", name, "", |m| m.yellow().to_string());
    }
    for name in &plan.removes {
        line("-", name, "", |m| m.red().to_string());
    }
    for name in &plan.conflicts {
        line(
            "!",
            name,
            " exists in the client but is not managed by mcpgw (left untouched)",
            |m| m.red().to_string(),
        );
    }
    for name in &plan.foreign {
        line(
            "?",
            name,
            " (unmanaged, untouched — `mcpgw import` to adopt)",
            |m| m.dimmed().to_string(),
        );
    }
}

fn rollback(targets: &[ClientKind], state_dir: &Path) -> anyhow::Result<()> {
    let mut restored = 0;
    for kind in targets {
        let Some(backup_path) = backup::latest_backup(state_dir, kind.id())? else {
            continue;
        };
        let Some(config_path) = kind.config_path() else {
            continue;
        };
        let text = std::fs::read_to_string(&backup_path)
            .with_context(|| format!("cannot read backup {}", backup_path.display()))?;
        write_text(&config_path, &text)?;
        println!(
            "restored {} from {}",
            config_path.display(),
            backup_path.display()
        );
        restored += 1;
    }
    if restored == 0 {
        bail!("no backups found for the selected clients");
    }
    Ok(())
}

fn write_json(path: &Path, root: &serde_json::Value) -> anyhow::Result<()> {
    let mut text = serde_json::to_string_pretty(root)?;
    text.push('\n');
    write_text(path, &text)
}

// Atomic replace, same discipline as the canonical store.
fn write_text(path: &Path, text: &str) -> anyhow::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".mcpgw-sync.")
        .tempfile_in(parent)?;
    tmp.write_all(text.as_bytes())?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)
        .map_err(|err| anyhow::Error::from(err.error))
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}
