//! What the app asks of the daemon itself rather than of its settings:
//! which build it runs (`probe_daemon`) and restarting it from Settings
//! (`run_restart`).

use std::path::PathBuf;
use std::time::Duration;

use overseer_proto::{Client, DaemonInfo, Request, Response};

/// What the app runs, as its own binary, to restart the daemon (Decision 9).
pub const RESTART_ARGS: [&str; 3] = ["ctl", "restart", "--yes"];

fn last_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).rfind(|l| !l.is_empty())
}

/// The status line once `ctl restart --yes` has exited: what it printed,
/// or the last line of its error.
pub fn restart_outcome(success: bool, stdout: &str, stderr: &str) -> String {
    if success {
        return last_line(stdout)
            .unwrap_or("Restarted the daemon.")
            .to_string();
    }
    match last_line(stderr) {
        Some(why) => format!("The daemon did not restart: {why}"),
        None => "The daemon did not restart, and said nothing about why. See gui.log.".to_string(),
    }
}

/// Runs `<exe> ctl restart --yes` with the app's environment, waits for it,
/// logs what it printed to `gui.log` and returns the status line. Blocks.
pub fn run_restart(exe: std::io::Result<PathBuf>) -> String {
    let exe = match exe {
        Ok(exe) => exe,
        Err(e) => {
            return format!("The daemon did not restart: cannot find this app's binary ({e})")
        }
    };
    let out = std::process::Command::new(&exe)
        .args(RESTART_ARGS)
        .stdin(std::process::Stdio::null())
        .output();
    match out {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !stdout.trim().is_empty() {
                tracing::info!("ctl restart: {}", stdout.trim());
            }
            if !stderr.trim().is_empty() {
                tracing::warn!("ctl restart ({}): {}", out.status, stderr.trim());
            }
            restart_outcome(out.status.success(), &stdout, &stderr)
        }
        Err(e) => format!(
            "The daemon did not restart: cannot run {} ({e})",
            exe.display()
        ),
    }
}

/// What the app knows about the daemon it is connected to.
#[derive(Debug, Clone, PartialEq)]
pub enum DaemonKnown {
    /// Not asked yet, or it did not answer.
    Unknown,
    Info(DaemonInfo),
    /// It answers but predates `DaemonInfo` (it hangs up on the request).
    Old,
}

/// How long asking the daemon about itself may take.
const PROBE_WAIT: Duration = Duration::from_secs(2);

/// Asks the daemon on `socket` about itself on a connection of its own: an
/// old daemon hangs up on a request it does not know, and that must not
/// take the app's connection down with it. Needs a tokio runtime.
pub async fn probe_daemon(socket: PathBuf) -> DaemonKnown {
    let ask = async {
        let client = Client::connect(&socket).await.ok()?;
        Some(match client.request(Request::DaemonInfo).await {
            Ok(Response::DaemonInfo(info)) => DaemonKnown::Info(info),
            Ok(Response::Error { .. }) | Err(_) => DaemonKnown::Old,
            Ok(_) => DaemonKnown::Unknown,
        })
    };
    tokio::time::timeout(PROBE_WAIT, ask)
        .await
        .ok()
        .flatten()
        .unwrap_or(DaemonKnown::Unknown)
}

/// The status line for a daemon that predates `DaemonInfo`.
pub const OLD_DAEMON_NOTICE: &str = "The daemon runs a build older than this app. Restart the daemon to finish the upgrade (overseer ctl restart, or Settings).";

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> DaemonInfo {
        DaemonInfo {
            version: "0.1.0".into(),
            build: "b1".into(),
            pid: 42,
            started_at: 0,
            exe: "/tmp/demo/overseer".into(),
        }
    }

    #[test]
    fn restart_outcome_reports_the_result_or_the_error() {
        assert_eq!(RESTART_ARGS, ["ctl", "restart", "--yes"]);
        assert_eq!(
            restart_outcome(true, "Restarted the daemon (build b1 to b2).\n", ""),
            "Restarted the daemon (build b1 to b2)."
        );
        assert_eq!(restart_outcome(true, "", ""), "Restarted the daemon.");
        assert_eq!(
            restart_outcome(
                false,
                "",
                "warning\nError: launchctl kickstart -k demo failed\n\n"
            ),
            "The daemon did not restart: Error: launchctl kickstart -k demo failed"
        );
        assert_eq!(
            restart_outcome(false, "", ""),
            "The daemon did not restart, and said nothing about why. See gui.log."
        );
    }

    /// A fake daemon on a temp socket: answers each request with `reply`,
    /// or hangs up on it when `reply` is None, as a daemon that predates
    /// `DaemonInfo` does.
    async fn fake_daemon(socket: PathBuf, reply: Option<Response>) {
        use overseer_proto::{read_frame, write_frame, Frame};
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (mut rd, mut wr) = stream.into_split();
            if let Ok(Some(Frame::Request { seq, .. })) = read_frame(&mut rd).await {
                if let Some(resp) = reply {
                    let _ = write_frame(&mut wr, &Frame::Response { seq, resp }).await;
                }
            }
        });
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn probe_tells_a_known_daemon_from_an_old_one() {
        let dir = tempfile::tempdir().unwrap();
        let info = info();
        let new = dir.path().join("new.sock");
        fake_daemon(new.clone(), Some(Response::DaemonInfo(info.clone()))).await;
        assert_eq!(probe_daemon(new).await, DaemonKnown::Info(info));
        let old = dir.path().join("old.sock");
        fake_daemon(old.clone(), None).await;
        assert_eq!(probe_daemon(old).await, DaemonKnown::Old);
        let refusing = dir.path().join("refusing.sock");
        let error = Response::Error {
            message: "unknown request".into(),
        };
        fake_daemon(refusing.clone(), Some(error)).await;
        assert_eq!(probe_daemon(refusing).await, DaemonKnown::Old);
        assert_eq!(
            probe_daemon(dir.path().join("none.sock")).await,
            DaemonKnown::Unknown
        );
    }
}
