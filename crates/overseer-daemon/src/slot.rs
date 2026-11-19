//! A single PTY-backed slot: child process, output ring, live screen and
//! output broadcast.

use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use overseer_proto::{slot_name, SlotId, SlotInfo, SlotState};
use overseer_term::{Snapshot, TermBridge, TermModes};
use tokio::sync::broadcast;

use crate::holder::client::{self, Greeting, HeldPty};
use crate::holder::HolderHello;
use crate::pty::{session_env, LocalPty, OnOutput, SpawnSpec};
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

/// After [`Slot::nudge_redraw`], the redraw counts as over once output has
/// come and then stopped for this long...
pub const REDRAW_QUIET: Duration = Duration::from_secs(1);
/// ...or once this long has passed since the nudge, output or not.
pub const REDRAW_GIVE_UP: Duration = Duration::from_secs(10);

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// What [`Slot::attach_held`] found at a holder's socket.
pub enum Attached {
    Slot(Slot),
    /// The holder speaks a protocol this daemon does not.
    Unsupported(HolderHello),
}

/// Where the slot's terminal lives.
enum Backend {
    /// A child of the daemon itself.
    Local(LocalPty),
    /// A child a holder process keeps.
    Held(HeldPty),
}

impl Backend {
    fn write(&self, data: &[u8]) -> io::Result<()> {
        match self {
            Backend::Local(p) => p.write(data),
            Backend::Held(p) => p.write(data),
        }
    }

    fn resize(&self, cols: u16, rows: u16) -> anyhow::Result<()> {
        match self {
            Backend::Local(p) => p.resize(cols, rows),
            Backend::Held(p) => p.resize(cols, rows),
        }
    }

    fn kill(&self) {
        match self {
            Backend::Local(p) => p.kill(),
            Backend::Held(p) => p.kill(),
        }
    }

    /// Leaves a held child to its holder; a local one has nowhere to go.
    fn let_go(&self) {
        if let Backend::Held(p) = self {
            p.close();
        }
    }

    fn exit(&self) -> tokio::sync::watch::Receiver<Option<Option<i32>>> {
        match self {
            Backend::Local(p) => p.exit(),
            Backend::Held(p) => p.exit(),
        }
    }

    fn exited_at(&self) -> Option<i64> {
        match self {
            Backend::Local(p) => p.exited_at(),
            Backend::Held(p) => p.exited_at(),
        }
    }
}

/// The output callback every backend gets: each chunk goes to the ring, the
/// broadcast and the screen, and its arrival is noted in `last_output`.
/// Dropping it (when the stream ends) drops `out`, which closes the
/// broadcast.
fn feeder(
    ring: Arc<Mutex<RingBuffer>>,
    screen: Arc<Mutex<TermBridge>>,
    out: broadcast::Sender<Bytes>,
    last_output: Arc<Mutex<Option<Instant>>>,
) -> OnOutput {
    Box::new(move |chunk| {
        *last_output.lock().unwrap() = Some(Instant::now());
        // Push and broadcast under one lock so `attach` sees each chunk
        // either in its snapshot or on its receiver, never both and never
        // neither.
        {
            let mut ring = ring.lock().unwrap();
            ring.push(chunk);
            let _ = out.send(Bytes::copy_from_slice(chunk));
        }
        // Replies to terminal queries are the attached clients' to send;
        // this screen only watches.
        let mut screen = screen.lock().unwrap();
        screen.feed(chunk);
        screen.take_pty_writes();
    })
}

/// The spec for starting `command` in slot `id` with `env` (already
/// scrubbed), under a fresh `OVERSEER_RUN`; see [`session_env`].
pub fn spawn_spec(
    id: SlotId,
    label: String,
    cwd: &Path,
    command: &[String],
    cols: u16,
    rows: u16,
    env: &[(OsString, OsString)],
) -> SpawnSpec {
    let run = next_run();
    SpawnSpec {
        slot: id,
        label,
        cwd: cwd.to_path_buf(),
        argv: command.to_vec(),
        env: session_env(env, id, &run),
        cols,
        rows,
        run,
    }
}

pub struct Slot {
    id: SlotId,
    label: String,
    cwd: std::path::PathBuf,
    command: Vec<String>,
    pid: u32,
    since: i64,
    size: Mutex<(u16, u16)>,
    pty: Backend,
    ring: Arc<Mutex<RingBuffer>>,
    /// The screen as a terminal following the whole output would show it,
    /// fed every chunk. Claude Code redraws only the cells that change, so a
    /// replay of the ring alone loses rows drawn before its oldest byte.
    screen: Arc<Mutex<TermBridge>>,
    /// Weak so the channel still closes when the reader thread ends.
    out: broadcast::WeakSender<Bytes>,
    /// Set as `OVERSEER_RUN` in the child's environment; unique per spawn.
    run: String,
    /// When the last output chunk arrived.
    last_output: Arc<Mutex<Option<Instant>>>,
    /// When [`Slot::nudge_redraw`] asked for a redraw that is not over yet.
    nudged: Mutex<Option<Instant>>,
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
        let spec = spawn_spec(id, label, cwd, command, cols, rows, env);
        let ring = Arc::new(Mutex::new(RingBuffer::new(RING_CAP)));
        let screen = Arc::new(Mutex::new(TermBridge::new(cols as usize, rows as usize, 0)));
        let out_weak = out.downgrade();
        let last_output = Arc::new(Mutex::new(None));
        // The pty drops the feeder, and with it `out`, when the output
        // stream ends, which closes the broadcast.
        let pty = LocalPty::spawn(
            &spec,
            feeder(ring.clone(), screen.clone(), out, last_output.clone()),
        )?;

