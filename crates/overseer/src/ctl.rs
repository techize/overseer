//! `overseer ctl` subcommands and daemon autostart.

use std::fs::OpenOptions;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context};
use clap::Subcommand;
use overseer_proto::paths::{log_dir, socket_path};
use overseer_proto::{
    slot_name, Client, ItemKind, Request, Response, SlotId, SlotInfo, SlotState, WorkItem, WorkView,
};

#[derive(Subcommand)]
pub enum CtlCmd {
    /// One line per slot.
    Status,
    /// Start a session in a slot.
    Start {
        n: SlotId,
        #[arg(long)]
        cwd: PathBuf,
        #[arg(long, default_value = "")]
        label: String,
        /// First prompt, passed to claude at launch.
        #[arg(long, allow_hyphen_values = true)]
        prompt: Option<String>,
        /// Command to run instead of the daemon default.
        #[arg(last = true)]
        cmd: Vec<String>,
    },
    /// Stop the session in a slot.
    Stop { n: SlotId },
    /// Continue a session that was running when the daemon stopped.
    Resume { n: SlotId },
    /// Type text into a slot, followed by Enter.
    Send { n: SlotId, text: String },
    /// List work items with owners, source health and notices.
    Items {
        /// Print the raw WorkView as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Give a work item to a slot.
    Assign {
        slot: SlotId,
        key: String,
        /// Accept the warning about overlapping work: a bead whose ticket
        /// another slot owns, or a ticket with beads another slot owns.
        #[arg(long)]
        yes: bool,
        /// Take the item from its current owner.
        #[arg(long = "move")]
        move_owner: bool,
    },
    /// Free a work item.
    Release { key: String },
    /// Re-read the work sources now.
    Refresh,
    /// Print the kickoff text for a slot's assigned items.
    Kickoff { n: SlotId },
    /// Stop the daemon and every session.
    Shutdown,
}

/// Starts the daemon. With the LaunchAgent installed (and the default state
/// dir) this kickstarts the agent, so the daemon runs under launchd with the
/// agent's PATH. Otherwise it spawns `current_exe() daemon` in its own
/// session, logging to `daemon.log`.
pub fn spawn_daemon() -> io::Result<()> {
    let plist_exists = crate::agent::plist_path().is_ok_and(|p| p.exists());
    if crate::agent::use_launchd(std::env::var_os("OVERSEER_HOME"), plist_exists)
        && crate::agent::kickstart()
    {
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    let dir = log_dir();
    // If the log cannot be opened the daemon still starts, just silently.
    let log = std::fs::create_dir_all(&dir)
        .and_then(|()| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("daemon.log"))
        })
        .ok();
    let (out, err) = match log.and_then(|f| f.try_clone().ok().map(|c| (f, c))) {
        Some((a, b)) => (Stdio::from(a), Stdio::from(b)),
        None => (Stdio::null(), Stdio::null()),
    };
    let mut cmd = Command::new(exe);
    cmd.arg("daemon")
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err);
    // SAFETY: setsid is async-signal-safe and touches no Rust state.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn().map(drop)
}

pub async fn connect() -> anyhow::Result<Client> {
    Client::connect_or_start(socket_path(), spawn_daemon).await
}

fn tilde(p: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        if let Ok(rest) = p.strip_prefix(&home) {
            return if rest.as_os_str().is_empty() {
                "~".into()
            } else {
                format!("~/{}", rest.display())
            };
        }
    }
    p.display().to_string()
}

pub fn format_slot(s: &SlotInfo) -> String {
    let name = slot_name(s.id);
    let mut line = match &s.state {
        SlotState::Empty => return format!("{name}  empty"),
        SlotState::Running { pid, .. } => format!("{name}  running  pid {pid}"),
        SlotState::Exited { code: Some(c), .. } => format!("{name}  exited  code {c}"),
        SlotState::Exited { code: None, .. } => format!("{name}  exited"),
        SlotState::Resumable { .. } => format!("{name}  resumable"),
    };
    if let Some(cwd) = &s.cwd {
        line.push_str(&format!("  {}", tilde(cwd)));
    }
    if !s.label.is_empty() {
        line.push_str(&format!("  {}", s.label));
    }
    line
}

fn expect_ok(resp: Response) -> anyhow::Result<()> {
    match resp {
        Response::Ok => Ok(()),
        Response::Error { message } => bail!("{message}"),
        other => bail!("unexpected reply: {other:?}"),
    }
}

/// Unix seconds as local HH:MM.
fn local_hhmm(at: i64) -> String {
    let t = at as libc::time_t;
    // SAFETY: localtime_r writes only into the zeroed tm we pass.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return "--:--".into();
        }
        tm
    };
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

fn item_line(i: &WorkItem, indent: &str) -> String {
    let owner = i.owner.map(slot_name).unwrap_or_else(|| "-".into());
    let gone = if i.in_list { "" } else { "  (left the list)" };
    format!(
        "{indent}{}  {}  {owner}  {}{gone}",
        i.key, i.status, i.title
    )
}

