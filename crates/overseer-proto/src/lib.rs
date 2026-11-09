//! Wire protocol and shared path helpers for the Overseer daemon and its clients.

pub mod frame;
pub mod msg;
pub mod paths;

pub use frame::{read_frame, write_frame, Frame, MAX_FRAME};
pub use msg::{slot_name, Event, Request, Response, SlotId, SlotInfo, SlotState};
