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
}

// Adjacent tagging because `Slots` is a tuple variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum Response {
    Ok,
    Slots(Vec<SlotInfo>),
    Error { message: String },
}

// Adjacent tagging because `SlotChanged` is a tuple variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum Event {
    SlotChanged(SlotInfo),
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
}
