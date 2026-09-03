use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Overrides every other source of the config path; also the seam the test
/// suite uses to keep tests away from the real home directory.
pub const CONFIG_ENV: &str = "MCPGW_CONFIG";

/// Overrides the state directory (managed-state, backups); test seam.
pub const STATE_ENV: &str = "MCPGW_STATE_DIR";

const XDG_ENV: &str = "XDG_CONFIG_HOME";
const XDG_DATA_ENV: &str = "XDG_DATA_HOME";

#[cfg(windows)]
const HOME_ENV: &str = "USERPROFILE";
#[cfg(not(windows))]
const HOME_ENV: &str = "HOME";

/// Resolves the canonical config path from the process environment.
///
/// Returns `None` only when no home directory can be determined.
#[must_use]
pub fn config_path() -> Option<PathBuf> {
    config_path_with(|key| std::env::var_os(key))
}

/// Same as [`config_path`], but reads the environment through `get` so tests
/// can exercise every branch without mutating process-global env vars.
///
/// mcpgw deliberately uses `~/.config/mcpgw/` on every platform (the dev-CLI
/// convention of git/gh/ripgrep) rather than platform-native config dirs.
#[must_use]
pub fn config_path_with(get: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    if let Some(explicit) = get(CONFIG_ENV).filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(explicit));
    }
    let base = match get(XDG_ENV).filter(|v| !v.is_empty()) {
        Some(xdg) => PathBuf::from(xdg),
        None => PathBuf::from(get(HOME_ENV).filter(|v| !v.is_empty())?).join(".config"),
    };
    Some(base.join("mcpgw").join("config.toml"))
}

/// Resolves mcpgw's state directory (`~/.local/share/mcpgw` by convention,
/// like the config path deliberately identical on every platform).
#[must_use]
pub fn state_dir() -> Option<PathBuf> {
    state_dir_with(|key| std::env::var_os(key))
}

/// Same as [`state_dir`] with an injectable environment.
#[must_use]
pub fn state_dir_with(get: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    if let Some(explicit) = get(STATE_ENV).filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(explicit));
    }
    let base = match get(XDG_DATA_ENV).filter(|v| !v.is_empty()) {
        Some(xdg) => PathBuf::from(xdg),
        None => PathBuf::from(get(HOME_ENV).filter(|v| !v.is_empty())?).join(".local/share"),
    };
    Some(base.join("mcpgw"))
}

/// The one spelling a path is stored, keyed and compared under.
///
/// The same file reaches mcpgw under several names: the process's working
/// directory, a path a user typed, a symlinked temp dir (`/var` and
/// `/private/var` are one directory on macOS, and every home path there is
/// spelled both ways). `canonicalize` is what collapses those, so it runs
/// first; a path that cannot be resolved — it may not exist yet — is kept as
/// written, because an unresolvable path is still a usable key.
///
/// On Windows `canonicalize` returns a verbatim `\\?\C:\...` path, and
/// nothing else in the process produces one: `current_dir`, discovery and
/// user input all yield drive-letter paths. So the prefix is stripped back
/// off and both sides meet at the drive-letter spelling, which is also the
/// only one worth printing.
#[must_use]
pub fn normalize(path: &Path) -> PathBuf {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    strip_verbatim(&resolved)
}

