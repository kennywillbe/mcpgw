//! Everything about mcpgw updating itself: the passive "a newer release
//! exists" notice and the `self-update` command it points at. This lives in
//! the CLI rather than in core because it is about how the binary got onto
//! the machine, not about MCP configuration.

pub mod archive;
pub mod notice;
pub mod release;

use std::path::Path;

/// The target triple this binary was built for, recorded by `build.rs`.
pub const TARGET: &str = env!("MCPGW_TARGET");

/// The triples `.github/workflows/release.yml` actually builds. A binary for
/// anything else came from `cargo install`, so there is no archive to hand
/// it and self-update has to say so instead of guessing an asset name.
pub const SHIPPED_TARGETS: [&str; 4] = [
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "x86_64-pc-windows-msvc",
];

/// How the running binary was installed, which decides whether replacing it
/// in place is mcpgw's business or its package manager's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMethod {
    Cargo,
    Homebrew,
    /// A downloaded archive, whether unpacked by install.sh or by hand.
    Standalone,
}

/// Classifies an executable path into the install method that put it there.
///
/// Matching runs on the path text with separators normalised rather than on
/// [`Path::components`], so a Windows path classifies the same way whatever
/// host the check (or its test) runs on.
#[must_use]
pub fn install_method(exe: &Path) -> InstallMethod {
    let path = exe.to_string_lossy().replace('\\', "/");
    if path.contains("/.cargo/bin/") {
        return InstallMethod::Cargo;
    }
    // Homebrew keeps the real binary under `Cellar` and links it from
    // `homebrew` (Apple silicon, /opt/homebrew) or `linuxbrew`; a
    // `current_exe` can be either, since it resolves symlinks on Linux but
    // not always on macOS.
    if ["/Cellar/", "/homebrew/", "/linuxbrew/"]
        .iter()
        .any(|marker| path.contains(marker))
    {
        return InstallMethod::Homebrew;
    }
    InstallMethod::Standalone
}

/// Parses a plain `x.y.z` version, with or without a leading `v`.
///
/// Anything carrying a pre-release or build suffix returns `None`: mcpgw
/// only ever tags plain releases, and refusing to rank the rest keeps the
/// notice from offering someone a release candidate.
#[must_use]
pub fn parse_version(text: &str) -> Option<[u64; 3]> {
    let text = text.trim().strip_prefix('v').unwrap_or(text.trim());
    if text.contains(['-', '+']) {
        return None;
    }
    let mut parts = text.split('.');
    let mut out = [0u64; 3];
    for slot in &mut out {
        *slot = parts.next()?.parse().ok()?;
    }
    parts.next().is_none().then_some(out)
}

/// Whether `latest` is a strictly newer release than `current`.
///
/// Unparseable input is never "newer": a version mcpgw cannot rank must not
/// nag the user about an update it cannot describe.
#[must_use]
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

/// The release asset for `version` on `target`, named exactly as
/// release.yml packages it. `None` for a target with no prebuilt archive.
#[must_use]
pub fn asset_name(version: &str, target: &str) -> Option<String> {
    if !SHIPPED_TARGETS.contains(&target) {
        return None;
    }
    let version = version.strip_prefix('v').unwrap_or(version);
    let extension = if target.contains("windows") {
        "zip"
    } else {
        "tar.gz"
    };
    Some(format!("mcpgw-{version}-{target}.{extension}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_with_and_without_the_v() {
        assert_eq!(parse_version("0.1.0"), Some([0, 1, 0]));
        assert_eq!(parse_version("v10.20.30"), Some([10, 20, 30]));
        assert_eq!(parse_version(" v1.2.3\n"), Some([1, 2, 3]));
    }

    #[test]
    fn incomplete_or_prerelease_versions_are_rejected() {
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version("1.2.x"), None);
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("1.2.3-rc1"), None);
        assert_eq!(parse_version("1.2.3+build"), None);
    }

    #[test]
    fn newer_compares_component_wise_not_lexically() {
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
    }

    #[test]
    fn unrankable_versions_never_claim_to_be_newer() {
        assert!(!is_newer("garbage", "0.1.0"));
        assert!(!is_newer("9.9.9-rc1", "0.1.0"));
        assert!(!is_newer("9.9.9", "not-a-version"));
    }

    #[test]
    fn cargo_installs_are_recognised_on_both_separators() {
        assert_eq!(
            install_method(Path::new("/home/u/.cargo/bin/mcpgw")),
            InstallMethod::Cargo
        );
        assert_eq!(
            install_method(Path::new(r"C:\Users\u\.cargo\bin\mcpgw.exe")),
            InstallMethod::Cargo
        );
    }

    #[test]
    fn homebrew_installs_are_recognised_from_either_prefix() {
        for path in [
            "/opt/homebrew/bin/mcpgw",
            "/usr/local/Cellar/mcpgw/0.1.0/bin/mcpgw",
            "/home/linuxbrew/.linuxbrew/bin/mcpgw",
        ] {
            assert_eq!(
                install_method(Path::new(path)),
                InstallMethod::Homebrew,
                "{path}"
            );
        }
    }

    #[test]
    fn anything_else_is_a_standalone_install() {
        for path in [
            "/home/u/.local/bin/mcpgw",
            "/usr/local/bin/mcpgw",
            r"C:\Program Files\mcpgw\mcpgw.exe",
            // A source checkout: a target/debug binary is not a package
            // manager's, so self-update is allowed to replace it.
            "/home/u/src/mcpgw/target/debug/mcpgw",
        ] {
            assert_eq!(
                install_method(Path::new(path)),
                InstallMethod::Standalone,
                "{path}"
            );
        }
    }

    #[test]
    fn asset_names_match_what_release_yml_packages() {
        assert_eq!(
            asset_name("0.2.0", "aarch64-apple-darwin").as_deref(),
            Some("mcpgw-0.2.0-aarch64-apple-darwin.tar.gz")
        );
        assert_eq!(
            asset_name("v0.2.0", "x86_64-apple-darwin").as_deref(),
            Some("mcpgw-0.2.0-x86_64-apple-darwin.tar.gz")
        );
        assert_eq!(
            asset_name("0.2.0", "x86_64-unknown-linux-gnu").as_deref(),
            Some("mcpgw-0.2.0-x86_64-unknown-linux-gnu.tar.gz")
        );
        assert_eq!(
            asset_name("0.2.0", "x86_64-pc-windows-msvc").as_deref(),
            Some("mcpgw-0.2.0-x86_64-pc-windows-msvc.zip")
        );
    }

    #[test]
    fn targets_without_a_prebuilt_archive_have_no_asset() {
        assert_eq!(asset_name("0.2.0", "aarch64-unknown-linux-gnu"), None);
        assert_eq!(asset_name("0.2.0", "x86_64-unknown-linux-musl"), None);
    }

    #[test]
    fn this_binary_knows_its_own_triple() {
        assert!(TARGET.contains('-'), "{TARGET}");
    }
}
