use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Slot index, `0..=6`.
pub type SlotId = u8;

/// Display name for a slot, e.g. `S3`.
pub fn slot_name(id: SlotId) -> String {
    format!("S{id}")
}

/// `1 running session` or `<n> running sessions`.
pub fn running_sessions_count(n: usize) -> String {
    match n {
        1 => "1 running session".to_string(),
        n => format!("{n} running sessions"),
    }
}

/// Running sessions by slot and label, comma separated, as the restart
/// warnings name them: `S1 demo lane, S3 docs`. A slot with no label is just
/// `S4`. Each caller wraps it in its own sentence.
pub fn running_sessions_text<'a>(running: impl IntoIterator<Item = (SlotId, &'a str)>) -> String {
    running
        .into_iter()
        .map(|(id, label)| match label.trim() {
            "" => slot_name(id),
            l => format!("{} {l}", slot_name(id)),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Lifecycle state of a slot. Timestamps are unix seconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SlotState {
    Empty,
    Running {
        pid: u32,
        since: i64,
    },
    Exited {
        code: Option<i32>,
        at: i64,
    },
    /// Was running when the daemon last stopped and can be continued with
    /// `claude --resume`. `at` is when that session started.
    Resumable {
        session_id: String,
        at: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlotInfo {
    pub id: SlotId,
    pub label: String,
    pub cwd: Option<PathBuf>,
    pub command: Option<Vec<String>>,
    pub state: SlotState,
    pub cols: u16,
    pub rows: u16,
    /// What the session in the slot is doing, from its hook events. `None`
    /// until the first event, for slots that are not running and for
    /// commands that send no hooks.
    #[serde(default)]
    pub activity: Option<Activity>,
    /// The slot's session note, relative to the 2ndBrain vault, once the
    /// ledger has one.
    #[serde(default)]
    pub session_note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivityKind {
    Working,
    NeedsYou,
    Idle,
}

/// A slot's current activity. `since` is unix seconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Activity {
    pub kind: ActivityKind,
    pub since: i64,
    /// The latest tool, while working.
    pub now_doing: Option<String>,
    /// Why the session needs you.
    pub reason: Option<String>,
    /// True for a `Notification` that is an idle-input wait, not a prompt
    /// for you.
    #[serde(default)]
    pub idle_wait: bool,
}

/// Which Claude Code hook fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HookKind {
    SessionStart,
    UserPromptSubmit,
    Notification,
    Stop,
    PostToolUse,
}

/// What `overseer hook` tells the daemon about one hook call. Every field
/// but `slot` and `event` is whatever Claude put in its event JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookEvent {
    pub slot: SlotId,
    pub event: HookKind,
    pub session_id: Option<String>,
    pub tool: Option<String>,
    /// One line, at most 80 chars, summarising the tool input.
    pub detail: Option<String>,
    pub notification: Option<String>,
    /// The tool call this event belongs to, for `PostToolUse`.
    #[serde(default)]
    pub tool_use_id: Option<String>,
    /// Claude's notification type, e.g. `idle_prompt`.
    #[serde(default)]
    pub notification_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ItemKind {
    Ticket,
    Bead,
}

/// The status of an item the sources did not describe: a ticket known only
/// from bead titles, or an owned item that has left the list.
pub const UNKNOWN_STATUS: &str = "unknown";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkItem {
    pub key: String,
    pub kind: ItemKind,
    pub title: String,
    pub status: String,
    pub priority: Option<String>,
    /// The ticket key for a bead grouped under a ticket.
    pub parent: Option<String>,
    pub owner: Option<SlotId>,
    /// False for an owned item that has left the cache.
    pub in_list: bool,
    /// The 2ndBrain ticket folder, when found.
    pub folder: Option<PathBuf>,
}

impl WorkItem {
    /// A ticket row the daemon made up from bead titles because the ticket
    /// source did not list it: its title is its key and its status unknown.
    pub fn is_placeholder(&self) -> bool {
        self.kind == ItemKind::Ticket && self.status == UNKNOWN_STATUS && self.title == self.key
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceStatus {
    pub name: String,
    pub ok: bool,
    pub count: usize,
    /// Unix seconds of the last successful read.
    pub at: Option<i64>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notice {
    pub slot: SlotId,
    pub at: i64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkView {
    pub items: Vec<WorkItem>,
    pub sources: Vec<SourceStatus>,
    pub notices: Vec<Notice>,
}

/// The Jira source's message while `jira_command` is empty. Jira is then
/// never read, so it never holds back an item's "not read yet".
pub const JIRA_OFF: &str = "jira_command is empty; Jira is off";

impl WorkView {
    /// An owned item missing from the list only because a source that could
    /// list it has not answered yet since the daemon started: beads for a
    /// bead, beads or Jira for a ticket.
    pub fn not_read_yet(&self, item: &WorkItem) -> bool {
        if item.in_list {
            return false;
        }
        let names: &[&str] = match item.kind {
            ItemKind::Bead => &["beads"],
            ItemKind::Ticket => &["beads", "jira"],
        };
        self.sources.iter().any(|s| {
            names.contains(&s.name.as_str())
                && s.at.is_none()
                && s.message.as_deref() != Some(JIRA_OFF)
        })
    }
}

/// The daemon's refusal when another slot owns `key`. Clients read the
/// owner back with [`owner_in_refusal`], so the two must stay in step.
pub fn owned_by_message(key: &str, owner: SlotId) -> String {
    format!("{key} is owned by {}", slot_name(owner))
}

/// The owner named by an [`owned_by_message`] refusal for `key`, else `None`.
pub fn owner_in_refusal(message: &str, key: &str) -> Option<SlotId> {
    let n = message.strip_prefix(key)?.strip_prefix(" is owned by S")?;
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    n.parse().ok()
}

/// What a claim on a write target is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClaimKind {
    /// Taken by a command the hook matched.
    Command,
    /// Handed to a waiter on release, not yet used.
    Grant,
    /// Taken with `claim_target`, held until released.
    Claim,
    /// A pinned ArgoCD revision.
    Pin,
}

/// One slot's hold on one write target. Times are unix seconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Claim {
    pub target: String,
    pub slot: SlotId,
    pub kind: ClaimKind,
    pub reason: Option<String>,
    /// The work item the claim is for.
    pub item: Option<String>,
    pub since: i64,
    pub last_used: i64,
    /// The revision a pin recorded.
    pub pin_revision: Option<String>,
}

/// A slot queued for a target. A target's waits are listed in queue order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Wait {
    pub target: String,
    pub slot: SlotId,
    pub at: i64,
    pub reason: Option<String>,
}

/// Who a relay is from. `You` is Jonathan, through the GUI or `ctl`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Sender {
    Overseer,
    You,
    Slot { slot: SlotId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelayState {
    Pending,
    Delivered,
    /// Retired unsent: the session it was for ended first.
    Dropped,
}

/// A message from one session (or Overseer, or you) to a slot's session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Relay {
    pub id: i64,
    pub at: i64,
    pub from: Sender,
    pub to: SlotId,
    pub text: String,
    pub state: RelayState,
    pub delivered_at: Option<i64>,
    /// When a `Dropped` relay was retired.
    #[serde(default)]
    pub dropped_at: Option<i64>,
    /// Why a pending relay is waiting, e.g. `session is working`. Filled by
    /// the courier.
    pub held: Option<String>,
}

/// Every claim, wait and relay worth showing. `relays` holds every pending
/// relay plus the 50 most recent delivered or dropped ones, newest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LockView {
    pub claims: Vec<Claim>,
    pub waits: Vec<Wait>,
    pub relays: Vec<Relay>,
    /// Seconds without use after which a claim is flagged as long-held.
    pub flag_after_secs: i64,
}

impl LockView {
    /// True when `c` has gone unused for longer than `flag_after_secs`.
    pub fn flagged(&self, c: &Claim, now: i64) -> bool {
        now - c.last_used > self.flag_after_secs
    }

    /// The targets `slot` is queued on, in the order of `waits`.
    pub fn waiting_on(&self, slot: SlotId) -> Vec<&str> {
        self.waits
            .iter()
            .filter(|w| w.slot == slot)
            .map(|w| w.target.as_str())
            .collect()
    }

    /// How many pending relays are addressed to `slot`.
    pub fn pending_for(&self, slot: SlotId) -> usize {
        self.relays
            .iter()
            .filter(|r| r.to == slot && r.state == RelayState::Pending)
            .count()
    }
}

/// The variables `BashCheck::env` may carry: the ones a target lookup reads.
pub const CHECK_ENV: [&str; 6] = [
    "KUBECONFIG",
    "TF_WORKSPACE",
    "ARGOCD_SERVER",
    "ARGOCD_CONFIG",
    "HOME",
    // Which beads database a `bd` write uses (the 2ndBrain writer).
    "BEADS_DIR",
];

/// A Bash command the `PreToolUse` hook asks the daemon to check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BashCheck {
    pub slot: SlotId,
    pub session_id: Option<String>,
    pub tool_use_id: Option<String>,
    pub command: String,
    /// The session's cwd, from the hook input. It decides which repo's
    /// beads a `bd` write would use.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Request {
    Hello {
        client: String,
    },
    ListSlots,
    StartSlot {
        id: SlotId,
        label: String,
        cwd: PathBuf,
        command: Option<Vec<String>>,
        /// First prompt, passed to claude at launch. Old clients omit it.
        #[serde(default)]
        prompt: Option<String>,
    },
    StopSlot {
        id: SlotId,
    },
    /// Relaunches a `Resumable` slot with `claude --resume`.
    ResumeSlot {
        id: SlotId,
    },
    Attach {
        id: SlotId,
    },
    Detach {
        id: SlotId,
    },
    Resize {
        id: SlotId,
        cols: u16,
        rows: u16,
    },
    Shutdown,
    /// A Claude Code hook event, relayed by `overseer hook`.
    Hook(HookEvent),
    /// The cached work-item list with owners, source health and notices.
    ListWorkItems,
    /// Re-reads the sources now instead of waiting for the next poll.
    RefreshWorkItems,
    /// Gives a work item to a slot. `confirm` accepts a busy-slot warning,
    /// `move_owner` takes the item from its current owner.
    Assign {
        key: String,
        slot: SlotId,
        #[serde(default)]
        confirm: bool,
        #[serde(default)]
        move_owner: bool,
    },
    Release {
        key: String,
    },
    /// Sends the slot's assigned items to its session as the first prompt.
    Kickoff {
        slot: SlotId,
    },
    ClearNotices {
        slot: SlotId,
    },
    ListLocks,
    ClaimTarget {
        slot: SlotId,
        target: String,
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        item: Option<String>,
    },
    ReleaseTarget {
        slot: SlotId,
        target: String,
    },
    /// Jonathan releases `target`. With `holder`, only while that slot
    /// still holds it; without, from whoever does.
    ForceRelease {
        target: String,
        #[serde(default)]
        holder: Option<SlotId>,
    },
    /// Queues a message for a slot's session. `from` is `None` for you.
    SendRelay {
        #[serde(default)]
        from: Option<SlotId>,
        to: SlotId,
        text: String,
    },
    CheckBash(BashCheck),
    /// Queues a pathspec commit of `paths` in the 2ndBrain. `from` is
    /// `None` for you.
    Commit {
        #[serde(default)]
        from: Option<SlotId>,
        paths: Vec<String>,
        message: String,
    },
    /// Adds `text` to the `section` of today's journal.
    AppendJournal {
        #[serde(default)]
        from: Option<SlotId>,
        section: String,
        text: String,
    },
    /// Replaces the one occurrence of `old_text` in a shared file.
    EditShared {
        #[serde(default)]
        from: Option<SlotId>,
        path: String,
        old_text: String,
        new_text: String,
    },
    /// Sets a slot's section of `CURRENT_SESSION.md`.
    UpdateCurrentSession {
        slot: SlotId,
        text: String,
    },
    /// Runs a `bd` write under the 2ndBrain lock.
    RunBd {
        #[serde(default)]
        from: Option<SlotId>,
        args: Vec<String>,
        /// Piped to bd's stdin (a heredoc body); bd's stdin is closed when `None`.
        #[serde(default)]
        stdin: Option<String>,
    },
    BrainStatus,
    /// A file write the hook asks about. Answered with `Verdict`.
    CheckWrite(WriteCheck),
    /// The whole board in one answer: `Response::Board`.
    GetBoard,
    /// The coordinator's part of the board: `Response::Coord`.
    CoordStatus,
    /// S0 (or Jonathan through `ctl`) proposes a plan for approval.
    ProposePlan {
        #[serde(default)]
        from: Option<SlotId>,
        plan: Plan,
    },
    /// Validates `plan` against the board now without recording it:
    /// `Response::PlanCheck`. The plan view asks after each edit.
    CheckPlan {
        plan: Plan,
    },
    /// Jonathan approves (with the plan as he edited it) or rejects plan `id`.
    DecidePlan {
        id: i64,
        #[serde(default)]
        from: Option<SlotId>,
        decision: PlanDecision,
    },
    /// S0 asks `slot` for a short status.
    RequestStatus {
        #[serde(default)]
        from: Option<SlotId>,
        slot: SlotId,
    },
    /// A slot reports its progress.
    ReportProgress {
        slot: SlotId,
        text: String,
    },
    /// S0 sets the board summary.
    SetBoardSummary {
        #[serde(default)]
        from: Option<SlotId>,
        text: String,
    },
    /// Ask the daemon which build it runs.
    DaemonInfo,
}

/// One write target a plan has a slot claim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanTarget {
    pub target: String,
    #[serde(default)]
    pub reason: Option<String>,
}

/// A git worktree a plan has made for a slot: `<worktree_root>/<repo>-<ticket>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanWorktree {
    pub repo: PathBuf,
    pub ticket: String,
}

/// What a plan does for one slot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanSlot {
    pub slot: SlotId,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub items: Vec<String>,
    #[serde(default)]
    pub targets: Vec<PlanTarget>,
    #[serde(default)]
    pub worktree: Option<PlanWorktree>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub kickoff: String,
    #[serde(default)]
    pub rules: Vec<String>,
}

