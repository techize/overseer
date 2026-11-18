//! Wire protocol and shared path helpers for the Overseer daemon and its clients.

pub mod build;
pub mod client;
pub mod frame;
pub mod msg;
pub mod paths;
pub mod settings;

pub use build::{
    build_from_plist, build_notice, build_of_exe, compare_builds, current_build, local_hhmm,
    BuildOrder, DEV_BUILD,
};
pub use client::Client;
pub use frame::{
    read_frame, undecodable_request, write_frame, Frame, UndecodableRequest, MAX_FRAME,
};
pub use msg::{
    owned_by_message, owner_in_refusal, running_sessions_count, running_sessions_text, slot_name,
    Activity, ActivityKind, Approvals, BashCheck, BoardView, BrainRun, BrainView, ChildBead, Claim,
    ClaimKind, CoordView, DaemonInfo, Event, HookEvent, HookKind, ItemDetail, ItemKind, LockView,
    NotesRemote, Notice, Plan, PlanDecision, PlanRecord, PlanSlot, PlanState, PlanTarget,
    PlanWorktree, PullRequest, QueueEntry, QueueState, Relay, RelayState, Report, ReportKind,
    Request, Response, Sender, SlotId, SlotInfo, SlotState, SourceStatus, SprintDay, Stage,
    Summary, TokenKind, Wait, WorkItem, WorkView, WriteCheck, CHECK_ENV, JIRA_OFF, PRS_OFF,
    UNKNOWN_STATUS,
};
