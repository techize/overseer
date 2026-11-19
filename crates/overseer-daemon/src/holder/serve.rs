//! The holder process: owns one child on a PTY, keeps its output in a ring,
//! and serves it to at most one daemon connection at a time.
//!
//! Threads: the main thread waits for the child to exit and then ends the
//! process; an accept thread takes connections; each connection has a
//! reader (daemon to holder) and a writer (holder to daemon) thread; the
//! PTY's own reader thread pushes output to the ring and queues it for the
//! connection.

use std::ffi::OsString;
use std::io::{self, Read};
use std::net::Shutdown;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context};

use super::frame::{write_frame, Frame, HolderHello, HOLDER_PROTOCOL};
use super::{exit_path, holders_dir, pid_path, socket_path, write_json_atomic, ExitFile, PidFile};
use crate::pty::{LocalPty, SpawnSpec};
use crate::ring::RingBuffer;
use crate::slot::RING_CAP;

/// Replay and Output payloads are cut to this size, well under `MAX_FRAME`.
const CHUNK: usize = 64 * 1024;
/// Frames queued for one connection. A full queue means the daemon is not
/// reading, and the connection is dropped; the daemon reconnects and gets a
/// fresh Replay. Large enough for a greeting with a full ring
/// (`RING_CAP / CHUNK` Replay frames plus a few).
const QUEUE: usize = 1024;
/// How long a connected daemon has to answer Exit with ExitSeen.
const EXIT_SEEN_WAIT: Duration = Duration::from_secs(5);
/// How long to wait, after the child exits, for its last output.
const OUTPUT_DRAIN_WAIT: Duration = Duration::from_secs(1);

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The process entry point; see [`super::main`].
pub fn main() -> ! {
    // SAFETY: setsid and the fd calls below take no pointers we own.
    if unsafe { libc::setsid() } < 0 {
        std::process::exit(1);
    }
    // The holder outlives the daemon that started it, and with it the
    // daemon's pipes: write nothing to them.
    redirect_to_dev_null(&[1, 2]);
    match run(std::env::args_os().skip(1).collect()) {
        Ok(never) => match never {},
        Err(_) => std::process::exit(1),
    }
}

fn redirect_to_dev_null(fds: &[libc::c_int]) {
    // SAFETY: a NUL-terminated path; dup2 onto standard fds.
    unsafe {
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if null < 0 {
            return;
        }
        for &fd in fds {
            libc::dup2(null, fd);
        }
        if !fds.contains(&null) {
            libc::close(null);
        }
    }
}

/// `[hold] <state_dir>`.
fn state_dir_arg(mut args: Vec<OsString>) -> anyhow::Result<PathBuf> {
    if args.first().is_some_and(|a| a == "hold") {
        args.remove(0);
    }
    match args.as_slice() {
        [dir] => Ok(PathBuf::from(dir)),
        _ => Err(anyhow!("usage: hold <state_dir>")),
    }
}