/// A plan S0 proposes and Jonathan approves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    #[serde(default)]
    pub summary: String,
    pub slots: Vec<PlanSlot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanState {
    Pending,
    Approved,
    Rejected,
    Superseded,
}

/// A plan as the ledger holds it. `plan` is the version that was (or will
/// be) run: the edited one after an approval with edits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanRecord {
    pub id: i64,
    pub at: i64,
    pub plan: Plan,
    pub state: PlanState,
    pub decided_at: Option<i64>,
    pub edited: bool,
    pub warnings: Vec<String>,
    pub outcome: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReportKind {
    Report,
    State,
}

/// A line on the timeline: a slot's own report or a state change noted by
/// the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub id: i64,
    pub at: i64,
    pub slot: SlotId,
    pub kind: ReportKind,
    pub text: String,
}

/// The board summary S0 keeps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub at: i64,
    pub text: String,
}

/// The coordinator's part of the board.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoordView {
    pub summary: Option<Summary>,
    /// Pending first, then newest.
    pub plans: Vec<PlanRecord>,
    /// Newest first.
    pub reports: Vec<Report>,
    /// Slots with an unanswered status request, and when it was asked.
    pub status_asked: Vec<(SlotId, i64)>,
}

impl CoordView {
    /// The plan waiting for Jonathan, if any.
    pub fn pending_plan(&self) -> Option<&PlanRecord> {
        self.plans.iter().find(|p| p.state == PlanState::Pending)
    }

