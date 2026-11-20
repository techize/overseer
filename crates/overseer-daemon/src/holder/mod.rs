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
/// The start times tell a pid that has been reused, after a crash or a
/// reboot, from the process that wrote the file; a pid file from before
/// they were recorded has none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PidFile {
    pub holder: u32,
    pub child: u32,
    pub version: u32,
    /// When the holder started, as [`process_start`] reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder_start: Option<u64>,
    /// When the child started, as [`process_start`] reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_start: Option<u64>,
}

/// When process `pid` started, for telling it from a later process with
/// the same pid: microseconds since the epoch on macOS, clock ticks since
/// boot elsewhere. Only ever compared for equality. `None` when there is no
/// such process or it cannot be read.
#[cfg(target_os = "macos")]
pub fn process_start(pid: u32) -> Option<u64> {
    let pid = i32::try_from(pid).ok().filter(|p| *p > 0)?;
    // SAFETY: a zeroed plain C struct, filled by the kernel up to the size
    // passed.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    if n != size {
        return None;
    }
    Some(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
}

#[cfg(not(target_os = "macos"))]
pub fn process_start(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The name, field 2, is in parentheses and may hold spaces.
    let rest = &stat[stat.rfind(')')? + 2..];
    // `rest` starts at field 3; the start time is field 22.
    rest.split(' ').nth(19)?.parse().ok()
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

/// The start names of [`HOLDER_PROGRAMS`]. macOS keeps up to 32 bytes, so
/// both are whole there; Linux keeps 15, which cuts the test holder's.
#[cfg(target_os = "macos")]
const HOLDER_NAMES: &[&str] = HOLDER_PROGRAMS;
#[cfg(not(target_os = "macos"))]
const HOLDER_NAMES: &[&str] = &["overseer", "overseer-test-h"];

/// How long [`end_by_pid`] gives a holder to go on its own once its child
/// has been asked to end.
pub const END_GRACE: Duration = Duration::from_secs(2);

/// What the pid a pid file names as its holder is now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HolderState {
    /// No process has the pid.
    Dead,
    /// An Overseer holder: its executable path, or failing that the name
    /// it was started as, is a holder program's.
    Holder,
    /// Another program: its start name can be read and is not a holder's.
    /// The pid file is stale.
    Other,
    /// A live process whose identity cannot be read either way. It may be
    /// the holder, so it is never taken for dead and never signalled.
    Unknown,
}

/// What `pid_file`'s holder pid is now; see [`HolderState`]. Liveness is
/// `kill(pid, 0)` alone; identity is checked only for a live pid. A pid
/// whose start time is not the recorded one has been reused: another
/// process ([`HolderState::Other`]), whatever its name. Otherwise the
/// executable path decides, and since it is gone once an install removes
/// the binary a holder was started from, the start name (`proc_name`,
/// which survives that) is the fallback.
pub fn holder_state(pid_file: &PidFile) -> HolderState {
    let pid = pid_file.holder;
    if !client::alive(pid) {
        return HolderState::Dead;
    }
    let state = match pid_file.holder_start.map(|at| (at, process_start(pid))) {
        Some((at, Some(now))) if now != at => HolderState::Other,
        Some((_, None)) => HolderState::Unknown,
        _ => identify(executable(pid).as_deref(), start_name(pid).as_deref()),
    };
    // Gone between the checks: a reading of a reused pid is not trusted.
    if !client::alive(pid) {
        return HolderState::Dead;
    }
    state
}

/// Classifies a live process by its executable path and start name, either
/// of which may be unreadable.
pub fn identify(path: Option<&Path>, name: Option<&str>) -> HolderState {
    let path_is_holder = path
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .is_some_and(|n| HOLDER_PROGRAMS.contains(&n));
    if path_is_holder || name.is_some_and(|n| HOLDER_NAMES.contains(&n)) {
        return HolderState::Holder;
    }
    match name {
        Some(_) => HolderState::Other,
        None => HolderState::Unknown,
    }
}

/// Whether the holder a pid file names is alive and shown to be an
/// Overseer holder ([`HolderState::Holder`]): the only case in which it or
/// its child may be signalled.
pub fn holder_alive(pid_file: &PidFile) -> bool {
    holder_state(pid_file) == HolderState::Holder
}

/// Whether the holder a pid file names has gone: its pid is dead, or now
/// belongs to another program. A live process that cannot be identified is
/// not gone.
pub fn holder_gone(pid_file: &PidFile) -> bool {
    matches!(
        holder_state(pid_file),
        HolderState::Dead | HolderState::Other
    )
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

/// The name a process was started as: the file name it was exec'd by, up
/// to 32 bytes on macOS, which outlives the file itself.
#[cfg(target_os = "macos")]
fn start_name(pid: u32) -> Option<String> {
    let pid = i32::try_from(pid).ok().filter(|p| *p > 0)?;
    let mut buf = vec![0u8; 256];
    // SAFETY: the buffer is as long as the size passed.
    let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    String::from_utf8(buf).ok()
}

#[cfg(not(target_os = "macos"))]
fn start_name(pid: u32) -> Option<String> {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    Some(comm.trim_end_matches('\n').to_string())
}

/// Whether the pid file's child pid is still the child it names: always,
/// for a pid file with no child start time; otherwise when the pid's start
/// time is the recorded one.
pub fn child_is_named(pid_file: &PidFile) -> bool {
    pid_file
        .child_start
        .is_none_or(|at| process_start(pid_file.child) == Some(at))
}

/// Ends a holder's session without speaking its protocol, by the pids in
/// its pid file: SIGTERM to the child's process group, then, if the holder
/// has not gone on its own within [`END_GRACE`], SIGTERM to the holder.
/// Does nothing unless [`holder_alive`]: the child's pid is only trusted
/// while the holder that wrote it lives, and only while it is still that
/// child ([`child_is_named`]). Blocks up to [`END_GRACE`].
pub fn end_by_pid(pid_file: &PidFile) {
    if !holder_alive(pid_file) {
        return;
    }
    // The child leads its own process group. A pid of 0 or 1 would signal
    // this process's group or every process, so it is never used.
    if let Ok(child) = i32::try_from(pid_file.child) {
        if child > 1 && pid_file.child != pid_file.holder && child_is_named(pid_file) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_takes_the_path_then_the_start_name() {
        let app = Path::new("/Applications/Overseer.app/Contents/MacOS/overseer");
        assert_eq!(identify(Some(app), None), HolderState::Holder);
        // The binary was removed: no path, but the start name survives.
        assert_eq!(identify(None, Some("overseer")), HolderState::Holder);
        // Another file at a path that no longer matches.
        let other = Path::new("/bin/sleep");
        assert_eq!(identify(Some(other), Some("overseer")), HolderState::Holder);
        assert_eq!(identify(Some(other), Some("sleep")), HolderState::Other);
        assert_eq!(identify(None, Some("sleep")), HolderState::Other);
        // Nothing readable proves anything: it is never taken for dead.
        assert_eq!(identify(None, None), HolderState::Unknown);
        assert_eq!(identify(Some(other), None), HolderState::Unknown);
        // Only exact names count.
        assert_eq!(identify(None, Some("overseer-x")), HolderState::Other);
        assert_eq!(identify(None, Some("overseer-test-h")), HolderState::Other);
    }
}
