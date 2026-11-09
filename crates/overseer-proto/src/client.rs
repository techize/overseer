//! Async client for the daemon socket.
//!
//! `read_frame` is not cancel-safe (N1), so a dedicated reader task owns the
//! read half. It routes `Response` frames to the waiting request and forwards
//! everything else (events, output) in arrival order through one channel, so
//! the server's frame order is preserved end to end.
//!
//! Writes are cancel-safe (N7): a dedicated writer task owns the write half and
//! drains an unbounded queue of frames. `request` and `send_input` only enqueue
//! (synchronously, so enqueue order is wire order) and never await a socket
//! write, so dropping their futures cannot leave half a frame on the stream.
//! The queue is unbounded because it is fed at human or request rate.
//!
//! Ordering marks: `request_marked` also puts the reply into the subscriber
//! stream, as `Frame::Response`, at the point it arrived on the wire. A
//! subscriber can then tell frames the daemon sent before the reply (such as
//! output from an earlier attach) from frames sent after it (the snapshot of
//! a new attach). Plain `request` replies never enter the stream.
//!
//! The inbox channel is bounded (`INBOX_CAP` frames). The reader never blocks on it,
//! because it also has to route responses. A consumer that falls that far
//! behind is cut off instead: the reader stops, `inbox_overflowed` turns true
//! and every request fails as if the daemon had hung up.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};
use bytes::Bytes;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};

use crate::{read_frame, write_frame, Frame, Request, Response, SlotId};

const RETRY_EVERY: Duration = Duration::from_millis(100);
const RETRY_FOR: Duration = Duration::from_secs(3);
/// Frames the reader may queue for the subscriber before it disconnects.
const INBOX_CAP: usize = 4096;
/// Frames buffered between the forwarder task and the subscriber.
const SUBSCRIBER_QUEUE: usize = 256;

/// A request waiting for its reply.
struct Waiter {
    reply: oneshot::Sender<Response>,
    /// Also put the reply into the subscriber stream (see `request_marked`).
    mark: bool,
}

/// `None` once the connection is gone, so late requests fail fast.
type Pending = StdMutex<Option<HashMap<u32, Waiter>>>;

/// Removes a request's pending entry if its future is dropped while waiting.
struct PendingGuard {
    pending: Arc<Pending>,
    seq: u32,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Some(m) = self.pending.lock().unwrap().as_mut() {
            m.remove(&self.seq);
        }
    }
}

pub struct Client {
    /// Guards the sequence counter; held while enqueuing so seq order equals
    /// wire order.
    next_seq: StdMutex<u32>,
    outbox: mpsc::UnboundedSender<Frame>,
    pending: Arc<Pending>,
    inbox: StdMutex<Option<mpsc::Receiver<Frame>>>,
    overflowed: Arc<AtomicBool>,
}

/// Queues `frame` for the subscriber. False when the subscriber has fallen
/// `INBOX_CAP` frames behind, which ends the connection.
fn forward(tx: &mpsc::Sender<Frame>, frame: Frame, overflowed: &AtomicBool) -> bool {
    match tx.try_send(frame) {
        Ok(()) => true,
        // No subscriber left; keep serving responses.
        Err(mpsc::error::TrySendError::Closed(_)) => true,
        Err(mpsc::error::TrySendError::Full(_)) => {
            overflowed.store(true, Ordering::SeqCst);
            false
        }
    }
}

impl Client {
    pub async fn connect(path: impl AsRef<Path>) -> io::Result<Client> {
        Client::connect_with_inbox_cap(path, INBOX_CAP).await
    }

    /// `connect` with a custom inbox cap, so tests can force an overflow.
    #[doc(hidden)]
    pub async fn connect_with_inbox_cap(path: impl AsRef<Path>, cap: usize) -> io::Result<Client> {
        let (mut rd, mut half) = UnixStream::connect(path.as_ref()).await?.into_split();
        let pending: Arc<Pending> = Arc::new(StdMutex::new(Some(HashMap::new())));
        let (tx, rx) = mpsc::channel(cap);
        let overflowed = Arc::new(AtomicBool::new(false));
        let reader_pending = pending.clone();
        let reader_overflowed = overflowed.clone();
        let (outbox, mut queue) = mpsc::unbounded_channel::<Frame>();
        let writer_pending = pending.clone();
        tokio::spawn(async move {
            while let Some(frame) = queue.recv().await {
                if write_frame(&mut half, &frame).await.is_err() {
                    break;
                }
            }
            // Dropping `queue` makes later enqueues fail; fail waiting requests.
            *writer_pending.lock().unwrap() = None;
        });
        tokio::spawn(async move {
            while let Ok(Some(frame)) = read_frame(&mut rd).await {
                match frame {
                    Frame::Response { seq, resp } => {
                        let waiter = reader_pending
                            .lock()
                            .unwrap()
                            .as_mut()
                            .and_then(|m| m.remove(&seq));
                        let Some(w) = waiter else { continue };
                        if w.mark {
                            let mark = Frame::Response {
                                seq,
                                resp: resp.clone(),
                            };
                            if !forward(&tx, mark, &reader_overflowed) {
                                break;
                            }
                        }
                        let _ = w.reply.send(resp);
                    }
                    Frame::Event(_) | Frame::Output { .. } => {
                        if !forward(&tx, frame, &reader_overflowed) {
                            break;
                        }
                    }
                    // Servers do not send these.
                    Frame::Request { .. } | Frame::Input { .. } => {}
                }
            }
            // Dropping the senders fails every in-flight request.
            *reader_pending.lock().unwrap() = None;
        });
        Ok(Client {
            next_seq: StdMutex::new(0),
            outbox,
            pending,
            inbox: StdMutex::new(Some(rx)),
            overflowed,
        })
    }

