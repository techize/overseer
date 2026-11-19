//! The daemon's side of a holder connection: [`HeldPty`] stands in for a
//! [`crate::pty::LocalPty`] whose child a holder process keeps, [`Greeting`]
//! connects to a holder, and [`start`] starts one.

use std::io::{self, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context};
use overseer_proto::{slot_name, SlotId};
use tokio::sync::watch;

use super::frame::{read_frame, write_frame, Frame, HolderHello, HOLDER_PROTOCOL, SUPPORTED};
use super::{exit_path, holder_alive, load_pid_file, pid_path, socket_path, PidFile};
use crate::pty::{OnOutput, SpawnSpec};

/// How long a new holder has to start listening, and a connected one to
/// send its Hello.
pub const HOLDER_START: Duration = Duration::from_secs(5);
/// Input payloads are cut to this size, well under `MAX_FRAME`.
const CHUNK: usize = 64 * 1024;
/// macOS limits a Unix socket path to 104 bytes, the NUL included.
const MAX_SOCKET_PATH: usize = 103;
const POLL: Duration = Duration::from_millis(10);

/// `Some(Some(code))` or `Some(None)` (killed by signal) once exited.
type ExitValue = Option<Option<i32>>;

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A holder's first frame was not a Hello this build can read: the holder
/// is another build's.
#[derive(Debug)]
pub struct UnreadableHello(String);

impl std::fmt::Display for UnreadableHello {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "holder hello not readable: {}", self.0)
    }
}

impl std::error::Error for UnreadableHello {}

/// A connection to a holder that has sent its Hello, before anything else
/// is read from it.
pub struct Greeting {
    stream: UnixStream,
    pub hello: HolderHello,
}

impl Greeting {
    /// Connects to the holder at `socket`, sends DaemonHello and reads its
    /// Hello.
    pub fn connect(socket: &Path) -> anyhow::Result<Greeting> {
        let mut stream = UnixStream::connect(socket)
            .with_context(|| format!("connect to holder at {}", socket.display()))?;
        write_frame(
            &mut stream,
            &Frame::DaemonHello {
                version: HOLDER_PROTOCOL,
            },
        )
        .context("send hello to holder")?;
        stream.set_read_timeout(Some(HOLDER_START))?;
        let hello = match read_frame(&mut stream) {
            Ok(Some(Frame::Hello(h))) => h,
            Ok(Some(other)) => {
                return Err(UnreadableHello(format!("{other:?} before its hello")).into())
            }
            Ok(None) => bail!("holder closed the connection before its hello"),
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                return Err(UnreadableHello(e.to_string()).into())
            }
            Err(e) => return Err(anyhow::Error::new(e).context("read holder hello")),
        };
        stream.set_read_timeout(None)?;
        Ok(Greeting { stream, hello })
    }

    /// Whether this daemon speaks the holder's protocol.
    pub fn supported(&self) -> bool {
        SUPPORTED.contains(&self.hello.version)
    }

    /// Feeds the whole replay to `on_output` before returning. Then a
    /// reader thread passes every Output payload to `on_output`, in order,
    /// answers Exit with ExitSeen, and drops `on_output` when the connection
    /// ends.
    pub fn serve(self, mut on_output: OnOutput) -> anyhow::Result<HeldPty> {
        let Greeting { stream, hello } = self;
        let writer = Arc::new(Mutex::new(stream.try_clone()?));
        let closer = stream.try_clone()?;
        // A burst arrives as many small frames: read them through a buffer.
        let mut reader = BufReader::with_capacity(CHUNK, stream);
        // The replay is the ring cut into Replay frames, closed by an empty
        // one; fed in order they are the same bytes.
        reader.get_ref().set_read_timeout(Some(HOLDER_START))?;
        loop {
            match read_frame(&mut reader).context("read holder replay")? {
                Some(Frame::Replay(bytes)) if bytes.is_empty() => break,
                Some(Frame::Replay(bytes)) => on_output(&bytes),
                Some(other) => bail!("holder sent {other:?} inside its replay"),
                None => bail!("holder closed the connection during its replay"),
            }
        }
        reader.get_ref().set_read_timeout(None)?;
        let (exit_tx, exit_rx) = watch::channel::<ExitValue>(None);
        let exited_at = Arc::new(Mutex::new(None));
        let (reply, at) = (writer.clone(), exited_at.clone());
        std::thread::Builder::new()
            .name(format!("held-read-{}", slot_name(hello.slot)))
            .spawn(move || {
                loop {
                    match read_frame(&mut reader) {
                        Ok(Some(Frame::Output(bytes))) => {
                            if !bytes.is_empty() {
                                on_output(&bytes);
                            }
                        }
                        Ok(Some(Frame::Exit { code, at: when })) => {
                            *at.lock().unwrap() = Some(when);
                            let _ = exit_tx.send(Some(code));
                            let mut w = reply.lock().unwrap();
                            if let Err(e) = write_frame(&mut *w, &Frame::ExitSeen) {
                                tracing::debug!("holder exit seen: {e}");
                            }
                        }
                        Ok(Some(_)) => {}
                        Ok(None) | Err(_) => break,
                    }
                }
                // The holder went without reporting an exit: the child is
                // lost to this daemon.
                if exit_tx.borrow().is_none() {
                    *at.lock().unwrap() = Some(now_secs());
                    let _ = exit_tx.send(Some(None));
                }
                drop(on_output);
            })
            .context("spawn holder reader thread")?;

        Ok(HeldPty {
            writer,
            closer,
            exit_rx,
            exited_at,
        })
    }
}

