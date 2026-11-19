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
use std::time::{Duration, Instant};

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

/// Reads the pid file at `path`: `Ok(None)` when there is none, an error
/// when it cannot be read or parsed.
pub fn load_pid_file(path: &Path) -> io::Result<Option<PidFile>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// The file names a holder process runs as: the app's own binary, and the
/// one the integration tests build.
const HOLDER_PROGRAMS: &[&str] = &["overseer", "overseer-test-holder"];

/// How long [`end_by_pid`] gives a holder to go on its own once its child
/// has been asked to end.
pub const END_GRACE: Duration = Duration::from_secs(2);

/// Whether the holder a pid file names is alive and still an Overseer
/// holder: the pid exists and its executable is `overseer` (or the test
/// holder). A pid reused by any other program reads as not alive, so a
/// stale pid file never leads to a signal.
pub fn holder_alive(pid_file: &PidFile) -> bool {
    client::alive(pid_file.holder)
        && executable(pid_file.holder).is_some_and(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| HOLDER_PROGRAMS.contains(&n))
        })
}

/// The executable a live process runs.
#[cfg(target_os = "macos")]
fn executable(pid: u32) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let pid = i32::try_from(pid).ok().filter(|p| *p > 0)?;
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the buffer is as long as the size passed.
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    Some(PathBuf::from(std::ffi::OsString::from_vec(buf)))
}

#[cfg(not(target_os = "macos"))]
fn executable(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

/// Ends a holder's session without speaking its protocol, by the pids in
/// its pid file: SIGTERM to the child's process group, then, if the holder
/// has not gone on its own within [`END_GRACE`], SIGTERM to the holder.
/// Does nothing unless [`holder_alive`]: the child's pid is only trusted
/// while the holder that wrote it lives. Blocks up to [`END_GRACE`].
pub fn end_by_pid(pid_file: &PidFile) {
    if !holder_alive(pid_file) {
        return;
    }
    // The child leads its own process group. A pid of 0 or 1 would signal
    // this process's group or every process, so it is never used.
    if let Ok(child) = i32::try_from(pid_file.child) {
        if child > 1 && pid_file.child != pid_file.holder {
            // SAFETY: a plain signal.
            unsafe { libc::kill(-child, libc::SIGTERM) };
        }
    }
    let deadline = Instant::now() + END_GRACE;
    while Instant::now() < deadline {
        if !holder_alive(pid_file) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if holder_alive(pid_file) {
        // SAFETY: a plain signal to a pid just checked to be a holder.
        unsafe { libc::kill(pid_file.holder as i32, libc::SIGTERM) };
    }
}

/// Entry point for `overseer hold <state_dir>` and the test binary.
/// Reads one SpawnSpec as JSON from stdin, then serves until the child has
/// exited and its exit has been seen (ExitSeen) or written to the exit file.
/// A leading `hold` argument is skipped.
pub fn main() -> ! {
    serve::main(HOLDER_PROTOCOL)
}

/// [`main`], announcing protocol `version` in the pid file and Hello. Only
/// the test holder uses it, to stand in for another build.
#[doc(hidden)]
pub fn main_announcing(version: u32) -> ! {
    serve::main(version)
}