    /// Connects, starting the daemon through `start` when nobody answers.
    /// Retries every 100 ms for up to 3 s.
    pub async fn connect_or_start(
        path: impl AsRef<Path>,
        start: impl FnOnce() -> io::Result<()>,
    ) -> anyhow::Result<Client> {
        let path: PathBuf = path.as_ref().to_path_buf();
        if let Ok(c) = Client::connect(&path).await {
            return Ok(c);
        }
        let started = start();
        let deadline = Instant::now() + RETRY_FOR;
        loop {
            tokio::time::sleep(RETRY_EVERY).await;
            if let Ok(c) = Client::connect(&path).await {
                return Ok(c);
            }
            if Instant::now() >= deadline {
                break;
            }
        }
        match started {
            Ok(()) => bail!("could not reach the Overseer daemon at {}", path.display()),
            Err(e) => bail!(
                "could not reach the Overseer daemon at {} (start failed: {e})",
                path.display()
            ),
        }
    }

    /// Sends a request and waits for its response. Cancel-safe: dropping the
    /// future forgets the reply but leaves the stream intact.
    pub async fn request(&self, req: Request) -> anyhow::Result<Response> {
        self.send_request(req, false, |_| {}).await
    }

    /// Like `request`, and the reply is also put into the subscriber stream
    /// as `Frame::Response { seq, .. }`, ordered with the events and output
    /// around it. `on_seq` gets the request's sequence number before the
    /// request is sent, so the subscriber can be told which mark to expect
    /// before it can possibly arrive.
    pub async fn request_marked(
        &self,
        req: Request,
        on_seq: impl FnOnce(u32),
    ) -> anyhow::Result<Response> {
        self.send_request(req, true, on_seq).await
    }

    async fn send_request(
        &self,
        req: Request,
        mark: bool,
        on_seq: impl FnOnce(u32),
    ) -> anyhow::Result<Response> {
        let (tx, rx) = oneshot::channel();
        let seq = {
            let mut next = self.next_seq.lock().unwrap();
            *next = next.wrapping_add(1);
            let seq = *next;
            match self.pending.lock().unwrap().as_mut() {
                Some(m) => {
                    m.insert(seq, Waiter { reply: tx, mark });
                }
                None => bail!("connection to the daemon is closed"),
            }
            on_seq(seq);
            if self.outbox.send(Frame::Request { seq, req }).is_err() {
                if let Some(m) = self.pending.lock().unwrap().as_mut() {
                    m.remove(&seq);
                }
                bail!("connection to the daemon is closed");
            }
            seq
        };
        let _guard = PendingGuard {
            pending: self.pending.clone(),
            seq,
        };
        rx.await
            .map_err(|_| anyhow!("connection to the daemon closed before it replied"))
    }

    /// Takes the stream of events, output frames and the replies of
    /// `request_marked` calls. Frames that arrived
    /// before the call are delivered first, in order. Only the first call
    /// gets frames; later calls return a closed receiver.
    pub fn subscribe(&self) -> mpsc::Receiver<Frame> {
        let (tx, rx) = mpsc::channel(SUBSCRIBER_QUEUE);
        if let Some(mut inbox) = self.inbox.lock().unwrap().take() {
            tokio::spawn(async move {
                while let Some(f) = inbox.recv().await {
                    if tx.send(f).await.is_err() {
                        break;
                    }
                }
            });
        }
        rx
    }

    /// True when the connection was dropped because the subscriber stopped
    /// draining frames (see the module docs).
    pub fn inbox_overflowed(&self) -> bool {
        self.overflowed.load(Ordering::SeqCst)
    }

    /// Fire-and-forget keystrokes for a slot. Only queues the frame; an
    /// error means the connection is already gone.
    pub async fn send_input(&self, id: SlotId, data: &[u8]) -> io::Result<()> {
        let _order = self.next_seq.lock().unwrap();
        self.outbox
            .send(Frame::Input {
                id,
                data: Bytes::copy_from_slice(data),
            })
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "connection to the daemon is closed",
                )
            })
    }
}
