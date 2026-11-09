//! Unix socket server: one reader task, one writer task and one handler per
//! connection.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context};
use bytes::Bytes;
use overseer_proto::{read_frame, slot_name, write_frame, Event, Frame, Request, Response, SlotId};
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, watch, Notify};
use tokio::task::{JoinHandle, JoinSet};

use crate::manager::{valid_id, Attachment, SlotManager};

/// Frames queued per connection before it is considered too slow.
const OUTBOUND_QUEUE: usize = 256;
const INBOUND_QUEUE: usize = 64;
const EVENT_CHANNEL_CAP: usize = 256;
/// Lets the `Shutdown` reply reach its client before connections are dropped.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(100);

pub struct DaemonConfig {
    pub socket: PathBuf,
    /// Command used when `StartSlot` does not name one. Normally `["claude"]`.
    pub default_command: Vec<String>,
}

type Manager = Arc<Mutex<SlotManager>>;

async fn bind(path: &PathBuf) -> anyhow::Result<UnixListener> {
    if path.exists() {
        match UnixStream::connect(path).await {
            Ok(_) => bail!("daemon already running"),
            Err(e) => {
                tracing::info!("removing stale socket {}: {e}", path.display());
                std::fs::remove_file(path).context("remove stale socket")?;
            }
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).context("create socket dir")?;
    }
    let listener = UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .context("chmod socket")?;
    }
    Ok(listener)
}

pub async fn run(cfg: DaemonConfig) -> anyhow::Result<()> {
    let listener = bind(&cfg.socket).await?;
    let (events, _) = broadcast::channel::<Event>(EVENT_CHANNEL_CAP);
    let manager: Manager = Arc::new(Mutex::new(SlotManager::new(cfg.default_command, events)));
    let shutdown = Arc::new(Notify::new());
    let mut conns = JoinSet::new();

    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    conns.spawn(handle_conn(stream, manager.clone(), shutdown.clone()));
                }
                Err(e) => tracing::warn!("accept failed: {e}"),
            },
            _ = shutdown.notified() => break,
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
        }
    }

    manager.lock().unwrap().shutdown_all();
    let _ = std::fs::remove_file(&cfg.socket);
    tokio::time::sleep(SHUTDOWN_GRACE).await;
    // Dropping `conns` aborts every connection task, closing the sockets.
    drop(conns);
    Ok(())
}

/// Bounded outbound queue for one connection. Sending never waits: if the
/// queue is full the client is too slow and the connection is dropped.
#[derive(Clone)]
struct Outbox {
    tx: mpsc::Sender<Frame>,
    dead: Arc<watch::Sender<bool>>,
}

impl Outbox {
    fn send(&self, frame: Frame) -> bool {
        match self.tx.try_send(frame) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                if !self.dead.send_replace(true) {
                    tracing::warn!("client too slow, disconnected");
                }
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.dead.send_replace(true);
                false
            }
        }
    }
}

