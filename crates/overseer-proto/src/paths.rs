//! Filesystem locations. These only compute paths; they never create directories.

use std::ffi::OsString;
use std::path::PathBuf;

/// LaunchAgent label.
pub const AGENT_LABEL: &str = "uk.co.techize.overseer";

const HOME_VAR: &str = "OVERSEER_HOME";

fn non_empty(v: Option<OsString>) -> Option<PathBuf> {
    v.filter(|s| !s.is_empty()).map(PathBuf::from)
}

fn home_dir(home: Option<OsString>) -> PathBuf {
    non_empty(home).unwrap_or_else(|| PathBuf::from("/"))
}

fn state_dir_from(over: Option<OsString>, home: Option<OsString>) -> PathBuf {
    non_empty(over).unwrap_or_else(|| home_dir(home).join("Library/Application Support/Overseer"))
}

fn socket_path_from(over: Option<OsString>, home: Option<OsString>) -> PathBuf {
    state_dir_from(over, home).join("overseer.sock")
}

fn log_dir_from(over: Option<OsString>, home: Option<OsString>) -> PathBuf {
    match non_empty(over) {
        Some(o) => o.join("logs"),
        None => home_dir(home).join("Library/Logs/Overseer"),
    }
}

fn config_path_from(over: Option<OsString>, home: Option<OsString>) -> PathBuf {
    non_empty(over).unwrap_or_else(|| home_dir(home).join(".config/overseer/config.toml"))
}

/// `$OVERSEER_CONFIG`, or `$HOME/.config/overseer/config.toml`.
pub fn config_path() -> PathBuf {
    config_path_from(
        std::env::var_os("OVERSEER_CONFIG"),
        std::env::var_os("HOME"),
    )
}

fn targets_path_from(over: Option<OsString>, home: Option<OsString>) -> PathBuf {
    non_empty(over).unwrap_or_else(|| home_dir(home).join(".config/overseer/targets.toml"))
}

/// `$OVERSEER_TARGETS`, or `$HOME/.config/overseer/targets.toml`.
pub fn targets_path() -> PathBuf {
    targets_path_from(
        std::env::var_os("OVERSEER_TARGETS"),
        std::env::var_os("HOME"),
    )
}

/// `$OVERSEER_HOME`, or `$HOME/Library/Application Support/Overseer`.
pub fn state_dir() -> PathBuf {
    state_dir_from(std::env::var_os(HOME_VAR), std::env::var_os("HOME"))
}

/// `$OVERSEER_HOME/overseer.sock`, or under the default state dir.
pub fn socket_path() -> PathBuf {
    socket_path_from(std::env::var_os(HOME_VAR), std::env::var_os("HOME"))
}

/// `$OVERSEER_HOME/logs`, or `$HOME/Library/Logs/Overseer`.
pub fn log_dir() -> PathBuf {
    log_dir_from(std::env::var_os(HOME_VAR), std::env::var_os("HOME"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn override_wins() {
        let h = Some(std::ffi::OsString::from("/tmp/x"));
        assert_eq!(state_dir_from(h.clone(), None), PathBuf::from("/tmp/x"));
        assert_eq!(
            socket_path_from(h.clone(), None),
            PathBuf::from("/tmp/x/overseer.sock")
        );
        assert_eq!(log_dir_from(h, None), PathBuf::from("/tmp/x/logs"));
    }

    #[test]
    fn defaults_under_home() {
        let home = Some(std::ffi::OsString::from("/Users/me"));
        assert_eq!(
            state_dir_from(None, home.clone()),
            PathBuf::from("/Users/me/Library/Application Support/Overseer")
        );
        assert_eq!(
            log_dir_from(None, home),
            PathBuf::from("/Users/me/Library/Logs/Overseer")
        );
    }

    #[test]
    fn config_path_override_and_default() {
        let home = Some(std::ffi::OsString::from("/Users/me"));
        assert_eq!(
            config_path_from(Some("/tmp/c.toml".into()), home.clone()),
            PathBuf::from("/tmp/c.toml")
        );
        assert_eq!(
            config_path_from(None, home),
            PathBuf::from("/Users/me/.config/overseer/config.toml")
        );
    }

    #[test]
    fn targets_path_override_and_default() {
        let home = Some(std::ffi::OsString::from("/Users/me"));
        assert_eq!(
            targets_path_from(Some("/tmp/t.toml".into()), home.clone()),
            PathBuf::from("/tmp/t.toml")
        );
        assert_eq!(
            targets_path_from(Some("".into()), home.clone()),
            PathBuf::from("/Users/me/.config/overseer/targets.toml")
        );
        assert_eq!(
            targets_path_from(None, home),
            PathBuf::from("/Users/me/.config/overseer/targets.toml")
        );
    }

    #[test]
    fn empty_override_ignored() {
        let home = Some(std::ffi::OsString::from("/Users/me"));
        assert_eq!(
            state_dir_from(Some("".into()), home),
            PathBuf::from("/Users/me/Library/Application Support/Overseer")
        );
    }
}
