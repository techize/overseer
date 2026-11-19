//! A single PTY-backed slot: child process, output ring, live screen and
//! output broadcast.

use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use overseer_proto::{SlotId, SlotInfo, SlotState};
use overseer_term::{Snapshot, TermBridge, TermModes};
use tokio::sync::broadcast;

use crate::pty::{session_env, LocalPty, SpawnSpec};
use crate::ring::RingBuffer;

/// Ring capacity used by the daemon.
/// What a lagging receiver gets from [`Slot::catch_up`].
#[derive(Debug, PartialEq)]
pub enum CatchUp {
    /// Exactly the output it missed.
    Missed(Vec<u8>),
    /// The missed output is gone: a terminal reset and a full snapshot.
    Resync(Vec<u8>),
}

pub const RING_CAP: usize = 4 * 1024 * 1024;

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub struct Slot {
    id: SlotId,
    label: String,
    cwd: std::path::PathBuf,
    command: Vec<String>,
    pid: u32,
    since: i64,
    size: Mutex<(u16, u16)>,
    pty: LocalPty,
    ring: Arc<Mutex<RingBuffer>>,
    /// The screen as a terminal following the whole output would show it,
    /// fed every chunk. Claude Code redraws only the cells that change, so a
    /// replay of the ring alone loses rows drawn before its oldest byte.
    screen: Arc<Mutex<TermBridge>>,
    /// Weak so the channel still closes when the reader thread ends.
    out: broadcast::WeakSender<Bytes>,
    /// Set as `OVERSEER_RUN` in the child's environment; unique per spawn.
    run: String,
}

/// A token no other spawn in any daemon run shares: the daemon's pid and a
/// counter.
fn next_run() -> String {
    static RUNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}-{n}", std::process::id())
}

impl Slot {
    /// Spawns with the daemon's own environment (scrubbed, see
    /// [`crate::env`]). The daemon proper uses [`Slot::spawn_in`].
    pub fn spawn(
        id: SlotId,
        label: String,
        cwd: &Path,
        command: &[String],
        cols: u16,
        rows: u16,
        out: broadcast::Sender<Bytes>,
    ) -> anyhow::Result<Slot> {
        let env = crate::env::own_env();
        Slot::spawn_in(id, label, cwd, command, cols, rows, out, &env)
    }

