use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use bytes::Bytes;
use overseer_daemon::{run, DaemonConfig};
use overseer_proto::{
    read_frame, write_frame, Event, Frame, Request, Response, SlotInfo, SlotState,
};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fake-claude.sh");
const WAIT: Duration = Duration::from_secs(5);

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_socket() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("ovs-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("s.sock")
}

struct Daemon {
    socket: PathBuf,
    task: JoinHandle<anyhow::Result<()>>,
}

async fn start_daemon() -> Daemon {
    let socket = temp_socket();
    start_daemon_at(socket).await
}

async fn start_daemon_at(socket: PathBuf) -> Daemon {
    let cfg = DaemonConfig {
        socket: socket.clone(),
        default_command: vec![SCRIPT.to_string()],
        env: overseer_daemon::EnvSource::Inherit,
    };
    let task = tokio::spawn(run(cfg));
    for _ in 0..100 {
        if UnixStream::connect(&socket).await.is_ok() {
            return Daemon { socket, task };
        }
        sleep(Duration::from_millis(20)).await;
    }
    panic!("daemon did not come up");
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Test client. Frames are read in a dedicated task (read_frame is not
/// cancel-safe), so timeouts here only ever cancel a channel receive.
struct Client {
    w: OwnedWriteHalf,
    rx: mpsc::UnboundedReceiver<Frame>,
    backlog: VecDeque<Frame>,
    seq: u32,
}

impl Client {
    async fn connect(path: &PathBuf) -> Client {
        let (mut rd, w) = UnixStream::connect(path).await.unwrap().into_split();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok(Some(f)) = read_frame(&mut rd).await {
                if tx.send(f).is_err() {
                    break;
                }
            }
        });
        Client {
            w,
            rx,
            backlog: VecDeque::new(),
            seq: 0,
        }
    }

    async fn send(&mut self, frame: Frame) {
        write_frame(&mut self.w, &frame).await.unwrap();
    }

    async fn recv(&mut self) -> Option<Frame> {
        if let Some(f) = self.backlog.pop_front() {
            return Some(f);
        }
        timeout(WAIT, self.rx.recv()).await.expect("timed out")
    }

    async fn request(&mut self, req: Request) -> Response {
        self.seq += 1;
        let seq = self.seq;
        self.send(Frame::Request { seq, req }).await;
        let mut skipped = VecDeque::new();
        let resp = loop {
            match self.recv().await.expect("connection closed") {
                Frame::Response { seq: s, resp } if s == seq => break resp,
                other => skipped.push_back(other),
            }
        };
        // Keep unrelated frames (events, output) for later, in order.
        skipped.extend(self.backlog.drain(..));
        self.backlog = skipped;
        resp
    }

    async fn start(&mut self, id: u8) -> Response {
        self.request(Request::StartSlot {
            id,
            label: format!("t{id}"),
            cwd: PathBuf::from("/tmp"),
            command: None,
        })
        .await
    }

    async fn list(&mut self) -> Vec<SlotInfo> {
        match self.request(Request::ListSlots).await {
            Response::Slots(s) => s,
            other => panic!("unexpected {other:?}"),
        }
    }

    /// Accumulate Output for `id` until `needle` shows up.
    async fn output_until(&mut self, id: u8, needle: &str) -> String {
        let mut acc = String::new();
        loop {
            match self.recv().await.expect("connection closed") {
                Frame::Output { id: i, data } if i == id => {
                    acc.push_str(&String::from_utf8_lossy(&data));
                    if acc.contains(needle) {
                        return acc;
                    }
                }
                _ => {}
            }
        }
    }

    async fn exited_event(&mut self, id: u8) -> SlotInfo {
        loop {
            match self.recv().await.expect("connection closed") {
                Frame::Event(Event::SlotChanged(info))
                    if info.id == id && matches!(info.state, SlotState::Exited { .. }) =>
                {
                    return info
                }
                _ => {}
            }
        }
    }
}

