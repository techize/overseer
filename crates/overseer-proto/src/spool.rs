//! Hook events that found no daemon listening.
//!
//! `overseer hook` appends the event here when it cannot connect; the daemon
//! takes the file at startup, after it has reattached held sessions, and
//! applies the events in order. The spool lives in the daemon's state dir.

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::HookEvent;

/// File name in the state dir.
pub const SPOOL_FILE: &str = "hook-spool.jsonl";
/// The file `take` moves the spool to while it reads it.
const TAKEN_FILE: &str = "hook-spool.jsonl.taken";
/// A daemon that stays down for long must not grow the spool without end:
/// `take` keeps this many of the newest events.
pub const SPOOL_MAX_LINES: usize = 10_000;

/// Adds `ev` to the spool as one JSON line. The line and its newline go out
/// in a single `write` on an `O_APPEND` descriptor, so hooks that run at the
/// same moment do not interleave. The file is private to the user: events
/// carry working directories and tool details.
pub fn append(state_dir: &Path, ev: &HookEvent) -> io::Result<()> {
    let mut line = serde_json::to_vec(ev).map_err(io::Error::other)?;
    line.push(b'\n');
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(state_dir.join(SPOOL_FILE))?;
    file.write_all(&line)
}

/// Removes the spool and returns its events, oldest first, at most
/// `SPOOL_MAX_LINES` of them (the newest). A line that does not parse is
/// skipped. No spool is an empty list. The file is renamed aside before it
/// is read, so a hook appending meanwhile starts a fresh spool; a rename
/// aside left by a crash is read first.
pub fn take(state_dir: &Path) -> io::Result<Vec<HookEvent>> {
    let taken = state_dir.join(TAKEN_FILE);
    match std::fs::rename(state_dir.join(SPOOL_FILE), &taken) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let text = match std::fs::read(&taken) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut events: VecDeque<HookEvent> = VecDeque::new();
    for line in text.split(|b| *b == b'\n') {
        if let Ok(ev) = serde_json::from_slice::<HookEvent>(line) {
            events.push_back(ev);
            if events.len() > SPOOL_MAX_LINES {
                events.pop_front();
            }
        }
    }
    std::fs::remove_file(&taken)?;
    Ok(events.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HookKind;

    fn ev(slot: u8, n: usize) -> HookEvent {
        HookEvent {
            slot,
            event: HookKind::PostToolUse,
            session_id: None,
            tool: Some("Bash".into()),
            detail: Some(format!("echo {n}")),
            notification: None,
            tool_use_id: None,
            notification_type: None,
            run: Some("run-1".into()),
            agent_id: None,
            transcript_path: None,
            prompt: None,
        }
    }

    #[test]
    fn append_then_take_returns_events_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let events: Vec<_> = (0..4).map(|n| ev(1, n)).collect();
        for e in &events {
            append(dir.path(), e).unwrap();
        }
        assert_eq!(take(dir.path()).unwrap(), events);
    }

    #[test]
    fn take_empties_the_spool() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), &ev(1, 0)).unwrap();
        assert_eq!(take(dir.path()).unwrap().len(), 1);
        assert!(take(dir.path()).unwrap().is_empty());
        assert!(!dir.path().join(SPOOL_FILE).exists());
        assert!(!dir.path().join(TAKEN_FILE).exists());
    }

    #[test]
    fn take_with_no_spool_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(take(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn take_keeps_the_newest_when_over_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        for n in 0..SPOOL_MAX_LINES + 5 {
            append(dir.path(), &ev(1, n)).unwrap();
        }
        let got = take(dir.path()).unwrap();
        assert_eq!(got.len(), SPOOL_MAX_LINES);
        assert_eq!(got.first(), Some(&ev(1, 5)));
        assert_eq!(got.last(), Some(&ev(1, SPOOL_MAX_LINES + 4)));
    }

    #[test]
    fn a_corrupt_line_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), &ev(1, 0)).unwrap();
        let path = dir.path().join(SPOOL_FILE);
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{not json\n\n").unwrap();
        append(dir.path(), &ev(1, 1)).unwrap();
        assert_eq!(take(dir.path()).unwrap(), [ev(1, 0), ev(1, 1)]);
    }

    #[test]
    fn the_spool_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), &ev(1, 0)).unwrap();
        let mode = std::fs::metadata(dir.path().join(SPOOL_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