/// Source lines, then tickets with their beads indented beneath, then
/// notices. A bead whose ticket is not listed stands at the top level.
pub fn format_work(v: &WorkView) -> Vec<String> {
    let mut out = Vec::new();
    for s in &v.sources {
        let name = &s.name;
        out.push(if s.ok {
            let at =
                s.at.map(|a| format!("  {}", local_hhmm(a)))
                    .unwrap_or_default();
            format!("{name}  ok  {} items{at}", s.count)
        } else {
            format!("{name}  {}", s.message.as_deref().unwrap_or("failed"))
        });
    }
    let is_child = |i: &WorkItem| {
        i.kind == ItemKind::Bead
            && i.parent
                .as_ref()
                .is_some_and(|p| v.items.iter().any(|t| &t.key == p))
    };
    for t in v.items.iter().filter(|i| !is_child(i)) {
        out.push(item_line(t, ""));
        for b in v
            .items
            .iter()
            .filter(|b| is_child(b) && b.parent.as_deref() == Some(t.key.as_str()))
        {
            out.push(item_line(b, "  "));
        }
    }
    for n in &v.notices {
        out.push(format!("notice  {}  {}", slot_name(n.slot), n.text));
    }
    out
}

pub async fn run(cmd: CtlCmd) -> anyhow::Result<()> {
    let client = connect().await?;
    match cmd {
        CtlCmd::Status => match client.request(Request::ListSlots).await? {
            Response::Slots(slots) => {
                for s in &slots {
                    println!("{}", format_slot(s));
                }
                Ok(())
            }
            Response::Error { message } => bail!("{message}"),
            other => bail!("unexpected reply: {other:?}"),
        },
        CtlCmd::Start {
            n,
            cwd,
            label,
            prompt,
            cmd,
        } => {
            let cwd = std::path::absolute(&cwd).context("resolve --cwd")?;
            let command = (!cmd.is_empty()).then_some(cmd);
            expect_ok(
                client
                    .request(Request::StartSlot {
                        id: n,
                        label,
                        cwd,
                        command,
                        prompt,
                    })
                    .await?,
            )
        }
        CtlCmd::Stop { n } => expect_ok(client.request(Request::StopSlot { id: n }).await?),
        CtlCmd::Resume { n } => expect_ok(client.request(Request::ResumeSlot { id: n }).await?),
        CtlCmd::Send { n, text } => {
            client.send_input(n, format!("{text}\r").as_bytes()).await?;
            // Input is fire-and-forget; a round trip proves the daemon read it
            // (it handles frames in order) before this process exits.
            expect_ok(
                client
                    .request(Request::Hello {
                        client: "ctl".into(),
                    })
                    .await?,
            )
        }
        CtlCmd::Items { json } => match client.request(Request::ListWorkItems).await? {
            Response::Work(v) => {
                if json {
                    println!("{}", serde_json::to_string(&v)?);
                } else {
                    for l in format_work(&v) {
                        println!("{l}");
                    }
                }
                Ok(())
            }
            Response::Error { message } => bail!("{message}"),
            other => bail!("unexpected reply: {other:?}"),
        },
        CtlCmd::Assign {
            slot,
            key,
            yes,
            move_owner,
        } => match client
            .request(Request::Assign {
                key,
                slot,
                confirm: yes,
                move_owner,
            })
            .await?
        {
            Response::NeedsConfirm { message } => {
                println!("{message}");
                std::process::exit(2);
            }
            other => expect_ok(other),
        },
        CtlCmd::Release { key } => expect_ok(client.request(Request::Release { key }).await?),
        CtlCmd::Refresh => match client.request(Request::RefreshWorkItems).await? {
            Response::Work(_) => Ok(()),
            Response::Error { message } => bail!("{message}"),
            other => bail!("unexpected reply: {other:?}"),
        },
        CtlCmd::Kickoff { n } => match client.request(Request::Kickoff { slot: n }).await? {
            Response::Text { text } => {
                println!("{text}");
                Ok(())
            }
            Response::Error { message } => bail!("{message}"),
            other => bail!("unexpected reply: {other:?}"),
        },
        CtlCmd::Shutdown => expect_ok(client.request(Request::Shutdown).await?),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(clap::Parser)]
    struct Cli {
        #[command(subcommand)]
        cmd: CtlCmd,
    }

    #[test]
    fn prompt_may_start_with_a_dash() {
        use clap::Parser;
        for p in ["- fix login", "--help me"] {
            let cli =
                Cli::try_parse_from(["t", "start", "3", "--cwd", "/tmp", "--prompt", p]).unwrap();
            match cli.cmd {
                CtlCmd::Start { prompt, .. } => assert_eq!(prompt.as_deref(), Some(p)),
                _ => panic!("not start"),
            }
        }
    }

    #[test]
    fn resumable_status_line() {
        let s = SlotInfo {
            id: 3,
            label: "eks".into(),
            cwd: None,
            command: None,
            state: SlotState::Resumable {
                session_id: "abc".into(),
                at: 1,
            },
            cols: 80,
            rows: 24,
            activity: None,
        };
        assert_eq!(format_slot(&s), "S3  resumable  eks");
    }
}
