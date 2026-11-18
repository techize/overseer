//! The build id stamped into a bundle's `Info.plist`, and the local time
//! helper the daemon, the CLI and the GUI share.

use std::path::Path;
use std::sync::OnceLock;

/// The build of a binary that does not run from a bundle.
pub const DEV_BUILD: &str = "dev";

const KEY: &str = "<key>OverseerBuild</key>";
const PLACEHOLDER: &str = "__BUILD__";

/// The `OverseerBuild` string of an `Info.plist`, trimmed. `None` when the
/// key is missing, empty or still the `__BUILD__` placeholder.
pub fn build_from_plist(text: &str) -> Option<String> {
    let after = &text[text.find(KEY)? + KEY.len()..];
    let after = &after[after.find("<string>")? + "<string>".len()..];
    let value = after[..after.find("</string>")?].trim();
    (!value.is_empty() && value != PLACEHOLDER).then(|| value.to_string())
}

/// The build of the bundle `exe` sits in (`X.app/Contents/MacOS/exe`), else
/// `dev`.
pub fn build_of_exe(exe: &Path) -> String {
    exe.parent()
        .and_then(Path::parent)
        .map(|contents| contents.join("Info.plist"))
        .and_then(|plist| std::fs::read_to_string(plist).ok())
        .and_then(|text| build_from_plist(&text))
        .unwrap_or_else(|| DEV_BUILD.to_string())
}

/// The build of this process, read once from its canonicalised executable.
pub fn current_build() -> &'static str {
    static BUILD: OnceLock<String> = OnceLock::new();
    BUILD.get_or_init(|| match std::env::current_exe() {
        Ok(exe) => build_of_exe(&exe.canonicalize().unwrap_or(exe)),
        Err(_) => DEV_BUILD.to_string(),
    })
}

/// How the daemon's build sits against the app's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildOrder {
    Match,
    DaemonOlder,
    DaemonNewer,
    Different,
}

/// The `yyyymmddHHMM` stamp after the last `-`, when it is 12 ASCII digits.
fn stamp(build: &str) -> Option<u64> {
    let (_, stamp) = build.rsplit_once('-')?;
    (stamp.len() == 12 && stamp.bytes().all(|b| b.is_ascii_digit()))
        .then(|| stamp.parse().ok())
        .flatten()
}

/// Compares two build ids by their timestamp suffix. Equal ids, or `dev` on
/// either side, match; ids without a readable stamp, or with equal stamps on
/// different commits, are `Different`.
pub fn compare_builds(app: &str, daemon: &str) -> BuildOrder {
    if app == daemon || app == DEV_BUILD || daemon == DEV_BUILD {
        return BuildOrder::Match;
    }
    match (stamp(app), stamp(daemon)) {
        (Some(a), Some(d)) if d < a => BuildOrder::DaemonOlder,
        (Some(a), Some(d)) if d > a => BuildOrder::DaemonNewer,
        _ => BuildOrder::Different,
    }
}

/// The line to show when the app and the daemon run different known builds,
/// naming the side to restart.
pub fn build_notice(app: &str, daemon: &str) -> Option<String> {
    match compare_builds(app, daemon) {
        BuildOrder::Match => None,
        BuildOrder::DaemonOlder => Some(format!(
            "The daemon runs build {daemon}, older than this app's build {app}. Restart the daemon to finish the upgrade (overseer ctl restart, or Settings)."
        )),
        BuildOrder::DaemonNewer => Some(format!(
            "The daemon runs build {daemon}, newer than this app's build {app}. Restart Overseer (quit and reopen the app) to finish the upgrade."
        )),
        BuildOrder::Different => Some(format!(
            "The app and the daemon are different builds ({app} and {daemon}); restart both."
        )),
    }
}

/// Local `HH:MM` for unix seconds, `?` when the time cannot be converted.
pub fn local_hhmm(t: i64) -> String {
    // SAFETY: localtime_r only writes the `tm` we pass it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let time = t as libc::time_t;
    if unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
        return "?".into();
    }
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLIST: &str =
        "<dict>\n    <key>OverseerBuild</key>\n    <string>abc1234-202611140930</string>\n</dict>";

    #[test]
    fn reads_the_build_key() {
        assert_eq!(
            build_from_plist(PLIST),
            Some("abc1234-202611140930".to_string())
        );
    }

    #[test]
    fn missing_or_placeholder_build_is_none() {
        assert_eq!(build_from_plist("<dict></dict>"), None);
        assert_eq!(
            build_from_plist("<key>OverseerBuild</key><string></string>"),
            None
        );
        assert_eq!(
            build_from_plist("<key>OverseerBuild</key><string>__BUILD__</string>"),
            None
        );
    }

    #[test]
    fn exe_outside_a_bundle_is_dev() {
        let tmp = tempfile::tempdir().unwrap();
        let contents = tmp.path().join("X.app/Contents");
        std::fs::create_dir_all(contents.join("MacOS")).unwrap();
        std::fs::write(contents.join("Info.plist"), PLIST).unwrap();
        assert_eq!(
            build_of_exe(&contents.join("MacOS/overseer")),
            "abc1234-202611140930"
        );
        assert_eq!(build_of_exe(Path::new("/tmp/overseer")), "dev");
    }

    #[test]
    fn compare_builds_reads_the_timestamp_suffix() {
        let new = "abc1234-202611141000";
        let old = "abc1234-202611140930";
        assert_eq!(compare_builds(new, old), BuildOrder::DaemonOlder);
        assert_eq!(compare_builds(old, new), BuildOrder::DaemonNewer);
        assert_eq!(compare_builds(new, new), BuildOrder::Match);
        assert_eq!(compare_builds("dev", old), BuildOrder::Match);
        assert_eq!(compare_builds(old, "dev"), BuildOrder::Match);
        assert_eq!(compare_builds("b2", "b1"), BuildOrder::Different);
        assert_eq!(
            compare_builds("feat-x-abc1234-202611141000", "feat-x-abc1234-202611140930"),
            BuildOrder::DaemonOlder
        );
        assert_eq!(
            compare_builds("a-20261114093", "b-202611140930"),
            BuildOrder::Different
        );
        assert_eq!(
            compare_builds("a-202611140930", "b-202611140930"),
            BuildOrder::Different
        );
    }

    #[test]
    fn notice_names_who_to_restart() {
        let new = "abc1234-202611141000";
        let old = "abc1234-202611140930";
        assert_eq!(
            build_notice(new, old).as_deref(),
            Some("The daemon runs build abc1234-202611140930, older than this app's build abc1234-202611141000. Restart the daemon to finish the upgrade (overseer ctl restart, or Settings).")
        );
        assert_eq!(
            build_notice(old, new).as_deref(),
            Some("The daemon runs build abc1234-202611141000, newer than this app's build abc1234-202611140930. Restart Overseer (quit and reopen the app) to finish the upgrade.")
        );
        assert_eq!(
            build_notice("b2", "b1").as_deref(),
            Some("The app and the daemon are different builds (b2 and b1); restart both.")
        );
        assert_eq!(build_notice("a", "a"), None);
        assert_eq!(build_notice("dev", "b"), None);
        assert_eq!(build_notice("a", "dev"), None);
    }

    #[test]
    fn hhmm_is_two_digit_fields() {
        let t = local_hhmm(1_700_000_000);
        assert_eq!(t.len(), 5, "{t}");
        assert_eq!(&t[2..3], ":");
    }
}
