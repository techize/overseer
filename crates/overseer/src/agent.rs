//! LaunchAgent plist generation and install/uninstall.

use std::ffi::{OsStr, OsString};
use std::io::{BufRead, IsTerminal, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use overseer_proto::paths::{log_dir, socket_path, AGENT_LABEL};
use overseer_proto::{
    current_build, running_sessions_count, running_sessions_text, Client, Request, Response,
    SlotId, SlotState,
};

/// How long to wait for launchd to finish removing a booted-out agent.
const BOOTOUT_WAIT: Duration = Duration::from_secs(10);

/// How long asking a running daemon about itself may take in all.
const QUERY_WAIT: Duration = Duration::from_secs(2);

/// Variables an isolated home's agent carries from the installing shell, so
/// its daemon sees the same home, config and Claude setup (after `PATH`).
pub const AGENT_ENV: [&str; 7] = [
    "HOME",
    "SHELL",
    "OVERSEER_HOME",
    "OVERSEER_CONFIG",
    "OVERSEER_TARGETS",
    "CLAUDE_CONFIG_DIR",
    "OVERSEER_DEFAULT_COMMAND",
];

fn custom_home(overseer_home: Option<&OsStr>) -> Option<&OsStr> {
    overseer_home.filter(|h| !h.is_empty())
}

/// FNV-1a, 32 bit.
fn fnv1a32(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c9dc5u32, |h, &b| {
        (h ^ u32::from(b)).wrapping_mul(0x01000193)
    })
}

/// The agent's label: the real one for the default home, and
/// `uk.co.techize.overseer-home-<8 hex>` for an `OVERSEER_HOME`, so an
/// isolated home can never name the real agent. Empty counts as unset.
pub fn agent_label(overseer_home: Option<&OsStr>) -> String {
    match custom_home(overseer_home) {
        Some(h) => format!("{AGENT_LABEL}-home-{:08x}", fnv1a32(h.as_bytes())),
        None => AGENT_LABEL.to_string(),
    }
}

/// Where the plist goes: `~/Library/LaunchAgents` for the default home,
/// `$OVERSEER_HOME/agent` otherwise (launchd does not load it at login).
pub fn plist_path_for(overseer_home: Option<&OsStr>, home: &Path) -> PathBuf {
    let label = agent_label(overseer_home);
    match custom_home(overseer_home) {
        Some(h) => Path::new(h).join("agent"),
        None => home.join("Library/LaunchAgents"),
    }
    .join(format!("{label}.plist"))
}

/// The isolating environment for a custom home's agent: each set, non-empty
/// variable of `AGENT_ENV`, in that order. Empty for the default home.
pub fn agent_env(
    custom_home: bool,
    get: impl Fn(&str) -> Option<OsString>,
) -> Vec<(String, String)> {
    if !custom_home {
        return Vec::new();
    }
    AGENT_ENV
        .iter()
        .filter_map(|&k| {
            get(k)
                .filter(|v| !v.is_empty())
                .map(|v| (k.to_string(), v.to_string_lossy().into_owned()))
        })
        .collect()
}

/// `OVERSEER_HOME` as an absolute path with no trailing slash, `.` or
/// doubled slash, so one home always hashes to one label. Symlinks are
/// left as they are.
fn canonical_home(h: &OsStr) -> OsString {
    let p = Path::new(h);
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    abs.components().collect::<PathBuf>().into_os_string()
}

fn this_home() -> Option<OsString> {
    std::env::var_os("OVERSEER_HOME")
        .filter(|h| !h.is_empty())
        .map(|h| canonical_home(&h))
}

fn this_label() -> String {
    agent_label(this_home().as_deref())
}

fn launchctl_program_from(over: Option<OsString>) -> OsString {
    over.filter(|p| !p.is_empty())
        .unwrap_or_else(|| OsString::from("launchctl"))
}

