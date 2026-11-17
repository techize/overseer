//! A single PTY-backed slot: child process, output ring, live screen and
//! output broadcast.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context};
use bytes::Bytes;
use overseer_proto::{slot_name, SlotId, SlotInfo, SlotState};
use overseer_term::{Snapshot, TermBridge, TermModes};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use tokio::sync::{broadcast, watch};

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

/// `Some(Some(code))` or `Some(None)` (killed by signal) once exited.
type ExitValue = Option<Option<i32>>;

pub struct Slot {
    id: SlotId,
    label: String,
    cwd: std::path::PathBuf,
    command: Vec<String>,
    pid: u32,
    since: i64,
    size: Mutex<(u16, u16)>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    ring: Arc<Mutex<RingBuffer>>,
    /// The screen as a terminal following the whole output would show it,
    /// fed every chunk. Claude Code redraws only the cells that change, so a
    /// replay of the ring alone loses rows drawn before its oldest byte.
    screen: Arc<Mutex<TermBridge>>,
    /// Weak so the channel still closes when the reader thread ends.
    out: broadcast::WeakSender<Bytes>,
    exit_rx: watch::Receiver<ExitValue>,
    exited_at: Arc<Mutex<Option<i64>>>,
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
        let (program, args) = command
            .split_first()
            .ok_or_else(|| anyhow!("empty command"))?;
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("open pty")?;

        let mut cmd = CommandBuilder::new(program);
        cmd.args(args);
        cmd.cwd(cwd);
        cmd.env_clear();
        for (k, v) in crate::env::for_session(env) {
            cmd.env(k, v);
        }
        cmd.env("OVERSEER_SLOT", slot_name(id));
        let run = next_run();
        cmd.env("OVERSEER_RUN", &run);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("spawn {program}"))?;
        drop(pair.slave);

        let pid = child.process_id().unwrap_or(0);
        let killer = child.clone_killer();
        let mut reader = pair.master.try_clone_reader().context("clone pty reader")?;
        let writer = pair.master.take_writer().context("take pty writer")?;

        let ring = Arc::new(Mutex::new(RingBuffer::new(RING_CAP)));
        let out_weak = out.downgrade();
        let reader_ring = ring.clone();
        let screen = Arc::new(Mutex::new(TermBridge::new(cols as usize, rows as usize, 0)));
        let reader_screen = screen.clone();
        std::thread::Builder::new()
            .name(format!("pty-read-{}", slot_name(id)))
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            // Push and broadcast under one lock so `attach` sees
                            // each chunk either in its snapshot or on its
                            // receiver, never both and never neither.
                            {
                                let mut ring = reader_ring.lock().unwrap();
                                ring.push(&buf[..n]);
                                let _ = out.send(Bytes::copy_from_slice(&buf[..n]));
                            }
                            // Replies to terminal queries are the attached
                            // clients' to send; this screen only watches.
                            let mut screen = reader_screen.lock().unwrap();
                            screen.feed(&buf[..n]);
                            screen.take_pty_writes();
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
            })
            .context("spawn reader thread")?;

        let (exit_tx, exit_rx) = watch::channel::<ExitValue>(None);
        let exited_at = Arc::new(Mutex::new(None));
        let at = exited_at.clone();
        std::thread::Builder::new()
            .name(format!("pty-wait-{}", slot_name(id)))
            .spawn(move || {
                let code = match child.wait() {
                    Ok(status) => {
                        let c = status.exit_code() as i32;
                        // portable-pty reports signal deaths as a signal name with code 1.
                        if status.signal().is_some() {
                            None
                        } else {
                            Some(c)
                        }
                    }
                    Err(e) => {
                        tracing::warn!("wait on child failed: {e}");
                        None
                    }
                };
                *at.lock().unwrap() = Some(now_secs());
                let _ = exit_tx.send(Some(code));
            })
            .context("spawn wait thread")?;

        Ok(Slot {
            id,
            label,
            cwd: cwd.to_path_buf(),
            command: command.to_vec(),
            pid,
            since: now_secs(),
            size: Mutex::new((cols, rows)),
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            killer: Mutex::new(killer),
            ring,
            screen,
            out: out_weak,
            exit_rx,
            exited_at,
            run,
        })
    }

    /// This spawn's `OVERSEER_RUN`.
    pub fn run(&self) -> &str {
        &self.run
    }

    pub fn write(&self, data: &[u8]) -> io::Result<()> {
        let mut w = self.writer.lock().unwrap();
        w.write_all(data)?;
        w.flush()
    }

    pub fn resize(&self, cols: u16, rows: u16) -> anyhow::Result<()> {
        // Held across the PTY resize, so the redraw the program sends on
        // SIGWINCH is fed to the screen at the new size.
        let mut screen = self.screen.lock().unwrap();
        self.master
            .lock()
            .unwrap()
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("resize pty")?;
        screen.resize(cols as usize, rows as usize);
        *self.size.lock().unwrap() = (cols, rows);
        Ok(())
    }

    /// Kill the child. Does nothing harmful if it has already exited.
    pub fn kill(&self) {
        if let Err(e) = self.killer.lock().unwrap().kill() {
            tracing::debug!("kill: {e}");
        }
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
        let state = match *self.exit_rx.borrow() {
            None => SlotState::Running {
                pid: self.pid,
                since: self.since,
            },
            Some(code) => SlotState::Exited {
                code,
                at: self.exited_at.lock().unwrap().unwrap_or_else(now_secs),
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
        let mut rx = self.exit_rx.clone();
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
