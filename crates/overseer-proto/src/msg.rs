use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::settings::{SettingEdit, SettingsView};

/// Slot index, `0..=6`.
pub type SlotId = u8;

/// Display name for a slot, e.g. `S3`.
pub fn slot_name(id: SlotId) -> String {
    format!("S{id}")
}

/// `0m` under a minute (and for clock skew), then `5m`, `2h`, `3d`.
pub fn age_text(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        0..=3599 => format!("{}m", s / 60),
        3600..=86399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
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
    /// True when a holder process keeps the slot's terminal, so the session
    /// can outlive the daemon.
    #[serde(default)]
    pub held: bool,
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
    /// Fires before a tool runs; carries the question for `AskUserQuestion`.
    PreToolUse,
    /// Claude is asking permission to run a tool.
    PermissionRequest,
    /// A hook this build does not know, so a newer hook cannot break an
    /// older daemon.
    #[serde(other)]
    Other,
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
    /// `$OVERSEER_RUN` from the hook's environment: which run of the slot
    /// sent it, so a late event from a killed run is not applied to the next.
    #[serde(default)]
    pub run: Option<String>,
    /// Claude Code's `agent_id`, set when the event fires inside a subagent
    /// or teammate rather than the main conversation.
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Path of the session's transcript file, as Claude reported it.
    #[serde(default)]
    pub transcript_path: Option<String>,
    /// The question or permission prompt this event carries, if any.
    #[serde(default)]
    pub prompt: Option<PromptInfo>,
}

/// What a session is asking you, as the hook saw it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum PromptInfo {
    /// An `AskUserQuestion` call.
    Question { questions: Vec<Question> },
    /// A permission request for a tool. `plan` is the plan text when the
    /// tool is exiting plan mode.
    Permission {
        tool: String,
        summary: String,
        plan: Option<String>,
    },
    /// A prompt kind this build does not know.
    #[serde(other)]
    Other,
}

/// One question in an `AskUserQuestion` call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub header: String,
    pub question: String,
    pub options: Vec<String>,
    /// More than one option may be picked.
    pub multi: bool,
}

/// One line of a slot's conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConvEntry {
    /// Unix seconds.
    pub at: i64,
    pub kind: ConvKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ConvKind {
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    Tool {
        name: String,
        summary: String,
    },
    /// An entry kind this build does not know. Clients leave it out.
    #[serde(other)]
    Other,
}

/// What the board can show of a prompt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "shown")]
pub enum PromptShown {
    Known(PromptInfo),
    /// A prompt Overseer cannot show, or a kind this build does not know;
    /// answer it in the terminal.
    #[serde(other)]
    Unknown,
}

/// A prompt the session is waiting on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Prompt {
    pub id: String,
    /// Raised inside a subagent rather than the main conversation.
    pub subagent: bool,
    pub info: PromptShown,
}

/// A slot's conversation: the entries from `entries_from` on, and the prompt
/// it is waiting on, if any.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationView {
    pub slot: SlotId,
    pub session: String,
    pub entries_from: usize,
    pub entries: Vec<ConvEntry>,
    pub prompt: Option<Prompt>,
    /// The context the session's last request used, in tokens, once its
    /// transcript has said.
    #[serde(default)]
    pub context: Option<u64>,
}

/// Your answer to a prompt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Answer {
    /// The permission option at this index.
    Permission { choice: usize },
    /// One answer per question, in order.
    Questions { answers: Vec<QuestionAnswer> },
    /// An answer kind this build does not know.
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum QuestionAnswer {
    /// The options picked, by index.
    Options { picked: Vec<usize> },
    /// Free text instead of an option.
    Other { text: String },
    /// An answer kind this build does not know. Distinct from `Other`
    /// above, which is free text.
    #[serde(other)]
    Unrecognised,
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
    /// The kanban stage shown for the ticket; `None` for beads and for
    /// tickets from an old daemon.
    #[serde(default)]
    pub stage: Option<Stage>,
    /// Unix seconds the ticket entered its Jira stage, only when the daemon
    /// saw it move.
    #[serde(default)]
    pub stage_since: Option<i64>,
}

/// A kanban column, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Stage {
    ToDo,
    /// Was `Waiting`; the alias lets an older peer's value still parse.
    #[serde(alias = "Waiting")]
    Blocked,
    Next,
    /// Absorbs the old `InReview`.
    #[serde(alias = "InReview")]
    InProgress,
    /// Was `Testing`.
    #[serde(alias = "Testing")]
    ToBeTested,
    Done,
}

impl Stage {
    pub const ALL: [Stage; 6] = [
        Stage::ToDo,
        Stage::Blocked,
        Stage::Next,
        Stage::InProgress,
        Stage::ToBeTested,
        Stage::Done,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Stage::ToDo => "TODO",
            Stage::Blocked => "BLOCKED",
            Stage::Next => "NEXT",
            Stage::InProgress => "IN PROGRESS",
            Stage::ToBeTested => "TO BE TESTED",
            Stage::Done => "DONE",
        }
    }
}

/// A bead under an item, shown in its detail.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChildBead {
    pub key: String,
    pub title: String,
    pub status: String,
}

/// Everything the detail view shows of one item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemDetail {
    pub key: String,
    pub kind: ItemKind,
    pub title: String,
    pub status: String,
    #[serde(default)]
    pub priority: Option<String>,
    #[serde(default)]
    pub stage: Option<Stage>,
    /// Unix seconds the item entered its stage.
    #[serde(default)]
    pub stage_since: Option<i64>,
    #[serde(default)]
    pub folder: Option<PathBuf>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub children: Vec<ChildBead>,
    #[serde(default)]
    pub prs: Vec<PullRequest>,
    /// The project overview paragraph, filled only when `body` is `None`.
    #[serde(default)]
    pub overview: Option<String>,
    /// Unix seconds the detail was fetched.
    #[serde(default)]
    pub fetched_at: Option<i64>,
    /// Why the fetch failed, when it did.
    #[serde(default)]
    pub error: Option<String>,
}