fn run(args: Vec<OsString>) -> anyhow::Result<std::convert::Infallible> {
    let state_dir = state_dir_arg(args)?;
    let dir = holders_dir(&state_dir);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .context("create holders dir")?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;

    let mut json = Vec::new();
    io::stdin().read_to_end(&mut json).context("read spec")?;
    redirect_to_dev_null(&[0]);
    let spec: SpawnSpec = serde_json::from_slice(&json).context("parse spec")?;

    let socket = socket_path(&state_dir, spec.slot);
    match std::fs::remove_file(&socket) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            return Err(e).context("remove stale socket")
        }
        _ => {}
    }
    let listener = UnixListener::bind(&socket).context("bind socket")?;

    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            ring: RingBuffer::new(RING_CAP),
            conn: None,
            next_id: 0,
            size: (spec.cols, spec.rows),
            exit: None,
            exit_seen: false,
        }),
        changed: Condvar::new(),
    });

    // The PTY drops this closure, and with it `drained_tx`, when the
    // output stream ends.
    let (drained_tx, drained_rx) = mpsc::channel::<()>();
    let out = shared.clone();
    let pty = LocalPty::spawn(
        &spec,
        Box::new(move |chunk| {
            let _keep = &drained_tx;
            // Push and queue under one lock so a connection sees each chunk
            // either in its Replay or as Output, never both and never neither.
            let mut st = out.lock();
            st.ring.push(chunk);
            for piece in chunk.chunks(CHUNK) {
                st.send(Frame::Output(piece.to_vec()));
            }
        }),
    )
    .inspect_err(|_| {
        let _ = std::fs::remove_file(&socket);
    })?;
    let pty = Arc::new(pty);

    let pid_file = pid_path(&state_dir, spec.slot);
    let pid = PidFile {
        holder: std::process::id(),
        child: pty.pid(),
        version: HOLDER_PROTOCOL,
    };
    if let Err(e) = write_json_atomic(&pid_file, &pid) {
        pty.kill();
        let _ = std::fs::remove_file(&socket);
        return Err(e).context("write pid file");
    }

    let hello = HolderHello {
        version: HOLDER_PROTOCOL,
        slot: spec.slot,
        pid: std::process::id(),
        child_pid: pty.pid(),
        run: spec.run.clone(),
        label: spec.label.clone(),
        cwd: spec.cwd.clone(),
        argv: spec.argv.clone(),
        cols: spec.cols,
        rows: spec.rows,
        since: now_secs(),
    };
    {
        let shared = shared.clone();
        let pty = pty.clone();
        std::thread::Builder::new()
            .name("holder-accept".into())
            .spawn(move || accept_loop(listener, shared, pty, hello))
            .context("spawn accept thread")?;
    }

    let code = wait_for_exit(&pty);
    let _ = drained_rx.recv_timeout(OUTPUT_DRAIN_WAIT);
    let at = pty.exited_at().unwrap_or_else(now_secs);
    let seen = report_exit(&shared, code, at);
    if !seen {
        let _ = write_json_atomic(&exit_path(&state_dir, spec.slot), &ExitFile { code, at });
    }
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(&pid_file);
    std::process::exit(0);
}

struct Shared {
    state: Mutex<State>,
    /// Signalled when ExitSeen arrives or the connection goes.
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

struct State {
    ring: RingBuffer,
    conn: Option<Conn>,
    next_id: u64,
    /// The PTY's size, for the next Hello.
    size: (u16, u16),
    exit: Option<(Option<i32>, i64)>,
    exit_seen: bool,
}

struct Conn {
    id: u64,
    tx: SyncSender<Frame>,
    stream: UnixStream,
}

impl State {
    /// Queues `frame` for the current connection, if any. A connection
    /// that cannot keep up is dropped.
    fn send(&mut self, frame: Frame) {
        if let Some(conn) = &self.conn {
            if conn.tx.try_send(frame).is_err() {
                self.drop_conn();
            }
        }
    }

    fn drop_conn(&mut self) {
        if let Some(conn) = self.conn.take() {
            let _ = conn.stream.shutdown(Shutdown::Both);
        }
    }