/// Verbatim paths only exist on Windows, so everywhere else the resolved
/// path is already the answer.
#[cfg(not(windows))]
fn strip_verbatim(path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// `\\?\C:\dir` becomes `C:\dir` and `\\?\UNC\server\share` becomes
/// `\\server\share`. Any other prefix — a device path, a plain drive — is
/// left exactly as it is: those have no shorter spelling that means the
/// same thing.
#[cfg(windows)]
fn strip_verbatim(path: &Path) -> PathBuf {
    use std::path::{Component, Prefix};

    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return path.to_path_buf();
    };
    let head = match prefix.kind() {
        Prefix::VerbatimDisk(letter) => format!("{}:\\", char::from(letter)),
        Prefix::VerbatimUNC(server, share) => format!(
            r"\\{}\{}\",
            server.to_string_lossy(),
            share.to_string_lossy()
        ),
        _ => return path.to_path_buf(),
    };
    // The root that followed the prefix is already in `head`, and re-joining
    // it would push the rest of the path back to the root of the drive.
    let mut out = PathBuf::from(head);
    out.extend(components.filter(|part| !matches!(part, Component::RootDir)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn explicit_override_wins() {
        let get = env(&[
            (CONFIG_ENV, "/tmp/custom.toml"),
            (XDG_ENV, "/xdg"),
            (HOME_ENV, "/home/u"),
        ]);
        assert_eq!(
            config_path_with(get),
            Some(PathBuf::from("/tmp/custom.toml"))
        );
    }

    #[test]
    fn xdg_beats_home() {
        let get = env(&[(XDG_ENV, "/xdg"), (HOME_ENV, "/home/u")]);
        assert_eq!(
            config_path_with(get),
            Some(PathBuf::from("/xdg/mcpgw/config.toml"))
        );
    }

    #[test]
    fn falls_back_to_home_dot_config() {
        let get = env(&[(HOME_ENV, "/home/u")]);
        assert_eq!(
            config_path_with(get),
            Some(PathBuf::from("/home/u/.config/mcpgw/config.toml"))
        );
    }

    #[test]
    fn empty_vars_are_ignored() {
        let get = env(&[(CONFIG_ENV, ""), (XDG_ENV, ""), (HOME_ENV, "/home/u")]);
        assert_eq!(
            config_path_with(get),
            Some(PathBuf::from("/home/u/.config/mcpgw/config.toml"))
        );
    }

    #[test]
    fn no_home_yields_none() {
        assert_eq!(config_path_with(env(&[])), None);
    }

    /// The shape `canonicalize` hands back on Windows is the one shape that
    /// must not reach a state key or a report line.
    #[cfg(windows)]
    #[test]
    fn a_verbatim_path_is_keyed_by_its_drive_letter_spelling() {
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\C:\Users\runneradmin\repo\.mcp.json")),
            PathBuf::from(r"C:\Users\runneradmin\repo\.mcp.json")
        );
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\UNC\server\share\repo\.mcp.json")),
            PathBuf::from(r"\\server\share\repo\.mcp.json")
        );
        // Already the short spelling, and a path that never had a prefix to
        // drop: both come back untouched.
        assert_eq!(
            strip_verbatim(Path::new(r"C:\Users\runneradmin\repo")),
            PathBuf::from(r"C:\Users\runneradmin\repo")
        );
        assert_eq!(
            strip_verbatim(Path::new(r"repo\.mcp.json")),
            PathBuf::from(r"repo\.mcp.json")
        );
    }

    /// Nothing to strip off a unix path, and an unresolvable one survives
    /// normalisation as written rather than becoming an error.
    #[cfg(not(windows))]
    #[test]
    fn a_unix_path_normalizes_to_itself() {
        assert_eq!(
            strip_verbatim(Path::new("/home/u/repo/.mcp.json")),
            PathBuf::from("/home/u/repo/.mcp.json")
        );
        let missing = Path::new("/nonexistent-mcpgw-test/repo/.mcp.json");
        assert_eq!(normalize(missing), missing.to_path_buf());
    }

    /// The property the state keys rest on: two spellings of one existing
    /// file normalize to one string, and normalising twice changes nothing.
    #[test]
    fn two_spellings_of_one_file_normalize_alike() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("repo").join(".mcp.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "{}").unwrap();

        let indirect = dir.path().join("repo").join(".").join(".mcp.json");
        assert_eq!(normalize(&file), normalize(&indirect));
        assert_eq!(normalize(&normalize(&file)), normalize(&file));
    }
}