/// A child on a PTY that a holder process keeps, reached over its socket.
pub struct HeldPty {
    /// Shared with the reader thread, which answers Exit with ExitSeen.
    writer: Arc<Mutex<UnixStream>>,
    /// The same socket, for closing it without waiting on `writer`.
    closer: UnixStream,
    exit_rx: watch::Receiver<ExitValue>,
    exited_at: Arc<Mutex<Option<i64>>>,
}

impl HeldPty {
    fn send(&self, frame: &Frame) -> io::Result<()> {
        let mut w = self.writer.lock().unwrap();
        write_frame(&mut *w, frame)?;
        w.flush()
    }

    pub fn write(&self, data: &[u8]) -> io::Result<()> {
        for piece in data.chunks(CHUNK) {
            self.send(&Frame::Input(piece.to_vec()))?;
        }
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16) -> anyhow::Result<()> {
        self.send(&Frame::Resize { cols, rows })
            .context("resize held pty")
    }

    /// Asks the holder to kill the child. Does nothing harmful if it has
    /// already exited.
    pub fn kill(&self) {
        if let Err(e) = self.send(&Frame::Kill) {
            tracing::debug!("kill held child: {e}");
        }
    }

    /// Closes the connection and leaves the child to its holder, which
    /// keeps it running for the next daemon.
    pub fn close(&self) {
        let _ = self.closer.shutdown(Shutdown::Both);
    }

    /// When the child exited, in Unix seconds, once it has. Set before
    /// [`HeldPty::exit`] reports the exit.
    pub fn exited_at(&self) -> Option<i64> {
        *self.exited_at.lock().unwrap()
    }

    /// `Some(Some(code))` or `Some(None)` (killed by signal, or the holder
    /// lost) once exited.
    pub fn exit(&self) -> watch::Receiver<ExitValue> {
        self.exit_rx.clone()
    }
}

impl Drop for HeldPty {
    fn drop(&mut self) {
        // Ends the reader thread too; the holder keeps the child.
        self.close();
    }
}

