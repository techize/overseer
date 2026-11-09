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
    Running { pid: u32, since: i64 },
    Exited { code: Option<i32>, at: i64 },
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
    },
    StopSlot {
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
    fn json_shape() {
        let v = serde_json::to_value(Request::StopSlot { id: 2 }).unwrap();
        assert_eq!(v, serde_json::json!({"type": "StopSlot", "id": 2}));
    }
}
