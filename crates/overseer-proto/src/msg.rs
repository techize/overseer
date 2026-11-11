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
}

// Adjacent tagging because `SlotChanged` is a tuple variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum Event {
    SlotChanged(SlotInfo),
    WorkChanged(WorkView),
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
}