    /// Spawns `command` with exactly `env` (already scrubbed), less
    /// Overseer's API tokens (`env::TOKEN_VARS`), plus
    /// `OVERSEER_SLOT`, `OVERSEER_RUN`, `TERM` and `COLORTERM`.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_in(
        id: SlotId,
        label: String,
        cwd: &Path,
        command: &[String],
        cols: u16,
        rows: u16,
        out: broadcast::Sender<Bytes>,
        env: &[(OsString, OsString)],
    ) -> anyhow::Result<Slot> {
        let ring = Arc::new(Mutex::new(RingBuffer::new(RING_CAP)));
        let out_weak = out.downgrade();
        let reader_ring = ring.clone();
        let screen = Arc::new(Mutex::new(TermBridge::new(cols as usize, rows as usize, 0)));
        let reader_screen = screen.clone();
        let run = next_run();
        let spec = SpawnSpec {
            slot: id,
            label: label.clone(),
            cwd: cwd.to_path_buf(),
            argv: command.to_vec(),
            env: session_env(env, id, &run),
            cols,
            rows,
            run: run.clone(),
        };
        // The pty drops this closure, and with it `out`, when the output
        // stream ends, which closes the broadcast.
        let pty = LocalPty::spawn(
            &spec,
            Box::new(move |chunk| {
                // Push and broadcast under one lock so `attach` sees each
                // chunk either in its snapshot or on its receiver, never both
                // and never neither.
                {
                    let mut ring = reader_ring.lock().unwrap();
                    ring.push(chunk);
                    let _ = out.send(Bytes::copy_from_slice(chunk));
                }
                // Replies to terminal queries are the attached clients' to
                // send; this screen only watches.
                let mut screen = reader_screen.lock().unwrap();
                screen.feed(chunk);
                screen.take_pty_writes();
            }),
        )?;

        Ok(Slot {
            id,
            label,
            cwd: cwd.to_path_buf(),
            command: command.to_vec(),
            pid: pty.pid(),
            since: now_secs(),
            size: Mutex::new((cols, rows)),
            pty,
            ring,
            screen,
            out: out_weak,
            run,
        })
    }

    /// This spawn's `OVERSEER_RUN`.
    pub fn run(&self) -> &str {
        &self.run
    }

    pub fn write(&self, data: &[u8]) -> io::Result<()> {
        self.pty.write(data)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> anyhow::Result<()> {
        // Held across the PTY resize, so the redraw the program sends on
        // SIGWINCH is fed to the screen at the new size.
        let mut screen = self.screen.lock().unwrap();
        self.pty.resize(cols, rows)?;
        screen.resize(cols as usize, rows as usize);
        *self.size.lock().unwrap() = (cols, rows);
        Ok(())
    }

    /// Kill the child. Does nothing harmful if it has already exited.
    pub fn kill(&self) {
        self.pty.kill();
    }

    /// The live screen and its terminal modes. A synchronized update that
    /// has timed out is flushed first; one still open shows the screen as it
    /// was before it began.
    pub fn screen(&self) -> (Snapshot, TermModes) {
        let mut screen = self.screen.lock().unwrap();
        screen.tick();
        (screen.snapshot(), screen.modes())
    }

    /// Current ring contents. For attaching a client use [`Slot::attach`],
    /// which pairs the snapshot with a gap-free live receiver.
    pub fn snapshot(&self) -> Vec<u8> {
        self.ring.lock().unwrap().contents()
    }

    /// Atomically take the ring snapshot and subscribe to live output.
    ///
    /// The reader thread pushes to the ring and broadcasts under the ring
    /// lock, and this takes the same lock, so the snapshot plus the receiver's
    /// bytes form the exact output stream: no gap and no duplicate. (If the
    /// ring has wrapped, the snapshot starts at the oldest retained byte,
    /// preceded by a preamble restoring the terminal modes set by the bytes
    /// it dropped; that preamble is computed under the same lock.)
    ///
    /// Terminal queries are removed from the snapshot (outside the lock), so
    /// a replaying terminal does not answer questions asked long ago. Live
    /// queries reach the receiver untouched.
    /// The receiver is `None` once the output stream has ended.
    pub fn attach(&self) -> (Vec<u8>, Option<broadcast::Receiver<Bytes>>) {
        let (snapshot, _, rx) = self.attach_at();
        (snapshot, rx)
    }

    /// [`Slot::attach`], plus the stream position the receiver starts at.
    pub fn attach_at(&self) -> (Vec<u8>, u64, Option<broadcast::Receiver<Bytes>>) {
        let (snapshot, end, rx) = {
            let ring = self.ring.lock().unwrap();
            let rx = self.out.upgrade().map(|tx| tx.subscribe());
            (ring.snapshot(), ring.end(), rx)
        };
        (crate::vt::strip_queries(&snapshot), end, rx)
    }

    /// For a receiver that lagged and lost chunks: the output from stream
    /// position `pos` on, a fresh receiver that continues exactly after it,
    /// and the position that receiver starts at. Taken under the ring lock,
    /// like `attach`, so there is no gap and no duplicate.
    ///
    /// If the ring has already dropped bytes after `pos`, the gap cannot be
    /// filled; the output is then a terminal reset and a full snapshot, so
    /// the client redraws from scratch instead of showing a corrupt screen.
    pub fn catch_up(&self, pos: u64) -> (CatchUp, u64, Option<broadcast::Receiver<Bytes>>) {
        let (missed, end, rx) = {
            let ring = self.ring.lock().unwrap();
            let rx = self.out.upgrade().map(|tx| tx.subscribe());
            let missed = ring.since(pos).ok_or_else(|| ring.snapshot());
            (missed, ring.end(), rx)
        };
        let out = match missed {
            Ok(bytes) => CatchUp::Missed(bytes),
            Err(snapshot) => {
                let mut bytes = b"\x1bc".to_vec();
                bytes.extend(crate::vt::strip_queries(&snapshot));
                CatchUp::Resync(bytes)
            }
        };
        (out, end, rx)
    }

    pub fn info(&self) -> SlotInfo {
        let state = match *self.pty.exit().borrow() {
            None => SlotState::Running {
                pid: self.pid,
                since: self.since,
            },
            Some(code) => SlotState::Exited {
                code,
                at: self.pty.exited_at().unwrap_or_else(now_secs),
            },
        };
        let (cols, rows) = *self.size.lock().unwrap();
        SlotInfo {
            id: self.id,
            label: self.label.clone(),
            cwd: Some(self.cwd.clone()),
            command: Some(self.command.clone()),
            state,
            cols,
            rows,
            activity: None,
            session_note: None,
        }
    }

    /// Resolve with the exit code (`None` if killed by a signal). Works after
    /// exit too.
    pub fn wait_exit(&self) -> impl std::future::Future<Output = Option<i32>> {
        let mut rx = self.pty.exit();
        async move {
            loop {
                if let Some(code) = *rx.borrow_and_update() {
                    return code;
                }
                if rx.changed().await.is_err() {
                    return None;
                }
            }
        }
    }
}
