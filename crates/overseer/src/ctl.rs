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
    slot_name, BrainView, ClaimKind, Client, ItemKind, LockView, QueueState, RelayState, Request,
    Response, SlotId, SlotInfo, SlotState, WorkItem, WorkView,
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
    /// Queue a message for a slot's session. It is typed in when the session
    /// is idle, as `$OVERSEER_SLOT` when set and as you otherwise.
    Send {
        n: SlotId,
        text: String,
        /// Type the text in at once, followed by Enter, without queueing.
        #[arg(long)]
        raw: bool,
    },
    /// List held targets, who waits for them and pending relays.
    Locks {
        /// Print the raw LockView as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Claim a write target for a slot, queueing it if another slot holds it.
    Claim {
        slot: SlotId,
        target: String,
        reason: Option<String>,
    },
    /// Take a slot's hold on a target away and grant it to the next waiter.
    ForceRelease { target: String },
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
    /// Free a work item (`release <key>`) or a claimed target (`release
    /// <slot> <target>`).
    Release {
        first: String,
        target: Option<String>,
    },
    /// Re-read the work sources now.
    Refresh,
    /// Print the kickoff text for a slot's assigned items.
    Kickoff { n: SlotId },
    /// Commit and push files in the 2ndBrain through the daemon's queue.
    /// Paths are relative to the vault or absolute.
    Commit {
        #[arg(short, long)]
        message: String,
        #[arg(required = true)]
        paths: Vec<String>,
    },
    /// Add text to a section of today's journal. `-` reads the text from stdin.
    Journal {
        section: String,
        #[arg(allow_hyphen_values = true)]
        text: String,
    },
    /// Set a slot's section of CURRENT_SESSION.md. `-` reads the text from
    /// stdin. The slot is `--slot`, else `$OVERSEER_SLOT`.
    CurrentSession {
        #[arg(long)]
        slot: Option<String>,
        #[arg(allow_hyphen_values = true)]
        text: String,
    },
    /// Run a bd write under the 2ndBrain lock: `ctl bd -- update ov-3 ...`.
    /// Prints bd's output and exits with its code. Stdin, when it is not a
    /// terminal, is passed to bd.
    Bd {
        #[arg(last = true, allow_hyphen_values = true, required = true)]
        args: Vec<String>,
    },
    /// The 2ndBrain writer: commit queue, last backup and sync, push trouble.
    Brain {
        /// Print the raw BrainView as JSON.
        #[arg(long)]
        json: bool,
    },
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

/// Between `send --raw`'s text and its Enter, as the courier waits.
const RAW_RETURN_DELAY: std::time::Duration = std::time::Duration::from_millis(100);

/// Input is fire-and-forget; a round trip proves the daemon read it (it
/// handles frames in order) before going on or exiting.
async fn synced(client: &overseer_proto::client::Client) -> anyhow::Result<()> {
    expect_ok(
        client
            .request(Request::Hello {
                client: "ctl".into(),
            })
            .await?,
    )
}

fn expect_ok(resp: Response) -> anyhow::Result<()> {
    match resp {
        Response::Ok => Ok(()),
        Response::Error { message } => bail!("{message}"),
        other => bail!("unexpected reply: {other:?}"),
    }
}

/// Unix seconds as local HH:MM.
pub(crate) fn local_hhmm(at: i64) -> String {
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

/// A slot given as `3` or `S3`.
fn parse_slot(text: &str) -> anyhow::Result<SlotId> {
    let name = if text.starts_with('S') {
        text.to_string()
    } else {
        format!("S{text}")
    };
    crate::hook::slot(&name).with_context(|| format!("not a slot: {text}"))
}

/// Cut to `max` chars on one line, with `...` when cut.
fn cut(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or("");
    if line.chars().count() > max || text.lines().count() > 1 {
        let head: String = line.chars().take(max.saturating_sub(3)).collect();
        format!("{head}...")
    } else {
        line.to_string()
    }
}

/// One line per claim (`repo:demo-api:main  S1  command  since 08:40  for
/// DEMO-33`, plus `  at <rev>` for a pin and `  held long` when flagged)
/// with its waiters beneath, then pending and dropped relays. Waits on a
/// target nobody holds follow the claims.
pub fn format_locks(v: &LockView, now: i64) -> Vec<String> {
    let kind = |k: &ClaimKind| match k {
        ClaimKind::Command => "command",
        ClaimKind::Grant => "grant",
        ClaimKind::Claim => "claim",
        ClaimKind::Pin => "pin",
    };
    let waiting = |w: &overseer_proto::Wait| {
        format!(
            "  waiting: {} since {}",
            slot_name(w.slot),
            local_hhmm(w.at)
        )
    };
    let mut out = Vec::new();
    for c in &v.claims {
        let mut line = format!(
            "{}  {}  {}  since {}",
            c.target,
            slot_name(c.slot),
            kind(&c.kind),
            local_hhmm(c.since)
        );
        if let Some(why) = c.item.as_deref().or(c.reason.as_deref()) {
            line.push_str(&format!("  for {why}"));
        }
        if let Some(rev) = &c.pin_revision {
            line.push_str(&format!("  at {rev}"));
        }
        if v.flagged(c, now) {
            line.push_str("  held long");
        }
        out.push(line);
        out.extend(v.waits.iter().filter(|w| w.target == c.target).map(waiting));
    }
    for w in v
        .waits
        .iter()
        .filter(|w| !v.claims.iter().any(|c| c.target == w.target))
    {
        out.push(w.target.clone());
        out.push(waiting(w));
    }
    for r in &v.relays {
        let state = match (r.state, r.held.as_deref()) {
            (RelayState::Delivered, _) => continue,
            (RelayState::Dropped, _) => "dropped: session ended".to_string(),
            // No reason from the courier yet: it simply has not been delivered.
            (RelayState::Pending, None) => "pending".to_string(),
            (RelayState::Pending, Some(why)) => format!("pending ({why})"),
        };
        out.push(format!(
            "relay  {}  {state}  {}",
            slot_name(r.to),
            cut(&r.text, 80)
        ));
    }
    out
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Prints a `Text` reply; `Queued` prints its message and exits 2.
fn print_text_or_queued(resp: Response) -> anyhow::Result<()> {
    match resp {
        Response::Text { text } => {
            println!("{text}");
            Ok(())
        }
        Response::Queued { message } => {
            println!("{message}");
            std::process::exit(2);
        }
        Response::Error { message } => bail!("{message}"),
        other => bail!("unexpected reply: {other:?}"),
    }
}

/// `$OVERSEER_SLOT`, when it names a slot: who a 2ndBrain write is from.
fn env_slot() -> Option<SlotId> {
    std::env::var("OVERSEER_SLOT")
        .ok()
        .and_then(|s| crate::hook::slot(&s))
}

/// The argument, or all of stdin when it is `-` (trailing newlines cut).
fn text_or_stdin(text: String) -> anyhow::Result<String> {
    if text != "-" {
        return Ok(text);
    }
    let mut buf = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf).context("read stdin")?;
    Ok(buf.trim_end_matches('\n').to_string())
}

/// Stdin for `ctl bd` (a heredoc the hook kept), unless it is a terminal
/// or empty.
fn piped_stdin() -> Option<String> {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        return None;
    }
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut std::io::stdin(), &mut bytes).ok()?;
    (!bytes.is_empty()).then(|| String::from_utf8_lossy(&bytes).into_owned())
}

