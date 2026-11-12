use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Slot index, `0..=6`.
pub type SlotId = u8;

/// Display name for a slot, e.g. `S3`.
pub fn slot_name(id: SlotId) -> String {
    format!("S{id}")
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
    /// Why a pending relay is waiting, e.g. `session is working`. Filled by
    /// the courier.
    pub held: Option<String>,
}

/// Every claim, wait and relay worth showing. `relays` holds every pending
/// relay plus the 50 most recent delivered ones, newest first.
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

/// A Bash command the `PreToolUse` hook asks the daemon to check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BashCheck {
    pub slot: SlotId,
    pub session_id: Option<String>,
    pub tool_use_id: Option<String>,
    pub command: String,
    pub cwd: Option<PathBuf>,
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
    ForceRelease {
        target: String,
    },
    /// Queues a message for a slot's session. `from` is `None` for you.
    SendRelay {
        #[serde(default)]
        from: Option<SlotId>,
        to: SlotId,
        text: String,
    },
    CheckBash(BashCheck),
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
    },
    /// The request was accepted and is waiting, e.g. a queued claim.
    Queued {
        message: String,
    },
}

// Adjacent tagging because `SlotChanged` is a tuple variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum Event {
    SlotChanged(SlotInfo),
    WorkChanged(WorkView),
    LocksChanged(LockView),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(slot_name(3), "S3");
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
            Response::Verdict { deny: None },
            Response::Verdict {
                deny: Some("S2 holds it".into()),
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
