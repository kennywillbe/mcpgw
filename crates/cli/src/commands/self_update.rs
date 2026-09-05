//! `mcpgw self-update`: replace this binary with the latest release.
//!
//! Only standalone installs are replaced in place. A binary owned by cargo
//! or Homebrew belongs to that tool: overwriting it would leave the package
//! manager's metadata describing a version that is no longer on disk, so
//! self-update names the right command instead and stops.

use std::time::Duration;

use anyhow::Context as _;

use crate::update::{self, InstallMethod, archive, release};

/// Exit code for `--check` when a newer release exists, so a script can
/// branch on it without parsing the message. Distinct from 1, which stays
/// "the command failed".
const UPDATE_AVAILABLE: u8 = 10;

/// Generous next to the notice's two seconds: this one is the user's actual
/// request, and it downloads several megabytes.
const TIMEOUT: Duration = Duration::from_secs(120);

/// How long the staged binary gets to answer `--version` before the update
/// is refused.
///
/// An order of magnitude above the watcher's own ceiling, because the two
/// bound opposite risks: the watcher is protecting a gateway that is
/// serving, while this is a foreground command whose expensive mistake is
/// refusing a release that was fine. The wait it has to absorb is the first
/// execution of a file written seconds ago — macOS checks the signature of
/// a new binary then, and several megabytes of it on a loaded machine is
/// seconds of wall clock.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(clap::Args)]
pub struct SelfUpdateArgs {
    /// Only report whether a newer release exists (exit 10 if it does)
    #[arg(long)]
    pub check: bool,
}

pub fn run(args: &SelfUpdateArgs) -> anyhow::Result<u8> {
    let current = env!("CARGO_PKG_VERSION");
    // The install-method check comes before any network use so the answer
    // for a cargo or Homebrew install is instant. `--check` is exempt:
    // reporting the latest version is useful however mcpgw was installed.
    let exe = std::env::current_exe().context("cannot locate the running mcpgw binary")?;
    let method = update::install_method(&exe);
    if !args.check {
        let hint = match method {
            InstallMethod::Cargo => Some(("cargo", "cargo install mcpgw")),
            InstallMethod::Homebrew => Some(("Homebrew", "brew upgrade mcpgw")),
            InstallMethod::Standalone => None,
        };
        if let Some((manager, command)) = hint {
            eprintln!("mcpgw was installed via {manager} — run: {command}");
            return Ok(1);
        }
    }

    let endpoints = release::Endpoints::from_env();
    let agent = release::agent(TIMEOUT);
    let latest = release::latest_version(&agent, &endpoints)?;

    if args.check {
        return Ok(if update::is_newer(&latest, current) {
            println!("mcpgw {latest} is available (you have {current})");
            UPDATE_AVAILABLE
        } else {
            println!("mcpgw {current} is the latest release");
            0
        });
    }

    if !update::is_newer(&latest, current) {
        println!("already up to date ({current})");
        return Ok(0);
    }

    let asset = update::asset_name(&latest, update::TARGET).with_context(|| {
        format!(
            "no prebuilt release for {} — reinstall with `cargo install mcpgw`",
            update::TARGET
        )
    })?;

    println!("downloading mcpgw {latest} ({asset})");
    let archive_bytes = release::fetch(&agent, &endpoints.asset_url(&latest, &asset))?;
    let sums = release::fetch(&agent, &endpoints.asset_url(&latest, "SHA256SUMS"))?;
    let sums = String::from_utf8(sums).context("the release's SHA256SUMS is not text")?;
    archive::verify(&sums, &asset, &archive_bytes)?;
    let binary = archive::extract_binary(&asset, &archive_bytes)?;

    // The replacement is staged next to the binary it replaces, not in the
    // system temp dir: self-replace ends in a rename, and a rename across
    // filesystems fails.
    let staging = exe.parent().unwrap_or_else(|| std::path::Path::new("."));
    let mut staged = tempfile::Builder::new()
        .prefix(".mcpgw-update.")
        .tempfile_in(staging)
        .with_context(|| format!("cannot write to {}", staging.display()))?;
    std::io::Write::write_all(&mut staged, &binary).context("cannot stage the new binary")?;
    staged
        .as_file()
        .sync_all()
        .context("cannot stage the new binary")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        staged
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o755))
            .context("cannot make the new binary executable")?;
    }
    let (file, staged_path) = staged.keep().context("cannot stage the new binary")?;
    // Closed before the file is executed: Linux refuses to exec an image
    // that is still open for writing anywhere in the system (ETXTBSY), and
    // the check below is an exec of exactly this file.
    drop(file);

    // The one-way door. `self_replace` renames over the binary this process
    // was started from, and there is no supervisor here to notice that what
    // landed does not start — the user is left with a broken `mcpgw` and no
    // working copy to run the update again from. So the staged file is run
    // once first, exactly as the watcher runs a replacement before standing
    // aside for it, and anything short of the release's own version number
    // stops the update with the old binary still in place.
    if let Err(why) = check_staged(&staged_path, &latest) {
        let _ = std::fs::remove_file(&staged_path);
        anyhow::bail!(
            "the downloaded mcpgw {latest} was not installed: {why} — {} is unchanged",
            exe.display()
        );
    }

    let replaced = self_replace::self_replace(&staged_path);
    // The staged copy has been moved into place on success and is litter on
    // failure; either way it must not be left behind.
    let _ = std::fs::remove_file(&staged_path);
    replaced.with_context(|| format!("cannot replace {}", exe.display()))?;

    println!("updated mcpgw {current} -> {latest}");
    Ok(0)
}

/// Whether the staged file is a runnable mcpgw of the version that was just
/// downloaded, as the clause that goes in the refusal.
fn check_staged(staged: &std::path::Path, latest: &str) -> Result<(), String> {
    let reported = mcpgw_core::upgrade::version_within(staged, VERIFY_TIMEOUT)?;
    if reported == latest {
        return Ok(());
    }
    Err(format!("it reports itself as mcpgw {reported}"))
}