/// Prints a writer reply. A `Queued` entry is durable and will be committed,
/// so it is success: scripts must not retry it.
fn print_brain_text(resp: Response) -> anyhow::Result<()> {
    match resp {
        Response::Text { text } | Response::Queued { message: text } => {
            println!("{text}");
            Ok(())
        }
        // `current-session` answers a bare Ok.
        Response::Ok => Ok(()),
        Response::Error { message } => bail!("{message}"),
        other => bail!("unexpected reply: {other:?}"),
    }
}

/// A short human summary of the writer.
pub fn format_brain(v: &BrainView) -> Vec<String> {
    if !v.on {
        return vec!["2ndBrain writer is off (brain_writer in config.toml)".into()];
    }
    let count = |st: QueueState| v.entries.iter().filter(|e| e.state == st).count();
    let when = |at: Option<i64>| at.map(local_hhmm).unwrap_or_else(|| "never".into());
    let mut out = vec![
        "2ndBrain writer is on".to_string(),
        format!(
            "queue  {} queued  {} committed  {} nothing to commit  {} failed",
            count(QueueState::Queued),
            count(QueueState::Committed),
            count(QueueState::NothingToCommit),
            count(QueueState::Failed)
        ),
        format!("last backup  {}", when(v.last_backup)),
        match v.last_sync {
            Some((at, ok)) => format!(
                "last sync  {}  {}",
                local_hhmm(at),
                if ok { "ok" } else { "failed" }
            ),
            None => "last sync  never".to_string(),
        },
    ];
    if let Some(p) = &v.push_problem {
        out.push(format!("push problem  {p}"));
    }
    out
}