    /// When the slot's current session started, or its last one for an
    /// exited slot (the newest `started` state change before the exit;
    /// `i64::MIN`, so everything counts, when that has left the timeline).
    /// `None` for an empty slot, which has no session.
    pub fn session_start(&self, info: &SlotInfo) -> Option<i64> {
        match &info.state {
            SlotState::Empty => None,
            SlotState::Running { since, .. } => Some(*since),
            SlotState::Resumable { at, .. } => Some(*at),
            SlotState::Exited { at, .. } => Some(
                self.reports_for(info.id)
                    .find(|r| r.kind == ReportKind::State && r.text == "started" && r.at <= *at)
                    .map_or(i64::MIN, |r| r.at),
            ),
        }
    }

    /// The slot's newest own report from its current (or last) session.
    pub fn last_report(&self, info: &SlotInfo) -> Option<&Report> {
        let start = self.session_start(info)?;
        self.reports_for(info.id)
            .find(|r| r.kind == ReportKind::Report && r.at >= start)
    }

    /// The reports and state changes of one slot, newest first.
    pub fn reports_for(&self, slot: SlotId) -> impl Iterator<Item = &Report> {
        let mut rows: Vec<&Report> = self.reports.iter().filter(|r| r.slot == slot).collect();
        rows.sort_by(|a, b| b.at.cmp(&a.at).then(b.id.cmp(&a.id)));
        rows.into_iter()
    }
}

