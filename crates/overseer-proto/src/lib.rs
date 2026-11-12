//! Wire protocol and shared path helpers for the Overseer daemon and its clients.

pub mod client;
pub mod frame;
pub mod msg;
pub mod paths;

pub use client::Client;
pub use frame::{read_frame, write_frame, Frame, MAX_FRAME};
pub use msg::{
    owned_by_message, owner_in_refusal, slot_name, Activity, ActivityKind, Event, HookEvent,
    HookKind, ItemKind, Notice, Request, Response, SlotId, SlotInfo, SlotState, SourceStatus,
    WorkItem, WorkView, JIRA_OFF,
};
