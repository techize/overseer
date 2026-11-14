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
    build_notice, local_hhmm, slot_name, ActivityKind, BoardView, BrainRun, BrainView, ClaimKind,
    Client, DaemonInfo, ItemKind, LockView, Plan, PlanDecision, PlanRecord, PlanState, QueueState,
    RelayState, Request, Response, SlotId, SlotInfo, SlotState, WorkItem, WorkView,
};

#[derive(Subcommand)]
pub enum CtlCmd {
    /// One line per slot.
    Status,
    /// The builds of this binary and of the running daemon.
    Version,
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
    /// Everything on the board: summary, slots, work, locks, pending plan.
    Board {
        /// Print the raw BoardView as JSON.
        #[arg(long)]
        json: bool,
    },
    /// The pending plan and the last five, or decide on one.
    Plan {
        /// Print the plans as JSON.
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        action: Option<PlanCmd>,
    },
    /// Set the board summary. `-` reads the text from stdin.
    Summary {
        #[arg(allow_hyphen_values = true)]
        text: String,
    },
    /// Ask a running slot for a short status.
    StatusOf { slot: String },
    /// Report your progress to the coordinator. `-` reads the text from
    /// stdin. Needs `$OVERSEER_SLOT`.
    Report {
        #[arg(allow_hyphen_values = true)]
        text: String,
    },
    /// Stop the daemon and every session.
    Shutdown,
}