/// Everything the board shows, in one answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardView {
    pub slots: Vec<SlotInfo>,
    pub work: WorkView,
    pub locks: LockView,
    pub coord: CoordView,
}

/// Jonathan's answer to a plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PlanDecision {
    Approve {
        plan: Plan,
    },
    Reject {
        #[serde(default)]
        reason: String,
    },
}

/// Where a queued 2ndBrain commit stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueueState {
    Queued,
    Committed,
    NothingToCommit,
    Failed,
}

/// One commit in the 2ndBrain queue. `at` is unix seconds; `pushed` is
/// `None` until a push has been tried.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueEntry {
    pub id: i64,
    pub at: i64,
    pub from: Sender,
    pub paths: Vec<String>,
    pub message: String,
    pub state: QueueState,
    pub commit: Option<String>,
    pub pushed: Option<bool>,
    pub error: Option<String>,
}

/// The last backup or sync run. `detail` is the error text when `ok` is
/// false, else a short note (`3 paths`, `sync done`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrainRun {
    pub at: i64,
    pub ok: bool,
    #[serde(default)]
    pub detail: String,
}

/// A run as an older daemon sent it: `last_backup` was a time and
/// `last_sync` was `(at, ok)`.
#[derive(Deserialize)]
#[serde(untagged)]
enum RunWire {
    Run(BrainRun),
    At(i64),
    AtOk(i64, bool),
}

fn run_compat<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<BrainRun>, D::Error> {
    Ok(Option::<RunWire>::deserialize(d)?.map(|w| match w {
        RunWire::Run(r) => r,
        RunWire::At(at) => BrainRun {
            at,
            ok: true,
            detail: String::new(),
        },
        RunWire::AtOk(at, ok) => BrainRun {
            at,
            ok,
            detail: String::new(),
        },
    }))
}

/// The 2ndBrain writer as the board shows it: every queued entry plus the
/// 20 most recent others, newest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrainView {
    pub on: bool,
    pub entries: Vec<QueueEntry>,
    #[serde(default, deserialize_with = "run_compat")]
    pub last_backup: Option<BrainRun>,
    #[serde(default, deserialize_with = "run_compat")]
    pub last_sync: Option<BrainRun>,
    pub push_problem: Option<String>,
    /// The vault directory, so the board can build an absolute path for a
    /// session note. `None` while the writer is off.
    #[serde(default)]
    pub brain_dir: Option<std::path::PathBuf>,
}

/// A file write the `PreToolUse` hook asks the daemon to check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WriteCheck {
    pub slot: SlotId,
    pub tool: String,
    pub paths: Vec<PathBuf>,
    pub cwd: Option<PathBuf>,
}

/// What a running daemon says about itself. `started_at` is unix seconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub version: String,
    pub build: String,
    pub pid: u32,
    pub started_at: i64,
    pub exe: PathBuf,
}

// Adjacent tagging because `Slots` is a tuple variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum Response {
    Ok,
    Slots(Vec<SlotInfo>),
    Error {
        message: String,
    },
    Work(WorkView),
    /// The request needs the caller to confirm and resend with `confirm` set.
    NeedsConfirm {
        message: String,
    },
    Text {
        text: String,
    },
    Locks(LockView),
    /// The answer to `CheckBash`: `Some` is the deny reason.
    Verdict {
        deny: Option<String>,
        /// A replacement Bash command to run instead of the original.
        #[serde(default)]
        rewrite: Option<String>,
    },
    /// The request was accepted and is waiting, e.g. a queued claim.
    Queued {
        message: String,
    },
    Brain(BrainView),
    /// What a `bd` run printed and its exit code.
    BdOutput {
        code: i32,
        stdout: String,
        stderr: String,
    },
    Board(BoardView),
    Coord(CoordView),
    /// The answer to `CheckPlan`: the warnings approval would confirm, or
    /// the refusal approval would meet (then `warnings` is empty).
    PlanCheck {
        warnings: Vec<String>,
        refusal: Option<String>,
    },
    DaemonInfo(DaemonInfo),
}