/// An open pull request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullRequest {
    pub repo: String,
    pub id: u64,
    pub title: String,
    pub branch: String,
    pub url: Option<String>,
    pub author: Option<String>,
    /// Unix seconds the PR was opened.
    pub created: Option<i64>,
    /// The ticket key found in the title or branch.
    pub ticket: Option<String>,
    /// The reviews Bitbucket reports, or `None` when the source does not
    /// list participants (`bb` does not).
    #[serde(default)]
    pub approvals: Option<Approvals>,
    /// The comment count the source reports, `None` when it does not.
    #[serde(default)]
    pub comments: Option<u32>,
    /// The latest build on the source branch, `None` when unknown.
    #[serde(default)]
    pub build: Option<BuildState>,
    /// The head commit hash, to tell a new push from a seen one.
    #[serde(default)]
    pub head: Option<String>,
}

/// How a pull request's latest build stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BuildState {
    Passed,
    Failed,
    Running,
}

/// The reviews on a pull request, not counting its author's own.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approvals {
    pub human: u32,
    /// Approvals from the accounts in `ai_reviewers`.
    pub ai: u32,
    /// Someone asked for changes.
    pub changes_requested: bool,
}

impl Approvals {
    /// Enough to merge: 2 human approvals, or 1 human and 1 AI, and no one
    /// asking for changes.
    pub fn ready(&self) -> bool {
        !self.changes_requested && (self.human >= 2 || (self.human >= 1 && self.ai >= 1))
    }

    /// `1H 1AI`, `2H` or `1AI`; empty with no approvals.
    pub fn badge(&self) -> String {
        let mut parts = Vec::new();
        if self.human > 0 {
            parts.push(format!("{}H", self.human));
        }
        if self.ai > 0 {
            parts.push(format!("{}AI", self.ai));
        }
        parts.join(" ")
    }
}

/// Where today falls in the sprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SprintDay {
    pub day: u32,
    pub days: u32,
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
    /// The sprint name and day for the board header, when there is a sprint.
    #[serde(default)]
    pub sprint: Option<String>,
    /// Open pull requests from the configured repos.
    #[serde(default)]
    pub prs: Vec<PullRequest>,
    /// The sprint day for the pace marker, when the sprint start is known.
    #[serde(default)]
    pub sprint_day: Option<SprintDay>,
    /// Why notes are off, when Overseer would not take the notes folder;
    /// the board shows it as a banner.
    #[serde(default)]
    pub notes_off: Option<String>,
}

/// The Jira source's message while `jira_command` is empty. Jira is then
/// never read, so it never holds back an item's "not read yet".
pub const JIRA_OFF: &str = "jira_command is empty; Jira is off";

/// The PR source's message while `pr_repos` is empty.
pub const PRS_OFF: &str = "pr_repos and pr_workspace are empty; PRs are off";

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
    /// Retired unsent; `Relay::drop_reason` says why.
    Dropped,
}

/// What a relay is about, so it can be retired once it no longer applies.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum RelayKind {
    /// A message, or a notice with nothing to re-check.
    #[default]
    Note,
    /// A plan's kickoff for a running slot. A later plan's kickoff to the
    /// same slot replaces it while it is pending.
    Kickoff { plan: i64 },
    /// `... You now hold it.` for `target`. Dropped unsent when the slot no
    /// longer holds it.
    Lock { target: String },
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
    #[serde(default)]
    pub kind: RelayKind,
    /// Why a `Dropped` relay was retired, e.g. `session ended`.
    #[serde(default)]
    pub drop_reason: Option<String>,
    /// Why a pending relay is waiting, e.g. `session is working`. Filled by
    /// the courier.
    pub held: Option<String>,
    /// Unix seconds the relay started waiting to be delivered.
    #[serde(default)]
    pub held_since: Option<i64>,
}

