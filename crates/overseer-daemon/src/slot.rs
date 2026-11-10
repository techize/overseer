//! A single PTY-backed slot: child process, output ring and output broadcast.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context};
use bytes::Bytes;
use overseer_proto::{slot_name, SlotId, SlotInfo, SlotState};
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
    /// Weak so the channel still closes when the reader thread ends.
    out: broadcast::WeakSender<Bytes>,
    exit_rx: watch::Receiver<ExitValue>,
    exited_at: Arc<Mutex<Option<i64>>>,
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

    /// Spawns `command` with exactly `env` (already scrubbed) plus
    /// `OVERSEER_SLOT`, `TERM` and `COLORTERM`.
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
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.env("OVERSEER_SLOT", slot_name(id));
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
                            let mut ring = reader_ring.lock().unwrap();
                            ring.push(&buf[..n]);
                            let _ = out.send(Bytes::copy_from_slice(&buf[..n]));
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
            out: out_weak,
            exit_rx,
            exited_at,
        })
    }

    pub fn write(&self, data: &[u8]) -> io::Result<()> {
        let mut w = self.writer.lock().unwrap();
        w.write_all(data)?;
        w.flush()
    }

    pub fn resize(&self, cols: u16, rows: u16) -> anyhow::Result<()> {
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
        *self.size.lock().unwrap() = (cols, rows);
        Ok(())
    }

    /// Kill the child. Does nothing harmful if it has already exited.
    pub fn kill(&self) {
        if let Err(e) = self.killer.lock().unwrap().kill() {
            tracing::debug!("kill: {e}");
        }
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