/// Whether a process with this pid exists. For a holder's pid, use
/// [`holder_alive`], which also checks what the process is.
pub(crate) fn alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 sends nothing; it only checks the pid.
    pid > 0
        && (unsafe { libc::kill(pid, 0) } == 0
            || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

pub(crate) fn read_pid_file(path: &Path) -> Option<PidFile> {
    load_pid_file(path).ok().flatten()
}

/// Ends a holder that never got going: it and its child, if it had one.
pub(crate) fn abandon(holder: u32, state_dir: &Path, slot: SlotId) {
    if let Some(p) = read_pid_file(&pid_path(state_dir, slot)).filter(|p| p.holder == holder) {
        // SAFETY: plain signals. The child leads its own process group.
        unsafe {
            libc::kill(-(p.child as i32), libc::SIGKILL);
        }
    }
    // SAFETY: as above; the pid is our own child, not yet reaped.
    unsafe {
        libc::kill(holder as i32, libc::SIGKILL);
    }
}

/// Starts `<program> hold <state_dir>` for `spec` and connects to it. The
/// caller starts reading with [`Greeting::serve`].
///
/// Refuses when the slot's pid file names a live holder ([`holder_alive`]) (after giving one
/// that is finishing up [`HOLDER_START`] to go), since a new holder would
/// take its socket. A stale exit file is removed first, so an old exit is
/// never read as the new child's.
pub fn start(spec: &SpawnSpec, program: &Path, state_dir: &Path) -> anyhow::Result<Greeting> {
    let slot = spec.slot;
    let name = slot_name(slot);
    let socket = socket_path(state_dir, slot);
    if socket.as_os_str().as_bytes().len() > MAX_SOCKET_PATH {
        bail!(
            "holder socket path {} is longer than {MAX_SOCKET_PATH} bytes",
            socket.display()
        );
    }
    if let Some(old) = read_pid_file(&pid_path(state_dir, slot)) {
        let deadline = Instant::now() + HOLDER_START;
        while holder_alive(&old) && Instant::now() < deadline {
            std::thread::sleep(POLL);
        }
        if holder_alive(&old) {
            bail!("{name} already has a holder running (pid {})", old.holder);
        }
    }
    for stale in [exit_path(state_dir, slot), socket.clone()] {
        match std::fs::remove_file(&stale) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("remove {}", stale.display()))
            }
            _ => {}
        }
    }

    let json = serde_json::to_vec(spec).context("encode spawn spec")?;
    let mut child = Command::new(program)
        .arg("hold")
        .arg(state_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("start holder {}", program.display()))?;
    let pid = child.id();
    let fed = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("holder has no stdin"))
        .and_then(|mut stdin| stdin.write_all(&json).context("send spec to holder"));
    // Reaped here, so the holder is never left a zombie while the daemon
    // lives; the channel tells the wait below if it ends early.
    let (ended_tx, ended_rx) = mpsc::channel::<io::Result<ExitStatus>>();
    std::thread::Builder::new()
        .name(format!("holder-reap-{name}"))
        .spawn(move || {
            let _ = ended_tx.send(child.wait());
        })
        .context("spawn holder reaper thread")?;
    let fail = |e: anyhow::Error| {
        abandon(pid, state_dir, slot);
        e
    };
    fed.map_err(fail)?;

    let deadline = Instant::now() + HOLDER_START;
    loop {
        if let Ok(status) = ended_rx.try_recv() {
            let status = status.map_or_else(|e| e.to_string(), |s| s.to_string());
            bail!("holder for {name} exited while starting ({status})");
        }
        if socket.exists() {
            break;
        }
        if Instant::now() >= deadline {
            return Err(fail(anyhow!(
                "holder for {name} did not start within {}s",
                HOLDER_START.as_secs()
            )));
        }
        std::thread::sleep(POLL);
    }
    let greeting = Greeting::connect(&socket).map_err(fail)?;
    if !greeting.supported() {
        return Err(fail(anyhow!(
            "holder {} speaks protocol {}, not {HOLDER_PROTOCOL}",
            program.display(),
            greeting.hello.version
        )));
    }
    Ok(greeting)
}