    fn is_current(&self, id: u64) -> bool {
        self.conn.as_ref().is_some_and(|c| c.id == id)
    }
}

/// The child's exit code, `None` when a signal ended it.
fn wait_for_exit(pty: &LocalPty) -> Option<i32> {
    let mut rx = pty.exit();
    let rt = match tokio::runtime::Builder::new_current_thread().build() {
        Ok(rt) => rt,
        Err(_) => {
            // Without a runtime, poll.
            loop {
                if let Some(code) = *rx.borrow() {
                    return code;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };
    rt.block_on(async {
        match rx.wait_for(|v| v.is_some()).await {
            Ok(v) => v.flatten(),
            Err(_) => None,
        }
    })
}

/// Sends Exit to the connected daemon and waits for its ExitSeen. Returns
/// whether it came; false when nothing was connected, the connection went,
/// or the wait ran out.
fn report_exit(shared: &Shared, code: Option<i32>, at: i64) -> bool {
    let deadline = Instant::now() + EXIT_SEEN_WAIT;
    let mut st = shared.lock();
    st.exit = Some((code, at));
    st.send(Frame::Exit { code, at });
    loop {
        if st.exit_seen {
            return true;
        }
        if st.conn.is_none() {
            return false;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        st = shared
            .changed
            .wait_timeout(st, left)
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
}

fn accept_loop(
    listener: UnixListener,
    shared: Arc<Shared>,
    pty: Arc<LocalPty>,
    hello: HolderHello,
) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let _ = attach(&shared, &pty, &hello, stream);
            }
            // Out of descriptors and the like: give it a moment.
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

/// Makes `stream` the current connection, replacing any other, and greets
/// it with Hello, the ring as Replay, and Exit if the child already ended.
fn attach(
    shared: &Arc<Shared>,
    pty: &Arc<LocalPty>,
    hello: &HolderHello,
    stream: UnixStream,
) -> io::Result<()> {
    let writer = stream.try_clone()?;
    let reader = stream.try_clone()?;
    let (tx, rx) = mpsc::sync_channel(QUEUE);
    let id = {
        let mut st = shared.lock();
        st.drop_conn();
        let id = st.next_id;
        st.next_id += 1;
        let (cols, rows) = st.size;
        // A fresh queue of QUEUE frames holds this whole greeting.
        let _ = tx.try_send(Frame::Hello(HolderHello {
            cols,
            rows,
            ..hello.clone()
        }));
        let snapshot = st.ring.snapshot();
        if snapshot.is_empty() {
            let _ = tx.try_send(Frame::Replay(Vec::new()));
        }
        for piece in snapshot.chunks(CHUNK) {
            let _ = tx.try_send(Frame::Replay(piece.to_vec()));
        }
        if let Some((code, at)) = st.exit {
            let _ = tx.try_send(Frame::Exit { code, at });
        }
        st.conn = Some(Conn { id, tx, stream });
        id
    };
    // The connection was dropped already if either thread fails to start.
    let spawned = std::thread::Builder::new()
        .name("holder-write".into())
        .spawn(move || write_loop(rx, writer))
        .and_then(|_| {
            let shared = shared.clone();
            let pty = pty.clone();
            std::thread::Builder::new()
                .name("holder-read".into())
                .spawn(move || read_loop(reader, id, &shared, &pty))
        });
    if let Err(e) = spawned {
        let mut st = shared.lock();
        if st.is_current(id) {
            st.drop_conn();
        }
        shared.changed.notify_all();
        return Err(e);
    }
    Ok(())
}

fn write_loop(rx: Receiver<Frame>, mut stream: UnixStream) {
    for frame in rx {
        if write_frame(&mut stream, &frame).is_err() {
            break;
        }
    }
    let _ = stream.shutdown(Shutdown::Both);
}

fn read_loop(mut stream: UnixStream, id: u64, shared: &Shared, pty: &LocalPty) {
    while let Ok(Some(frame)) = super::frame::read_frame(&mut stream) {
        match frame {
            Frame::Input(bytes) => {
                if let Err(e) = pty.write(&bytes) {
                    tracing::debug!("pty write: {e}");
                }
            }
            Frame::Resize { cols, rows } => {
                if pty.resize(cols, rows).is_ok() {
                    shared.lock().size = (cols, rows);
                }
            }
            Frame::Kill => pty.kill(),
            Frame::ExitSeen => {
                let mut st = shared.lock();
                if st.is_current(id) && st.exit.is_some() {
                    st.exit_seen = true;
                    shared.changed.notify_all();
                }
            }
            _ => {}
        }
    }
    let mut st = shared.lock();
    if st.is_current(id) {
        st.drop_conn();
    }
    shared.changed.notify_all();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn a_leading_hold_is_skipped() {
        let dir = state_dir_arg(vec!["hold".into(), "/tmp/demo".into()]).unwrap();
        assert_eq!(dir, Path::new("/tmp/demo"));
        let dir = state_dir_arg(vec!["/tmp/demo".into()]).unwrap();
        assert_eq!(dir, Path::new("/tmp/demo"));
        assert!(state_dir_arg(vec![]).is_err());
        assert!(state_dir_arg(vec!["hold".into()]).is_err());
    }
}