impl Relay {
    /// `dropped: <reason>` for a dropped relay (`dropped` when no reason
    /// was kept), else `None`.
    pub fn drop_text(&self) -> Option<String> {
        (self.state == RelayState::Dropped).then(|| match self.drop_reason.as_deref() {
            Some(why) => format!("dropped: {why}"),
            None => "dropped".into(),
        })
    }
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
    /// The attention queue.
    Attention,
    /// Hide the attention row `key` while its `since` and `text` stay as
    /// given. A new occurrence or a changed text shows it again.
    DismissAttention {
        key: String,
        since: i64,
        text: String,
    },
    /// Today's timeline of what each slot did and where its log has gaps.
    Timeline,
    /// Build the end-of-day draft: today's hours per ticket and epic.
    /// `target_quarters` books a day other than the default length.
    EndOfDay {
        #[serde(default)]
        target_quarters: Option<u32>,
    },
    /// Write draft `id` with the chosen quarter-hours per epic key.
    /// `replace` overwrites files that already exist for the day.
    SaveDay {
        id: u64,
        hours: Vec<(String, u32)>,
        replace: bool,
    },
    /// Mark a pull request's current comments and build as seen.
    PrSeen {
        repo: String,
        id: u64,
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
    /// Ends every session, held ones included, leaving their ledger rows
    /// live so the next daemon offers them as resumable, then stops the
    /// daemon as `Shutdown` does. Answers an error naming the sessions it
    /// could not end (the daemon stops anyway and they keep running).
    StopAll,
    /// A Claude Code hook event, relayed by `overseer hook`.
    Hook(HookEvent),
    /// The slot's conversation in full, answered `Response::Conversation`.
    Conversation {
        slot: SlotId,
    },
    /// Answers the slot's pending prompt `prompt`.
    Answer {
        slot: SlotId,
        prompt: String,
        answer: Answer,
    },
    /// Types `text` into the slot's session, if it is still `session` and
    /// still waiting on `prompt`.
    Reply {
        slot: SlotId,
        session: String,
        #[serde(default)]
        prompt: Option<String>,
        text: String,
    },
    /// Sends the slot's session an interrupt.
    Interrupt {
        slot: SlotId,
    },
    /// The cached work-item list with owners, source health and notices.
    ListWorkItems,
    /// Re-reads the sources now instead of waiting for the next poll.
    RefreshWorkItems,
    /// The detail of one owned item. `refresh` bypasses the cache.
    GetItemDetail {
        key: String,
        #[serde(default)]
        refresh: bool,
    },
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
    /// What dropping ticket `key` on `slot` would do, with nothing done
    /// yet: `Response::StartDraft`. `repo` picks the repo; `None` takes the
    /// last one used.
    PrepareStart {
        slot: SlotId,
        key: String,
        #[serde(default)]
        repo: Option<PathBuf>,
    },
    /// Starts `slot` on ticket `key` in a new worktree of `repo` on
    /// `branch`, with the start brief as its first prompt.
    StartTicket {
        slot: SlotId,
        key: String,
        repo: PathBuf,
        branch: String,
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
    /// Drops pending relays unsent: the one with `id`, or every one to
    /// `to`. `from` is the slot asking, which may only drop relays it sent;
    /// `None` is you. Answered with `Text` saying how many were dropped.
    DropRelays {
        #[serde(default)]
        from: Option<SlotId>,
        #[serde(default)]
        id: Option<i64>,
        #[serde(default)]
        to: Option<SlotId>,
    },
    /// From `slot`'s own MCP server after each tool call: if a relay to
    /// `slot` has been pending for `STALE_RELAY_SECS`, every pending relay
    /// to it is marked delivered and their text answered as `Text`, to be
    /// shown in the tool's reply. `Text` is empty when none is that old.
    /// The fallback for a slot the courier cannot type into (#58).
    TakeStaleRelays {
        slot: SlotId,
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
    /// Fetches the notes folder, merges its remote and pushes. `confirm`
    /// pushes a merge that was held for making a file much shorter.
    SyncNotes {
        #[serde(default)]
        from: Option<SlotId>,
        #[serde(default)]
        confirm: bool,
    },
    /// Writes `path` as it was at commit `rev` and commits it.
    RestoreNotesFile {
        #[serde(default)]
        from: Option<SlotId>,
        path: String,
        rev: String,
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
    /// `Response::Text`: a slot's live screen as text, and what the relay
    /// courier reads from it (the input box, the working line).
    SlotScreen {
        id: SlotId,
    },
    /// `Response::Settings`: `config.toml` as Settings shows it.
    GetSettings,
    /// Apply `edits` to `config.toml` when its hash is still `base_hash`;
    /// answers the new `Response::Settings` or the refusal.
    SaveSettings {
        base_hash: String,
        edits: Vec<SettingEdit>,
    },
    /// Store an API token in the Keychain for `jira_email`; answers
    /// `Response::Ok` or an error. The token is never shown in a `Debug`
    /// dump, and the daemon never logs or answers it.
    SetToken {
        kind: TokenKind,
        token: SecretText,
    },
}

/// Which API token a `SetToken` stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenKind {
    Jira,
    Bitbucket,
}

impl TokenKind {
    /// The name people know the token by: `Jira` or `Bitbucket`.
    pub fn label(self) -> &'static str {
        match self {
            TokenKind::Jira => "Jira",
            TokenKind::Bitbucket => "Bitbucket",
        }
    }
}

/// A secret in a request. It travels as a plain string, but `Debug` shows
/// `"<hidden>"`, so a logged request cannot leak it.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretText(String);

impl SecretText {
    /// The secret itself, for the one place that must use it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl From<String> for SecretText {
    fn from(s: String) -> Self {
        SecretText(s)
    }
}

impl From<&str> for SecretText {
    fn from(s: &str) -> Self {
        SecretText(s.to_owned())
    }
}

impl std::fmt::Debug for SecretText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("\"<hidden>\"")
    }
}

/// One write target a plan has a slot claim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanTarget {
    pub target: String,
    #[serde(default)]
    pub reason: Option<String>,
}

/// What starting a slot on a ticket would do, for the confirm card.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartDraft {
    pub slot: SlotId,
    pub key: String,
    pub title: String,
    /// The repos a worktree can come from, the chosen one among them.
    pub repos: Vec<PathBuf>,
    pub repo: PathBuf,
    /// A branch no other branch or worktree folder has yet.
    pub branch: String,
    pub worktree: PathBuf,
    /// The first prompt the session starts with.
    pub brief: String,
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
    /// Items another slot owns that this slot takes over. Approving the
    /// plan moves them; naming one in `items` instead is refused.
    #[serde(default)]
    pub moves: Vec<String>,
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
    /// When it committed (unix seconds); `None` until then, and from an
    /// older daemon.
    #[serde(default)]
    pub committed_at: Option<i64>,
}

impl QueueEntry {
    /// When it committed, else when it was queued.
    pub fn when(&self) -> i64 {
        self.committed_at.unwrap_or(self.at)
    }
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
    /// The notes folder when it exists, whether or not the writer is on:
    /// where a new session starts.
    #[serde(default)]
    pub notes_dir: Option<std::path::PathBuf>,
    /// How the notes folder stands against its upstream, from the last
    /// check. `None` before the first check or with no upstream.
    #[serde(default)]
    pub remote: Option<NotesRemote>,
    /// The last push Overseer tried.
    #[serde(default)]
    pub last_push: Option<BrainRun>,
}

