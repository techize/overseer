//! Async client for the daemon socket.
//!
//! `read_frame` is not cancel-safe (N1), so a dedicated reader task owns the
//! read half. It routes `Response` frames to the waiting request and forwards
//! everything else (events, output) in arrival order through one channel, so
//! the server's frame order is preserved end to end.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};
use bytes::Bytes;
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::{read_frame, write_frame, Frame, Request, Response, SlotId};

const RETRY_EVERY: Duration = Duration::from_millis(100);
const RETRY_FOR: Duration = Duration::from_secs(3);
/// Frames buffered for the subscriber before the reader is back-pressured.
const SUBSCRIBER_QUEUE: usize = 256;

/// `None` once the connection is gone, so late requests fail fast.
type Pending = StdMutex<Option<HashMap<u32, oneshot::Sender<Response>>>>;

struct Writer {
    half: OwnedWriteHalf,
    next_seq: u32,
}

pub struct Client {
    writer: Mutex<Writer>,
    pending: std::sync::Arc<Pending>,
    inbox: StdMutex<Option<mpsc::UnboundedReceiver<Frame>>>,
}

impl Client {
    pub async fn connect(path: impl AsRef<Path>) -> io::Result<Client> {
        let (mut rd, half) = UnixStream::connect(path.as_ref()).await?.into_split();
        let pending: std::sync::Arc<Pending> =
            std::sync::Arc::new(StdMutex::new(Some(HashMap::new())));
        let (tx, rx) = mpsc::unbounded_channel();
        let reader_pending = pending.clone();
        tokio::spawn(async move {
            while let Ok(Some(frame)) = read_frame(&mut rd).await {
                match frame {
                    Frame::Response { seq, resp } => {
                        let waiter = reader_pending
                            .lock()
                            .unwrap()
                            .as_mut()
                            .and_then(|m| m.remove(&seq));
                        if let Some(w) = waiter {
                            let _ = w.send(resp);
                        }
                    }
                    Frame::Event(_) | Frame::Output { .. } => {
                        if tx.send(frame).is_err() {
                            // No subscriber handle left; keep serving responses.
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
            writer: Mutex::new(Writer { half, next_seq: 0 }),
            pending,
            inbox: StdMutex::new(Some(rx)),
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

    /// Sends a request and waits for its response.
    pub async fn request(&self, req: Request) -> anyhow::Result<Response> {
        let (tx, rx) = oneshot::channel();
        {
            // Registering and writing under one lock keeps send order equal
            // to seq order.
            let mut w = self.writer.lock().await;
            w.next_seq = w.next_seq.wrapping_add(1);
            let seq = w.next_seq;
            match self.pending.lock().unwrap().as_mut() {
                Some(m) => {
                    m.insert(seq, tx);
                }
                None => bail!("connection to the daemon is closed"),
            }
            if let Err(e) = write_frame(&mut w.half, &Frame::Request { seq, req }).await {
                if let Some(m) = self.pending.lock().unwrap().as_mut() {
                    m.remove(&seq);
                }
                return Err(anyhow!(e).context("write to daemon"));
            }
        }
        rx.await
            .map_err(|_| anyhow!("connection to the daemon closed before it replied"))
    }

    /// Takes the stream of events and output frames. Frames that arrived
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

    /// Fire-and-forget keystrokes for a slot.
    pub async fn send_input(&self, id: SlotId, data: &[u8]) -> io::Result<()> {
        let mut w = self.writer.lock().await;
        write_frame(
            &mut w.half,
            &Frame::Input {
                id,
                data: Bytes::copy_from_slice(data),
            },
        )
        .await
    }
}