async fn handle_conn(stream: UnixStream, manager: Manager, shutdown: Arc<Notify>) {
    let (mut rd, mut wr) = stream.into_split();
    let (out_tx, mut out_rx) = mpsc::channel::<Frame>(OUTBOUND_QUEUE);
    let (dead_tx, mut dead_rx) = watch::channel(false);
    let dead_tx = Arc::new(dead_tx);
    let outbox = Outbox {
        tx: out_tx,
        dead: dead_tx.clone(),
    };

    // N1: read_frame is not cancel-safe, so it lives in its own task and is
    // never polled inside a select!. The handler only selects on channels.
    let (in_tx, mut in_rx) = mpsc::channel::<Frame>(INBOUND_QUEUE);
    let reader = tokio::spawn(async move {
        loop {
            match read_frame(&mut rd).await {
                Ok(Some(frame)) => {
                    if in_tx.send(frame).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::debug!("read error: {e}");
                    break;
                }
            }
        }
    });

    let writer_dead = dead_tx.clone();
    let writer = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if let Err(e) = write_frame(&mut wr, &frame).await {
                tracing::debug!("write error: {e}");
                break;
            }
        }
        let _ = wr.shutdown().await;
        writer_dead.send_replace(true);
    });

    let mut events = manager.lock().unwrap().subscribe_events();
    let mut attached: [Option<JoinHandle<()>>; crate::manager::SLOT_COUNT] = Default::default();

    loop {
        tokio::select! {
            frame = in_rx.recv() => match frame {
                Some(f) => handle_frame(f, &manager, &outbox, &shutdown, &mut attached),
                None => break, // client hung up
            },
            ev = events.recv() => match ev {
                Ok(ev) => {
                    if !outbox.send(Frame::Event(ev)) { break; }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("event stream lagged by {n}");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            _ = dead_rx.changed() => break,
        }
    }

    for h in attached.iter().flatten() {
        h.abort();
    }
    reader.abort();
    writer.abort();
}

fn handle_frame(
    frame: Frame,
    manager: &Manager,
    outbox: &Outbox,
    shutdown: &Notify,
    attached: &mut [Option<JoinHandle<()>>],
) {
    match frame {
        Frame::Request { seq, req } => {
            let is_shutdown = matches!(req, Request::Shutdown);
            let mut pending_attach = None;
            let resp = handle_request(req, manager, attached, &mut pending_attach);
            outbox.send(Frame::Response { seq, resp });
            // Ok first, then the snapshot, then live output.
            if let Some((id, att)) = pending_attach {
                start_stream(id, att, outbox, attached);
            }
            if is_shutdown {
                shutdown.notify_one();
            }
        }
        Frame::Input { id, data } => {
            // N2: a bad id on a fire-and-forget frame is logged and dropped.
            if !valid_id(id) {
                tracing::warn!("ignoring input frame for invalid slot id {id}");
                return;
            }
            let slot = manager.lock().unwrap().slot(id);
            match slot {
                Some(slot) => {
                    // The PTY write is a short blocking call; the kernel
                    // buffer absorbs keystroke-sized input.
                    if let Err(e) = slot.write(&data) {
                        tracing::debug!("input to {} failed: {e}", slot_name(id));
                    }
                }
                None => tracing::debug!("ignoring input for empty slot {}", slot_name(id)),
            }
        }
        other => tracing::debug!("ignoring unexpected frame from client: {other:?}"),
    }
}

fn handle_request(
    req: Request,
    manager: &Manager,
    attached: &mut [Option<JoinHandle<()>>],
    pending_attach: &mut Option<(SlotId, Attachment)>,
) -> Response {
    let err = |message: String| Response::Error { message };
    match req {
        Request::Hello { client } => {
            tracing::debug!("hello from {client}");
            Response::Ok
        }
        Request::ListSlots => Response::Slots(manager.lock().unwrap().list()),
        Request::StartSlot {
            id,
            label,
            cwd,
            command,
        } => match manager.lock().unwrap().start(id, label, &cwd, command) {
            Ok(()) => Response::Ok,
            Err(m) => err(m),
        },
        Request::StopSlot { id } => match manager.lock().unwrap().stop(id) {
            Ok(()) => Response::Ok,
            Err(m) => err(m),
        },
        Request::Attach { id } => match manager.lock().unwrap().attach(id) {
            Ok(a) => {
                *pending_attach = Some((id, a));
                Response::Ok
            }
            Err(m) => err(m),
        },
        Request::Detach { id } => {
            if !valid_id(id) {
                return err(format!("invalid slot id {id}"));
            }
            if let Some(h) = attached[id as usize].take() {
                h.abort();
            }
            Response::Ok
        }
        Request::Resize { id, cols, rows } => match manager.lock().unwrap().slot(id) {
            Some(slot) => match slot.resize(cols, rows) {
                Ok(()) => Response::Ok,
                Err(e) => err(format!("{e:#}")),
            },
            None if !valid_id(id) => err(format!("invalid slot id {id}")),
            None => err(format!("{} is empty", slot_name(id))),
        },
        Request::Shutdown => Response::Ok,
    }
}

/// Sends the snapshot as one `Output` frame, then forwards live output until
/// the stream ends, the connection dies or the client detaches.
fn start_stream(
    id: SlotId,
    att: Attachment,
    outbox: &Outbox,
    attached: &mut [Option<JoinHandle<()>>],
) {
    // Re-attaching replaces any earlier stream for this slot.
    if let Some(old) = attached[id as usize].take() {
        old.abort();
    }
    if !outbox.send(Frame::Output {
        id,
        data: Bytes::from(att.snapshot),
    }) {
        return;
    }
    let Some(mut live) = att.live else { return };
    let outbox = outbox.clone();
    attached[id as usize] = Some(tokio::spawn(async move {
        loop {
            match live.recv().await {
                Ok(data) => {
                    if !outbox.send(Frame::Output { id, data }) {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("{} output lagged by {n} chunks", slot_name(id));
                }
                // Output ended (child exited and the PTY drained). Everything
                // buffered before the close has already been delivered.
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }));
}