#[derive(Subcommand)]
pub enum PlanCmd {
    /// Approve plan <id> as it is stored. Only Jonathan can.
    Approve { id: i64 },
    /// Reject plan <id>, with a reason if you have one. Only Jonathan can.
    Reject { id: i64, reason: Option<String> },
    /// Propose a plan from a JSON file, or `-` for stdin.
    Propose { source: String },
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

pub(crate) fn now_secs() -> i64 {
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

/// The slot whose CURRENT_SESSION.md section is written: inside a session
/// only its own, so `--slot` must agree with `$OVERSEER_SLOT`.
fn session_slot(named: Option<SlotId>, env: Option<SlotId>) -> anyhow::Result<SlotId> {
    match (named, env) {
        (Some(n), Some(e)) if n != e => {
            bail!("this session is S{e}; it may not write S{n}'s section of CURRENT_SESSION.md")
        }
        (Some(n), _) => Ok(n),
        (None, Some(e)) => Ok(e),
        (None, None) => bail!("name the slot with --slot S<n>"),
    }
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

/// Whether a `bd` argument asks for stdin: exactly `-` or `--flag=-`.
fn wants_stdin(args: &[String]) -> bool {
    args.iter().any(|a| a == "-" || a.ends_with("=-"))
}

/// Stdin for `ctl bd` (a heredoc the hook kept). It is read only when an
/// argument names it, so a stdin that never closes cannot hang a plain write.
fn piped_stdin(args: &[String]) -> Option<String> {
    use std::io::IsTerminal;
    if !wants_stdin(args) || std::io::stdin().is_terminal() {
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
    let run = |name: &str, r: &Option<BrainRun>| match r {
        Some(r) if r.ok => format!("last {name}  {}  ok", local_hhmm(r.at)),
        Some(r) => format!("last {name}  {}  failed: {}", local_hhmm(r.at), r.detail),
        None => format!("last {name}  never"),
    };
    let mut out = vec![
        "2ndBrain writer is on".to_string(),
        format!(
            "queue  {} queued  {} committed  {} nothing to commit  {} failed",
            count(QueueState::Queued),
            count(QueueState::Committed),
            count(QueueState::NothingToCommit),
            count(QueueState::Failed)
        ),
        run("backup", &v.last_backup),
        run("sync", &v.last_sync),
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

/// `5m`, or `2h` from an hour on.
fn short_age(secs: i64) -> String {
    let mins = secs.max(0) / 60;
    if mins < 60 {
        format!("{mins}m")
    } else {
        format!("{}h", mins / 60)
    }
}

/// `empty`, `exited`, `resumable`, `working 5m`, `needs you 2m`, `idle 12m`.
fn board_state(s: &SlotInfo, now: i64) -> String {
    match (&s.state, &s.activity) {
        (SlotState::Empty, _) => "empty".into(),
        (SlotState::Exited { .. }, _) => "exited".into(),
        (SlotState::Resumable { .. }, _) => "resumable".into(),
        (SlotState::Running { .. }, None) => "running".into(),
        (SlotState::Running { .. }, Some(a)) => {
            let word = match a.kind {
                ActivityKind::Working => "working",
                ActivityKind::NeedsYou => "needs you",
                ActivityKind::Idle => "idle",
            };
            format!("{word} {}", short_age(now - a.since))
        }
    }
}

/// The board as text, shared by `get_board` and `ctl board`: the summary,
/// one line per slot (with its last report beneath), work items, locks and
/// the plan waiting for Jonathan.
pub fn format_board(view: &BoardView, now: i64) -> String {
    let mut out = Vec::new();
    out.push(match &view.coord.summary {
        Some(s) => format!("Summary ({}): {}", local_hhmm(s.at), s.text),
        None => "Summary: none yet".to_string(),
    });
    for s in &view.slots {
        let mut line = slot_name(s.id);
        if !s.label.is_empty() {
            line.push_str(&format!(" {}", s.label));
        }
        line.push_str(&format!("  {}", board_state(s, now)));
        let keys = |it: Vec<&str>| it.join(", ");
        let owns = keys(
            view.work
                .items
                .iter()
                .filter(|i| i.owner == Some(s.id))
                .map(|i| i.key.as_str())
                .collect(),
        );
        if !owns.is_empty() {
            line.push_str(&format!("  owns {owns}"));
        }
        let holds = keys(
            view.locks
                .claims
                .iter()
                .filter(|c| c.slot == s.id)
                .map(|c| c.target.as_str())
                .collect(),
        );
        if !holds.is_empty() {
            line.push_str(&format!("  holds {holds}"));
        }
        let waits = keys(
            view.locks
                .waits
                .iter()
                .filter(|w| w.slot == s.id)
                .map(|w| w.target.as_str())
                .collect(),
        );
        if !waits.is_empty() {
            line.push_str(&format!("  waiting {waits}"));
        }
        let pending = view
            .locks
            .relays
            .iter()
            .filter(|r| r.to == s.id && r.state == RelayState::Pending)
            .count();
        if pending > 0 {
            line.push_str(&format!("  {pending} relays pending"));
        }
        if let Some((_, at)) = view.coord.status_asked.iter().find(|(id, _)| *id == s.id) {
            line.push_str(&format!("  status asked {}", local_hhmm(*at)));
        }
        out.push(line);
        // Only a report from the slot's current (or last) session.
        if let Some(r) = view.coord.last_report(s) {
            out.push(format!(
                "  last report {}: {}",
                local_hhmm(r.at),
                cut(&r.text, 200)
            ));
        }
    }
    out.push("Work items:".into());
    out.extend(format_work(&view.work));
    out.push("Locks:".into());
    out.extend(format_locks(&view.locks, now));
    if let Some(p) = view.coord.pending_plan() {
        out.push(format!(
            "Plan {} waiting for Jonathan since {}",
            p.id,
            local_hhmm(p.at)
        ));
    }
    out.join("\n")
}

/// One plan: its state, summary, what it does for each slot, the warnings
/// approval confirms and, once run, the outcome lines.
pub fn format_plan(rec: &PlanRecord) -> String {
    let state = match rec.state {
        PlanState::Pending => "pending".to_string(),
        PlanState::Approved => format!(
            "approved{}",
            rec.decided_at
                .map(|a| format!(" {}", local_hhmm(a)))
                .unwrap_or_default()
        ),
        PlanState::Rejected => "rejected".to_string(),
        PlanState::Superseded => "superseded".to_string(),
    };
    let edited = if rec.edited { " (edited)" } else { "" };
    let mut out = vec![format!(
        "Plan {}  {state}{edited}  proposed {}",
        rec.id,
        local_hhmm(rec.at)
    )];
    if !rec.plan.summary.is_empty() {
        out.push(format!("  {}", rec.plan.summary));
    }
    for s in &rec.plan.slots {
        let mut line = slot_name(s.slot);
        if !s.label.is_empty() {
            line.push_str(&format!(" {}", s.label));
        }
        if !s.items.is_empty() {
            line.push_str(&format!("  items {}", s.items.join(", ")));
        }
        if !s.targets.is_empty() {
            let t: Vec<&str> = s.targets.iter().map(|t| t.target.as_str()).collect();
            line.push_str(&format!("  targets {}", t.join(", ")));
        }
        if let Some(w) = &s.worktree {
            line.push_str(&format!("  worktree {} {}", tilde(&w.repo), w.ticket));
        }
        if let Some(c) = &s.cwd {
            line.push_str(&format!("  cwd {}", tilde(c)));
        }
        out.push(format!("  {line}"));
        for r in &s.rules {
            out.push(format!("    rule: {r}"));
        }
    }
    for w in &rec.warnings {
        out.push(format!("  warning: {w}"));
    }
    for o in &rec.outcome {
        out.push(format!("  {o}"));
    }
    out.join("\n")
}

/// The pending plan, then the newest five others.
fn plans_to_list(plans: &[PlanRecord]) -> Vec<&PlanRecord> {
    let pending = plans.iter().filter(|p| p.state == PlanState::Pending);
    let rest = plans.iter().filter(|p| p.state != PlanState::Pending);
    pending.chain(rest.take(5)).collect()
}

async fn run_plan(
    client: &overseer_proto::client::Client,
    json: bool,
    action: Option<PlanCmd>,
) -> anyhow::Result<()> {
    match action {
        None => match client.request(Request::CoordStatus).await? {
            Response::Coord(v) => {
                let plans = plans_to_list(&v.plans);
                if json {
                    println!("{}", serde_json::to_string(&plans)?);
                } else if plans.is_empty() {
                    println!("No plans yet.");
                } else {
                    for p in plans {
                        println!("{}", format_plan(p));
                    }
                }
                Ok(())
            }
            Response::Error { message } => bail!("{message}"),
            other => bail!("unexpected reply: {other:?}"),
        },
        Some(PlanCmd::Approve { id }) => {
            let from = env_slot();
            // Inside a session the daemon refuses before it looks at the
            // plan, so there is no need to fetch it.
            let plan = if from.is_some() {
                Plan {
                    summary: String::new(),
                    slots: vec![],
                }
            } else {
                match client.request(Request::CoordStatus).await? {
                    Response::Coord(v) => v
                        .plans
                        .into_iter()
                        .find(|p| p.id == id)
                        .map(|p| p.plan)
                        .with_context(|| format!("no plan {id}"))?,
                    Response::Error { message } => bail!("{message}"),
                    other => bail!("unexpected reply: {other:?}"),
                }
            };
            print_brain_text(
                client
                    .request(Request::DecidePlan {
                        id,
                        from,
                        decision: PlanDecision::Approve { plan },
                    })
                    .await?,
            )
        }
        Some(PlanCmd::Reject { id, reason }) => {
            let resp = client
                .request(Request::DecidePlan {
                    id,
                    from: env_slot(),
                    decision: PlanDecision::Reject {
                        reason: reason.unwrap_or_default(),
                    },
                })
                .await?;
            if matches!(resp, Response::Ok) {
                println!("Plan {id} rejected.");
                return Ok(());
            }
            print_brain_text(resp)
        }
        Some(PlanCmd::Propose { source }) => {
            let text = if source == "-" {
                text_or_stdin(source)?
            } else {
                std::fs::read_to_string(&source).with_context(|| format!("read {source}"))?
            };
            let value: serde_json::Value =
                serde_json::from_str(&text).context("the plan is not valid JSON")?;
            let plan = crate::mcp::plan_from_json(&value).map_err(|e| anyhow::anyhow!(e))?;
            print_brain_text(
                client
                    .request(Request::ProposePlan {
                        from: env_slot(),
                        plan,
                    })
                    .await?,
            )
        }
    }
}

/// The `ctl version` lines: this binary, the daemon, and a notice when
/// their builds differ.
pub fn format_version(
    app_version: &str,
    app_build: &str,
    daemon: Option<&DaemonInfo>,
) -> Vec<String> {
    let mut lines = vec![format!("app     {app_version} build {app_build}")];
    match daemon {
        Some(d) => {
            lines.push(format!(
                "daemon  {} build {}  pid {}  since {}",
                d.version,
                d.build,
                d.pid,
                local_hhmm(d.started_at)
            ));
            lines.extend(build_notice(app_build, &d.build));
        }
        None => lines.push("daemon  not running".to_string()),
    }
    lines
}

/// Ask a running daemon about itself, without starting one.
async fn version() -> anyhow::Result<()> {
    let info = match Client::connect(socket_path()).await {
        Ok(client) => match client.request(Request::DaemonInfo).await? {
            Response::DaemonInfo(info) => Some(info),
            Response::Error { message } => bail!("{message}"),
            other => bail!("unexpected reply: {other:?}"),
        },
        Err(_) => None,
    };
    for line in format_version(
        env!("CARGO_PKG_VERSION"),
        overseer_proto::current_build(),
        info.as_ref(),
    ) {
        println!("{line}");
    }
    Ok(())
}

pub async fn run(cmd: CtlCmd) -> anyhow::Result<()> {
    if matches!(cmd, CtlCmd::Version) {
        return version().await;
    }
    let client = connect().await?;
    match cmd {
        CtlCmd::Version => unreachable!("handled before connecting"),
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
            let named = slot.as_deref().map(parse_slot).transpose()?;
            let slot = session_slot(named, env_slot())?;
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
            let stdin = piped_stdin(&args);
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
        CtlCmd::Board { json } => match client.request(Request::GetBoard).await? {
            Response::Board(v) => {
                if json {
                    println!("{}", serde_json::to_string(&v)?);
                } else {
                    println!("{}", format_board(&v, now_secs()));
                }
                Ok(())
            }
            Response::Error { message } => bail!("{message}"),
            other => bail!("unexpected reply: {other:?}"),
        },
        CtlCmd::Plan { json, action } => run_plan(&client, json, action).await,
        CtlCmd::Summary { text } => print_brain_text(
            client
                .request(Request::SetBoardSummary {
                    from: env_slot(),
                    text: text_or_stdin(text)?,
                })
                .await?,
        ),
        CtlCmd::StatusOf { slot } => print_brain_text(
            client
                .request(Request::RequestStatus {
                    from: env_slot(),
                    slot: parse_slot(&slot)?,
                })
                .await?,
        ),
        CtlCmd::Report { text } => {
            let Some(slot) = env_slot() else {
                bail!("report needs a slot; run it from a session");
            };
            print_brain_text(
                client
                    .request(Request::ReportProgress {
                        slot,
                        text: text_or_stdin(text)?,
                    })
                    .await?,
            )
        }
        CtlCmd::Shutdown => expect_ok(client.request(Request::Shutdown).await?),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_lines_show_both_builds_and_the_notice() {
        let info = DaemonInfo {
            version: "0.1.0".into(),
            build: "abc1234-202611140930".into(),
            pid: 4242,
            started_at: 1_700_000_000,
            exe: "/Applications/Overseer.app/Contents/MacOS/overseer".into(),
        };
        let at = local_hhmm(info.started_at);
        assert_eq!(
            format_version("0.1.0", "def5678-202611141000", Some(&info)),
            vec![
                "app     0.1.0 build def5678-202611141000".to_string(),
                format!("daemon  0.1.0 build abc1234-202611140930  pid 4242  since {at}"),
                build_notice("def5678-202611141000", "abc1234-202611140930").unwrap(),
            ]
        );
        // Same build: no notice. Dev builds never warn.
        assert_eq!(
            format_version("0.1.0", "abc1234-202611140930", Some(&info)).len(),
            2
        );
        assert_eq!(format_version("0.1.0", "dev", Some(&info)).len(), 2);
        assert_eq!(
            format_version("0.1.0", "dev", None),
            vec![
                "app     0.1.0 build dev".to_string(),
                "daemon  not running".to_string()
            ]
        );
    }
    use overseer_proto::ReportKind;

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
    fn only_a_dash_argument_asks_for_stdin() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(wants_stdin(&v(&["create", "--body-file", "-"])));
        assert!(wants_stdin(&v(&["create", "--body-file=-"])));
        assert!(!wants_stdin(&v(&["list"])));
        assert!(!wants_stdin(&v(&["create", "--title", "a-b", "-x"])));
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
            session_note: None,
        };
        assert_eq!(format_slot(&s), "S3  resumable  eks");
    }

    fn slot_info(
        id: SlotId,
        label: &str,
        state: SlotState,
        kind: Option<ActivityKind>,
    ) -> SlotInfo {
        SlotInfo {
            id,
            label: label.into(),
            cwd: None,
            command: None,
            state,
            cols: 80,
            rows: 24,
            activity: kind.map(|kind| overseer_proto::Activity {
                kind,
                since: 700,
                now_doing: None,
                reason: None,
                idle_wait: false,
            }),
            session_note: None,
        }
    }

    fn board(summary: Option<overseer_proto::Summary>) -> BoardView {
        use overseer_proto::{Claim, CoordView, Relay, Report, Sender, Wait};
        let running = SlotState::Running { pid: 1, since: 0 };
        let mut work = view(Some(0));
        work.items[0].owner = Some(3);
        work.items[0].in_list = true;
        work.items.push(WorkItem {
            key: "DEMO-12".into(),
            kind: ItemKind::Ticket,
            title: "Widget wiring".into(),
            status: "open".into(),
            priority: None,
            parent: None,
            owner: Some(3),
            in_list: true,
            folder: None,
        });
        let claim = |target: &str, slot| Claim {
            target: target.into(),
            slot,
            kind: ClaimKind::Claim,
            reason: None,
            item: None,
            since: 100,
            last_used: 100,
            pin_revision: None,
        };
        BoardView {
            slots: vec![
                slot_info(0, "", SlotState::Empty, None),
                slot_info(1, "api", running.clone(), Some(ActivityKind::Idle)),
                slot_info(2, "", SlotState::Exited { code: None, at: 1 }, None),
                slot_info(3, "demo lane", running.clone(), Some(ActivityKind::Working)),
                slot_info(4, "", running, Some(ActivityKind::NeedsYou)),
            ],
            work,
            locks: LockView {
                claims: vec![claim("repo:demo-api:main", 3)],
                waits: vec![Wait {
                    target: "argo:uat/root".into(),
                    slot: 3,
                    at: 200,
                    reason: None,
                }],
                relays: vec![Relay {
                    id: 1,
                    at: 0,
                    from: Sender::You,
                    to: 3,
                    text: "hi".into(),
                    state: RelayState::Pending,
                    delivered_at: None,
                    dropped_at: None,
                    held: None,
                }],
                flag_after_secs: 3600,
            },
            coord: CoordView {
                summary,
                plans: vec![PlanRecord {
                    id: 2,
                    at: 400,
                    plan: Plan {
                        summary: "two lanes".into(),
                        slots: vec![],
                    },
                    state: PlanState::Pending,
                    decided_at: None,
                    edited: false,
                    warnings: vec![],
                    outcome: vec![],
                }],
                reports: vec![Report {
                    id: 1,
                    at: 500,
                    slot: 3,
                    kind: ReportKind::Report,
                    text: "tests green".into(),
                }],
                status_asked: vec![(3, 600)],
            },
        }
    }

    #[test]
    fn board_text_lists_summary_slots_work_locks_and_plan() {
        let v = board(Some(overseer_proto::Summary {
            at: 300,
            text: "Two lanes running.".into(),
        }));
        let text = format_board(&v, 1000);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            format!("Summary ({}): Two lanes running.", local_hhmm(300))
        );
        assert_eq!(lines[1], "S0  empty");
        assert_eq!(lines[2], "S1 api  idle 5m");
        assert_eq!(lines[3], "S2  exited");
        assert_eq!(
            lines[4],
            format!(
                "S3 demo lane  working 5m  owns ov-3, DEMO-12  holds repo:demo-api:main  waiting argo:uat/root  1 relays pending  status asked {}",
                local_hhmm(600)
            )
        );
        assert_eq!(
            lines[5],
            format!("  last report {}: tests green", local_hhmm(500))
        );
        assert_eq!(lines[6], "S4  needs you 5m");
        let work_at = lines.iter().position(|l| *l == "Work items:").unwrap();
        assert_eq!(
            lines[work_at + 1],
            "beads  ok  4 items  ".to_owned() + &local_hhmm(0)
        );
        let locks_at = lines.iter().position(|l| *l == "Locks:").unwrap();
        assert!(locks_at > work_at);
        assert!(lines[locks_at + 1].starts_with("repo:demo-api:main  S3  claim  since "));
        assert_eq!(
            lines.last().unwrap(),
            &format!("Plan 2 waiting for Jonathan since {}", local_hhmm(400))
        );
    }

    #[test]
    fn board_text_drops_a_last_report_from_an_earlier_session() {
        let mut v = board(None);
        // S3's report was made at 500; its session now started at 700.
        v.slots[3].state = SlotState::Running { pid: 2, since: 700 };
        let text = format_board(&v, 1000);
        assert!(!text.contains("last report"), "{text}");
        // A report from the new session shows.
        v.coord.reports.push(overseer_proto::Report {
            id: 2,
            at: 800,
            slot: 3,
            kind: ReportKind::Report,
            text: "schema done".into(),
        });
        let text = format_board(&v, 1000);
        assert!(
            text.contains(&format!("  last report {}: schema done", local_hhmm(800))),
            "{text}"
        );
        assert!(!text.contains("tests green"), "{text}");
    }

    #[test]
    fn board_text_without_summary_says_none_yet() {
        let mut v = board(None);
        v.coord.plans.clear();
        let text = format_board(&v, 1000);
        assert_eq!(text.lines().next().unwrap(), "Summary: none yet");
        assert!(!text.contains("waiting for Jonathan"), "{text}");
    }

    #[test]
    fn plan_text_shows_slots_warnings_and_outcome() {
        use overseer_proto::{PlanSlot, PlanTarget, PlanWorktree};
        let rec = PlanRecord {
            id: 7,
            at: 0,
            plan: Plan {
                summary: "Ship the widget".into(),
                slots: vec![PlanSlot {
                    slot: 3,
                    label: "demo lane".into(),
                    items: vec!["DEMO-12".into(), "ov-3".into()],
                    targets: vec![PlanTarget {
                        target: "repo:demo-api:main".into(),
                        reason: None,
                    }],
                    worktree: Some(PlanWorktree {
                        repo: "/srv/demo-api".into(),
                        ticket: "DEMO-12".into(),
                    }),
                    cwd: None,
                    kickoff: String::new(),
                    rules: vec!["Do not deploy before S2 merges".into()],
                }],
            },
            state: PlanState::Approved,
            decided_at: Some(0),
            edited: true,
            warnings: vec!["S3 is running; it will be told to work in /x but not moved".into()],
            outcome: vec!["S3: assigned DEMO-12, ov-3".into()],
        };
        let at = local_hhmm(0);
        assert_eq!(
            format_plan(&rec),
            format!(
                "Plan 7  approved {at} (edited)  proposed {at}\n  Ship the widget\n  S3 demo lane  items DEMO-12, ov-3  targets repo:demo-api:main  worktree /srv/demo-api DEMO-12\n    rule: Do not deploy before S2 merges\n  warning: S3 is running; it will be told to work in /x but not moved\n  S3: assigned DEMO-12, ov-3"
            )
        );
    }
}