/// `$OVERSEER_LAUNCHCTL`, or `launchctl`. Every launchctl call goes
/// through this, so tests can point it at a stub.
fn launchctl_program() -> OsString {
    launchctl_program_from(std::env::var_os("OVERSEER_LAUNCHCTL"))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// PATH for the daemon under launchd, whose own PATH is only
/// /usr/bin:/bin:/usr/sbin:/sbin and so cannot find a Homebrew `claude`.
/// Uses the installing shell's PATH with duplicates removed, or a default
/// covering the usual install locations when that is unset or empty.
pub fn agent_path(path: Option<OsString>, home: Option<OsString>) -> String {
    let given = path
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|p| !p.trim().is_empty());
    let raw = given.unwrap_or_else(|| {
        let mut dirs = vec!["/opt/homebrew/bin".to_string(), "/usr/local/bin".into()];
        if let Some(h) = home.filter(|h| !h.is_empty()) {
            dirs.push(format!("{}/.local/bin", h.to_string_lossy()));
        }
        dirs.extend(["/usr/bin", "/bin", "/usr/sbin", "/sbin"].map(String::from));
        dirs.join(":")
    });
    let mut seen = Vec::new();
    for dir in raw.split(':').filter(|d| !d.is_empty()) {
        if !seen.contains(&dir) {
            seen.push(dir);
        }
    }
    seen.join(":")
}

/// The LaunchAgent plist for running the daemon in the foreground under launchd.
///
/// KeepAlive only restarts the daemon when it did not exit cleanly, so
/// `overseer ctl shutdown` (exit 0) keeps it down until the next client
/// kickstarts it, while a crash is respawned.
pub fn plist_xml(
    label: &str,
    exe: &Path,
    log_dir: &Path,
    path_env: &str,
    extra_env: &[(String, String)],
) -> String {
    let label = xml_escape(label);
    let exe = xml_escape(&exe.display().to_string());
    let out = xml_escape(&log_dir.join("daemon.out.log").display().to_string());
    let err = xml_escape(&log_dir.join("daemon.err.log").display().to_string());
    let path_env = xml_escape(path_env);
    let extra: String = extra_env
        .iter()
        .map(|(k, v)| {
            format!(
                "\n        <key>{}</key>\n        <string>{}</string>",
                xml_escape(k),
                xml_escape(v)
            )
        })
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>daemon</string>
        <string>--foreground</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>{path_env}</string>{extra}
    </dict>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key><false/>
    </dict>
    <key>StandardOutPath</key>
    <string>{out}</string>
    <key>StandardErrorPath</key>
    <string>{err}</string>
</dict>
</plist>
"#
    )
}

/// This process's plist: `plist_path_for` with the environment.
pub fn plist_path() -> anyhow::Result<PathBuf> {
    let over = this_home();
    let home = match &over {
        Some(_) => std::env::var_os("HOME").unwrap_or_default(),
        None => std::env::var_os("HOME").context("HOME is not set")?,
    };
    Ok(plist_path_for(over.as_deref(), Path::new(&home)))
}

fn domain() -> String {
    // SAFETY: getuid has no preconditions and cannot fail.
    format!("gui/{}", unsafe { libc::getuid() })
}

fn service() -> String {
    format!("{}/{}", domain(), this_label())
}

fn launchctl(args: &[&str]) -> anyhow::Result<std::process::ExitStatus> {
    Command::new(launchctl_program())
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("run launchctl")
}

fn agent_loaded() -> bool {
    launchctl(&["print", &service()]).is_ok_and(|s| s.success())
}