fn item_line(v: &WorkView, i: &WorkItem, indent: &str) -> String {
    let owner = i.owner.map(slot_name).unwrap_or_else(|| "-".into());
    let gone = if i.in_list {
        ""
    } else if v.not_read_yet(i) {
        "  (not read yet)"
    } else {
        "  (left the list)"
    };
    format!(
        "{indent}{}  {}  {owner}  {}{gone}",
        i.key, i.status, i.title
    )
}

/// One line per source: `beads  ok  4 items  14:05` or `jira  <why not>`.
pub fn format_sources(v: &WorkView) -> Vec<String> {
    v.sources
        .iter()
        .map(|s| {
            let name = &s.name;
            if s.ok {
                let at =
                    s.at.map(|a| format!("  {}", local_hhmm(a)))
                        .unwrap_or_default();
                format!("{name}  ok  {} items{at}", s.count)
            } else {
                format!("{name}  {}", s.message.as_deref().unwrap_or("failed"))
            }
        })
        .collect()
}

/// Source lines, then tickets with their beads indented beneath, then
/// notices. A bead whose ticket is not listed stands at the top level.
pub fn format_work(v: &WorkView) -> Vec<String> {
    let mut out = format_sources(v);
    let is_child = |i: &WorkItem| {
        i.kind == ItemKind::Bead
            && i.parent
                .as_ref()
                .is_some_and(|p| v.items.iter().any(|t| &t.key == p))
    };
    for t in v.items.iter().filter(|i| !is_child(i)) {
        out.push(item_line(v, t, ""));
        for b in v
            .items
            .iter()
            .filter(|b| is_child(b) && b.parent.as_deref() == Some(t.key.as_str()))
        {
            out.push(item_line(v, b, "  "));
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
        CtlCmd::Send { n, text, raw: true } => {
            // Text and Enter go apart: Claude Code reads a long burst as a
            // paste, and an Enter inside a paste is a newline, not a submit.
            client.send_input(n, text.as_bytes()).await?;
            synced(&client).await?;
            tokio::time::sleep(RAW_RETURN_DELAY).await;
            client.send_input(n, b"\r").await?;
            synced(&client).await
        }
        CtlCmd::Send {
            n,
            text,
            raw: false,
        } => {
            let from = env_slot();
            print_text_or_queued(
                client
                    .request(Request::SendRelay { from, to: n, text })
                    .await?,
            )
        }
        CtlCmd::Commit { message, paths } => print_brain_text(
            client
                .request(Request::Commit {
                    from: env_slot(),
                    paths,
                    message,
                })
                .await?,
        ),
        CtlCmd::Journal { section, text } => print_brain_text(
            client
                .request(Request::AppendJournal {
                    from: env_slot(),
                    section,
                    text: text_or_stdin(text)?,
                })
                .await?,
        ),
        CtlCmd::CurrentSession { slot, text } => {
            let slot = match slot {
                Some(s) => parse_slot(&s)?,
                None => env_slot().context("name the slot with --slot S<n>")?,
            };
            print_brain_text(
                client
                    .request(Request::UpdateCurrentSession {
                        slot,
                        text: text_or_stdin(text)?,
                    })
                    .await?,
            )
        }
        CtlCmd::Bd { args } => {
            let stdin = piped_stdin();
            match client
                .request(Request::RunBd {
                    from: env_slot(),
                    args,
                    stdin,
                })
                .await?
            {
                Response::BdOutput {
                    code,
                    stdout,
                    stderr,
                } => {
                    use std::io::Write;
                    let _ = std::io::stdout().write_all(stdout.as_bytes());
                    let _ = std::io::stderr().write_all(stderr.as_bytes());
                    let _ = std::io::stdout().flush();
                    std::process::exit(if code < 0 { 1 } else { code });
                }
                Response::Error { message } => bail!("{message}"),
                other => bail!("unexpected reply: {other:?}"),
            }
        }
        CtlCmd::Brain { json } => match client.request(Request::BrainStatus).await? {
            Response::Brain(v) => {
                if json {
                    println!("{}", serde_json::to_string(&v)?);
                } else {
                    for l in format_brain(&v) {
                        println!("{l}");
                    }
                }
                Ok(())
            }
            Response::Error { message } => bail!("{message}"),
            other => bail!("unexpected reply: {other:?}"),
        },
        CtlCmd::Locks { json } => match client.request(Request::ListLocks).await? {
            Response::Locks(v) => {
                if json {
                    println!("{}", serde_json::to_string(&v)?);
                } else {
                    for l in format_locks(&v, now_secs()) {
                        println!("{l}");
                    }
                }
                Ok(())
            }
            Response::Error { message } => bail!("{message}"),
            other => bail!("unexpected reply: {other:?}"),
        },
        CtlCmd::Claim {
            slot,
            target,
            reason,
        } => print_text_or_queued(
            client
                .request(Request::ClaimTarget {
                    slot,
                    target,
                    reason,
                    item: None,
                })
                .await?,
        ),
        CtlCmd::ForceRelease { target } => print_text_or_queued(
            client
                .request(Request::ForceRelease {
                    target,
                    holder: None,
                })
                .await?,
        ),
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
        CtlCmd::Release {
            first,
            target: None,
        } => expect_ok(client.request(Request::Release { key: first }).await?),
        CtlCmd::Release {
            first,
            target: Some(target),
        } => print_text_or_queued(
            client
                .request(Request::ReleaseTarget {
                    slot: parse_slot(&first)?,
                    target,
                })
                .await?,
        ),
        CtlCmd::Refresh => match client.request(Request::RefreshWorkItems).await? {
            Response::Work(v) => {
                for l in format_sources(&v) {
                    println!("{l}");
                }
                Ok(())
            }
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
    fn locks_lines_show_pins_and_relay_states() {
        use overseer_proto::{Claim, Relay, Sender};
        let relay = |id, to, state, held: Option<&str>, text: &str| Relay {
            id,
            at: 0,
            from: Sender::You,
            to,
            text: text.into(),
            state,
            delivered_at: None,
            dropped_at: (state == RelayState::Dropped).then_some(5),
            held: held.map(Into::into),
        };
        let v = LockView {
            claims: vec![Claim {
                target: "argo:uat/demo-api".into(),
                slot: 1,
                kind: ClaimKind::Pin,
                reason: None,
                item: Some("DEMO-12".into()),
                since: 0,
                last_used: 0,
                pin_revision: Some("abc123".into()),
            }],
            waits: vec![],
            relays: vec![
                relay(4, 3, RelayState::Dropped, None, "status?"),
                relay(
                    3,
                    2,
                    RelayState::Pending,
                    Some("session is working"),
                    "hello",
                ),
                relay(2, 2, RelayState::Pending, None, "again"),
                relay(1, 2, RelayState::Delivered, None, "seen"),
            ],
            flag_after_secs: 3600,
        };
        let at = local_hhmm(0);
        assert_eq!(
            format_locks(&v, 0),
            vec![
                format!("argo:uat/demo-api  S1  pin  since {at}  for DEMO-12  at abc123"),
                "relay  S3  dropped: session ended  status?".to_string(),
                "relay  S2  pending (session is working)  hello".to_string(),
                "relay  S2  pending  again".to_string(),
            ]
        );
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

    fn view(beads_at: Option<i64>) -> WorkView {
        use overseer_proto::SourceStatus;
        WorkView {
            items: vec![WorkItem {
                key: "ov-3".into(),
                kind: ItemKind::Bead,
                title: "ov-3".into(),
                status: "unknown".into(),
                priority: None,
                parent: None,
                owner: Some(4),
                in_list: false,
                folder: None,
            }],
            sources: vec![
                SourceStatus {
                    name: "beads".into(),
                    ok: beads_at.is_some(),
                    count: 4,
                    at: beads_at,
                    message: beads_at.is_none().then(|| "not read yet".into()),
                },
                SourceStatus {
                    name: "jira".into(),
                    ok: false,
                    count: 0,
                    at: None,
                    message: Some("acli not found".into()),
                },
            ],
            notices: vec![],
        }
    }

    #[test]
    fn owned_item_not_read_yet_is_not_called_gone() {
        assert_eq!(
            format_work(&view(None)),
            [
                "beads  not read yet",
                "jira  acli not found",
                "ov-3  unknown  S4  ov-3  (not read yet)"
            ]
        );
        let lines = format_work(&view(Some(0)));
        assert_eq!(lines[2], "ov-3  unknown  S4  ov-3  (left the list)");
    }

    #[test]
    fn refresh_prints_the_source_lines() {
        assert_eq!(
            format_sources(&view(None)),
            ["beads  not read yet", "jira  acli not found"]
        );
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