#[tokio::test]
async fn start_list_stop() {
    let d = start_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    assert_eq!(c.start(1).await, Response::Ok);
    let slots = c.list().await;
    assert_eq!(slots.len(), 7);
    assert!(matches!(slots[1].state, SlotState::Running { .. }));
    assert_eq!(slots[0].state, SlotState::Empty);
    assert_eq!(c.request(Request::StopSlot { id: 1 }).await, Response::Ok);
    let info = c.exited_event(1).await;
    assert!(matches!(info.state, SlotState::Exited { .. }));
}

#[tokio::test]
async fn double_start_refused() {
    let d = start_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    assert_eq!(c.start(1).await, Response::Ok);
    match c.start(1).await {
        Response::Error { message } => assert!(message.contains("S1 is already running")),
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn attach_replays_then_streams() {
    let d = start_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    assert_eq!(c.start(0).await, Response::Ok);
    sleep(Duration::from_millis(300)).await;
    assert_eq!(c.request(Request::Attach { id: 0 }).await, Response::Ok);
    // First Output after the Ok is the snapshot (the Running event may come first).
    let snapshot = loop {
        match c.recv().await.unwrap() {
            Frame::Output { id: 0, data } => break data,
            Frame::Event(_) => {}
            other => panic!("expected snapshot, got {other:?}"),
        }
    };
    assert!(String::from_utf8_lossy(&snapshot).contains("READY"));
    c.send(Frame::Input {
        id: 0,
        data: Bytes::from_static(b"hi\r"),
    })
    .await;
    c.output_until(0, "ECHO:hi").await;
}

#[tokio::test]
async fn two_clients_same_slot() {
    let d = start_daemon().await;
    let mut a = Client::connect(&d.socket).await;
    let mut b = Client::connect(&d.socket).await;
    assert_eq!(a.start(2).await, Response::Ok);
    assert_eq!(a.request(Request::Attach { id: 2 }).await, Response::Ok);
    assert_eq!(b.request(Request::Attach { id: 2 }).await, Response::Ok);
    a.output_until(2, "READY").await;
    b.output_until(2, "READY").await;

    a.send(Frame::Input {
        id: 2,
        data: Bytes::from_static(b"yo\r"),
    })
    .await;
    a.output_until(2, "ECHO:yo").await;
    b.output_until(2, "ECHO:yo").await;

    let resize = |cols, rows| Request::Resize { id: 2, cols, rows };
    assert_eq!(b.request(resize(100, 30)).await, Response::Ok);
    assert_eq!(a.request(resize(90, 25)).await, Response::Ok);
    let info = &a.list().await[2];
    assert_eq!((info.cols, info.rows), (90, 25));
}

#[tokio::test]
async fn slow_client_dropped() {
    let d = start_daemon().await;
    // A raw stream with no reader task: nothing ever drains its socket.
    let mut slow = UnixStream::connect(&d.socket).await.unwrap();
    let req = |seq, req| Frame::Request { seq, req };
    write_frame(
        &mut slow,
        &req(
            1,
            Request::StartSlot {
                id: 3,
                label: "t3".into(),
                cwd: PathBuf::from("/tmp"),
                command: None,
            },
        ),
    )
    .await
    .unwrap();
    write_frame(&mut slow, &req(2, Request::Attach { id: 3 }))
        .await
        .unwrap();
    write_frame(
        &mut slow,
        &Frame::Input {
            id: 3,
            data: Bytes::from_static(b"flood\r"),
        },
    )
    .await
    .unwrap();
    // Give the flood time to overrun the socket buffer and the queue.
    sleep(Duration::from_secs(2)).await;

    let mut other = Client::connect(&d.socket).await;
    let slots = timeout(Duration::from_secs(1), other.list())
        .await
        .expect("daemon blocked on slow client");
    assert_eq!(slots.len(), 7);

    // The slow connection was closed by the daemon: reading it now reaches
    // EOF (or a reset) after whatever was already buffered.
    let closed = timeout(WAIT, async {
        use tokio::io::AsyncReadExt;
        let mut buf = [0u8; 65536];
        loop {
            match slow.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "slow connection was not closed");
}

#[tokio::test]
async fn stale_socket_removed() {
    let socket = temp_socket();
    std::fs::write(&socket, b"not a socket").unwrap();
    let d = start_daemon_at(socket).await;
    let mut c = Client::connect(&d.socket).await;
    assert_eq!(c.list().await.len(), 7);
}

#[tokio::test]
async fn second_daemon_refused() {
    let d = start_daemon().await;
    let err = run(DaemonConfig {
        socket: d.socket.clone(),
        default_command: vec![SCRIPT.to_string()],
        env: overseer_daemon::EnvSource::Inherit,
    })
    .await
    .unwrap_err();
    assert!(err.to_string().contains("daemon already running"), "{err}");
    // M4: typed, so main can exit 0 and launchd does not respawn it.
    assert!(err
        .downcast_ref::<overseer_daemon::AlreadyRunning>()
        .is_some());
}

#[tokio::test]
async fn exit_then_restart() {
    let d = start_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    assert_eq!(c.start(4).await, Response::Ok);
    c.send(Frame::Input {
        id: 4,
        data: Bytes::from_static(b"quit\r"),
    })
    .await;
    let info = c.exited_event(4).await;
    assert!(matches!(
        info.state,
        SlotState::Exited { code: Some(3), .. }
    ));
    assert_eq!(c.start(4).await, Response::Ok);
    assert!(matches!(c.list().await[4].state, SlotState::Running { .. }));
}

#[tokio::test]
async fn bad_slot_ids_rejected() {
    let d = start_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    for req in [
        Request::StopSlot { id: 7 },
        Request::Attach { id: 200 },
        Request::Detach { id: 9 },
        Request::Resize {
            id: 7,
            cols: 80,
            rows: 24,
        },
    ] {
        assert!(
            matches!(c.request(req.clone()).await, Response::Error { .. }),
            "{req:?}"
        );
    }
    assert!(matches!(c.start(7).await, Response::Error { .. }));
    // Input with a bad id is ignored and the connection survives.
    c.send(Frame::Input {
        id: 99,
        data: Bytes::from_static(b"x"),
    })
    .await;
    assert_eq!(c.list().await.len(), 7);
}

#[tokio::test]
async fn exit_event_follows_final_output() {
    // N5: by the time Exited is announced, a fresh attach sees the whole tail.
    let d = start_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    assert_eq!(c.start(5).await, Response::Ok);
    c.send(Frame::Input {
        id: 5,
        data: Bytes::from_static(b"flood\r"),
    })
    .await;
    c.exited_event(5).await;
    assert_eq!(c.request(Request::Attach { id: 5 }).await, Response::Ok);
    c.output_until(5, "FLOOD-END").await;
}

#[tokio::test]
async fn shutdown_kills_and_removes_socket() {
    let d = start_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    assert_eq!(c.start(6).await, Response::Ok);
    assert_eq!(c.request(Request::Shutdown).await, Response::Ok);
    let res = timeout(WAIT, async {
        let mut d = d;
        let r = (&mut d.task).await;
        (r, d)
    })
    .await
    .expect("run did not return");
    assert!(res.0.unwrap().is_ok());
    assert!(!res.1.socket.exists());
}

/// A program writing one short line per syscall (as `seq` does on a TTY)
/// produces a flood of tiny PTY reads. An attached client that keeps up must
/// not be dropped as slow, which needs queued chunks merged into few frames.
#[tokio::test]
async fn line_at_a_time_burst_does_not_drop_live_client() {
    let d = start_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    let start = Request::StartSlot {
        id: 2,
        label: "burst".into(),
        cwd: PathBuf::from("/tmp"),
        command: Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            "sleep 0.5; seq 1 200000; echo BURST-END".into(),
        ]),
    };
    assert_eq!(c.request(start).await, Response::Ok);
    assert_eq!(c.request(Request::Attach { id: 2 }).await, Response::Ok);
    let out = timeout(WAIT * 4, c.output_until(2, "BURST-END"))
        .await
        .expect("burst did not arrive");
    assert!(out.contains("199999"));
}
