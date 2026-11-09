//! LaunchAgent plist generation and install/uninstall.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context};
use overseer_proto::paths::{log_dir, AGENT_LABEL};

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The LaunchAgent plist for running the daemon in the foreground under launchd.
pub fn plist_xml(exe: &Path, log_dir: &Path) -> String {
    let exe = xml_escape(&exe.display().to_string());
    let out = xml_escape(&log_dir.join("daemon.out.log").display().to_string());
    let err = xml_escape(&log_dir.join("daemon.err.log").display().to_string());
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
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>StandardOutPath</key>
    <string>{out}</string>
    <key>StandardErrorPath</key>
    <string>{err}</string>
</dict>
</plist>
"#
    )
}

fn plist_path() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{AGENT_LABEL}.plist")))
}

fn domain() -> String {
    // SAFETY: getuid has no preconditions and cannot fail.
    format!("gui/{}", unsafe { libc::getuid() })
}

pub fn install() -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("locate current executable")?;
    let logs = log_dir();
    std::fs::create_dir_all(&logs).context("create log dir")?;
    let path = plist_path()?;
    std::fs::create_dir_all(path.parent().unwrap()).context("create LaunchAgents dir")?;
    std::fs::write(&path, plist_xml(&exe, &logs)).context("write plist")?;
    let status = Command::new("launchctl")
        .args(["bootstrap", &domain()])
        .arg(&path)
        .status()
        .context("run launchctl")?;
    if !status.success() {
        bail!("launchctl bootstrap failed ({status})");
    }
    println!("installed {}", path.display());
    Ok(())
}

pub fn uninstall() -> anyhow::Result<()> {
    let path = plist_path()?;
    // bootout fails when the agent is not loaded; removing the plist is still right.
    let status = Command::new("launchctl")
        .args(["bootout", &format!("{}/{AGENT_LABEL}", domain())])
        .status()
        .context("run launchctl")?;
    if !status.success() {
        eprintln!("launchctl bootout reported {status} (agent may not have been loaded)");
    }
    match std::fs::remove_file(&path) {
        Ok(()) => println!("removed {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("no plist at {}", path.display())
        }
        Err(e) => return Err(e).context("remove plist"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_plist_contents() {
        let xml = plist_xml(
            Path::new("/opt/o verseer&co/overseer"),
            Path::new("/tmp/logs"),
        );
        assert!(xml.contains("<string>daemon</string>"));
        assert!(xml.contains("<string>--foreground</string>"));
        assert!(xml.contains("<key>KeepAlive</key><true/>"));
        assert!(xml.contains("<key>RunAtLoad</key><true/>"));
        assert!(xml.contains("<string>uk.co.techize.overseer</string>"));
        assert!(xml.contains("/opt/o verseer&amp;co/overseer"));
        assert!(xml.contains("/tmp/logs/daemon.out.log"));
    }
}