/// The notes folder against its upstream branch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotesRemote {
    /// Local commits the upstream does not have.
    pub ahead: u32,
    /// Upstream commits not merged here, as of the last fetch.
    pub behind: u32,
    /// Unix seconds of the check.
    pub checked_at: i64,
    /// Why the fetch failed, when it did; the counts are then from the
    /// last good fetch.
    #[serde(default)]
    pub fetch_error: Option<String>,
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
    Attention(AttentionView),
    Timeline(DayView),
    /// The end-of-day draft.
    EndOfDay(DayDraft),
    /// The day was written. Paths are shown with the home folder as `~`.
    DaySaved {
        time_log: String,
        allocation: Option<String>,
        push: Option<String>,
    },
    /// Files for the day already exist and `replace` was not set.
    DayExists {
        paths: Vec<String>,
    },
    Conversation(ConversationView),
    Slots(Vec<SlotInfo>),
    Error {
        message: String,
    },
    Work(WorkView),
    ItemDetail(ItemDetail),
    /// The request needs the caller to confirm and resend with `confirm` set.
    NeedsConfirm {
        message: String,
    },
    /// The answer to `PrepareStart`.
    StartDraft(StartDraft),
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
    Settings(SettingsView),
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
    AttentionChanged(AttentionView),
    TimelineChanged(DayView),
    ConversationChanged(ConversationView),
    /// An event type this build does not know, built only by the decoder so
    /// a newer daemon cannot break an older client. Never sent.
    #[serde(skip)]
    Unknown {
        kind: String,
    },
}

/// The `type` names `Event` serializes to. The frame decoder uses it to tell
/// an event from the future from a damaged one.
pub const EVENT_TYPES: &[&str] = &[
    "SlotChanged",
    "WorkChanged",
    "LocksChanged",
    "BrainChanged",
    "CoordChanged",
    "AttentionChanged",
    "TimelineChanged",
    "ConversationChanged",
];

/// Why something is on the attention queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionKind {
    NeedsYou,
    Waiting,
    RelayStuck,
    Notice,
    PrBuildFailed,
    PrChangesRequested,
    PrComments,
    /// A session ran for a while with nothing written to its log.
    LogGap,
    /// A slot idle for a while with uncommitted changes in its folder.
    Uncommitted,
    /// An open PR with no push, comment or build change for a while.
    PrQuiet,
    /// A slot's branch is many commits behind the remote's main branch.
    BehindMain,
    /// A kind this build does not know, so a newer daemon cannot break an
    /// older client. Clients leave these out of what they show.
    #[serde(other)]
    Other,
}

/// One thing that wants your attention.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttentionItem {
    /// Stable across refreshes, e.g. `needs_you:3`.
    pub key: String,
    pub kind: AttentionKind,
    pub slot: Option<SlotId>,
    /// Repo and id of the pull request, for the PR kinds.
    pub pr: Option<(String, u64)>,
    pub url: Option<String>,
    /// Unix seconds it started wanting attention.
    pub since: i64,
    pub text: String,
    /// The user dismissed this row as it is now. Still sent so a client can
    /// list it, but left out of the badge and the board's panel.
    #[serde(default)]
    pub dismissed: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AttentionView {
    pub items: Vec<AttentionItem>,
}

impl AttentionView {
    /// How many items to badge: kinds this build does not know and
    /// dismissed rows are left out.
    pub fn count(&self) -> usize {
        self.items
            .iter()
            .filter(|i| i.kind != AttentionKind::Other && !i.dismissed)
            .count()
    }
}

/// What one kind of stretch of time on the timeline was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanKind {
    /// Covered by the session log.
    Logged,
    /// Nothing in the log for longer than the flag threshold.
    Gap,
    /// Running with no log to compare against.
    Unlogged,
    /// A long wait cut out of the stretch, drawn as a thin line.
    Wait,
    /// Uncovered but short enough not to flag; drawn as logged, with the
    /// hover `short gap, not flagged`.
    Short,
    /// A kind this build does not know, so a newer daemon cannot break an
    /// older client. Clients draw and list nothing for it.
    #[serde(other)]
    Other,
}

/// A stretch of one kind, in unix seconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Span {
    pub from: i64,
    pub to: i64,
    pub kind: SpanKind,
}

/// One run of a session in a slot, with how its time was spent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DayStretch {
    pub session: String,
    pub first: i64,
    pub last: i64,
    /// Still going now.
    pub running: bool,
    pub spans: Vec<Span>,
    /// The log files the spans were read from.
    pub logs: Vec<String>,
}

/// Everything one slot did today.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DayRow {
    pub slot: SlotId,
    pub stretches: Vec<DayStretch>,
}

/// The day timeline: one row per slot that ran today.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DayView {
    /// Local date, `YYYY-MM-DD`.
    pub day: String,
    /// Unix seconds the day starts.
    pub start: i64,
    /// Unix seconds when this view was built.
    pub now: i64,
    pub rows: Vec<DayRow>,
}

impl DayView {
    /// The gap spans for a slot, in time order.
    pub fn gaps(&self, slot: SlotId) -> Vec<&Span> {
        let mut gaps: Vec<&Span> = self
            .rows
            .iter()
            .filter(|r| r.slot == slot)
            .flat_map(|r| r.stretches.iter())
            .flat_map(|s| s.spans.iter())
            .filter(|sp| sp.kind == SpanKind::Gap)
            .collect();
        gaps.sort_by_key(|sp| sp.from);
        gaps
    }
}

/// Something the end-of-day draft could not settle on its own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DayProblem {
    pub text: String,
    /// The slot the problem belongs to, if any.
    #[serde(default)]
    pub slot: Option<SlotId>,
    /// A file to look at, shown with the home folder as `~`.
    #[serde(default)]
    pub path: Option<String>,
}

/// Minutes worked on one ticket today.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TicketMinutes {
    pub ticket: String,
    pub minutes: f64,
    /// The epic the ticket rolls up to, if known.
    #[serde(default)]
    pub epic: Option<String>,
}

/// Hours for one epic: the quarter-hours chosen and the hours derived from
/// the logs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EpicHours {
    pub key: String,
    pub summary: String,
    pub quarters: u32,
    pub derived_hours: f64,
    #[serde(default)]
    pub tickets: Vec<String>,
}

