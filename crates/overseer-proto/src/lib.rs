//! Wire protocol and shared path helpers for the Overseer daemon and its clients.

pub mod client;
pub mod frame;
pub mod msg;
pub mod paths;

pub use client::Client;
pub use frame::{read_frame, write_frame, Frame, MAX_FRAME};
pub use msg::{
    owned_by_message, owner_in_refusal, slot_name, Activity, ActivityKind, BashCheck, BoardView,
    BrainRun, BrainView, Claim, ClaimKind, CoordView, Event, HookEvent, HookKind, ItemKind,
    LockView, Notice, Plan, PlanDecision, PlanRecord, PlanSlot, PlanState, PlanTarget,
    PlanWorktree, QueueEntry, QueueState, Relay, RelayState, Report, ReportKind, Request, Response,
    Sender, SlotId, SlotInfo, SlotState, SourceStatus, Summary, Wait, WorkItem, WorkView,
    WriteCheck, CHECK_ENV, JIRA_OFF,
};