        Ok(Slot {
            id,
            label: spec.label,
            cwd: spec.cwd,
            command: spec.argv,
            pid: pty.pid(),
            since: now_secs(),
            size: Mutex::new((cols, rows)),
            pty: Backend::Local(pty),
            ring,
            screen,
            out: out_weak,
            run: spec.run,
            last_output,
            nudged: Mutex::new(None),
        })
    }

    /// Starts `spec` under a holder, `<program> hold <state_dir>`, and
    /// attaches to it as [`Slot::attach_held`] does. Fails if the holder is
    /// not listening within [`client::HOLDER_START`], exits first, or the
    /// slot already has a live holder.
    pub fn spawn_held(
        spec: SpawnSpec,
        program: &Path,
        state_dir: &Path,
        out: broadcast::Sender<Bytes>,
    ) -> anyhow::Result<Slot> {
        let greeting = client::start(&spec, program, state_dir)?;
        let holder = greeting.hello.pid;
        Slot::from_greeting(greeting, out).inspect_err(|_| {
            client::abandon(holder, state_dir, spec.slot);
        })
    }

    /// Attaches to the holder listening at `socket`. Its replay and then its
    /// live output feed the ring, screen and `out` exactly as a local child's
    /// output does; the whole replay is in the ring and screen before this
    /// returns. When the child exits the slot reads as exited; when the
    /// holder goes without saying so, as killed (`None`).
    pub fn attach_held(socket: &Path, out: broadcast::Sender<Bytes>) -> anyhow::Result<Attached> {
        let greeting = Greeting::connect(socket)?;
        if !greeting.supported() {
            return Ok(Attached::Unsupported(greeting.hello));
        }
        Slot::from_greeting(greeting, out).map(Attached::Slot)
    }

    fn from_greeting(greeting: Greeting, out: broadcast::Sender<Bytes>) -> anyhow::Result<Slot> {
        let hello = greeting.hello.clone();
        let ring = Arc::new(Mutex::new(RingBuffer::new(RING_CAP)));
        let screen = Arc::new(Mutex::new(TermBridge::new(
            hello.cols as usize,
            hello.rows as usize,
            0,
        )));
        let out_weak = out.downgrade();
        let last_output = Arc::new(Mutex::new(None));
        let pty = greeting.serve(feeder(
            ring.clone(),
            screen.clone(),
            out,
            last_output.clone(),
        ))?;
        Ok(Slot {
            id: hello.slot,
            label: hello.label,
            cwd: hello.cwd,
            command: hello.argv,
            pid: hello.child_pid,
            since: hello.since,
            size: Mutex::new((hello.cols, hello.rows)),
            pty: Backend::Held(pty),
            ring,
            screen,
            out: out_weak,
            run: hello.run,
            last_output,
            nudged: Mutex::new(None),
        })
    }

    /// Whether a holder process keeps this slot's terminal.
    pub fn is_held(&self) -> bool {
        matches!(self.pty, Backend::Held(_))
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

    /// Closes a held slot's connection and leaves its child running under
    /// its holder, for the next daemon to attach. The slot then reads as
    /// exited with no code. Does nothing to a local slot.
    pub fn let_go(&self) {
        self.pty.let_go();
    }

    /// Makes the program redraw its whole screen: resizes the terminal one
    /// column narrower and back, which sends it SIGWINCH. Until that redraw
    /// is over, [`Slot::redraw_pending`] is true. For a slot attached after
    /// a restart, whose screen was rebuilt from a replay that can miss rows.
    pub fn nudge_redraw(&self) {
        let (cols, rows) = *self.size.lock().unwrap();
        let other = if cols > 1 { cols - 1 } else { cols + 1 };
        // Before the resize, so the redraw it brings counts as after it.
        *self.nudged.lock().unwrap() = Some(Instant::now());
        if let Err(e) = self
            .resize(other, rows)
            .and_then(|()| self.resize(cols, rows))
        {
            tracing::warn!("{} redraw not asked for: {e:#}", slot_name(self.id));
        }
    }

    /// Whether the redraw [`Slot::nudge_redraw`] asked for is still to come:
    /// true until output has arrived after the nudge and then
    /// [`REDRAW_QUIET`] has passed with no more, or [`REDRAW_GIVE_UP`] has
    /// passed since the nudge. Never true for a slot never nudged.
    pub fn redraw_pending(&self) -> bool {
        let mut nudged = self.nudged.lock().unwrap();
        let Some(at) = *nudged else {
            return false;
        };
        let quiet = self
            .last_output
            .lock()
            .unwrap()
            .is_some_and(|last| last > at && last.elapsed() >= REDRAW_QUIET);
        if quiet || at.elapsed() >= REDRAW_GIVE_UP {
            *nudged = None;
            return false;
        }
        true
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
            held: self.is_held(),
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
