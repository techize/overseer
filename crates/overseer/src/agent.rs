//! LaunchAgent plist generation and install/uninstall.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use overseer_proto::paths::{log_dir, socket_path, AGENT_LABEL};
use overseer_proto::{Client, Request};

/// How long to wait for launchd to finish removing a booted-out agent.
const BOOTOUT_WAIT: Duration = Duration::from_secs(10);

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
pub fn plist_xml(exe: &Path, log_dir: &Path, path_env: &str) -> String {
    let exe = xml_escape(&exe.display().to_string());
    let out = xml_escape(&log_dir.join("daemon.out.log").display().to_string());
    let err = xml_escape(&log_dir.join("daemon.err.log").display().to_string());
    let path_env = xml_escape(path_env);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{AGENT_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>daemon</string>
        <string>--foreground</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>{path_env}</string>
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

pub fn plist_path() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{AGENT_LABEL}.plist")))
}

fn domain() -> String {
    // SAFETY: getuid has no preconditions and cannot fail.
    format!("gui/{}", unsafe { libc::getuid() })
}

fn service() -> String {
    format!("{}/{AGENT_LABEL}", domain())
}

fn launchctl(args: &[&str]) -> anyhow::Result<std::process::ExitStatus> {
    Command::new("launchctl")
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

pub fn install() -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("locate current executable")?;
    // Through a symlink such as ~/.local/bin/overseer, point the agent at
    // the real binary inside the app bundle.
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let logs = log_dir();
    std::fs::create_dir_all(&logs).context("create log dir")?;
    let path = plist_path()?;
    std::fs::create_dir_all(path.parent().unwrap()).context("create LaunchAgents dir")?;
    let path_env = agent_path(std::env::var_os("PATH"), std::env::var_os("HOME"));

    bootout()?;
    shutdown_running_daemon()?;
    std::fs::write(&path, plist_xml(&exe, &logs, &path_env)).context("write plist")?;
    let status = Command::new("launchctl")
        .args(["bootstrap", &domain()])
        .arg(&path)
        .status()
        .context("run launchctl")?;
    if !status.success() {
        bail!("launchctl bootstrap failed ({status})");
    }
    println!("installed {} (runs {})", path.display(), exe.display());
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

/// Whether autostart should go through launchd: only for the default state
/// dir (the agent's daemon serves that socket) and only once the agent is
/// installed.
pub fn use_launchd(overseer_home: Option<OsString>, plist_exists: bool) -> bool {
    overseer_home.is_none_or(|h| h.is_empty()) && plist_exists
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
            Path::new("/opt/o verseer&co/overseer"),
            Path::new("/tmp/logs"),
            "/opt/homebrew/bin:/usr/bin",
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
        let xml = plist_xml(Path::new("/x"), Path::new("/l"), "/usr/bin");
        let compact: String = xml.split_whitespace().collect();
        assert!(
            compact.contains("<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>"),
            "{xml}"
        );
        assert!(!compact.contains("<key>KeepAlive</key><true/>"));
    }

    #[test]
    fn agent_plist_sets_path() {
        let xml = plist_xml(Path::new("/x"), Path::new("/l"), "/opt/homebrew/bin:/a&b");
        let compact: String = xml.split_whitespace().collect();
        assert!(compact.contains(
            "<key>EnvironmentVariables</key><dict><key>PATH</key>\
             <string>/opt/homebrew/bin:/a&amp;b</string></dict>"
        ));
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
    fn autostart_uses_launchd_only_for_the_default_home_with_an_agent() {
        assert!(use_launchd(None, true));
        assert!(use_launchd(Some("".into()), true));
        assert!(!use_launchd(None, false));
        assert!(!use_launchd(Some("/tmp/x".into()), true));
    }
}