/// The end-of-day draft: what the logs say about today, ready to adjust and
/// save.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DayDraft {
    pub id: u64,
    /// Local date, `YYYY-MM-DD`.
    pub day: String,
    /// The day's total in quarter-hours.
    pub target_quarters: u32,
    /// Whether the epic quarters add up to the target.
    pub split_ok: bool,
    /// True when the day's target cannot be changed.
    #[serde(default)]
    pub target_fixed: bool,
    #[serde(default)]
    pub problems: Vec<DayProblem>,
    #[serde(default)]
    pub tickets: Vec<TicketMinutes>,
    #[serde(default)]
    pub epics: Vec<EpicHours>,
    /// A note on how well the split fits the target.
    #[serde(default)]
    pub fit: Option<String>,
}

#[cfg(test)]
mod tests {
    #[test]
    fn old_pull_request_and_relay_json_still_read() {
        let pr: PullRequest = serde_json::from_str(
            r#"{"repo":"demo-api","id":7,"title":"t","branch":"DEMO-1","url":null,"author":null,"created":null,"ticket":null}"#,
        )
        .unwrap();
        assert_eq!((pr.comments, pr.build, pr.head), (None, None, None));
        let r: Relay = serde_json::from_str(
            r#"{"id":1,"at":2,"from":{"type":"You"},"to":3,"text":"hi","state":"Pending","delivered_at":null,"held":null}"#,
        )
        .unwrap();
        assert_eq!(r.held_since, None);
    }

    fn demo_day() -> DayView {
        DayView {
            day: "2026-11-24".into(),
            start: 1000,
            now: 9000,
            rows: vec![DayRow {
                slot: 2,
                stretches: vec![DayStretch {
                    session: "demo-session".into(),
                    first: 1000,
                    last: 8000,
                    running: true,
                    spans: vec![
                        Span {
                            from: 1000,
                            to: 2000,
                            kind: SpanKind::Logged,
                        },
                        Span {
                            from: 2000,
                            to: 3000,
                            kind: SpanKind::Gap,
                        },
                        Span {
                            from: 3000,
                            to: 4000,
                            kind: SpanKind::Unlogged,
                        },
                        Span {
                            from: 4000,
                            to: 5000,
                            kind: SpanKind::Wait,
                        },
                        Span {
                            from: 5000,
                            to: 5200,
                            kind: SpanKind::Short,
                        },
                    ],
                    logs: vec!["demo-ws/a.jsonl".into()],
                }],
            }],
        }
    }

    #[test]
    fn day_view_round_trips() {
        let v = demo_day();
        let ev = Event::TimelineChanged(v.clone());
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains(r#""kind":"gap""#), "{json}");
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ev);
        assert_eq!(v.gaps(2).len(), 1);
        assert_eq!(v.gaps(2)[0].from, 2000);
        assert!(v.gaps(3).is_empty());
    }

    #[test]
    fn timeline_request_and_response_round_trip() {
        let req = Request::Timeline;
        let back: Request = serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(back, req);
        let resp = Response::Timeline(demo_day());
        let back: Response = serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
        assert_eq!(back, resp);
    }

