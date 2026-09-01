use std::ffi::OsString;
use std::path::PathBuf;

/// Overrides every other source of the config path; also the seam the test
/// suite uses to keep tests away from the real home directory.
pub const CONFIG_ENV: &str = "MCPGW_CONFIG";

const XDG_ENV: &str = "XDG_CONFIG_HOME";

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
}