// Adjacent tagging because `SlotChanged` is a tuple variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum Event {
    SlotChanged(SlotInfo),
    WorkChanged(WorkView),
    LocksChanged(LockView),
    BrainChanged(BrainView),
    CoordChanged(CoordView),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_round_trips_with_defaults() {
        let plan: Plan = serde_json::from_str(r#"{"slots":[{"slot":3}]}"#).unwrap();
        assert_eq!(plan.summary, "");
        let s = &plan.slots[0];
        assert_eq!(s.slot, 3);
        assert!(s.label.is_empty() && s.items.is_empty() && s.targets.is_empty());
        assert!(s.kickoff.is_empty() && s.rules.is_empty());
        assert!(s.worktree.is_none() && s.cwd.is_none());
        let back: Plan = serde_json::from_str(&serde_json::to_string(&plan).unwrap()).unwrap();
        assert_eq!(back, plan);
    }

    fn coord() -> CoordView {
        let plan = Plan {
            summary: "two lanes".into(),
            slots: vec![PlanSlot {
                slot: 3,
                label: "demo".into(),
                items: vec!["DEMO-12".into()],
                targets: vec![PlanTarget {
                    target: "repo:demo-api:main".into(),
                    reason: Some("merge".into()),
                }],
                worktree: Some(PlanWorktree {
                    repo: PathBuf::from("/r/demo-api"),
                    ticket: "DEMO-12".into(),
                }),
                cwd: None,
                kickoff: "go".into(),
                rules: vec!["wait for S2".into()],
            }],
        };
        let rep = |id, at, slot| Report {
            id,
            at,
            slot,
            kind: ReportKind::Report,
            text: "x".into(),
        };
        CoordView {
            summary: Some(Summary {
                at: 5,
                text: "ok".into(),
            }),
            plans: vec![
                PlanRecord {
                    id: 2,
                    at: 9,
                    plan: plan.clone(),
                    state: PlanState::Pending,
                    decided_at: None,
                    edited: false,
                    warnings: vec!["w".into()],
                    outcome: vec![],
                },
                PlanRecord {
                    id: 1,
                    at: 4,
                    plan,
                    state: PlanState::Superseded,
                    decided_at: Some(9),
                    edited: false,
                    warnings: vec![],
                    outcome: vec![],
                },
            ],
            reports: vec![rep(3, 30, 3), rep(2, 20, 4), rep(1, 10, 3)],
            status_asked: vec![(3, 40)],
        }
    }

    #[test]
    fn coord_view_helpers() {
        let c = coord();
        assert_eq!(c.pending_plan().map(|p| p.id), Some(2));
        let ids: Vec<i64> = c.reports_for(3).map(|r| r.id).collect();
        assert_eq!(ids, vec![3, 1]);
        assert_eq!(c.reports_for(6).count(), 0);
    }

    #[test]
    fn session_start_and_last_report_belong_to_the_current_session() {
        let row = |id, at, kind, text: &str| Report {
            id,
            at,
            slot: 3,
            kind,
            text: text.into(),
        };
        let mut c = coord();
        c.reports = vec![
            row(1, 10, ReportKind::State, "started"),
            row(2, 20, ReportKind::Report, "old session report"),
            row(3, 30, ReportKind::State, "exited"),
            row(4, 40, ReportKind::State, "started"),
            row(5, 50, ReportKind::State, "exited"),
        ];
        let info = |state| SlotInfo {
            id: 3,
            label: String::new(),
            cwd: None,
            command: None,
            state,
            cols: 80,
            rows: 24,
            activity: None,
            session_note: None,
        };
        let running = info(SlotState::Running { pid: 1, since: 40 });
        assert_eq!(c.session_start(&running), Some(40));
        assert_eq!(c.last_report(&running), None);
        let resumable = info(SlotState::Resumable {
            session_id: "s".into(),
            at: 40,
        });
        assert_eq!(c.session_start(&resumable), Some(40));
        // An exited session started at its newest `started` row before the exit.
        let exited = info(SlotState::Exited { code: None, at: 50 });
        assert_eq!(c.session_start(&exited), Some(40));
        assert_eq!(c.last_report(&exited), None);
        let first_exit = info(SlotState::Exited { code: None, at: 30 });
        assert_eq!(c.session_start(&first_exit), Some(10));
        assert_eq!(
            c.last_report(&first_exit).map(|r| r.text.as_str()),
            Some("old session report")
        );
        // No `started` row left on the timeline: every report counts.
        c.reports.retain(|r| r.text != "started");
        assert_eq!(c.session_start(&exited), Some(i64::MIN));
        assert_eq!(c.last_report(&exited).map(|r| r.id), Some(2));
        // No session at all.
        let empty = info(SlotState::Empty);
        assert_eq!(c.session_start(&empty), None);
        assert_eq!(c.last_report(&empty), None);
        // A report in the current session is the last report.
        c.reports
            .push(row(6, 45, ReportKind::Report, "new session report"));
        assert_eq!(c.last_report(&running).map(|r| r.id), Some(6));
    }

    #[test]
    fn coordinator_messages_round_trip() {
        let plan = coord().plans[0].plan.clone();
        let reqs = vec![
            Request::GetBoard,
            Request::CoordStatus,
            Request::ProposePlan {
                from: Some(0),
                plan: plan.clone(),
            },
            Request::CheckPlan { plan: plan.clone() },
            Request::DecidePlan {
                id: 2,
                from: None,
                decision: PlanDecision::Approve { plan },
            },
            Request::DecidePlan {
                id: 2,
                from: None,
                decision: PlanDecision::Reject {
                    reason: "no".into(),
                },
            },
            Request::RequestStatus {
                from: Some(0),
                slot: 3,
            },
            Request::ReportProgress {
                slot: 3,
                text: "done".into(),
            },
            Request::SetBoardSummary {
                from: None,
                text: "all quiet".into(),
            },
        ];
        for r in reqs {
            let back: Request = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
            assert_eq!(back, r);
        }
        let reject: PlanDecision = serde_json::from_str(r#"{"Reject":{}}"#).unwrap();
        assert_eq!(
            reject,
            PlanDecision::Reject {
                reason: String::new()
            }
        );
        for resp in [
            Response::Coord(coord()),
            Response::PlanCheck {
                warnings: vec!["w".into()],
                refusal: Some("r".into()),
            },
        ] {
            let back: Response =
                serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
            assert_eq!(back, resp);
        }
        let ev = Event::CoordChanged(coord());
        let back: Event = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        assert_eq!(back, ev);
        let board = Response::Board(BoardView {
            slots: vec![],
            work: WorkView {
                items: vec![],
                sources: vec![],
                notices: vec![],
            },
            locks: LockView {
                claims: vec![],
                waits: vec![],
                relays: vec![],
                flag_after_secs: 3600,
            },
            coord: coord(),
        });
        let back: Response = serde_json::from_str(&serde_json::to_string(&board).unwrap()).unwrap();
        assert_eq!(back, board);
    }

    #[test]
    fn names() {
        assert_eq!(slot_name(3), "S3");
    }

    #[test]
    fn running_sessions_are_named_with_their_labels() {
        assert_eq!(
            running_sessions_text([(1, "demo lane"), (3, "docs"), (4, "")]),
            "S1 demo lane, S3 docs, S4"
        );
        assert_eq!(running_sessions_text([]), "");
        assert_eq!(running_sessions_count(1), "1 running session");
        assert_eq!(running_sessions_count(2), "2 running sessions");
    }

    #[test]
    fn hook_request_round_trips() {
        let req = Request::Hook(HookEvent {
            slot: 1,
            event: HookKind::Stop,
            session_id: Some("abc".into()),
            tool: None,
            detail: None,
            notification: None,
            tool_use_id: None,
            notification_type: None,
        });
        let back: Request = serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn resume_shapes() {
        let v = serde_json::to_value(Request::ResumeSlot { id: 2 }).unwrap();
        assert_eq!(v, serde_json::json!({"type": "ResumeSlot", "id": 2}));
        let st = SlotState::Resumable {
            session_id: "abc".into(),
            at: 5,
        };
        let back: SlotState = serde_json::from_str(&serde_json::to_string(&st).unwrap()).unwrap();
        assert_eq!(back, st);
    }

    #[test]
    fn json_shape() {
        let v = serde_json::to_value(Request::StopSlot { id: 2 }).unwrap();
        assert_eq!(v, serde_json::json!({"type": "StopSlot", "id": 2}));
    }

    fn view() -> WorkView {
        WorkView {
            items: vec![
                WorkItem {
                    key: "DEMO-1".into(),
                    kind: ItemKind::Ticket,
                    title: "A ticket".into(),
                    status: "In Progress".into(),
                    priority: Some("High".into()),
                    parent: None,
                    owner: Some(2),
                    in_list: true,
                    folder: Some(PathBuf::from("/tmp/DEMO-1")),
                },
                WorkItem {
                    key: "bd-9".into(),
                    kind: ItemKind::Bead,
                    title: "A bead".into(),
                    status: "open".into(),
                    priority: None,
                    parent: Some("DEMO-1".into()),
                    owner: None,
                    in_list: false,
                    folder: None,
                },
            ],
            sources: vec![SourceStatus {
                name: "jira".into(),
                ok: true,
                count: 3,
                at: Some(100),
                message: None,
            }],
            notices: vec![Notice {
                slot: 2,
                at: 50,
                text: "left the list".into(),
            }],
        }
    }

    #[test]
    fn owned_by_refusal_text_is_pinned_and_reads_back() {
        let m = owned_by_message("DEMO-12", 3);
        assert_eq!(m, "DEMO-12 is owned by S3");
        assert_eq!(owner_in_refusal(&m, "DEMO-12"), Some(3));
        assert_eq!(
            owner_in_refusal(&owned_by_message("ov-3", 0), "ov-3"),
            Some(0)
        );
        assert_eq!(owner_in_refusal(&m, "DEMO-1"), None);
        assert_eq!(owner_in_refusal("DEMO-12 is owned by Sx", "DEMO-12"), None);
        assert_eq!(
            owner_in_refusal("DEMO-12 is owned by S3 now", "DEMO-12"),
            None
        );
        assert_eq!(owner_in_refusal("invalid slot id 9", "DEMO-12"), None);
    }

    fn src(name: &str, at: Option<i64>, message: Option<&str>) -> SourceStatus {
        SourceStatus {
            name: name.into(),
            ok: at.is_some(),
            count: 0,
            at,
            message: message.map(String::from),
        }
    }

    #[test]
    fn not_read_yet_until_the_items_source_answers() {
        let mut v = view();
        let bead = v.items[1].clone();
        let mut ticket = v.items[0].clone();
        ticket.in_list = false;
        v.sources = vec![
            src("beads", None, Some("not read yet")),
            src("jira", Some(1), None),
        ];
        assert!(v.not_read_yet(&bead));
        assert!(v.not_read_yet(&ticket), "beads can list a ticket too");
        v.sources = vec![
            src("beads", Some(1), None),
            src("jira", None, Some("not read yet")),
        ];
        assert!(!v.not_read_yet(&bead));
        assert!(v.not_read_yet(&ticket));
        // Jira off is never read and does not count.
        v.sources = vec![
            src("beads", Some(1), None),
            src("jira", None, Some(JIRA_OFF)),
        ];
        assert!(!v.not_read_yet(&ticket));
        // An item in the list is never "not read yet".
        assert!(!v.not_read_yet(&v.items[0].clone()));
    }

    #[test]
    fn work_requests_round_trip() {
        for req in [
            Request::ListWorkItems,
            Request::RefreshWorkItems,
            Request::Assign {
                key: "DEMO-1".into(),
                slot: 2,
                confirm: true,
                move_owner: true,
            },
            Request::Release {
                key: "DEMO-1".into(),
            },
            Request::Kickoff { slot: 3 },
            Request::ClearNotices { slot: 3 },
        ] {
            let json = serde_json::to_string(&req).unwrap();
            assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), req);
        }
    }

    #[test]
    fn force_release_and_relay_read_without_their_new_fields() {
        let req: Request =
            serde_json::from_str(r#"{"type":"ForceRelease","target":"argo:uat/root"}"#).unwrap();
        assert_eq!(
            req,
            Request::ForceRelease {
                target: "argo:uat/root".into(),
                holder: None,
            }
        );
        let relay: Relay = serde_json::from_str(
            r#"{"id":1,"at":2,"from":{"type":"You"},"to":3,"text":"hi","state":"Pending","delivered_at":null,"held":null}"#,
        )
        .unwrap();
        assert_eq!(relay.dropped_at, None);
    }

    #[test]
    fn assign_defaults_flags() {
        let req: Request =
            serde_json::from_str(r#"{"type":"Assign","key":"DEMO-1","slot":2}"#).unwrap();
        assert_eq!(
            req,
            Request::Assign {
                key: "DEMO-1".into(),
                slot: 2,
                confirm: false,
                move_owner: false,
            }
        );
    }

    #[test]
    fn work_responses_round_trip() {
        for resp in [
            Response::Work(view()),
            Response::NeedsConfirm {
                message: "S2 is busy".into(),
            },
            Response::Text { text: "hi".into() },
        ] {
            let json = serde_json::to_string(&resp).unwrap();
            assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), resp);
        }
    }

    #[test]
    fn work_changed_round_trips() {
        let ev = Event::WorkChanged(view());
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), ev);
    }

    fn claim() -> Claim {
        Claim {
            target: "repo:demo-api:main".into(),
            slot: 2,
            kind: ClaimKind::Command,
            reason: Some("pushing".into()),
            item: Some("DEMO-12".into()),
            since: 100,
            last_used: 150,
            pin_revision: None,
        }
    }

    fn lock_view() -> LockView {
        let relay = |id, to, state| Relay {
            id,
            at: 10 + id,
            from: Sender::Slot { slot: 1 },
            to,
            text: "hello".into(),
            state,
            delivered_at: None,
            dropped_at: None,
            held: None,
        };
        LockView {
            claims: vec![claim()],
            waits: vec![
                Wait {
                    target: "repo:demo-api:main".into(),
                    slot: 3,
                    at: 120,
                    reason: None,
                },
                Wait {
                    target: "uat/root".into(),
                    slot: 2,
                    at: 121,
                    reason: Some("apply".into()),
                },
                Wait {
                    target: "staging-eu".into(),
                    slot: 2,
                    at: 122,
                    reason: None,
                },
            ],
            relays: vec![
                relay(3, 2, RelayState::Pending),
                relay(2, 2, RelayState::Delivered),
                relay(1, 4, RelayState::Pending),
            ],
            flag_after_secs: 3600,
        }
    }

    #[test]
    fn daemon_info_round_trips() {
        let req = Request::DaemonInfo;
        let back: Request = serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(back, req);
        let resp = Response::DaemonInfo(DaemonInfo {
            version: "0.1.0".into(),
            build: "abc1234-202611140930".into(),
            pid: 42,
            started_at: 1_700_000_000,
            exe: "/Applications/Overseer.app/Contents/MacOS/overseer".into(),
        });
        let back: Response = serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
        assert_eq!(back, resp);
    }

    #[test]
    fn lock_requests_round_trip() {
        for req in [
            Request::ListLocks,
            Request::ClaimTarget {
                slot: 2,
                target: "repo:demo-api:main".into(),
                reason: Some("push".into()),
                item: Some("DEMO-12".into()),
            },
            Request::ReleaseTarget {
                slot: 2,
                target: "uat/root".into(),
            },
            Request::ForceRelease {
                target: "uat/root".into(),
                holder: None,
            },
            Request::ForceRelease {
                target: "uat/root".into(),
                holder: Some(2),
            },
            Request::SendRelay {
                from: Some(0),
                to: 2,
                text: "hi".into(),
            },
            Request::SendRelay {
                from: None,
                to: 2,
                text: "hi".into(),
            },
            Request::CheckBash(BashCheck {
                slot: 2,
                session_id: Some("abc".into()),
                tool_use_id: Some("tu1".into()),
                command: "git push origin main".into(),
                cwd: Some(PathBuf::from("/tmp/demo-api")),
                env: vec![("HOME".into(), "/tmp".into())],
            }),
        ] {
            let json = serde_json::to_string(&req).unwrap();
            assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), req);
        }
    }

    #[test]
    fn lock_responses_and_event_round_trip() {
        for resp in [
            Response::Locks(lock_view()),
            Response::Verdict {
                deny: None,
                rewrite: None,
            },
            Response::Verdict {
                deny: Some("S2 holds it".into()),
                rewrite: None,
            },
            Response::Queued {
                message: "queued".into(),
            },
        ] {
            let json = serde_json::to_string(&resp).unwrap();
            assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), resp);
        }
        let ev = Event::LocksChanged(lock_view());
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), ev);
    }

    #[test]
    fn claim_target_defaults() {
        let req: Request = serde_json::from_str(
            r#"{"type":"ClaimTarget","slot":2,"target":"repo:demo-api:main"}"#,
        )
        .unwrap();
        assert_eq!(
            req,
            Request::ClaimTarget {
                slot: 2,
                target: "repo:demo-api:main".into(),
                reason: None,
                item: None,
            }
        );
    }

    #[test]
    fn old_hook_event_parses() {
        let ev: HookEvent =
            serde_json::from_str(r#"{"slot":1,"event":"Stop","session_id":"abc","tool":null,"detail":null,"notification":null}"#)
                .unwrap();
        assert_eq!(ev.tool_use_id, None);
        assert_eq!(ev.notification_type, None);
        let a: Activity =
            serde_json::from_str(r#"{"kind":"Idle","since":5,"now_doing":null,"reason":null}"#)
                .unwrap();
        assert!(!a.idle_wait);
    }

    #[test]
    fn brain_runs_from_an_older_daemon_parse() {
        let v: BrainView = serde_json::from_str(
            r#"{"on":true,"entries":[],"last_backup":1700000100,"last_sync":[1700000200,false],"push_problem":null}"#,
        )
        .unwrap();
        assert_eq!(
            v.last_backup,
            Some(BrainRun {
                at: 1700000100,
                ok: true,
                detail: String::new()
            })
        );
        assert_eq!(
            v.last_sync,
            Some(BrainRun {
                at: 1700000200,
                ok: false,
                detail: String::new()
            })
        );
        let v: BrainView =
            serde_json::from_str(r#"{"on":false,"entries":[],"push_problem":null}"#).unwrap();
        assert_eq!((v.last_backup, v.last_sync), (None, None));
    }

    #[test]
    fn bash_check_without_cwd_or_env_parses() {
        let c: BashCheck = serde_json::from_str(
            r#"{"slot":1,"session_id":null,"tool_use_id":null,"command":"bd close DEMO-12"}"#,
        )
        .unwrap();
        assert_eq!(c.cwd, None);
        assert!(c.env.is_empty());
        assert!(CHECK_ENV.contains(&"BEADS_DIR"));
    }

    #[test]
    fn sender_shapes() {
        let v = serde_json::to_value(Sender::Slot { slot: 3 }).unwrap();
        assert_eq!(v, serde_json::json!({"type": "Slot", "slot": 3}));
        assert_eq!(
            serde_json::to_value(Sender::You).unwrap(),
            serde_json::json!({"type": "You"})
        );
        let back: Sender = serde_json::from_value(v).unwrap();
        assert_eq!(back, Sender::Slot { slot: 3 });
    }

    fn queue_entry() -> QueueEntry {
        QueueEntry {
            id: 4,
            at: 1700000000,
            from: Sender::Slot { slot: 3 },
            paths: vec!["Sprints/sprint-26-01/DEMO-12-demo-lane/notes.md".into()],
            message: "S3: notes".into(),
            state: QueueState::Committed,
            commit: Some("abc1234".into()),
            pushed: Some(false),
            error: Some("rejected".into()),
        }
    }

    fn brain_view() -> BrainView {
        BrainView {
            on: true,
            entries: vec![
                queue_entry(),
                QueueEntry {
                    id: 3,
                    at: 1699999000,
                    from: Sender::You,
                    paths: vec![],
                    message: "m".into(),
                    state: QueueState::Queued,
                    commit: None,
                    pushed: None,
                    error: None,
                },
            ],
            last_backup: Some(BrainRun {
                at: 1700000100,
                ok: false,
                detail: "index.lock still held after 4 attempts".into(),
            }),
            last_sync: Some(BrainRun {
                at: 1700000200,
                ok: true,
                detail: "sync done".into(),
            }),
            push_problem: None,
            brain_dir: None,
        }
    }

    #[test]
    fn brain_types_round_trip() {
        let e = queue_entry();
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<QueueEntry>(&json).unwrap(), e);
        for state in [
            QueueState::Queued,
            QueueState::Committed,
            QueueState::NothingToCommit,
            QueueState::Failed,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            assert_eq!(serde_json::from_str::<QueueState>(&json).unwrap(), state);
        }
        let v = brain_view();
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(serde_json::from_str::<BrainView>(&json).unwrap(), v);
        let ev = Event::BrainChanged(v.clone());
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), ev);
        let resp = Response::Brain(v);
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), resp);
        let resp = Response::BdOutput {
            code: 1,
            stdout: "o".into(),
            stderr: "e".into(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), resp);
    }

    #[test]
    fn brain_requests_round_trip() {
        let check = WriteCheck {
            slot: 2,
            tool: "Edit".into(),
            paths: vec![PathBuf::from("/tmp/vault/a b.md")],
            cwd: Some(PathBuf::from("/tmp/vault")),
        };
        let json = serde_json::to_string(&check).unwrap();
        assert_eq!(serde_json::from_str::<WriteCheck>(&json).unwrap(), check);
        for req in [
            Request::Commit {
                from: Some(2),
                paths: vec!["a.md".into()],
                message: "m".into(),
            },
            Request::AppendJournal {
                from: None,
                section: "Work Log".into(),
                text: "t".into(),
            },
            Request::EditShared {
                from: Some(1),
                path: "p".into(),
                old_text: "o".into(),
                new_text: "n".into(),
            },
            Request::UpdateCurrentSession {
                slot: 3,
                text: "t".into(),
            },
            Request::RunBd {
                from: Some(3),
                args: vec!["list".into()],
                stdin: Some("body\n".into()),
            },
            Request::BrainStatus,
            Request::CheckWrite(check),
        ] {
            let json = serde_json::to_string(&req).unwrap();
            assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), req);
        }
    }

    #[test]
    fn run_bd_without_stdin_decodes_to_none() {
        let old = r#"{"type":"RunBd","from":null,"args":["list"]}"#;
        let req: Request = serde_json::from_str(old).unwrap();
        assert_eq!(
            req,
            Request::RunBd {
                from: None,
                args: vec!["list".into()],
                stdin: None
            }
        );
    }

    #[test]
    fn verdict_without_rewrite_decodes_to_none() {
        let resp: Response =
            serde_json::from_str(r#"{"type":"Verdict","data":{"deny":"no"}}"#).unwrap();
        assert_eq!(
            resp,
            Response::Verdict {
                deny: Some("no".into()),
                rewrite: None
            }
        );
        let with = Response::Verdict {
            deny: None,
            rewrite: Some("overseer ctl bd -- list".into()),
        };
        let json = serde_json::to_string(&with).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), with);
    }

    #[test]
    fn lock_view_helpers() {
        let v = lock_view();
        let c = claim();
        // last_used is 150 and the limit 3600.
        assert!(!v.flagged(&c, 150 + 3600));
        assert!(v.flagged(&c, 150 + 3601));
        assert_eq!(v.waiting_on(2), vec!["uat/root", "staging-eu"]);
        assert_eq!(v.waiting_on(3), vec!["repo:demo-api:main"]);
        assert!(v.waiting_on(5).is_empty());
        assert_eq!(v.pending_for(2), 1);
        assert_eq!(v.pending_for(4), 1);
        assert_eq!(v.pending_for(0), 0);
    }
}