/// Boots out the agent if it is loaded and waits until launchd has let go
/// of it, so a following bootstrap does not fail with "already loaded".
fn bootout() -> anyhow::Result<()> {
    if !agent_loaded() {
        return Ok(());
    }
    // A failure here (for example "not found" after a race) is checked by
    // the wait below, not reported.
    launchctl(&["bootout", &service()])?;
    let deadline = Instant::now() + BOOTOUT_WAIT;
    while agent_loaded() {
        if Instant::now() >= deadline {
            bail!("launchctl bootout {} did not finish", service());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// Asks a daemon that is not run by launchd (one autostarted before the
/// agent existed) to exit, so the agent's daemon can take the socket.
fn shutdown_running_daemon() -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let Ok(client) = Client::connect(socket_path()).await else {
            return Ok(());
        };
        println!("stopping the running daemon");
        let _ = client.request(Request::Shutdown).await;
        let deadline = Instant::now() + BOOTOUT_WAIT;
        while Client::connect(socket_path()).await.is_ok() {
            if Instant::now() >= deadline {
                bail!("the running daemon did not stop");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(())
    })
}

/// The daemon answering on the socket: its build and its running sessions.
pub struct RunningDaemon {
    pub build: String,
    /// Slot and label of each `Running` slot.
    pub running: Vec<(SlotId, String)>,
}

/// `install-agent`'s and `ctl restart`'s flags, and whether stdin is a terminal.
#[derive(Clone, Copy)]
pub struct RestartOpts {
    pub yes: bool,
    pub no_restart: bool,
    pub tty: bool,
}

impl RestartOpts {
    /// The flags, with `tty` from this process's stdin.
    pub fn from_flags(yes: bool, no_restart: bool) -> RestartOpts {
        RestartOpts {
            yes,
            no_restart,
            tty: std::io::stdin().is_terminal(),
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum RestartChoice {
    /// No daemon answers: start one.
    Start,
    /// Same build and plist: leave the daemon alone.
    Nothing,
    Restart,
    /// Ask on the terminal.
    Ask,
    /// Leave the old daemon and its sessions running.
    Keep,
}

/// The restart rules, in order: no daemon starts one; an unchanged build and
/// plist restarts nothing; no running session restarts without asking; with
/// running sessions `--no-restart` keeps (it wins over `--yes`), `--yes`
/// restarts, a terminal is asked and anything else keeps. `ctl restart`
/// passes `unchanged = false`.
pub fn restart_choice(
    daemon: Option<&RunningDaemon>,
    unchanged: bool,
    opts: RestartOpts,
) -> RestartChoice {
    let Some(d) = daemon else {
        return RestartChoice::Start;
    };
    if unchanged {
        RestartChoice::Nothing
    } else if d.running.is_empty() {
        RestartChoice::Restart
    } else if opts.no_restart {
        RestartChoice::Keep
    } else if opts.yes {
        RestartChoice::Restart
    } else if opts.tty {
        RestartChoice::Ask
    } else {
        RestartChoice::Keep
    }
}

/// What a restart would end, said before it happens.
pub fn restart_warning(d: &RunningDaemon) -> String {
    format!(
        "The daemon (build {}) has {}: {}. Restarting it ends them; each comes back as resumable, and r on its card resumes it.",
        d.build,
        running_sessions_count(d.running.len()),
        running_sessions_text(d.running.iter().map(|(id, l)| (*id, l.as_str()))),
    )
}

pub const RESTART_PROMPT: &str = "Restart the daemon now? [y/N] ";

pub fn keep_text(build: &str) -> String {
    format!(
        "The daemon keeps running build {build}. Run overseer ctl restart when you are ready to finish the upgrade."
    )
}

pub fn nothing_text(build: &str) -> String {
    format!("The daemon already runs build {build}; nothing to restart.")
}

fn restarted_text(old: &str, new: &str) -> String {
    format!("Restarted the daemon (build {old} to {new}).")
}

fn started_text(build: &str) -> String {
    format!("Started the daemon (build {build}).")
}

/// Only `y` or `yes`, in any case, restarts.
fn answer_is_yes(line: &str) -> bool {
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// The build and running sessions of the daemon on `socket`, without
/// starting one. `None` when nobody answers. A daemon that predates
/// `DaemonInfo` reports build `unknown`.
fn query_daemon(socket: &Path) -> Option<RunningDaemon> {
    let rt = tokio::runtime::Runtime::new().ok()?;
    rt.block_on(async {
        let ask = async {
            let client = Client::connect(socket).await.ok()?;
            let build = match client.request(Request::DaemonInfo).await {
                Ok(Response::DaemonInfo(info)) => info.build,
                _ => "unknown".to_string(),
            };
            // An old daemon hangs up on DaemonInfo, so list on a fresh connection.
            let client = match Client::connect(socket).await {
                Ok(c) => c,
                Err(_) => client,
            };
            let running = match client.request(Request::ListSlots).await {
                Ok(Response::Slots(slots)) => slots
                    .into_iter()
                    .filter(|s| matches!(s.state, SlotState::Running { .. }))
                    .map(|s| (s.id, s.label))
                    .collect(),
                _ => Vec::new(),
            };
            Some(RunningDaemon { build, running })
        };
        tokio::time::timeout(QUERY_WAIT, ask).await.ok().flatten()
    })
}

/// Settles `Ask` by prompting on the terminal; other choices pass through.
fn settle(choice: RestartChoice) -> anyhow::Result<RestartChoice> {
    if choice != RestartChoice::Ask {
        return Ok(choice);
    }
    print!("{RESTART_PROMPT}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(if answer_is_yes(&line) {
        RestartChoice::Restart
    } else {
        RestartChoice::Keep
    })
}

/// Prints the warning when the choice could end running sessions, then
/// settles `Ask`. Returns the choice to act on.
fn warn_and_settle(
    daemon: Option<&RunningDaemon>,
    choice: RestartChoice,
) -> anyhow::Result<RestartChoice> {
    if let Some(d) = daemon.filter(|d| !d.running.is_empty()) {
        if matches!(
            choice,
            RestartChoice::Restart | RestartChoice::Ask | RestartChoice::Keep
        ) {
            println!("{}", restart_warning(d));
        }
    }
    settle(choice)
}

/// Writes this home's plist, then starts, restarts or keeps the daemon by
/// the restart rules. `Keep` and `Nothing` leave launchd alone.
pub fn install(opts: RestartOpts) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("locate current executable")?;
    // Through a symlink such as ~/.local/bin/overseer, point the agent at
    // the real binary inside the app bundle.
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let logs = log_dir();
    std::fs::create_dir_all(&logs).context("create log dir")?;
    let path = plist_path()?;
    // For a custom home this is $OVERSEER_HOME/agent.
    std::fs::create_dir_all(path.parent().unwrap()).context("create the plist folder")?;
    let path_env = agent_path(std::env::var_os("PATH"), std::env::var_os("HOME"));
    let label = this_label();
    let home = this_home();
    let extra = agent_env(home.is_some(), |k| match k {
        "OVERSEER_HOME" => home.clone(),
        _ => std::env::var_os(k),
    });
    let xml = plist_xml(&label, &exe, &logs, &path_env, &extra);

    let daemon = query_daemon(&socket_path());
    let unchanged = std::fs::read_to_string(&path).is_ok_and(|old| old == xml)
        && daemon.as_ref().is_some_and(|d| d.build == current_build());
    let choice = restart_choice(daemon.as_ref(), unchanged, opts);

    std::fs::write(&path, &xml).context("write plist")?;
    println!(
        "installed {} as {label} (runs {})",
        path.display(),
        exe.display()
    );
    match warn_and_settle(daemon.as_ref(), choice)? {
        RestartChoice::Nothing => {
            let build = daemon
                .as_ref()
                .map_or(current_build(), |d| d.build.as_str());
            println!("{}", nothing_text(build));
        }
        RestartChoice::Keep | RestartChoice::Ask => {
            let build = daemon.as_ref().map_or("unknown", |d| d.build.as_str());
            println!("{}", keep_text(build));
        }
        how @ (RestartChoice::Start | RestartChoice::Restart) => {
            bootout()?;
            shutdown_running_daemon()?;
            let status = Command::new(launchctl_program())
                .args(["bootstrap", &domain()])
                .arg(&path)
                .status()
                .context("run launchctl")?;
            if !status.success() {
                bail!("launchctl bootstrap failed ({status})");
            }
            match (how, &daemon) {
                (RestartChoice::Restart, Some(d)) => {
                    println!("{}", restarted_text(&d.build, current_build()))
                }
                _ => println!("{}", started_text(current_build())),
            }
        }
    }
    Ok(())
}

/// `ctl restart`: the restart rules with `unchanged = false`. Under this
/// home's agent it kickstarts the agent with `-k`; otherwise it asks the
/// daemon to shut down, waits for the socket to go and autostarts it again.
pub fn restart(opts: RestartOpts) -> anyhow::Result<()> {
    let daemon = query_daemon(&socket_path());
    let choice = restart_choice(daemon.as_ref(), false, opts);
    match warn_and_settle(daemon.as_ref(), choice)? {
        RestartChoice::Keep | RestartChoice::Ask | RestartChoice::Nothing => {
            let build = daemon.as_ref().map_or("unknown", |d| d.build.as_str());
            println!("{}", keep_text(build));
            return Ok(());
        }
        RestartChoice::Start | RestartChoice::Restart => {}
    }
    let under_launchd = plist_path().is_ok_and(|p| p.exists()) && agent_loaded();
    if daemon.is_some() {
        if under_launchd {
            let status = launchctl(&["kickstart", "-k", &service()])?;
            if !status.success() {
                bail!("launchctl kickstart -k {} failed ({status})", service());
            }
        } else {
            shutdown_running_daemon()?;
        }
    }
    // Wait for the daemon to answer again, starting it if nobody does.
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(crate::ctl::connect())?;
    match &daemon {
        Some(d) => println!("{}", restarted_text(&d.build, current_build())),
        None => println!("{}", started_text(current_build())),
    }
    Ok(())
}

pub fn uninstall() -> anyhow::Result<()> {
    let path = plist_path()?;
    bootout()?;
    shutdown_running_daemon()?;
    match std::fs::remove_file(&path) {
        Ok(()) => println!("removed {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("no plist at {}", path.display())
        }
        Err(e) => return Err(e).context("remove plist"),
    }
    Ok(())
}

/// Whether autostart should go through launchd. The plist looked for is
/// this home's own (its agent serves this home's socket), so whenever it
/// exists the agent is kickstarted.
pub fn use_launchd(plist_exists: bool) -> bool {
    plist_exists
}

/// Starts the installed agent's daemon. Returns false when launchd could not.
pub fn kickstart() -> bool {
    launchctl(&["kickstart", &service()]).is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_plist_contents() {
        let xml = plist_xml(
            AGENT_LABEL,
            Path::new("/opt/o verseer&co/overseer"),
            Path::new("/tmp/logs"),
            "/opt/homebrew/bin:/usr/bin",
            &[],
        );
        assert!(xml.contains("<string>daemon</string>"));
        assert!(xml.contains("<string>--foreground</string>"));
        assert!(xml.contains("<key>RunAtLoad</key><true/>"));
        assert!(xml.contains("<string>uk.co.techize.overseer</string>"));
        assert!(xml.contains("<string>/opt/o verseer&amp;co/overseer</string>"));
        assert!(xml.contains("<string>/tmp/logs/daemon.out.log</string>"));
        assert!(xml.contains("<string>/tmp/logs/daemon.err.log</string>"));
    }

    #[test]
    fn agent_restarts_only_after_a_crash() {
        let xml = plist_xml(
            AGENT_LABEL,
            Path::new("/x"),
            Path::new("/l"),
            "/usr/bin",
            &[],
        );
        let compact: String = xml.split_whitespace().collect();
        assert!(
            compact.contains("<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>"),
            "{xml}"
        );
        assert!(!compact.contains("<key>KeepAlive</key><true/>"));
    }

    #[test]
    fn agent_plist_sets_path() {
        let xml = plist_xml(
            AGENT_LABEL,
            Path::new("/x"),
            Path::new("/l"),
            "/opt/homebrew/bin:/a&b",
            &[],
        );
        let compact: String = xml.split_whitespace().collect();
        assert!(compact.contains(
            "<key>EnvironmentVariables</key><dict><key>PATH</key>\
             <string>/opt/homebrew/bin:/a&amp;b</string></dict>"
        ));
    }

    /// The text between `<key>EnvironmentVariables</key><dict>` and its `</dict>`.
    fn env_block(xml: &str) -> String {
        let compact: String = xml.split_whitespace().collect();
        let start = compact
            .find("<key>EnvironmentVariables</key><dict>")
            .unwrap()
            + "<key>EnvironmentVariables</key><dict>".len();
        let len = compact[start..].find("</dict>").unwrap();
        compact[start..start + len].to_string()
    }

    #[test]
    fn default_home_plist_is_unchanged() {
        assert_eq!(agent_label(None), "uk.co.techize.overseer");
        assert_eq!(
            plist_path_for(None, Path::new("/Users/me")),
            Path::new("/Users/me/Library/LaunchAgents/uk.co.techize.overseer.plist")
        );
        let xml = plist_xml(
            &agent_label(None),
            Path::new("/x"),
            Path::new("/l"),
            "/usr/bin",
            &[],
        );
        let block = env_block(&xml);
        assert_eq!(block.matches("<key>").count(), 1, "{block}");
        assert!(block.starts_with("<key>PATH</key>"), "{block}");
    }

    #[test]
    fn custom_home_gets_its_own_label_and_place() {
        let a = agent_label(Some(OsStr::new("/tmp/ov-a")));
        let hex = a
            .strip_prefix("uk.co.techize.overseer-home-")
            .unwrap_or_else(|| panic!("{a}"));
        assert_eq!(hex.len(), 8, "{a}");
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "{a}"
        );
        assert_eq!(a, agent_label(Some(OsStr::new("/tmp/ov-a"))));
        assert_ne!(a, agent_label(Some(OsStr::new("/tmp/ov-b"))));
        assert_ne!(a, AGENT_LABEL);
        assert_eq!(agent_label(Some(OsStr::new(""))), AGENT_LABEL);
        assert_eq!(
            plist_path_for(Some(OsStr::new("/tmp/ov-a")), Path::new("/Users/me")),
            PathBuf::from(format!("/tmp/ov-a/agent/{a}.plist"))
        );
        assert_eq!(
            plist_path_for(Some(OsStr::new("")), Path::new("/Users/me")),
            Path::new("/Users/me/Library/LaunchAgents/uk.co.techize.overseer.plist")
        );
    }

    #[test]
    fn custom_home_plist_carries_the_isolating_env() {
        let env = vec![
            ("HOME".to_string(), "/tmp/ov-a/home".to_string()),
            ("SHELL".to_string(), "/bin/sh".to_string()),
            ("OVERSEER_HOME".to_string(), "/tmp/ov-a&b<c>".to_string()),
        ];
        let label = agent_label(Some(OsStr::new("/tmp/ov-a&b<c>")));
        let xml = plist_xml(&label, Path::new("/x"), Path::new("/l"), "/usr/bin", &env);
        let block = env_block(&xml);
        assert!(
            block.starts_with("<key>PATH</key><string>/usr/bin</string>"),
            "{block}"
        );
        assert!(
            block.contains("<key>HOME</key><string>/tmp/ov-a/home</string>"),
            "{block}"
        );
        assert!(
            block.contains("<key>SHELL</key><string>/bin/sh</string>"),
            "{block}"
        );
        assert!(
            block.contains("<key>OVERSEER_HOME</key><string>/tmp/ov-a&amp;b&lt;c&gt;</string>"),
            "{block}"
        );
        assert!(xml.contains(&format!("<string>{label}</string>")));
    }

    #[test]
    fn agent_env_skips_unset_and_empty() {
        let get = |k: &str| match k {
            "HOME" => Some(OsString::from("/h")),
            "SHELL" => Some(OsString::from("")),
            "OVERSEER_HOME" => Some(OsString::from("/o")),
            "CLAUDE_CONFIG_DIR" => Some(OsString::from("/c")),
            "OVERSEER_CONFIG" => Some(OsString::from("/o/config.toml")),
            _ => None,
        };
        assert!(agent_env(false, get).is_empty());
        let pairs = |v: &[(&str, &str)]| -> Vec<(String, String)> {
            v.iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect()
        };
        assert_eq!(
            agent_env(true, get),
            pairs(&[
                ("HOME", "/h"),
                ("OVERSEER_HOME", "/o"),
                ("OVERSEER_CONFIG", "/o/config.toml"),
                ("CLAUDE_CONFIG_DIR", "/c"),
            ])
        );
    }

    #[test]
    fn agent_path_keeps_the_installing_path_without_duplicates() {
        let p = agent_path(
            Some("/Users/me/.local/bin:/opt/homebrew/bin:/Users/me/.local/bin::/usr/bin".into()),
            Some("/Users/me".into()),
        );
        assert_eq!(p, "/Users/me/.local/bin:/opt/homebrew/bin:/usr/bin");
    }

    #[test]
    fn agent_path_falls_back_to_the_usual_places() {
        let want =
            "/opt/homebrew/bin:/usr/local/bin:/Users/me/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin";
        assert_eq!(agent_path(None, Some("/Users/me".into())), want);
        assert_eq!(
            agent_path(Some("  ".into()), Some("/Users/me".into())),
            want
        );
        assert_eq!(
            agent_path(None, None),
            "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
        );
    }

    #[test]
    fn launchctl_is_the_override_when_set() {
        assert_eq!(launchctl_program_from(None), "launchctl");
        assert_eq!(launchctl_program_from(Some("".into())), "launchctl");
        assert_eq!(
            launchctl_program_from(Some("/tmp/stub".into())),
            "/tmp/stub"
        );
    }

    fn daemon(running: &[(SlotId, &str)]) -> RunningDaemon {
        RunningDaemon {
            build: "b1".to_string(),
            running: running.iter().map(|(i, l)| (*i, l.to_string())).collect(),
        }
    }

    fn opts(yes: bool, no_restart: bool, tty: bool) -> RestartOpts {
        RestartOpts {
            yes,
            no_restart,
            tty,
        }
    }

    #[test]
    fn no_daemon_starts() {
        for o in [opts(false, false, true), opts(true, true, false)] {
            assert_eq!(restart_choice(None, false, o), RestartChoice::Start);
            assert_eq!(restart_choice(None, true, o), RestartChoice::Start);
        }
    }

    #[test]
    fn same_build_and_plist_restarts_nothing() {
        let d = daemon(&[(1, "demo lane")]);
        assert_eq!(
            restart_choice(Some(&d), true, opts(true, false, true)),
            RestartChoice::Nothing
        );
        assert_eq!(
            restart_choice(Some(&daemon(&[])), true, opts(false, false, false)),
            RestartChoice::Nothing
        );
    }

    #[test]
    fn no_running_session_restarts_without_asking() {
        let d = daemon(&[]);
        assert_eq!(
            restart_choice(Some(&d), false, opts(false, false, true)),
            RestartChoice::Restart
        );
        assert_eq!(
            restart_choice(Some(&d), false, opts(false, false, false)),
            RestartChoice::Restart
        );
    }

    #[test]
    fn running_sessions_ask_on_a_terminal() {
        let d = daemon(&[(1, "demo lane")]);
        assert_eq!(
            restart_choice(Some(&d), false, opts(false, false, true)),
            RestartChoice::Ask
        );
    }

    #[test]
    fn running_sessions_without_a_terminal_are_kept() {
        let d = daemon(&[(1, "demo lane")]);
        assert_eq!(
            restart_choice(Some(&d), false, opts(false, false, false)),
            RestartChoice::Keep
        );
    }

    #[test]
    fn yes_restarts_and_no_restart_keeps() {
        let d = daemon(&[(1, "demo lane")]);
        assert_eq!(
            restart_choice(Some(&d), false, opts(true, false, false)),
            RestartChoice::Restart
        );
        assert_eq!(
            restart_choice(Some(&d), false, opts(false, true, true)),
            RestartChoice::Keep
        );
        // --no-restart wins over --yes.
        assert_eq!(
            restart_choice(Some(&d), false, opts(true, true, true)),
            RestartChoice::Keep
        );
    }

    #[test]
    fn warning_names_each_running_session() {
        assert_eq!(
            restart_warning(&daemon(&[(1, "demo lane"), (3, "docs")])),
            "The daemon (build b1) has 2 running sessions: S1 demo lane, S3 docs. \
             Restarting it ends them; each comes back as resumable, and r on its card resumes it."
        );
        let one = restart_warning(&daemon(&[(4, "")]));
        assert!(
            one.starts_with("The daemon (build b1) has 1 running session: S4. Restarting"),
            "{one}"
        );
    }

    #[test]
    fn outcome_texts() {
        assert_eq!(
            keep_text("b1"),
            "The daemon keeps running build b1. Run overseer ctl restart when you are ready to finish the upgrade."
        );
        assert_eq!(
            nothing_text("b1"),
            "The daemon already runs build b1; nothing to restart."
        );
        assert_eq!(
            restarted_text("b1", "b2"),
            "Restarted the daemon (build b1 to b2)."
        );
        assert_eq!(started_text("b2"), "Started the daemon (build b2).");
        assert_eq!(RESTART_PROMPT, "Restart the daemon now? [y/N] ");
    }

    #[test]
    fn only_y_or_yes_restarts() {
        for a in ["y", "Y", "yes", " YES \n"] {
            assert!(answer_is_yes(a), "{a:?}");
        }
        for a in ["", "\n", "n", "no", "yep", "y es"] {
            assert!(!answer_is_yes(a), "{a:?}");
        }
    }

    #[test]
    fn overseer_home_is_absolute_without_a_trailing_slash() {
        assert_eq!(canonical_home(OsStr::new("/tmp/ov-a/")), "/tmp/ov-a");
        assert_eq!(canonical_home(OsStr::new("/tmp//ov-a/./")), "/tmp/ov-a");
        assert_eq!(canonical_home(OsStr::new("/tmp/ov-a")), "/tmp/ov-a");
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            canonical_home(OsStr::new("ov-a/")),
            cwd.join("ov-a").into_os_string()
        );
        assert_eq!(
            agent_label(Some(&canonical_home(OsStr::new("/tmp/ov-a/")))),
            agent_label(Some(OsStr::new("/tmp/ov-a")))
        );
    }

    #[test]
    fn autostart_uses_this_homes_agent_when_its_plist_exists() {
        assert!(use_launchd(true));
        assert!(!use_launchd(false));
    }
}
