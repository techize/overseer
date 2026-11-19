//! The session holder: a small process that owns one Claude PTY so the
//! session survives a daemon restart. It serves the terminal to the daemon
//! over a Unix socket in `<state>/holders/`, speaking the frames in
//! [`frame`]; [`serve`] is the process itself, and [`client`] the daemon's
//! end.

pub mod client;
pub mod frame;
pub mod serve;

use std::io;
use std::path::{Path, PathBuf};

use overseer_proto::{slot_name, SlotId};
use serde::{Deserialize, Serialize};

pub use frame::{
    read_frame, write_frame, Frame, HolderHello, HOLDER_PROTOCOL, MAX_FRAME, SUPPORTED,
};

/// `<state>/holders`, where every holder keeps its socket and files.
pub fn holders_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("holders")
}

fn slot_file(state_dir: &Path, slot: SlotId, ext: &str) -> PathBuf {
    holders_dir(state_dir).join(format!("{}.{ext}", slot_name(slot)))
}

/// `holders/S<n>.sock`: the holder listens here.
pub fn socket_path(state_dir: &Path, slot: SlotId) -> PathBuf {
    slot_file(state_dir, slot, "sock")
}

/// `holders/S<n>.pid`: a [`PidFile`], present while the holder runs.
pub fn pid_path(state_dir: &Path, slot: SlotId) -> PathBuf {
    slot_file(state_dir, slot, "pid")
}

/// `holders/S<n>.exit`: an [`ExitFile`], written when the child exited
/// and no daemon saw it.
pub fn exit_path(state_dir: &Path, slot: SlotId) -> PathBuf {
    slot_file(state_dir, slot, "exit")
}

/// `holders/S<n>.activity`.
pub fn activity_path(state_dir: &Path, slot: SlotId) -> PathBuf {
    slot_file(state_dir, slot, "activity")
}

/// Who a holder is: its own pid, its child's, and the protocol it speaks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PidFile {
    pub holder: u32,
    pub child: u32,
    pub version: u32,
}

/// How a child ended while no daemon was there to see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitFile {
    /// `None` when a signal ended the child.
    pub code: Option<i32>,
    /// Unix seconds.
    pub at: i64,
}

/// Writes `value` as JSON to `path` through a temporary file and a rename,
/// so a reader never sees half of it.
pub(crate) fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Entry point for `overseer hold <state_dir>` and the test binary.
/// Reads one SpawnSpec as JSON from stdin, then serves until the child has
/// exited and its exit has been seen (ExitSeen) or written to the exit file.
/// A leading `hold` argument is skipped.
pub fn main() -> ! {
    serve::main()
}