    #[test]
    fn end_of_day_messages_round_trip() {
        let req = Request::SaveDay {
            id: 3,
            hours: vec![("DEMO-1".into(), 30)],
            replace: false,
        };
        let back: Request = serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(back, req);
        for req in [
            Request::EndOfDay {
                target_quarters: Some(36),
            },
            Request::EndOfDay {
                target_quarters: None,
            },
        ] {
            let back: Request =
                serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
            assert_eq!(back, req);
        }
        let bare: Request = serde_json::from_str(r#"{"type":"EndOfDay"}"#).unwrap();
        assert_eq!(
            bare,
            Request::EndOfDay {
                target_quarters: None
            }
        );
        let draft = DayDraft {
            id: 3,
            day: "2026-01-02".into(),
            target_quarters: 30,
            split_ok: true,
            target_fixed: true,
            problems: vec![DayProblem {
                text: "log gap".into(),
                slot: Some(2),
                path: Some("~/logs/a.log".into()),
            }],
            tickets: vec![TicketMinutes {
                ticket: "DEMO-1".into(),
                minutes: 90.5,
                epic: Some("DEMO-9".into()),
            }],
            epics: vec![EpicHours {
                key: "DEMO-9".into(),
                summary: "Demo epic".into(),
                quarters: 30,
                derived_hours: 7.5,
                tickets: vec!["DEMO-1".into()],
            }],
            fit: Some("fits".into()),
        };
        for resp in [
            Response::EndOfDay(draft),
            Response::DaySaved {
                time_log: "~/time.csv".into(),
                allocation: None,
                push: Some("pushed".into()),
            },
            Response::DayExists {
                paths: vec!["~/a".into()],
            },
        ] {
            let back: Response =
                serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
            assert_eq!(back, resp);
        }
        let d: DayDraft = serde_json::from_str(
            r#"{"id":1,"day":"2026-01-02","target_quarters":30,"split_ok":false}"#,
        )
        .unwrap();
        assert!(d.problems.is_empty() && d.tickets.is_empty() && d.epics.is_empty());
        assert_eq!(d.fit, None);
        assert!(!d.target_fixed);
    }

    #[test]
    fn attention_kind_log_gap_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&AttentionKind::LogGap).unwrap(),
            "\"log_gap\""
        );
    }

    #[test]
    fn unknown_attention_kind_reads_as_other() {
        assert_eq!(
            serde_json::from_str::<AttentionKind>("\"from_the_future\"").unwrap(),
            AttentionKind::Other
        );
        let v: AttentionView = serde_json::from_str(
            r#"{"items":[{"key":"k","kind":"from_the_future","slot":null,"pr":null,"url":null,"since":1,"text":"t"}]}"#,
        )
        .unwrap();
        assert_eq!(v.items[0].kind, AttentionKind::Other);
    }

    #[test]
    fn unknown_span_kind_reads_as_other() {
        let sp: Span =
            serde_json::from_str(r#"{"from":1,"to":2,"kind":"from_the_future"}"#).unwrap();
        assert_eq!(sp.kind, SpanKind::Other);
    }

    #[test]
    fn event_types_lists_timeline_changed() {
        assert!(EVENT_TYPES.contains(&"TimelineChanged"));
    }

    #[test]
    fn attention_changed_round_trips() {
        let v = AttentionView {
            items: vec![AttentionItem {
                key: "needs_you:3".into(),
                kind: AttentionKind::NeedsYou,
                slot: Some(3),
                pr: None,
                url: None,
                since: 100,
                text: "permission prompt".into(),
                dismissed: false,
            }],
        };
        let ev = Event::AttentionChanged(v.clone());
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains(r#""kind":"needs_you""#), "{json}");
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ev);
        assert_eq!(v.count(), 1);
    }

    #[test]
    fn attention_count_skips_kinds_this_build_does_not_know() {
        let item = |kind| AttentionItem {
            key: "k".into(),
            kind,
            slot: Some(1),
            pr: None,
            url: None,
            since: 1,
            text: "t".into(),
            dismissed: false,
        };
        let v = AttentionView {
            items: vec![item(AttentionKind::Other), item(AttentionKind::LogGap)],
        };
        assert_eq!(v.count(), 1);
    }

    #[test]
    fn attention_count_skips_dismissed_rows() {
        let item = |key: &str, dismissed| AttentionItem {
            key: key.into(),
            kind: AttentionKind::NeedsYou,
            slot: Some(1),
            pr: None,
            url: None,
            since: 1,
            text: "t".into(),
            dismissed,
        };
        let v = AttentionView {
            items: vec![item("a", true), item("b", false), item("c", true)],
        };
        assert_eq!(v.count(), 1);
    }

    #[test]
    fn attention_item_without_dismissed_reads_as_not_dismissed() {
        let v: AttentionView = serde_json::from_str(
            r#"{"items":[{"key":"k","kind":"needs_you","slot":1,"pr":null,"url":null,"since":1,"text":"t"}]}"#,
        )
        .unwrap();
        assert!(!v.items[0].dismissed);
    }

    #[test]
    fn every_event_variant_has_a_listed_type_name() {
        let events = [
            Event::SlotChanged(SlotInfo {
                id: 1,
                label: String::new(),
                cwd: None,
                command: None,
                state: SlotState::Empty,
                cols: 80,
                rows: 24,
                activity: None,
                session_note: None,
                held: false,
            }),
            Event::WorkChanged(view()),
            Event::LocksChanged(lock_view()),
            Event::BrainChanged(brain_view()),
            Event::CoordChanged(coord()),
            Event::AttentionChanged(AttentionView::default()),
            Event::TimelineChanged(DayView::default()),
            Event::ConversationChanged(demo_view()),
        ];
        for ev in events {
            let v: serde_json::Value = serde_json::to_value(&ev).unwrap();
            let name = v["type"].as_str().unwrap().to_string();
            assert!(EVENT_TYPES.contains(&name.as_str()), "{name} not listed");
        }
        assert_eq!(EVENT_TYPES.len(), 8);
        assert!(serde_json::to_string(&Event::Unknown { kind: "x".into() }).is_err());
    }

    #[test]
    fn attention_requests_round_trip() {
        for req in [
            Request::Attention,
            Request::Timeline,
            Request::PrSeen {
                repo: "demo-api".into(),
                id: 7,
            },
            Request::DismissAttention {
                key: "needs_you:3".into(),
                since: 100,
                text: "permission prompt".into(),
            },
        ] {
            let back: Request =
                serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
            assert_eq!(back, req);
        }
        let resp = Response::Attention(AttentionView::default());
        let back: Response = serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
        assert_eq!(back, resp);
    }

    #[test]
    fn approvals_ready_rule_and_badge() {
        let a = |human, ai, changes_requested| Approvals {
            human,
            ai,
            changes_requested,
        };
        assert!(a(2, 0, false).ready());
        assert!(a(1, 1, false).ready());
        assert!(!a(1, 0, false).ready());
        assert!(!a(0, 2, false).ready(), "AI approvals alone are not enough");
        assert!(!a(2, 1, true).ready());
        assert_eq!(a(1, 1, false).badge(), "1H 1AI");
        assert_eq!(a(2, 0, false).badge(), "2H");
        assert_eq!(a(0, 1, false).badge(), "1AI");
        assert_eq!(a(0, 0, true).badge(), "");
    }

    #[test]
    fn old_work_view_json_still_reads() {
        let v: WorkView = serde_json::from_str(
            r#"{"items":[{"key":"DEMO-1","kind":"Ticket","title":"t","status":"Done","priority":null,"parent":null,"owner":null,"in_list":true,"folder":null}],"sources":[],"notices":[]}"#,
        )
        .unwrap();
        assert_eq!(v.items[0].stage, None);
        assert_eq!(v.items[0].stage_since, None);
        assert!(v.prs.is_empty());
        assert_eq!(v.sprint_day, None);
        assert_eq!(
            Stage::ALL.map(Stage::label),
            [
                "TODO",
                "BLOCKED",
                "NEXT",
                "IN PROGRESS",
                "TO BE TESTED",
                "DONE"
            ]
        );
    }

    #[test]
    fn old_stage_names_still_parse() {
        for (old, want) in [
            ("Waiting", Stage::Blocked),
            ("InReview", Stage::InProgress),
            ("Testing", Stage::ToBeTested),
        ] {
            let got: Stage = serde_json::from_str(&format!("\"{old}\"")).unwrap();
            assert_eq!(got, want, "{old}");
        }
    }

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
                moves: vec![],
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
            held: false,
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
                notes_off: None,
                items: vec![],
                sources: vec![],
                notices: vec![],
                sprint: None,
                prs: Vec::new(),
                sprint_day: None,
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
            run: None,
            agent_id: None,
            transcript_path: None,
            prompt: None,
        });
        let back: Request = serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(back, req);
    }

    fn demo_view() -> ConversationView {
        ConversationView {
            slot: 2,
            session: "demo-session".into(),
            entries_from: 0,
            entries: vec![
                ConvEntry {
                    at: 10,
                    kind: ConvKind::User {
                        text: "fix DEMO-1".into(),
                    },
                },
                ConvEntry {
                    at: 11,
                    kind: ConvKind::Tool {
                        name: "Bash".into(),
                        summary: "cargo test".into(),
                    },
                },
            ],
            prompt: Some(Prompt {
                id: "p1".into(),
                subagent: false,
                info: PromptShown::Known(PromptInfo::Question {
                    questions: vec![Question {
                        header: "Pick".into(),
                        question: "Which?".into(),
                        options: vec!["a".into(), "b".into()],
                        multi: true,
                    }],
                }),
            }),
            context: Some(41_312),
        }
    }

    #[test]
    fn conversation_types_round_trip() {
        fn rt<T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug>(v: T) {
            let back: T = serde_json::from_str(&serde_json::to_string(&v).unwrap()).unwrap();
            assert_eq!(back, v);
        }
        for req in [
            Request::Conversation { slot: 2 },
            Request::Answer {
                slot: 2,
                prompt: "p1".into(),
                answer: Answer::Permission { choice: 1 },
            },
            Request::Answer {
                slot: 2,
                prompt: "p1".into(),
                answer: Answer::Questions {
                    answers: vec![
                        QuestionAnswer::Options { picked: vec![0, 1] },
                        QuestionAnswer::Other {
                            text: "neither".into(),
                        },
                    ],
                },
            },
            Request::Reply {
                slot: 2,
                session: "demo-session".into(),
                prompt: Some("p1".into()),
                text: "go on".into(),
            },
            Request::Reply {
                slot: 2,
                session: "demo-session".into(),
                prompt: None,
                text: "hi".into(),
            },
            Request::Interrupt { slot: 2 },
        ] {
            rt(req);
        }
        rt(Event::ConversationChanged(demo_view()));
        rt(Response::Conversation(demo_view()));
        rt(Prompt {
            id: "p2".into(),
            subagent: true,
            info: PromptShown::Unknown,
        });
        rt(PromptInfo::Permission {
            tool: "Bash".into(),
            summary: "rm -rf demo".into(),
            plan: Some("the plan".into()),
        });
        assert!(EVENT_TYPES.contains(&"ConversationChanged"));
    }

    #[test]
    fn old_hook_event_json_still_reads() {
        let ev: HookEvent = serde_json::from_str(
            r#"{"slot":1,"event":"Stop","session_id":"abc","tool":null,"detail":null,"notification":null}"#,
        )
        .unwrap();
        assert_eq!(ev.transcript_path, None);
        assert_eq!(ev.prompt, None);
    }

    #[test]
    fn unknown_hook_kind_reads_as_other() {
        let ev: HookEvent = serde_json::from_str(
            r#"{"slot":1,"event":"SubagentStop","session_id":null,"tool":null,"detail":null,"notification":null}"#,
        )
        .unwrap();
        assert_eq!(ev.event, HookKind::Other);
    }

    #[test]
    fn unknown_conv_kind_reads_as_other() {
        let e: ConvEntry =
            serde_json::from_str(r#"{"at":5,"kind":{"kind":"Thinking","text":"hm"}}"#).unwrap();
        assert_eq!(e.kind, ConvKind::Other);
    }

    #[test]
    fn unknown_answer_kinds_read_as_other() {
        let a: Answer = serde_json::from_str(r#"{"kind":"Future","x":1}"#).unwrap();
        assert_eq!(a, Answer::Other);
        let q: QuestionAnswer = serde_json::from_str(r#"{"kind":"Future","x":1}"#).unwrap();
        assert_eq!(q, QuestionAnswer::Unrecognised);
    }

    #[test]
    fn unknown_prompt_shown_reads_as_unknown() {
        let p: PromptShown = serde_json::from_str(r#"{"shown":"Future"}"#).unwrap();
        assert_eq!(p, PromptShown::Unknown);
        let p: PromptShown = serde_json::from_str(r#"{"shown":"Future","x":1}"#).unwrap();
        assert_eq!(p, PromptShown::Unknown);
    }

    #[test]
    fn reply_without_prompt_reads_as_none() {
        let r: Request =
            serde_json::from_str(r#"{"type":"Reply","slot":1,"session":"s","text":"hi"}"#).unwrap();
        assert_eq!(
            r,
            Request::Reply {
                slot: 1,
                session: "s".into(),
                prompt: None,
                text: "hi".into()
            }
        );
    }

    #[test]
    fn unknown_prompt_info_reads_as_other() {
        let p: PromptInfo = serde_json::from_str(r#"{"kind":"Elicitation","x":1}"#).unwrap();
        assert_eq!(p, PromptInfo::Other);
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
            notes_off: None,
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
                    stage: None,
                    stage_since: None,
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
                    stage: None,
                    stage_since: None,
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
            sprint: None,
            prs: Vec::new(),
            sprint_day: None,
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
            Request::GetItemDetail {
                key: "DEMO-1".into(),
                refresh: true,
            },
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
    fn get_item_detail_reads_without_refresh() {
        let req: Request =
            serde_json::from_str(r#"{"type":"GetItemDetail","key":"DEMO-1"}"#).unwrap();
        assert_eq!(
            req,
            Request::GetItemDetail {
                key: "DEMO-1".into(),
                refresh: false,
            }
        );
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
        assert_eq!(relay.kind, RelayKind::Note);
        assert_eq!(relay.drop_reason, None);
    }

    #[test]
    fn drop_text_names_the_reason() {
        let mut r: Relay = serde_json::from_str(
            r#"{"id":1,"at":2,"from":{"type":"You"},"to":3,"text":"hi","state":"Pending","delivered_at":null,"held":null}"#,
        )
        .unwrap();
        assert_eq!(r.drop_text(), None);
        r.state = RelayState::Dropped;
        assert_eq!(r.drop_text().as_deref(), Some("dropped"));
        r.drop_reason = Some("replaced by plan 20's kickoff".into());
        assert_eq!(
            r.drop_text().as_deref(),
            Some("dropped: replaced by plan 20's kickoff")
        );
    }

    #[test]
    fn drop_relays_reads_with_only_one_of_its_fields() {
        let req: Request = serde_json::from_str(r#"{"type":"DropRelays","id":7}"#).unwrap();
        assert_eq!(
            req,
            Request::DropRelays {
                from: None,
                id: Some(7),
                to: None,
            }
        );
        let req: Request = serde_json::from_str(r#"{"type":"DropRelays","to":2}"#).unwrap();
        assert_eq!(
            req,
            Request::DropRelays {
                from: None,
                id: None,
                to: Some(2),
            }
        );
    }

    #[test]
    fn relay_kinds_and_drop_reasons_round_trip() {
        for kind in [
            RelayKind::Note,
            RelayKind::Kickoff { plan: 20 },
            RelayKind::Lock {
                target: "deploy:demo/api".into(),
            },
        ] {
            let relay = Relay {
                id: 1,
                at: 2,
                from: Sender::Overseer,
                to: 3,
                text: "hi".into(),
                state: RelayState::Dropped,
                delivered_at: None,
                dropped_at: Some(4),
                kind,
                drop_reason: Some("replaced by plan 20's kickoff".into()),
                held: None,
                held_since: None,
            };
            let json = serde_json::to_string(&relay).unwrap();
            assert_eq!(serde_json::from_str::<Relay>(&json).unwrap(), relay);
        }
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
            Response::ItemDetail(ItemDetail {
                key: "DEMO-1".into(),
                kind: ItemKind::Ticket,
                title: "T".into(),
                status: "To Do".into(),
                priority: None,
                stage: None,
                stage_since: None,
                folder: None,
                body: None,
                notes: None,
                depends_on: vec![],
                children: vec![],
                prs: vec![],
                overview: None,
                fetched_at: None,
                error: None,
            }),
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
            kind: RelayKind::Note,
            drop_reason: None,
            held: None,
            held_since: None,
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
    fn slot_screen_round_trips() {
        let req = Request::SlotScreen { id: 3 };
        let back: Request = serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(back, req);
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
    fn set_token_debug_hides_the_token() {
        for (kind, name) in [
            (TokenKind::Jira, "Jira"),
            (TokenKind::Bitbucket, "Bitbucket"),
        ] {
            let req = Request::SetToken {
                kind,
                token: "demo-secret-token".into(),
            };
            let shown = format!("{req:?}");
            assert_eq!(
                shown,
                format!(r#"SetToken {{ kind: {name}, token: "<hidden>" }}"#)
            );
            assert!(!shown.contains("demo-secret"));
            // On the wire it is a plain string, and it round-trips.
            let json = serde_json::to_string(&req).unwrap();
            assert!(json.contains("demo-secret-token"));
            let back: Request = serde_json::from_str(&json).unwrap();
            assert_eq!(back, req);
            // A frame holding it hides it too.
            let frame = format!("{:?}", crate::Frame::Request { seq: 1, req });
            assert!(!frame.contains("demo-secret"), "{frame}");
        }
        let json = serde_json::to_string(&Request::SetToken {
            kind: TokenKind::Bitbucket,
            token: "x".into(),
        })
        .unwrap();
        assert!(json.contains(r#""kind":"bitbucket""#), "{json}");
    }

    #[test]
    fn settings_messages_round_trip() {
        use crate::settings::{Applies, Setting, SettingKind, SettingValue};
        for req in [
            Request::GetSettings,
            Request::SaveSettings {
                base_hash: "0123456789abcdef".into(),
                edits: vec![
                    SettingEdit::Set {
                        key: "terminal.font_size".into(),
                        value: SettingValue::Float(14.0),
                    },
                    SettingEdit::Reset { key: "jql".into() },
                ],
            },
        ] {
            let back: Request =
                serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
            assert_eq!(back, req);
        }
        let resp = Response::Settings(SettingsView {
            config_path: "/tmp/demo/config.toml".into(),
            file_hash: String::new(),
            load_error: None,
            settings: vec![Setting {
                key: "shared_files".into(),
                group: "Notes".into(),
                kind: SettingKind::List,
                applies: Applies::Restart,
                value: SettingValue::List(vec!["a.md".into()]),
                default: SettingValue::List(vec![]),
                in_file: true,
                restart_needed: true,
                note: None,
            }],
            targets_path: "/tmp/demo/targets.toml".into(),
            targets_note: "3 built-in patterns; /tmp/demo/targets.toml does not exist".into(),
            targets_error: None,
            jira_token: Default::default(),
            bitbucket_token: Default::default(),
            daemon: DaemonInfo {
                version: "0.1.0".into(),
                build: "dev".into(),
                pid: 1,
                started_at: 0,
                exe: "/tmp/demo/overseer".into(),
            },
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
            committed_at: Some(1700000050),
        }
    }

    fn brain_view() -> BrainView {
        BrainView {
            remote: None,
            last_push: None,
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
                    committed_at: None,
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
            notes_dir: Some(PathBuf::from("/tmp/vault")),
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

    #[test]
    fn age_text_buckets() {
        assert_eq!(age_text(-5), "0m");
        assert_eq!(age_text(59), "0m");
        assert_eq!(age_text(61), "1m");
        assert_eq!(age_text(3600), "1h");
        assert_eq!(age_text(90000), "1d");
    }
}
