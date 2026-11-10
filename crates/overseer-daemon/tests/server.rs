use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use bytes::Bytes;
use overseer_daemon::{run, DaemonConfig};
use overseer_proto::{
    read_frame, write_frame, Activity, ActivityKind, Event, Frame, HookEvent, HookKind, Request,
    Response, SlotInfo, SlotState,
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
    state_dir: PathBuf,
    task: JoinHandle<anyhow::Result<()>>,
}

async fn start_daemon() -> Daemon {
    let socket = temp_socket();
    start_daemon_at(socket).await
}

async fn start_daemon_at(socket: PathBuf) -> Daemon {
    let state_dir = socket.parent().unwrap().join("state");
    let cfg = DaemonConfig {
        socket: socket.clone(),
        default_command: vec![SCRIPT.to_string()],
        env: overseer_daemon::EnvSource::Inherit,
        state_dir: state_dir.clone(),
        hook_program: PathBuf::from("/bin/true"),
    };
    let task = tokio::spawn(run(cfg));
    for _ in 0..100 {
        if UnixStream::connect(&socket).await.is_ok() {
            return Daemon {
                socket,
                state_dir,
                task,
            };
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
    // Give the flood time to overrun the socket buffer and the queue, and
    // the stream's grace for a full queue to run out.
    sleep(overseer_daemon::server::SLOW_CLIENT_GRACE + Duration::from_secs(2)).await;

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

/// A client that stops reading for a moment (less than the slow-client
/// grace) is not cut off, and the output it gets once it reads again has no
/// gap: chunks the daemon's stream lost while waiting are filled from the
/// ring.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paused_client_gets_the_whole_stream() {
    let d = start_daemon().await;
    let mut raw = UnixStream::connect(&d.socket).await.unwrap();
    let req = |seq, req| Frame::Request { seq, req };
    let count = "i=0; while [ $i -lt 400000 ]; do i=$((i+1)); echo $i; done; sleep 30";
    let start = Request::StartSlot {
        id: 4,
        label: "t4".into(),
        cwd: PathBuf::from("/tmp"),
        command: Some(vec!["/bin/sh".into(), "-c".into(), count.into()]),
    };
    write_frame(&mut raw, &req(1, start)).await.unwrap();
    write_frame(&mut raw, &req(2, Request::Attach { id: 4 }))
        .await
        .unwrap();
    // Read nothing while the loop runs, so the socket and the queue fill.
    sleep(overseer_daemon::server::SLOW_CLIENT_GRACE / 2).await;

    let (mut rd, _w) = raw.into_split();
    let mut text = Vec::new();
    let done = timeout(Duration::from_secs(60), async {
        while let Ok(Some(f)) = read_frame(&mut rd).await {
            if let Frame::Output { id: 4, data } = f {
                text.extend_from_slice(&data);
                let tail = &text[text.len().saturating_sub(16)..];
                let tail: Vec<u8> = tail.iter().copied().filter(|&b| b != b'\r').collect();
                if tail.ends_with(b"\n400000\n") {
                    return true;
                }
            }
        }
        false
    })
    .await
    .expect("timed out waiting for the end of the count");
    assert!(done, "connection closed before the count ended");
    let text = String::from_utf8_lossy(&text);
    // A resync (ring overrun) starts over with a reset and a snapshot.
    let text = text.rsplit("\x1bc").next().unwrap();
    let mut prev: Option<u64> = None;
    // Split on LF and drop every CR: under output pressure the macOS line
    // discipline can emit `\r\r\n` for one newline, which a terminal shows
    // the same as `\r\n`.
    for line in text
        .split('\n')
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.is_empty())
    {
        let n: u64 = line
            .parse()
            .unwrap_or_else(|_| panic!("not a number: {line:?}"));
        if let Some(p) = prev {
            if n != p + 1 {
                let at = text.find(&format!("\n{p}\r")).unwrap_or(0);
                let around = &text[at..(at + 40).min(text.len())];
                panic!("stream jumped from {p} to {n}: {around:?}");
            }
        }
        prev = Some(n);
    }
    assert_eq!(prev, Some(400000));
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
        state_dir: d.state_dir.clone(),
        hook_program: PathBuf::from("/bin/true"),
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

fn live_slots(d: &Daemon) -> Vec<u8> {
    let ledger = overseer_daemon::ledger::Ledger::open(&d.state_dir.join("overseer.db")).unwrap();
    ledger
        .live_at_shutdown()
        .unwrap()
        .iter()
        .map(|r| r.slot)
        .collect()
}

#[tokio::test]
async fn exit_is_recorded_in_the_ledger() {
    let d = start_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    assert_eq!(c.start(4).await, Response::Ok);
    assert_eq!(live_slots(&d), vec![4]);
    c.send(Frame::Input {
        id: 4,
        data: Bytes::from_static(b"quit\r"),
    })
    .await;
    c.exited_event(4).await;
    assert!(live_slots(&d).is_empty(), "self exit not recorded");

    // StopSlot counts too.
    assert_eq!(c.start(5).await, Response::Ok);
    assert_eq!(live_slots(&d), vec![5]);
    assert_eq!(c.request(Request::StopSlot { id: 5 }).await, Response::Ok);
    c.exited_event(5).await;
    assert!(live_slots(&d).is_empty(), "stop not recorded");
}

#[tokio::test]
async fn shutdown_does_not_record_exits() {
    let d = start_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    assert_eq!(c.start(6).await, Response::Ok);
    assert_eq!(c.request(Request::Shutdown).await, Response::Ok);
    let mut d = d;
    timeout(WAIT, &mut d.task)
        .await
        .expect("run returned")
        .unwrap()
        .unwrap();
    // Give the exit monitor time to see the killed child.
    sleep(Duration::from_millis(500)).await;
    assert_eq!(
        live_slots(&d),
        vec![6],
        "shutdown kill was recorded as an exit"
    );
}

fn hook(slot: u8, event: HookKind) -> HookEvent {
    HookEvent {
        slot,
        event,
        session_id: None,
        tool: None,
        detail: None,
        notification: None,
    }
}

impl Client {
    /// The next SlotChanged for `id`, whatever it carries.
    async fn next_change(&mut self, id: u8) -> SlotInfo {
        loop {
            match self.recv().await.expect("connection closed") {
                Frame::Event(Event::SlotChanged(info)) if info.id == id => return info,
                _ => {}
            }
        }
    }

    async fn hook(&mut self, ev: HookEvent) {
        assert_eq!(self.request(Request::Hook(ev)).await, Response::Ok);
    }
}

fn activity_of(info: &SlotInfo) -> &Activity {
    info.activity.as_ref().expect("activity set")
}

#[tokio::test]
async fn hook_requests_change_slot_activity() {
    let d = start_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    assert_eq!(c.start(2).await, Response::Ok);
    // The start announcement carries no activity yet.
    assert_eq!(c.next_change(2).await.activity, None);

    c.hook(hook(2, HookKind::UserPromptSubmit)).await;
    let info = c.next_change(2).await;
    assert_eq!(activity_of(&info).kind, ActivityKind::Working);
    let since = activity_of(&info).since;

    let mut tool = hook(2, HookKind::PostToolUse);
    tool.tool = Some("Bash".into());
    tool.detail = Some("cargo test".into());
    c.hook(tool).await;
    let info = c.next_change(2).await;
    let a = activity_of(&info);
    assert_eq!(a.kind, ActivityKind::Working);
    assert_eq!(a.since, since);
    assert_eq!(a.now_doing.as_deref(), Some("Bash cargo test"));
    // list agrees with the event.
    assert_eq!(c.list().await[2].activity.as_ref(), Some(a));

    let mut note = hook(2, HookKind::Notification);
    note.notification = Some("needs permission".into());
    c.hook(note).await;
    let info = c.next_change(2).await;
    assert_eq!(activity_of(&info).kind, ActivityKind::NeedsYou);
    assert_eq!(
        activity_of(&info).reason.as_deref(),
        Some("needs permission")
    );

    c.hook(hook(2, HookKind::Stop)).await;
    let info = c.next_change(2).await;
    assert_eq!(activity_of(&info).kind, ActivityKind::Idle);

    // A repeat changes nothing, so the next change seen is the exit.
    c.hook(hook(2, HookKind::Stop)).await;
    assert_eq!(c.request(Request::StopSlot { id: 2 }).await, Response::Ok);
    let info = c.exited_event(2).await;
    assert_eq!(info.activity, None, "exit clears activity");
    assert_eq!(c.list().await[2].activity, None);
}

#[tokio::test]
async fn hooks_for_slots_that_are_not_running_are_ignored() {
    let d = start_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    // Empty slot, and an id that does not exist: still Ok, nothing happens.
    c.hook(hook(3, HookKind::UserPromptSubmit)).await;
    c.hook(hook(99, HookKind::UserPromptSubmit)).await;
    assert_eq!(c.start(4).await, Response::Ok);
    assert_eq!(c.next_change(4).await.activity, None);
    c.send(Frame::Input {
        id: 4,
        data: Bytes::from_static(b"quit\r"),
    })
    .await;
    c.exited_event(4).await;
    // A late hook after the exit must not revive the slot's activity.
    c.hook(hook(4, HookKind::UserPromptSubmit)).await;
    // The next observable change is slot 5's start, not slot 4's.
    assert_eq!(c.start(5).await, Response::Ok);
    loop {
        let Frame::Event(Event::SlotChanged(info)) = c.recv().await.unwrap() else {
            continue;
        };
        assert_ne!(info.id, 3, "empty slot announced");
        assert_ne!(info.id, 4, "exited slot announced after its exit");
        if info.id == 5 {
            break;
        }
    }
    assert_eq!(c.list().await[4].activity, None);
}

#[tokio::test]
async fn session_start_with_new_id_updates_the_ledger() {
    let d = start_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    assert_eq!(c.start(1).await, Response::Ok);
    let ledger = overseer_daemon::ledger::Ledger::open(&d.state_dir.join("overseer.db")).unwrap();
    let id_of =
        |l: &overseer_daemon::ledger::Ledger| l.live_at_shutdown().unwrap()[0].session_id.clone();
    assert_eq!(id_of(&ledger), "", "non-claude slots start with no id");

    let mut ev = hook(1, HookKind::SessionStart);
    ev.session_id = Some("new-session-id".into());
    c.hook(ev).await;
    c.next_change(1).await; // start
    let info = c.next_change(1).await;
    assert_eq!(activity_of(&info).kind, ActivityKind::Idle);
    assert_eq!(id_of(&ledger), "new-session-id");

    // A later change is picked up too.
    let mut ev = hook(1, HookKind::SessionStart);
    ev.session_id = Some("later-id".into());
    c.hook(ev).await;
    // Already idle, so no change is announced; the next event proves the
    // hook was handled before the ledger is read.
    c.hook(hook(1, HookKind::UserPromptSubmit)).await;
    c.next_change(1).await;
    assert_eq!(id_of(&ledger), "later-id");
}

/// A script named `claude` that logs its cwd and argv, then sleeps.
fn stub_claude(dir: &std::path::Path) -> (PathBuf, PathBuf) {
    std::fs::create_dir_all(dir).unwrap();
    let log = dir.join("calls.log");
    let bin = dir.join("claude");
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\nprintf '%s|%s\\n' \"$(pwd -P)\" \"$*\" >> '{}'\nexec sleep 30\n",
            log.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    (bin, log)
}

/// Stops the daemon through its shutdown path, so exits are not recorded.
async fn shutdown(mut d: Daemon) -> PathBuf {
    let socket = d.socket.clone();
    let mut c = Client::connect(&socket).await;
    assert_eq!(c.request(Request::Shutdown).await, Response::Ok);
    timeout(WAIT, &mut d.task)
        .await
        .expect("run returned")
        .unwrap()
        .unwrap();
    // Give the exit monitors time to see the killed children.
    sleep(Duration::from_millis(500)).await;
    socket
}

async fn wait_for_log(log: &std::path::Path, lines: usize) -> Vec<String> {
    for _ in 0..100 {
        if let Ok(s) = std::fs::read_to_string(log) {
            if s.lines().count() >= lines {
                return s.lines().map(String::from).collect();
            }
        }
        sleep(Duration::from_millis(50)).await;
    }
    panic!("stub never logged {lines} lines");
}

fn arg_after(line: &str, flag: &str) -> Option<String> {
    let args = line.split_once('|').unwrap().1;
    let mut it = args.split(' ');
    it.find(|a| *a == flag)?;
    it.next().map(String::from)
}

#[tokio::test]
async fn restart_offers_resume() {
    let dir = std::env::temp_dir().join(format!("ovs-resume-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (bin, log) = stub_claude(&dir);
    let cwd = dir.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let cwd = cwd.canonicalize().unwrap();

    let d = start_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    let start = Request::StartSlot {
        id: 3,
        label: "resumer".into(),
        cwd: cwd.clone(),
        command: Some(vec![bin.to_string_lossy().into_owned()]),
    };
    assert_eq!(c.request(start).await, Response::Ok);
    let first = wait_for_log(&log, 1).await;
    let session_id = arg_after(&first[0], "--session-id").expect("fresh start has an id");
    drop(c);

    let socket = shutdown(d).await;
    let d = start_daemon_at(socket).await;
    let mut c = Client::connect(&d.socket).await;
    let info = c.list().await.remove(3);
    assert_eq!(
        info.state,
        SlotState::Resumable {
            session_id: session_id.clone(),
            at: match info.state {
                SlotState::Resumable { at, .. } => at,
                _ => 0,
            }
        }
    );
    assert_eq!(info.label, "resumer");
    assert_eq!(info.cwd.as_deref(), Some(cwd.as_path()));

    assert_eq!(c.request(Request::ResumeSlot { id: 3 }).await, Response::Ok);
    let lines = wait_for_log(&log, 2).await;
    assert_eq!(lines[1].split_once('|').unwrap().0, cwd.to_string_lossy());
    assert_eq!(
        arg_after(&lines[1], "--resume").as_deref(),
        Some(session_id.as_str())
    );
    assert!(!lines[1].contains("--session-id"), "{}", lines[1]);
    assert!(matches!(c.list().await[3].state, SlotState::Running { .. }));
    // Same ledger row, same id, live again.
    let ledger = overseer_daemon::ledger::Ledger::open(&d.state_dir.join("overseer.db")).unwrap();
    let live = ledger.live_at_shutdown().unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].session_id, session_id);
    // A second resume is refused: it is running now.
    assert!(matches!(
        c.request(Request::ResumeSlot { id: 3 }).await,
        Response::Error { .. }
    ));
}

#[tokio::test]
async fn non_claude_slot_is_not_resumable() {
    let d = start_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    assert_eq!(c.start(2).await, Response::Ok);
    drop(c);
    let socket = shutdown(d).await;
    let d = start_daemon_at(socket).await;
    let mut c = Client::connect(&d.socket).await;
    assert!(matches!(
        c.list().await[2].state,
        SlotState::Exited { code: None, .. }
    ));
    assert!(matches!(
        c.request(Request::ResumeSlot { id: 2 }).await,
        Response::Error { .. }
    ));
    assert!(live_slots(&d).is_empty(), "row marked exited");
}

#[tokio::test]
async fn exited_slot_is_not_resumable() {
    let dir = std::env::temp_dir().join(format!("ovs-exited-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (bin, _log) = stub_claude(&dir);
    let d = start_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    let start = Request::StartSlot {
        id: 1,
        label: "x".into(),
        cwd: PathBuf::from("/tmp"),
        command: Some(vec![bin.to_string_lossy().into_owned()]),
    };
    assert_eq!(c.request(start).await, Response::Ok);
    assert_eq!(c.request(Request::StopSlot { id: 1 }).await, Response::Ok);
    c.exited_event(1).await;
    drop(c);
    let socket = shutdown(d).await;
    let d = start_daemon_at(socket).await;
    let mut c = Client::connect(&d.socket).await;
    assert_eq!(c.list().await[1].state, SlotState::Empty);
    assert!(matches!(
        c.request(Request::ResumeSlot { id: 1 }).await,
        Response::Error { .. }
    ));
}

#[tokio::test]
async fn start_and_stop_on_a_resumable_slot() {
    let dir = std::env::temp_dir().join(format!("ovs-offer-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (bin, log) = stub_claude(&dir);
    let cmd = Some(vec![bin.to_string_lossy().into_owned()]);
    let d = start_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    for id in [0u8, 1] {
        let start = Request::StartSlot {
            id,
            label: format!("s{id}"),
            cwd: PathBuf::from("/tmp"),
            command: cmd.clone(),
        };
        assert_eq!(c.request(start).await, Response::Ok);
    }
    wait_for_log(&log, 2).await;
    drop(c);
    let socket = shutdown(d).await;
    let d = start_daemon_at(socket).await;
    let mut c = Client::connect(&d.socket).await;
    let slots = c.list().await;
    assert!(matches!(slots[0].state, SlotState::Resumable { .. }));
    assert!(matches!(slots[1].state, SlotState::Resumable { .. }));

    // Stop declines the offer.
    assert_eq!(c.request(Request::StopSlot { id: 0 }).await, Response::Ok);
    assert_eq!(c.list().await[0].state, SlotState::Empty);
    // Start goes fresh, with a new session id and no --resume.
    let start = Request::StartSlot {
        id: 1,
        label: "s1".into(),
        cwd: PathBuf::from("/tmp"),
        command: cmd,
    };
    assert_eq!(c.request(start).await, Response::Ok);
    let lines = wait_for_log(&log, 3).await;
    assert!(lines[2].contains("--session-id"), "{}", lines[2]);
    assert!(!lines[2].contains("--resume"), "{}", lines[2]);
    assert!(matches!(c.list().await[1].state, SlotState::Running { .. }));
    // Only the running one is live in the ledger; the declined one is gone.
    assert_eq!(live_slots(&d), vec![1]);
}
