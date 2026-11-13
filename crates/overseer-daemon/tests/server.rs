use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use bytes::Bytes;
use overseer_daemon::locks::targets::{default_patterns, load_patterns, Pattern};
use overseer_daemon::{run, DaemonConfig};
use overseer_proto::{
    read_frame, write_frame, Activity, ActivityKind, BashCheck, BoardView, ClaimKind, CoordView,
    Event, Frame, HookEvent, HookKind, ItemKind, LockView, Plan, PlanDecision, PlanSlot, PlanState,
    PlanTarget, PlanWorktree, RelayState, ReportKind, Request, Response, Sender, SlotInfo,
    SlotState, SourceStatus, WorkItem, WorkView,
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
    start_daemon_with(socket, overseer_daemon::EnvSource::Inherit).await
}

async fn start_daemon_with(socket: PathBuf, env: overseer_daemon::EnvSource) -> Daemon {
    start_daemon_cfg(socket, env, offline_settings()).await
}

/// Settings that read no real work sources: Jira is off and the beads
/// folder does not exist.
fn offline_settings() -> overseer_daemon::Config {
    overseer_daemon::Config {
        beads_dir: PathBuf::from("/nonexistent/overseer-test-beads"),
        brain_dir: PathBuf::from("/nonexistent/overseer-test-brain"),
        jira_command: vec![],
        ..overseer_daemon::Config::default()
    }
}

async fn start_daemon_cfg(
    socket: PathBuf,
    env: overseer_daemon::EnvSource,
    settings: overseer_daemon::Config,
) -> Daemon {
    start_daemon_full(socket, env, settings, default_patterns()).await
}

async fn start_daemon_full(
    socket: PathBuf,
    env: overseer_daemon::EnvSource,
    settings: overseer_daemon::Config,
    patterns: Vec<Pattern>,
) -> Daemon {
    let state_dir = socket.parent().unwrap().join("state");
    let cfg = DaemonConfig {
        socket: socket.clone(),
        default_command: vec![SCRIPT.to_string()],
        env,
        state_dir: state_dir.clone(),
        hook_program: PathBuf::from("/bin/true"),
        settings,
        patterns,
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
            prompt: None,
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
                prompt: None,
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
        prompt: None,
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
        settings: offline_settings(),
        patterns: default_patterns(),
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
        prompt: None,
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
        tool_use_id: None,
        notification_type: None,
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
            "#!/bin/sh\n\
             # Like claude, save the conversation for a new session id. Saved\n\
             # before the call is logged, so a test that has seen the log line\n\
             # never stops the stub between the two.\n\
             if [ \"$1\" = --session-id ] && [ -n \"$STUB_SAVES\" ]; then\n\
             mkdir -p \"$CLAUDE_CONFIG_DIR/projects/p\" && : > \"$CLAUDE_CONFIG_DIR/projects/p/$2.jsonl\"\n\
             fi\nprintf '%s|%s\\n' \"$(pwd -P)\" \"$*\" >> '{}'\nexec sleep 30\n",
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

/// Slots see their own Claude config folder; with `saves` the stub writes a
/// transcript for each new session, as claude does once it has a message.
fn claude_env(dir: &std::path::Path, saves: bool) -> overseer_daemon::EnvSource {
    let mut vars = vec![(
        "CLAUDE_CONFIG_DIR".to_string(),
        dir.join("cfg").to_string_lossy().into_owned(),
    )];
    if saves {
        vars.push(("STUB_SAVES".into(), "1".into()));
    }
    std::fs::create_dir_all(dir.join("cfg/projects")).unwrap();
    overseer_daemon::EnvSource::InheritWith(vars)
}

#[tokio::test]
async fn restart_offers_resume() {
    let dir = std::env::temp_dir().join(format!("ovs-resume-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (bin, log) = stub_claude(&dir);
    let cwd = dir.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let cwd = cwd.canonicalize().unwrap();

    let d = start_daemon_with(temp_socket(), claude_env(&dir, true)).await;
    let mut c = Client::connect(&d.socket).await;
    let start = Request::StartSlot {
        id: 3,
        label: "resumer".into(),
        cwd: cwd.clone(),
        command: Some(vec![bin.to_string_lossy().into_owned()]),
        prompt: None,
    };
    assert_eq!(c.request(start).await, Response::Ok);
    let first = wait_for_log(&log, 1).await;
    let session_id = arg_after(&first[0], "--session-id").expect("fresh start has an id");
    drop(c);

    let socket = shutdown(d).await;
    let d = start_daemon_with(socket, claude_env(&dir, true)).await;
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
        prompt: None,
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
            prompt: None,
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
        prompt: None,
    };
    assert_eq!(c.request(start).await, Response::Ok);
    let lines = wait_for_log(&log, 3).await;
    assert!(lines[2].contains("--session-id"), "{}", lines[2]);
    assert!(!lines[2].contains("--resume"), "{}", lines[2]);
    assert!(matches!(c.list().await[1].state, SlotState::Running { .. }));
    // Only the running one is live in the ledger; the declined one is gone.
    assert_eq!(live_slots(&d), vec![1]);
}

/// The last argv element of a stub log line.
fn last_arg(line: &str) -> String {
    line.split_once('|')
        .unwrap()
        .1
        .rsplit(' ')
        .next()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn start_with_prompt_reaches_the_command() {
    let dir = std::env::temp_dir().join(format!("ovs-prompt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (bin, log) = stub_claude(&dir);
    let d = start_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    let start = Request::StartSlot {
        id: 2,
        label: "p".into(),
        cwd: dir.clone(),
        command: Some(vec![bin.to_string_lossy().into_owned()]),
        // One word: the stub joins argv with spaces.
        prompt: Some("hello".into()),
    };
    assert_eq!(c.request(start).await, Response::Ok);
    let lines = wait_for_log(&log, 1).await;
    assert_eq!(last_arg(&lines[0]), "hello");
}

#[tokio::test]
async fn s0_started_fresh_gets_the_kickoff_last() {
    let dir = std::env::temp_dir().join(format!("ovs-kick-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (bin, log) = stub_claude(&dir);
    let d = start_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    let start = Request::StartSlot {
        id: 0,
        label: String::new(),
        cwd: dir.clone(),
        command: Some(vec![bin.to_string_lossy().into_owned()]),
        prompt: None,
    };
    assert_eq!(c.request(start).await, Response::Ok);
    let lines = wait_for_log(&log, 1).await;
    // Check the whole tail of the kickoff, not one word.
    assert!(
        lines[0].ends_with("propose a plan with propose_plan."),
        "{}",
        lines[0]
    );
    assert!(
        lines[0].contains("Run the start-of-day round:"),
        "{}",
        lines[0]
    );
    // The coordinator role, not the worker role, was written for S0.
    let role = std::fs::read_to_string(d.state_dir.join("slots/S0/role.md")).unwrap();
    assert!(role.contains("bd ready --json"), "{role}");
    assert!(!role.contains("You are S0 of an Overseer team"), "{role}");
}

/// How many argv elements of a stub log line equal `arg`.
fn count_arg(line: &str, arg: &str) -> usize {
    line.split_once('|')
        .unwrap()
        .1
        .split(' ')
        .filter(|a| *a == arg)
        .count()
}

/// Starts `id` with `command`, stops it, then starts it again with the
/// command `list` reports, as the GUI's Restart and Start dialog do. Returns
/// the second argv line and the slot info after the second start.
async fn restart_with_listed_command(
    c: &mut Client,
    log: &std::path::Path,
    id: u8,
    command: Vec<String>,
    cwd: &std::path::Path,
) -> (String, SlotInfo) {
    let start = |command| Request::StartSlot {
        id,
        label: "again".into(),
        cwd: cwd.to_path_buf(),
        command: Some(command),
        prompt: None,
    };
    assert_eq!(c.request(start(command.clone())).await, Response::Ok);
    wait_for_log(log, 1).await;
    assert_eq!(c.list().await[id as usize].command, Some(command.clone()));
    assert_eq!(c.request(Request::StopSlot { id }).await, Response::Ok);
    c.exited_event(id).await;
    let listed = c.list().await[id as usize]
        .command
        .clone()
        .expect("exited slot keeps its command");
    assert_eq!(listed, command, "exited slot reports the base command");
    assert_eq!(c.request(start(listed)).await, Response::Ok);
    let lines = wait_for_log(log, 2).await;
    let info = c.list().await.remove(id as usize);
    (lines[1].clone(), info)
}

#[tokio::test]
async fn restart_with_listed_command_does_not_double_the_flags() {
    let dir = std::env::temp_dir().join(format!("ovs-again-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (bin, log) = stub_claude(&dir);
    let base = vec![bin.to_string_lossy().into_owned()];
    let d = start_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    let (line, info) = restart_with_listed_command(&mut c, &log, 2, base.clone(), &dir).await;
    for flag in ["--session-id", "--name", "--settings", "--mcp-config"] {
        assert_eq!(count_arg(&line, flag), 1, "{flag} in {line}");
    }
    assert_eq!(info.command, Some(base.clone()));
    let ledger = overseer_daemon::ledger::Ledger::open(&d.state_dir.join("overseer.db")).unwrap();
    let live = ledger.live_at_shutdown().unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].command, base, "ledger keeps the base command");
}

#[tokio::test]
async fn s0_restart_with_listed_command_gets_one_kickoff() {
    let dir = std::env::temp_dir().join(format!("ovs-again0-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (bin, log) = stub_claude(&dir);
    let base = vec![bin.to_string_lossy().into_owned()];
    let d = start_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    let (line, info) = restart_with_listed_command(&mut c, &log, 0, base.clone(), &dir).await;
    let kickoff = overseer_daemon::launch::kickoff_prompt(&d.state_dir).unwrap();
    assert!(line.ends_with(&format!(" -- {kickoff}")), "{line}");
    assert_eq!(count_arg(&line, "--"), 1, "{line}");
    assert_eq!(line.matches(kickoff.as_str()).count(), 1, "{line}");
    for flag in ["--session-id", "--name", "--settings", "--mcp-config"] {
        assert_eq!(count_arg(&line, flag), 1, "{flag} in {line}");
    }
    assert_eq!(info.command, Some(base.clone()));
    let ledger = overseer_daemon::ledger::Ledger::open(&d.state_dir.join("overseer.db")).unwrap();
    assert_eq!(ledger.live_at_shutdown().unwrap()[0].command, base);
}

#[tokio::test]
async fn resume_without_a_saved_conversation_starts_fresh_under_the_same_id() {
    let dir = std::env::temp_dir().join(format!("ovs-unsaved-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (bin, log) = stub_claude(&dir);
    let cwd = dir.join("work");
    std::fs::create_dir_all(&cwd).unwrap();

    // Started and never used: claude saved nothing for this session.
    let d = start_daemon_with(temp_socket(), claude_env(&dir, false)).await;
    let mut c = Client::connect(&d.socket).await;
    let start = Request::StartSlot {
        id: 1,
        label: "unused".into(),
        cwd: cwd.clone(),
        command: Some(vec![bin.to_string_lossy().into_owned()]),
        prompt: None,
    };
    assert_eq!(c.request(start).await, Response::Ok);
    let first = wait_for_log(&log, 1).await;
    let session_id = arg_after(&first[0], "--session-id").expect("fresh start has an id");
    drop(c);

    let socket = shutdown(d).await;
    let d = start_daemon_with(socket, claude_env(&dir, false)).await;
    let mut c = Client::connect(&d.socket).await;
    assert!(matches!(
        c.list().await[1].state,
        SlotState::Resumable { .. }
    ));
    assert_eq!(c.request(Request::ResumeSlot { id: 1 }).await, Response::Ok);
    let lines = wait_for_log(&log, 2).await;
    assert!(!lines[1].contains("--resume"), "{}", lines[1]);
    assert_eq!(
        arg_after(&lines[1], "--session-id").as_deref(),
        Some(session_id.as_str())
    );
    assert!(matches!(c.list().await[1].state, SlotState::Running { .. }));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- Work items ----

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

/// Stub `bd` and Jira commands in a fresh folder, plus a beads folder and a
/// 2ndBrain folder with one ticket folder. Each stub logs `cwd|argv`.
struct Sources {
    dir: PathBuf,
    beads_dir: PathBuf,
    bd_log: PathBuf,
    jira_log: PathBuf,
}

fn write_script(path: &std::path::Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// With `hang`, both stubs sleep instead of answering.
fn stub_sources(name: &str, hang: bool) -> Sources {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("ovs-work-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let bin = dir.join("bin");
    let beads_dir = dir.join("beads");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&beads_dir).unwrap();
    std::fs::create_dir_all(dir.join("brain/Sprints/sprint-1/DEMO-12-widget")).unwrap();
    std::fs::create_dir_all(dir.join("brain/Sprints/sprint-1/DEMO-400")).unwrap();
    let beads_dir = beads_dir.canonicalize().unwrap();
    let bd_log = dir.join("bd.log");
    let jira_log = dir.join("jira.log");
    let log = |l: &PathBuf| {
        format!(
            "printf '%s|%s\\n' \"$(pwd -P)\" \"$*\" >> '{}'\n",
            l.display()
        )
    };
    let (bd_body, jira_body) = if hang {
        let hang = format!(
            "echo $$ >> '{}'\nexec sleep 10\n",
            dir.join("pids").display()
        );
        (hang.clone(), hang)
    } else {
        (
            format!(
                "{}if [ \"$1\" = list ]; then cat '{FIXTURES}/bd-list.json'; fi\n",
                log(&bd_log)
            ),
            format!("{}cat '{FIXTURES}/acli-search.json'\n", log(&jira_log)),
        )
    };
    write_script(&bin.join("bd"), &bd_body);
    write_script(&bin.join("jira-stub"), &jira_body);
    Sources {
        dir,
        beads_dir,
        bd_log,
        jira_log,
    }
}

impl Sources {
    fn settings(&self) -> overseer_daemon::Config {
        overseer_daemon::Config {
            beads_dir: self.beads_dir.clone(),
            brain_dir: self.dir.join("brain"),
            jira_command: vec![
                self.dir
                    .join("bin/jira-stub")
                    .to_string_lossy()
                    .into_owned(),
                "--jql".into(),
                "{jql}".into(),
            ],
            jql: "project = DEMO".into(),
            ticket_pattern: r"DEMO-\d+".into(),
            ..overseer_daemon::Config::default()
        }
    }

    /// `bd` is found on PATH, like the real one.
    fn env(&self) -> overseer_daemon::EnvSource {
        let path = format!(
            "{}:{}",
            self.dir.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        overseer_daemon::EnvSource::InheritWith(vec![("PATH".into(), path)])
    }

    async fn daemon(&self) -> Daemon {
        start_daemon_cfg(temp_socket(), self.env(), self.settings()).await
    }
}

impl Drop for Sources {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Client {
    async fn refresh(&mut self) -> WorkView {
        match self.request(Request::RefreshWorkItems).await {
            Response::Work(v) => v,
            other => panic!("unexpected {other:?}"),
        }
    }

    async fn work(&mut self) -> WorkView {
        match self.request(Request::ListWorkItems).await {
            Response::Work(v) => v,
            other => panic!("unexpected {other:?}"),
        }
    }

    async fn assign(&mut self, key: &str, slot: u8, confirm: bool, move_owner: bool) -> Response {
        self.request(Request::Assign {
            key: key.into(),
            slot,
            confirm,
            move_owner,
        })
        .await
    }
}

fn item<'a>(v: &'a WorkView, key: &str) -> &'a WorkItem {
    v.items
        .iter()
        .find(|i| i.key == key)
        .unwrap_or_else(|| panic!("no {key} in {:?}", v.items))
}

fn source<'a>(v: &'a WorkView, name: &str) -> &'a SourceStatus {
    v.sources
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no {name} source in {:?}", v.sources))
}

#[tokio::test]
async fn work_items_load_from_stub_sources() {
    let s = stub_sources("load", false);
    let d = s.daemon().await;
    let mut c = Client::connect(&d.socket).await;
    let v = c.refresh().await;
    let keys: Vec<_> = v.items.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(
        keys,
        ["DEMO-12", "ov-1", "ov-2", "DEMO-40", "DEMO-7", "DEMO-30", "ov-4", "ov-3"]
    );
    assert_eq!(item(&v, "DEMO-12").kind, ItemKind::Ticket);
    assert_eq!(item(&v, "DEMO-12").title, "Widget wiring");
    assert_eq!(item(&v, "ov-2").parent.as_deref(), Some("DEMO-12"));
    assert!(v.items.iter().all(|i| i.in_list && i.owner.is_none()));
    // The ticket folder is found; DEMO-400 is not DEMO-40's.
    assert_eq!(
        item(&v, "DEMO-12").folder.as_deref(),
        Some(
            s.dir
                .join("brain/Sprints/sprint-1/DEMO-12-widget")
                .as_path()
        )
    );
    assert_eq!(item(&v, "DEMO-40").folder, None);
    let beads = source(&v, "beads");
    assert!(
        beads.ok && beads.count == 4 && beads.at.is_some(),
        "{beads:?}"
    );
    let jira = source(&v, "jira");
    assert!(jira.ok && jira.count == 3 && jira.at.is_some(), "{jira:?}");
    // bd runs in the beads folder; the jql reaches the Jira command.
    let bd = wait_for_log(&s.bd_log, 1).await;
    assert_eq!(
        bd[0],
        format!("{}|list --json --limit 0", s.beads_dir.display())
    );
    let jira = wait_for_log(&s.jira_log, 1).await;
    assert!(jira[0].ends_with("|--jql project = DEMO"), "{}", jira[0]);
    // The list is cached: ListWorkItems answers with the same view.
    assert_eq!(c.work().await.items, v.items);
}

#[tokio::test]
async fn assign_then_second_slot_is_refused() {
    let s = stub_sources("refuse", false);
    let d = s.daemon().await;
    let mut c = Client::connect(&d.socket).await;
    c.refresh().await;
    assert_eq!(c.assign("DEMO-12", 1, false, false).await, Response::Ok);
    // Clients hear about it.
    loop {
        if let Frame::Event(Event::WorkChanged(v)) = c.recv().await.unwrap() {
            if item(&v, "DEMO-12").owner == Some(1) {
                break;
            }
        }
    }
    assert_eq!(
        c.assign("DEMO-12", 2, false, false).await,
        Response::Error {
            message: "DEMO-12 is owned by S1".into()
        }
    );
    // Asking again for the owner is fine and changes nothing.
    assert_eq!(c.assign("DEMO-12", 1, false, false).await, Response::Ok);
    assert_eq!(item(&c.work().await, "DEMO-12").owner, Some(1));
    assert!(matches!(
        c.assign("DEMO-999", 1, false, false).await,
        Response::Error { .. }
    ));
    assert!(matches!(
        c.assign("DEMO-40", 9, false, false).await,
        Response::Error { .. }
    ));
}

#[tokio::test]
async fn move_releases_old_owner_and_adds_a_notice() {
    let s = stub_sources("move", false);
    let d = s.daemon().await;
    let mut c = Client::connect(&d.socket).await;
    c.refresh().await;
    assert_eq!(c.assign("DEMO-40", 1, false, false).await, Response::Ok);
    assert_eq!(c.assign("DEMO-40", 2, false, true).await, Response::Ok);
    let v = c.work().await;
    assert_eq!(item(&v, "DEMO-40").owner, Some(2));
    let for_s1: Vec<_> = v.notices.iter().filter(|n| n.slot == 1).collect();
    assert_eq!(for_s1.len(), 1, "{:?}", v.notices);
    assert_eq!(for_s1[0].text, "DEMO-40 was moved to S2");
    // Clearing the notices empties S1's list only.
    assert_eq!(
        c.request(Request::ClearNotices { slot: 1 }).await,
        Response::Ok
    );
    assert!(c.work().await.notices.iter().all(|n| n.slot != 1));
    // Release frees it.
    assert_eq!(
        c.request(Request::Release {
            key: "DEMO-40".into()
        })
        .await,
        Response::Ok
    );
    assert_eq!(item(&c.work().await, "DEMO-40").owner, None);
}

#[tokio::test]
async fn warn_needs_confirm_then_assigns() {
    let s = stub_sources("warn", false);
    let d = s.daemon().await;
    let mut c = Client::connect(&d.socket).await;
    c.refresh().await;
    assert_eq!(c.assign("DEMO-12", 1, false, false).await, Response::Ok);
    assert_eq!(
        c.assign("ov-1", 2, false, false).await,
        Response::NeedsConfirm {
            message: "This bead is under DEMO-12, owned by S1. Assign to S2 anyway?".into()
        }
    );
    assert_eq!(item(&c.work().await, "ov-1").owner, None);
    assert_eq!(c.assign("ov-1", 2, true, false).await, Response::Ok);
    let v = c.work().await;
    assert_eq!(item(&v, "ov-1").owner, Some(2));
    assert_eq!(item(&v, "DEMO-12").owner, Some(1));
    assert!(
        v.notices
            .iter()
            .any(|n| n.slot == 1 && n.text.contains("ov-1")),
        "{:?}",
        v.notices
    );
}

/// Waits for a line ending with `suffix` in a stub's log.
async fn wait_for_line(log: &std::path::Path, suffix: &str) -> String {
    for _ in 0..100 {
        if let Ok(s) = std::fs::read_to_string(log) {
            if let Some(l) = s.lines().find(|l| l.ends_with(suffix)) {
                return l.to_string();
            }
        }
        sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "no line ending {suffix:?} in {:?}",
        std::fs::read_to_string(log)
    );
}

#[tokio::test]
async fn bead_assign_labels_through_bd() {
    let s = stub_sources("label", false);
    let d = s.daemon().await;
    let mut c = Client::connect(&d.socket).await;
    c.refresh().await;
    assert_eq!(c.assign("ov-3", 2, false, false).await, Response::Ok);
    let line = wait_for_line(&s.bd_log, "|update ov-3 --add-label overseer:S2").await;
    assert_eq!(
        line,
        format!(
            "{}|update ov-3 --add-label overseer:S2",
            s.beads_dir.display()
        )
    );
    // A move swaps the label.
    assert_eq!(c.assign("ov-3", 4, false, true).await, Response::Ok);
    wait_for_line(
        &s.bd_log,
        "|update ov-3 --add-label overseer:S4 --remove-label overseer:S2",
    )
    .await;
    assert_eq!(
        c.request(Request::Release { key: "ov-3".into() }).await,
        Response::Ok
    );
    wait_for_line(&s.bd_log, "|update ov-3 --remove-label overseer:S4").await;
    // A ticket is never labelled.
    assert_eq!(c.assign("DEMO-40", 1, false, false).await, Response::Ok);
    sleep(Duration::from_millis(300)).await;
    let all = std::fs::read_to_string(&s.bd_log).unwrap();
    assert!(!all.contains("DEMO-40"), "{all}");
}

/// A bd stub at `bin/<name>` that logs `cwd|OVERSEER_COMMIT|args` and
/// answers `list` (its first argument after `--sandbox`).
fn configured_bd(s: &Sources, name: &str) -> String {
    let path = s.dir.join("bin").join(name);
    write_script(
        &path,
        &format!(
            "printf '%s|%s|%s\\n' \"$(pwd -P)\" \"$OVERSEER_COMMIT\" \"$*\" >> '{}'\n\
             if [ \"$2\" = list ]; then cat '{FIXTURES}/bd-list.json'; fi\n",
            s.bd_log.display()
        ),
    );
    std::fs::remove_file(s.dir.join("bin/bd")).unwrap();
    path.to_string_lossy().into_owned()
}

#[tokio::test]
async fn work_store_runs_the_configured_bd_command() {
    let s = stub_sources("bdcmd", false);
    let bd = configured_bd(&s, "bd-alt");
    let settings = overseer_daemon::Config {
        bd_command: vec![bd, "--sandbox".into()],
        ..s.settings()
    };
    let d = start_daemon_cfg(temp_socket(), s.env(), settings).await;
    let mut c = Client::connect(&d.socket).await;
    c.refresh().await;
    assert_eq!(
        item(&c.work().await, "ov-3").owner,
        None,
        "listed through bd-alt"
    );
    wait_for_line(&s.bd_log, "||--sandbox list --json --limit 0").await;
    assert_eq!(c.assign("ov-3", 2, false, false).await, Response::Ok);
    wait_for_line(&s.bd_log, "||--sandbox update ov-3 --add-label overseer:S2").await;
}

#[tokio::test]
async fn label_writes_go_through_the_brain_when_the_writer_is_on() {
    let s = stub_sources("bdbrain", false);
    let bd = configured_bd(&s, "bd-alt");
    // The beads live in the vault, as on the home Mac.
    let brain = s.beads_dir.clone();
    let settings = overseer_daemon::Config {
        brain_writer: true,
        brain_dir: brain.clone(),
        current_session: s.dir.join("home/.claude/CURRENT_SESSION.md"),
        backup_mins: 0,
        bd_command: vec![bd, "--sandbox".into()],
        ..s.settings()
    };
    let d = start_daemon_cfg(temp_socket(), s.env(), settings).await;
    let mut c = Client::connect(&d.socket).await;
    c.refresh().await;
    assert_eq!(c.assign("ov-3", 2, false, false).await, Response::Ok);
    // OVERSEER_COMMIT=1 is set only by the writer's bd runner.
    let line = wait_for_line(
        &s.bd_log,
        "|1|--sandbox update ov-3 --add-label overseer:S2",
    )
    .await;
    assert_eq!(
        line,
        format!(
            "{}|1|--sandbox update ov-3 --add-label overseer:S2",
            s.beads_dir.display()
        )
    );
}

#[tokio::test]
async fn bead_label_failure_becomes_a_notice() {
    let s = stub_sources("label-fail", false);
    write_script(
        &s.dir.join("bin/bd"),
        &format!(
            "if [ \"$1\" = list ]; then cat '{FIXTURES}/bd-list.json'; exit 0; fi\n\
             echo 'database is locked' >&2; exit 1\n"
        ),
    );
    let d = s.daemon().await;
    let mut c = Client::connect(&d.socket).await;
    c.refresh().await;
    assert_eq!(c.assign("ov-3", 2, false, false).await, Response::Ok);
    loop {
        if let Frame::Event(Event::WorkChanged(v)) = c.recv().await.unwrap() {
            if let Some(n) = v.notices.iter().find(|n| n.slot == 2) {
                assert!(
                    n.text.starts_with("could not label ov-3 in beads: ")
                        && n.text.contains("database is locked"),
                    "{n:?}"
                );
                break;
            }
        }
    }
    // The assignment stands.
    assert_eq!(item(&c.work().await, "ov-3").owner, Some(2));
}

#[tokio::test]
async fn assignments_survive_restart_before_any_refresh() {
    let s = stub_sources("restart", false);
    let socket = temp_socket();
    let d = start_daemon_cfg(socket, s.env(), s.settings()).await;
    let mut c = Client::connect(&d.socket).await;
    c.refresh().await;
    assert_eq!(c.assign("DEMO-12", 3, false, false).await, Response::Ok);
    drop(c);
    let socket = shutdown(d).await;

    // Now both sources hang, so nothing is read before the list is asked for.
    let hung = stub_sources("restart-hung", true);
    let d = start_daemon_cfg(socket, hung.env(), hung.settings()).await;
    let mut c = Client::connect(&d.socket).await;
    let v = timeout(Duration::from_secs(1), c.work())
        .await
        .expect("list answered while sources hang");
    let it = item(&v, "DEMO-12");
    assert_eq!(it.owner, Some(3));
    assert!(!it.in_list);
}

#[tokio::test]
async fn missing_jira_program_reports_status_and_beads_still_load() {
    let s = stub_sources("nojira", false);
    let mut settings = s.settings();
    settings.jira_command = vec!["no-such-jira-xyz".into(), "{jql}".into()];
    let d = start_daemon_cfg(temp_socket(), s.env(), settings).await;
    let mut c = Client::connect(&d.socket).await;
    let v = c.refresh().await;
    let jira = source(&v, "jira");
    assert!(!jira.ok, "{jira:?}");
    assert_eq!(jira.message.as_deref(), Some("no-such-jira-xyz not found"));
    let beads = source(&v, "beads");
    assert!(beads.ok && beads.count == 4, "{beads:?}");
    // Beads still group under ticket nodes made from their own keys.
    assert_eq!(item(&v, "ov-1").parent.as_deref(), Some("DEMO-12"));
    assert_eq!(item(&v, "DEMO-12").status, "unknown");
}

#[tokio::test]
async fn bad_settings_are_reported_not_fatal() {
    let s = stub_sources("badcfg", false);
    let mut settings = s.settings();
    settings.jira_command = vec![];
    settings.ticket_pattern = "[DEMO".into();
    settings.beads_dir = s.dir.join("no-such-beads");
    let d = start_daemon_cfg(temp_socket(), s.env(), settings).await;
    let mut c = Client::connect(&d.socket).await;
    let v = c.refresh().await;
    let jira = source(&v, "jira");
    assert!(!jira.ok);
    assert!(
        jira.message
            .as_deref()
            .unwrap_or("")
            .contains("jira_command"),
        "{jira:?}"
    );
    let beads = source(&v, "beads");
    assert!(!beads.ok);
    let msg = beads.message.as_deref().unwrap_or("");
    assert!(
        msg.contains("no-such-beads") && msg.contains("not found"),
        "{msg}"
    );
    assert!(msg.contains("bad ticket_pattern"), "{msg}");
    // Still serving.
    assert_eq!(c.list().await.len(), 7);

    // A bad pattern alone: beads load, ungrouped.
    let mut settings = s.settings();
    settings.ticket_pattern = "[DEMO".into();
    let d2 = start_daemon_cfg(temp_socket(), s.env(), settings).await;
    let mut c = Client::connect(&d2.socket).await;
    let v = c.refresh().await;
    assert!(source(&v, "beads").ok);
    assert!(v
        .items
        .iter()
        .filter(|i| i.kind == ItemKind::Bead)
        .all(|i| i.parent.is_none()));
    drop(d);
}

#[tokio::test]
async fn malformed_bead_list_lands_in_the_status() {
    let s = stub_sources("badbd", false);
    write_script(&s.dir.join("bin/bd"), "echo '[{\"title\":\"no id\"}]'\n");
    let d = s.daemon().await;
    let mut c = Client::connect(&d.socket).await;
    let v = c.refresh().await;
    let beads = source(&v, "beads");
    assert!(!beads.ok);
    assert!(
        beads
            .message
            .as_deref()
            .unwrap_or("")
            .contains("without an id"),
        "{beads:?}"
    );
    assert!(source(&v, "jira").ok);
}

#[tokio::test]
async fn hanging_source_does_not_block_list_slots() {
    let s = stub_sources("hang", true);
    let d = s.daemon().await;
    let mut c = Client::connect(&d.socket).await;
    // A refresh is in flight (the poll at start, and this one).
    c.seq += 1;
    let seq = c.seq;
    c.send(Frame::Request {
        seq,
        req: Request::RefreshWorkItems,
    })
    .await;
    sleep(Duration::from_millis(100)).await;
    let slots = timeout(Duration::from_secs(1), c.list())
        .await
        .expect("ListSlots answered within 1 s");
    assert_eq!(slots.len(), 7);
    timeout(Duration::from_secs(1), c.work())
        .await
        .expect("ListWorkItems answered within 1 s");
    let mut other = Client::connect(&d.socket).await;
    timeout(Duration::from_secs(1), other.list())
        .await
        .expect("another client is answered too");
}

/// Several clients race for the same item, round after round on a
/// multi-thread runtime; each round exactly one may win. A decision and its
/// write that were not under one lock would let two through now and then.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_assigns_one_wins() {
    const CLIENTS: u8 = 4;
    const ROUNDS: usize = 10;
    const KEYS: [&str; 5] = ["DEMO-12", "DEMO-40", "DEMO-7", "DEMO-30", "ov-3"];
    let s = stub_sources("race", false);
    let d = s.daemon().await;
    Client::connect(&d.socket).await.refresh().await;
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(CLIENTS as usize));
    let mut tasks = Vec::new();
    for slot in 1..=CLIENTS {
        let mut c = Client::connect(&d.socket).await;
        let barrier = barrier.clone();
        // No asserts in here: a panicking client would leave the others
        // waiting at the barrier. Everything is checked after the join.
        tasks.push(tokio::spawn(async move {
            let mut got = Vec::new();
            for _ in 0..ROUNDS {
                for key in KEYS {
                    barrier.wait().await;
                    let r = c.assign(key, slot, false, false).await;
                    barrier.wait().await;
                    // The winner frees it for the next round.
                    if r == Response::Ok {
                        c.request(Request::Release { key: key.into() }).await;
                    }
                    got.push(r);
                }
            }
            got
        }));
    }
    let mut results = Vec::new();
    for t in tasks {
        results.push(t.await.unwrap());
    }
    for round in 0..ROUNDS * KEYS.len() {
        let key = KEYS[round % KEYS.len()];
        let here: Vec<_> = results.iter().map(|r| &r[round]).collect();
        let wins = here.iter().filter(|r| ***r == Response::Ok).count();
        assert_eq!(wins, 1, "contest {round} ({key}): {here:?}");
        for r in here.iter().filter(|r| ***r != Response::Ok) {
            assert!(
                matches!(r, Response::Error { message }
                    if message.starts_with(&format!("{key} is owned by S"))),
                "{key}: {r:?}"
            );
        }
    }
}

/// True while `pid` runs (a zombie counts as gone).
fn alive(pid: &str) -> bool {
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", pid])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    let stat = stat.trim();
    !stat.is_empty() && !stat.starts_with('Z')
}

#[tokio::test]
async fn shutdown_stops_hung_sources_and_requested_refreshes() {
    let s = stub_sources("hang-stop", true);
    let pids = s.dir.join("pids");
    let d = s.daemon().await;
    let mut c = Client::connect(&d.socket).await;
    // A requested refresh queues behind the hung polls.
    c.seq += 1;
    let seq = c.seq;
    c.send(Frame::Request {
        seq,
        req: Request::RefreshWorkItems,
    })
    .await;
    wait_for_log(&pids, 2).await;
    drop(c);
    shutdown(d).await;
    // Had the refresh outlived the daemon it would start new stubs now.
    sleep(Duration::from_millis(500)).await;
    let all = std::fs::read_to_string(&pids).unwrap();
    let running: Vec<_> = all.lines().filter(|p| alive(p)).collect();
    assert!(running.is_empty(), "still running: {running:?} of {all:?}");
}

#[tokio::test]
async fn kickoff_request_returns_text() {
    let s = stub_sources("kickoff", false);
    let d = s.daemon().await;
    let mut c = Client::connect(&d.socket).await;
    c.refresh().await;
    // Nothing assigned yet.
    assert_eq!(
        c.request(Request::Kickoff { slot: 3 }).await,
        Response::Error {
            message: "S3 owns no work items".into()
        }
    );
    assert_eq!(c.assign("DEMO-12", 3, false, false).await, Response::Ok);
    let text = match c.request(Request::Kickoff { slot: 3 }).await {
        Response::Text { text } => text,
        other => panic!("unexpected {other:?}"),
    };
    assert!(text.starts_with("You are S3."), "{text}");
    assert!(
        text.contains("- DEMO-12 (") && text.contains("): Widget wiring"),
        "{text}"
    );
    assert!(text.contains("DEMO-12-widget"), "{text}");
    // The template is written for Jonathan to edit.
    assert!(d.state_dir.join("roles/assignment.md").is_file());
}

/// `bd list` stops at 50 rows unless asked for all of them.
#[tokio::test]
async fn bead_list_asks_for_every_row() {
    let s = stub_sources("limit", false);
    write_script(
        &s.dir.join("bin/bd"),
        &format!(
            "if [ \"$*\" = 'list --json --limit 0' ]; then cat '{FIXTURES}/bd-list.json'; exit 0; fi\n\
             echo \"want --limit 0, got: $*\" >&2; exit 1\n"
        ),
    );
    let d = s.daemon().await;
    let mut c = Client::connect(&d.socket).await;
    let v = c.refresh().await;
    let beads = source(&v, "beads");
    assert!(beads.ok && beads.count == 4, "{beads:?}");
}

#[tokio::test]
async fn kickoff_for_an_invalid_slot_is_refused() {
    let d = start_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    assert_eq!(
        c.request(Request::Kickoff { slot: 9 }).await,
        Response::Error {
            message: "invalid slot id 9".into()
        }
    );
}

#[tokio::test]
async fn bead_kickoff_names_its_tickets_folder() {
    let s = stub_sources("kickoff-bead", false);
    let d = s.daemon().await;
    let mut c = Client::connect(&d.socket).await;
    c.refresh().await;
    assert_eq!(c.assign("ov-1", 4, false, false).await, Response::Ok);
    let text = match c.request(Request::Kickoff { slot: 4 }).await {
        Response::Text { text } => text,
        other => panic!("unexpected {other:?}"),
    };
    let folder = s.dir.join("brain/Sprints/sprint-1/DEMO-12-widget");
    assert!(
        text.contains(&format!("Ticket folders in 2ndBrain: {}", folder.display())),
        "{text}"
    );
}

/// Moving a bead whose ticket a third slot owns asks first, like a fresh
/// assign, and tells both the ticket's owner and the old owner.
#[tokio::test]
async fn moving_a_bead_under_an_owned_ticket_needs_confirm() {
    let s = stub_sources("move-warn", false);
    let d = s.daemon().await;
    let mut c = Client::connect(&d.socket).await;
    c.refresh().await;
    assert_eq!(c.assign("DEMO-12", 1, false, false).await, Response::Ok);
    assert_eq!(c.assign("ov-1", 2, true, false).await, Response::Ok);
    assert_eq!(
        c.assign("ov-1", 3, false, true).await,
        Response::NeedsConfirm {
            message: "This bead is under DEMO-12, owned by S1. Assign to S3 anyway?".into()
        }
    );
    assert_eq!(item(&c.work().await, "ov-1").owner, Some(2));
    assert_eq!(c.assign("ov-1", 3, true, true).await, Response::Ok);
    let v = c.work().await;
    assert_eq!(item(&v, "ov-1").owner, Some(3));
    let texts: Vec<_> = v
        .notices
        .iter()
        .map(|n| (n.slot, n.text.as_str()))
        .collect();
    assert!(
        texts.contains(&(1, "S3 took ov-1, which overlaps your work")),
        "{texts:?}"
    );
    assert!(texts.contains(&(2, "ov-1 was moved to S3")), "{texts:?}");
    wait_for_line(
        &s.bd_log,
        "|update ov-1 --add-label overseer:S3 --remove-label overseer:S2",
    )
    .await;
}

/// A release right before a shutdown still reaches `bd`, even when `bd` is
/// slow, as long as it finishes within the drain time.
#[tokio::test]
async fn shutdown_waits_for_queued_labels() {
    let s = stub_sources("drain", false);
    write_script(
        &s.dir.join("bin/bd"),
        &format!(
            "if [ \"$1\" = list ]; then cat '{FIXTURES}/bd-list.json'; exit 0; fi\n\
             sleep 1\nprintf '%s\\n' \"$*\" >> '{}'\n",
            s.bd_log.display()
        ),
    );
    let d = s.daemon().await;
    let mut c = Client::connect(&d.socket).await;
    c.refresh().await;
    assert_eq!(c.assign("ov-3", 2, false, false).await, Response::Ok);
    assert_eq!(
        c.request(Request::Release { key: "ov-3".into() }).await,
        Response::Ok
    );
    drop(c);
    shutdown(d).await;
    let log = std::fs::read_to_string(&s.bd_log).unwrap_or_default();
    assert_eq!(
        log.lines().collect::<Vec<_>>(),
        [
            "update ov-3 --add-label overseer:S2",
            "update ov-3 --remove-label overseer:S2"
        ],
        "{log}"
    );
}

/// A `bd` that hangs holds shutdown up for the drain time only.
#[tokio::test]
async fn shutdown_gives_up_on_a_hung_label() {
    let s = stub_sources("drain-hung", false);
    write_script(
        &s.dir.join("bin/bd"),
        &format!(
            "if [ \"$1\" = list ]; then cat '{FIXTURES}/bd-list.json'; exit 0; fi\n\
             echo $$ >> '{}'\nexec sleep 30\n",
            s.dir.join("pids").display()
        ),
    );
    let d = s.daemon().await;
    let mut c = Client::connect(&d.socket).await;
    c.refresh().await;
    assert_eq!(c.assign("ov-3", 2, false, false).await, Response::Ok);
    wait_for_log(&s.dir.join("pids"), 1).await;
    drop(c);
    let started = std::time::Instant::now();
    shutdown(d).await;
    let took = started.elapsed();
    assert!(took < Duration::from_secs(6), "shutdown took {took:?}");
    let pids = std::fs::read_to_string(s.dir.join("pids")).unwrap();
    let running: Vec<_> = pids.lines().filter(|p| alive(p)).collect();
    assert!(running.is_empty(), "still running: {running:?}");
}

// ---- Locks ----

/// The test patterns (plus the defaults): writes that need no lookups.
const LOCK_TARGETS: &str = r#"
[[pattern]]
name = "demo deploy"
command = '^demo-deploy\s+(?P<app>\S+)'
target = "deploy:demo/{app}"

[[pattern]]
name = "demo push"
command = '^demo-push\b'
target = "repo:demo-api:main"
"#;

fn lock_patterns() -> Vec<Pattern> {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("ovs-targets-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("targets.toml");
    std::fs::write(&path, LOCK_TARGETS).unwrap();
    let (patterns, err) = load_patterns(&path);
    assert!(err.is_none(), "{err:?}");
    let _ = std::fs::remove_dir_all(&dir);
    patterns
}

async fn lock_daemon() -> Daemon {
    lock_daemon_at(temp_socket()).await
}

async fn lock_daemon_at(socket: PathBuf) -> Daemon {
    start_daemon_full(
        socket,
        overseer_daemon::EnvSource::Inherit,
        offline_settings(),
        lock_patterns(),
    )
    .await
}

fn bash(slot: u8, tool_use_id: &str, command: &str) -> Request {
    Request::CheckBash(BashCheck {
        slot,
        session_id: None,
        tool_use_id: Some(tool_use_id.into()),
        command: command.into(),
        cwd: Some(PathBuf::from("/tmp")),
        env: vec![("ARGOCD_SERVER".into(), "uat".into())],
    })
}

fn post_tool_use(slot: u8, tool_use_id: &str) -> HookEvent {
    HookEvent {
        tool: Some("Bash".into()),
        tool_use_id: Some(tool_use_id.into()),
        ..hook(slot, HookKind::PostToolUse)
    }
}

fn claim_target(slot: u8, target: &str, item: Option<&str>) -> Request {
    Request::ClaimTarget {
        slot,
        target: target.into(),
        reason: None,
        item: item.map(String::from),
    }
}

/// `(target, slot, kind)` of every claim, by target.
fn holds(v: &LockView) -> Vec<(String, u8, ClaimKind)> {
    v.claims
        .iter()
        .map(|c| (c.target.clone(), c.slot, c.kind))
        .collect()
}

/// `(target, slot)` of every wait, in queue order.
fn queued(v: &LockView) -> Vec<(String, u8)> {
    v.waits.iter().map(|w| (w.target.clone(), w.slot)).collect()
}

fn pending(v: &LockView) -> Vec<(u8, String)> {
    v.relays
        .iter()
        .filter(|r| r.state == RelayState::Pending)
        .map(|r| (r.to, r.text.clone()))
        .collect()
}

fn t(s: &str) -> String {
    s.to_string()
}

impl Client {
    /// The deny reason, if any.
    async fn check(&mut self, slot: u8, tool_use_id: &str, command: &str) -> Option<String> {
        match self.request(bash(slot, tool_use_id, command)).await {
            Response::Verdict { deny, .. } => deny,
            other => panic!("unexpected {other:?}"),
        }
    }

    async fn locks(&mut self) -> LockView {
        match self.request(Request::ListLocks).await {
            Response::Locks(v) => v,
            other => panic!("unexpected {other:?}"),
        }
    }

    /// Polls the lock view until `done` holds.
    async fn locks_until(&mut self, done: impl Fn(&LockView) -> bool) -> LockView {
        for _ in 0..250 {
            let v = self.locks().await;
            if done(&v) {
                return v;
            }
            sleep(Duration::from_millis(20)).await;
        }
        panic!("locks never matched: {:?}", self.locks().await);
    }
}

#[tokio::test]
async fn check_bash_claims_a_free_target_and_post_tool_use_releases_it() {
    let d = lock_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    assert_eq!(c.check(1, "tu-1", "demo-deploy api").await, None);
    let v = c.locks().await;
    assert_eq!(holds(&v), [(t("deploy:demo/api"), 1, ClaimKind::Command)]);
    // Clients hear about it.
    loop {
        if let Frame::Event(Event::LocksChanged(v)) = c.recv().await.unwrap() {
            if !v.claims.is_empty() {
                break;
            }
        }
    }
    // Another command's PostToolUse leaves it; its own releases it.
    c.hook(post_tool_use(1, "tu-other")).await;
    assert_eq!(c.locks().await.claims.len(), 1);
    c.hook(post_tool_use(1, "tu-1")).await;
    assert!(c.locks().await.claims.is_empty());
}

#[tokio::test]
async fn check_bash_denies_a_held_target_and_queues() {
    let d = lock_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    let target = "repo:demo-api:main";
    assert_eq!(
        c.request(claim_target(1, target, Some("DEMO-33"))).await,
        Response::Text {
            text: t("You hold repo:demo-api:main.")
        }
    );
    assert_eq!(
        c.request(claim_target(1, target, None)).await,
        Response::Text {
            text: t("You already hold repo:demo-api:main.")
        }
    );
    let since = c.locks().await.claims[0].since;
    let at = overseer_daemon::locks::local_hhmm(since);
    let deny = c.check(2, "tu-1", "demo-push").await;
    assert_eq!(
        deny,
        Some(format!("Overseer: repo:demo-api:main is held by S1 since {at} for DEMO-33. You are in its queue and Overseer will tell you here when you hold it. Do not run this again until then."))
    );
    let v = c.locks().await;
    assert_eq!(queued(&v), [(t(target), 2)]);
    assert_eq!(holds(&v), [(t(target), 1, ClaimKind::Claim)]);
    // A claim behind it is told its place.
    assert_eq!(
        c.request(claim_target(3, target, None)).await,
        Response::Queued {
            message: format!("repo:demo-api:main is held by S1 since {at} for DEMO-33. You are number 2 in the queue; Overseer will tell you here when you hold it.")
        }
    );
    // Bad requests are refused.
    assert!(matches!(
        c.request(claim_target(1, "nonsense", None)).await,
        Response::Error { .. }
    ));
    assert!(matches!(
        c.request(claim_target(9, target, None)).await,
        Response::Error { .. }
    ));
}

#[tokio::test]
async fn compound_command_with_one_held_target_claims_nothing() {
    let d = lock_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    assert!(matches!(
        c.request(claim_target(1, "deploy:demo/a", None)).await,
        Response::Text { .. }
    ));
    let deny = c
        .check(2, "tu-1", "demo-deploy b && demo-deploy a")
        .await
        .expect("denied");
    assert!(deny.contains("deploy:demo/a is held by S1"), "{deny}");
    let v = c.locks().await;
    assert_eq!(holds(&v), [(t("deploy:demo/a"), 1, ClaimKind::Claim)]);
    assert_eq!(queued(&v), [(t("deploy:demo/a"), 2)]);
}

#[tokio::test]
async fn unmatched_command_is_allowed_without_records() {
    let d = lock_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    for cmd in [
        "git status; ls -la",
        "argocd app get demo-api",
        "git push --dry-run",
    ] {
        assert_eq!(c.check(1, "tu-1", cmd).await, None, "{cmd}");
    }
    let v = c.locks().await;
    assert!(v.claims.is_empty() && v.waits.is_empty() && v.relays.is_empty());
}

#[tokio::test]
async fn release_grants_the_waiter_and_queues_a_relay() {
    let d = lock_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    assert_eq!(c.check(1, "tu-1", "demo-deploy api").await, None);
    assert!(c.check(2, "tu-2", "demo-deploy api").await.is_some());
    c.hook(post_tool_use(1, "tu-1")).await;
    let v = c.locks().await;
    assert_eq!(holds(&v), [(t("deploy:demo/api"), 2, ClaimKind::Grant)]);
    assert!(v.waits.is_empty());
    assert_eq!(
        pending(&v),
        [(2, t("S1 has released deploy:demo/api. You now hold it."))]
    );
    // The grant is used by S2's next matching command, then released.
    assert_eq!(c.check(2, "tu-3", "demo-deploy api").await, None);
    let v = c.locks().await;
    assert_eq!(holds(&v), [(t("deploy:demo/api"), 2, ClaimKind::Command)]);
    c.hook(post_tool_use(2, "tu-3")).await;
    assert!(c.locks().await.claims.is_empty());

    // An explicit release hands on too.
    c.request(claim_target(1, "custom:z", None)).await;
    c.request(claim_target(3, "custom:z", None)).await;
    assert_eq!(
        c.request(Request::ReleaseTarget {
            slot: 1,
            target: t("custom:z")
        })
        .await,
        Response::Text {
            text: t("Released custom:z. S3 holds it now.")
        }
    );
    assert_eq!(
        holds(&c.locks().await),
        [(t("custom:z"), 3, ClaimKind::Claim)]
    );
    // Jonathan was shown S1 holding it; S3 holds it now, so nothing goes.
    assert_eq!(
        c.request(Request::ForceRelease {
            target: t("custom:z"),
            holder: Some(1),
        })
        .await,
        Response::Error {
            message: t("custom:z is now held by S3; nothing released.")
        }
    );
    assert_eq!(
        holds(&c.locks().await),
        [(t("custom:z"), 3, ClaimKind::Claim)]
    );
    assert_eq!(
        c.request(Request::ForceRelease {
            target: t("custom:z"),
            holder: Some(3),
        })
        .await,
        Response::Text {
            text: t("Released custom:z.")
        }
    );
    assert_eq!(
        c.request(Request::ForceRelease {
            target: t("custom:z"),
            holder: None,
        })
        .await,
        Response::Error {
            message: t("custom:z is not held")
        }
    );
}

#[tokio::test]
async fn pin_survives_post_tool_use_and_unpin_releases_it() {
    let d = lock_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    let app = "argo:uat/demo-api";
    let pin = "argocd app set demo-api --revision abc123";
    assert_eq!(c.check(1, "tu-1", pin).await, None);
    c.hook(post_tool_use(1, "tu-1")).await;
    let v = c.locks().await;
    assert_eq!(holds(&v), [(t(app), 1, ClaimKind::Pin)]);
    assert_eq!(v.claims[0].pin_revision.as_deref(), Some("abc123"));
    // A sync by its holder keeps the pin.
    assert_eq!(c.check(1, "tu-2", "argocd app sync demo-api").await, None);
    c.hook(post_tool_use(1, "tu-2")).await;
    assert_eq!(holds(&c.locks().await), [(t(app), 1, ClaimKind::Pin)]);
    // Anyone else waits.
    assert!(c
        .check(2, "tu-3", "argocd app sync demo-api")
        .await
        .is_some());
    // Setting it back to main ends the pin once that command is done.
    let unpin = "argocd app set demo-api --revision main";
    assert_eq!(c.check(1, "tu-4", unpin).await, None);
    assert_eq!(holds(&c.locks().await), [(t(app), 1, ClaimKind::Command)]);
    c.hook(post_tool_use(1, "tu-4")).await;
    let v = c.locks().await;
    assert_eq!(holds(&v), [(t(app), 2, ClaimKind::Grant)]);
    assert_eq!(
        pending(&v),
        [(
            2,
            t("S1 has released argo:uat/demo-api (back on main). You now hold it.")
        )]
    );
}

#[tokio::test]
async fn granted_pin_survives_post_tool_use_and_stop() {
    let d = lock_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    let app = "argo:uat/demo-api";
    let unpin = "argocd app set demo-api --revision main";
    assert_eq!(
        c.check(1, "tu-1", "argocd app set demo-api --revision abc123")
            .await,
        None
    );
    c.hook(post_tool_use(1, "tu-1")).await;
    let pin = "argocd app set demo-api --revision def456";
    assert!(c.check(2, "tu-2", pin).await.is_some());
    assert_eq!(c.check(1, "tu-3", unpin).await, None);
    c.hook(post_tool_use(1, "tu-3")).await;
    assert_eq!(holds(&c.locks().await), [(t(app), 2, ClaimKind::Grant)]);

    // The granted slot reruns its pin: the grant becomes that pin.
    assert_eq!(c.check(2, "tu-4", pin).await, None);
    c.hook(post_tool_use(2, "tu-4")).await;
    c.hook(hook(2, HookKind::Stop)).await;
    let v = c.locks().await;
    assert_eq!(holds(&v), [(t(app), 2, ClaimKind::Pin)]);
    assert_eq!(v.claims[0].pin_revision.as_deref(), Some("def456"));

    // An unpin ends it and hands it on.
    assert!(c
        .check(3, "tu-5", "argocd app sync demo-api")
        .await
        .is_some());
    assert_eq!(c.check(2, "tu-6", unpin).await, None);
    c.hook(post_tool_use(2, "tu-6")).await;
    let v = c.locks().await;
    assert_eq!(holds(&v), [(t(app), 3, ClaimKind::Grant)]);
    assert!(pending(&v).contains(&(
        3,
        t("S2 has released argo:uat/demo-api (back on main). You now hold it.")
    )));
}

#[tokio::test]
async fn held_claim_that_pins_records_the_revision_and_stays_a_claim() {
    let d = lock_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    let app = "argo:uat/demo-api";
    c.request(claim_target(1, app, None)).await;
    let pin = "argocd app set demo-api --revision abc123";
    assert_eq!(c.check(1, "tu-1", pin).await, None);
    c.hook(post_tool_use(1, "tu-1")).await;
    c.hook(hook(1, HookKind::Stop)).await;
    let v = c.locks().await;
    assert_eq!(holds(&v), [(t(app), 1, ClaimKind::Claim)]);
    assert_eq!(v.claims[0].pin_revision.as_deref(), Some("abc123"));
}

#[tokio::test]
async fn stop_releases_command_claims() {
    let d = lock_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    assert_eq!(c.check(1, "tu-1", "demo-deploy api").await, None);
    c.request(claim_target(1, "custom:kept", None)).await;
    c.hook(hook(1, HookKind::Stop)).await;
    assert_eq!(
        holds(&c.locks().await),
        [(t("custom:kept"), 1, ClaimKind::Claim)]
    );
}

#[tokio::test]
async fn holder_exit_hands_on_and_waiter_exit_leaves_the_queue() {
    let d = lock_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    for id in [1, 2, 3] {
        assert_eq!(c.start(id).await, Response::Ok);
    }
    let (cmd, app) = ("deploy:demo/api", "argo:uat/demo-api");
    assert_eq!(c.check(1, "tu-1", "demo-deploy api").await, None);
    let pin = "argocd app set demo-api --revision abc123";
    assert_eq!(c.check(1, "tu-2", pin).await, None);
    c.hook(post_tool_use(1, "tu-2")).await;
    let both = "demo-deploy api && argocd app sync demo-api";
    assert!(c.check(2, "tu-3", both).await.is_some());
    assert!(c.check(3, "tu-4", "demo-deploy api").await.is_some());
    assert_eq!(
        queued(&c.locks().await),
        [(t(cmd), 2), (t(app), 2), (t(cmd), 3)]
    );

    assert_eq!(c.request(Request::StopSlot { id: 3 }).await, Response::Ok);
    let v = c.locks_until(|v| v.waiting_on(3).is_empty()).await;
    assert_eq!(queued(&v), [(t(cmd), 2), (t(app), 2)]);
    assert_eq!(
        holds(&v),
        [(t(app), 1, ClaimKind::Pin), (t(cmd), 1, ClaimKind::Command)]
    );

    assert_eq!(c.request(Request::StopSlot { id: 1 }).await, Response::Ok);
    let v = c
        .locks_until(|v| v.claims.iter().any(|c| c.slot == 2))
        .await;
    assert_eq!(
        holds(&v),
        [(t(app), 1, ClaimKind::Pin), (t(cmd), 2, ClaimKind::Grant)]
    );
    assert_eq!(queued(&v), [(t(app), 2)]);
    assert!(v.waiting_on(3).is_empty());
    assert_eq!(
        pending(&v),
        [(2, t("S1 has released deploy:demo/api. You now hold it."))]
    );
}

#[tokio::test]
async fn releasing_a_work_item_releases_its_claims() {
    let s = stub_sources("lockitem", false);
    let d = start_daemon_full(temp_socket(), s.env(), s.settings(), lock_patterns()).await;
    let mut c = Client::connect(&d.socket).await;
    c.refresh().await;
    assert_eq!(c.assign("DEMO-12", 1, false, false).await, Response::Ok);
    // S1 owns one item, so its claims are for it.
    c.request(claim_target(1, "custom:x", None)).await;
    assert_eq!(c.check(1, "tu-1", "demo-deploy api").await, None);
    c.request(claim_target(2, "custom:x", None)).await;
    let v = c.locks().await;
    assert!(
        v.claims
            .iter()
            .all(|c| c.item.as_deref() == Some("DEMO-12")),
        "{v:?}"
    );
    assert_eq!(
        c.request(Request::Release { key: t("DEMO-12") }).await,
        Response::Ok
    );
    let v = c.locks().await;
    assert_eq!(holds(&v), [(t("custom:x"), 2, ClaimKind::Claim)]);
    assert_eq!(
        pending(&v),
        [(2, t("S1 has released custom:x. You now hold it."))]
    );

    // A move takes the item's claims away from the old owner.
    assert_eq!(c.assign("DEMO-40", 3, false, false).await, Response::Ok);
    c.request(claim_target(3, "custom:y", Some("DEMO-40")))
        .await;
    assert_eq!(c.assign("DEMO-40", 4, false, true).await, Response::Ok);
    let v = c.locks().await;
    assert!(v.claims.iter().all(|c| c.target != "custom:y"), "{v:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_claims_one_wins() {
    let d = lock_daemon().await;
    let mut a = Client::connect(&d.socket).await;
    let mut b = Client::connect(&d.socket).await;
    let (ra, rb) = tokio::join!(
        a.request(claim_target(1, "custom:race", None)),
        b.request(claim_target(2, "custom:race", None)),
    );
    let mut got = [ra, rb];
    got.sort_by_key(|r| matches!(r, Response::Queued { .. }));
    assert!(matches!(&got[0], Response::Text { text } if text == "You hold custom:race."));
    assert!(matches!(&got[1], Response::Queued { .. }), "{got:?}");
    let v = a.locks().await;
    assert_eq!(v.claims.len(), 1);
    assert_eq!(v.waits.len(), 1);
    assert_ne!(v.claims[0].slot, v.waits[0].slot);
}

#[tokio::test]
async fn locks_survive_restart() {
    let stub = box_stub("locks-restart");
    let socket = temp_socket();
    let start = |socket| start_daemon_full(socket, stub.env(), offline_settings(), lock_patterns());
    let d = start(socket).await;
    let mut c = Client::connect(&d.socket).await;
    // The shutdown kills S1's session; that exit must not drop its claim,
    // its wait or its pending relay, since S1 can be resumed.
    c.start_stub(1, &stub).await;
    c.hook(hook(1, HookKind::UserPromptSubmit)).await;
    assert_eq!(c.check(1, "tu-1", "demo-deploy api").await, None);
    c.request(claim_target(2, "custom:a", Some("DEMO-12")))
        .await;
    c.request(claim_target(1, "custom:a", None)).await;
    c.request(claim_target(1, "custom:b", None)).await;
    c.request(claim_target(4, "custom:b", None)).await;
    c.request(Request::ReleaseTarget {
        slot: 1,
        target: t("custom:b"),
    })
    .await;
    assert_eq!(c.relay(None, 1, "hello").await, queued_for(1));
    let before = c.locks().await;
    assert_eq!(before.claims.len(), 3);
    assert_eq!(before.waits.len(), 1);
    assert_eq!(pending(&before).len(), 2);
    drop(c);
    let socket = shutdown(d).await;
    let d = start(socket).await;
    let mut c = Client::connect(&d.socket).await;
    assert!(matches!(
        c.list().await[1].state,
        SlotState::Resumable { .. }
    ));
    let after = c.locks().await;
    assert_eq!(after.claims, before.claims);
    assert_eq!(after.waits, before.waits);
    // S4 never ran, so the handoff relay to it is dropped at the restart.
    assert_eq!(
        relay_states(&after),
        [
            (1, t("hello"), RelayState::Pending),
            (
                4,
                t("S1 has released custom:b. You now hold it."),
                RelayState::Dropped
            ),
        ]
    );
}

#[tokio::test]
async fn slow_resolution_allows_and_records_nothing() {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("ovs-slowgit-{}-{n}", std::process::id()));
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let ran = dir.join("ran");
    write_script(
        &bin.join("git"),
        &format!(": > '{}'\nexec sleep 5\n", ran.display()),
    );
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let env = overseer_daemon::EnvSource::InheritWith(vec![("PATH".into(), path)]);
    let d = start_daemon_full(temp_socket(), env, offline_settings(), lock_patterns()).await;
    let mut c = Client::connect(&d.socket).await;
    let start = std::time::Instant::now();
    assert_eq!(c.check(1, "tu-1", "git push").await, None);
    let took = start.elapsed();
    assert!(took < Duration::from_secs(1), "took {took:?}");
    assert!(ran.exists(), "the stub git was never asked");
    let v = c.locks().await;
    assert!(v.claims.is_empty() && v.waits.is_empty(), "{v:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

impl Client {
    async fn relay(&mut self, from: Option<u8>, to: u8, text: &str) -> Response {
        self.request(Request::SendRelay {
            from,
            to,
            text: text.into(),
        })
        .await
    }
}

fn queued_for(slot: u8) -> Response {
    Response::Text {
        text: format!("Queued for S{slot}. It will be typed in when S{slot} is idle."),
    }
}

/// Marks every pending relay to `to` delivered, as the courier will.
fn deliver_all(d: &Daemon, to: u8, v: &LockView) {
    let ledger = overseer_daemon::ledger::Ledger::open(&d.state_dir.join("overseer.db")).unwrap();
    let ids: Vec<i64> = v
        .relays
        .iter()
        .filter(|r| r.to == to && r.state == RelayState::Pending)
        .map(|r| r.id)
        .collect();
    ledger.mark_relays_delivered(&ids, 100).unwrap();
}

fn is_error(r: &Response, text: &str) -> bool {
    matches!(r, Response::Error { message } if message == text)
}

#[tokio::test]
async fn jonathan_and_s0_can_send_to_anyone() {
    let d = lock_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    for id in [2, 3] {
        assert_eq!(c.start(id).await, Response::Ok);
    }
    assert_eq!(c.relay(None, 3, "hello").await, queued_for(3));
    assert_eq!(c.relay(Some(0), 2, "status?").await, queued_for(2));
    let v = c.locks().await;
    let mine: Vec<_> = v
        .relays
        .iter()
        .map(|r| (r.from, r.to, r.text.as_str()))
        .collect();
    assert!(mine.contains(&(Sender::You, 3, "hello")), "{mine:?}");
    assert!(
        mine.contains(&(Sender::Slot { slot: 0 }, 2, "status?")),
        "{mine:?}"
    );
    assert!(is_error(
        &c.relay(None, 3, "  ").await,
        "the message is empty"
    ));
    assert!(matches!(
        c.relay(None, 3, &"x".repeat(4001)).await,
        Response::Error { .. }
    ));
    assert!(matches!(
        c.relay(None, 99, "hi").await,
        Response::Error { .. }
    ));
}

#[tokio::test]
async fn worker_can_answer_s0_once() {
    let d = lock_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    for id in [0, 2] {
        assert_eq!(c.start(id).await, Response::Ok);
    }
    assert_eq!(c.relay(Some(0), 2, "status?").await, queued_for(2));
    // Not delivered yet, so there is nothing to answer.
    assert!(matches!(
        c.relay(Some(2), 0, "early").await,
        Response::Error { .. }
    ));
    let v = c.locks().await;
    deliver_all(&d, 2, &v);
    assert_eq!(c.relay(Some(2), 0, "on it").await, queued_for(0));
    assert!(is_error(
        &c.relay(Some(2), 0, "and another").await,
        "S2 can message S0 only to answer a message from S0."
    ));
}

#[tokio::test]
async fn worker_cannot_start_a_conversation() {
    let d = lock_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    for id in [0, 1, 2] {
        assert_eq!(c.start(id).await, Response::Ok);
    }
    assert!(is_error(
        &c.relay(Some(2), 1, "hi").await,
        "S2 can message S1 only to answer a message from S1."
    ));
    assert!(is_error(
        &c.relay(Some(2), 2, "hi").await,
        "you cannot message yourself"
    ));
    // A relay from Jonathan is not a message from S0 to answer.
    assert_eq!(c.relay(None, 2, "hello").await, queued_for(2));
    let v = c.locks().await;
    deliver_all(&d, 2, &v);
    assert!(matches!(
        c.relay(Some(2), 0, "hi").await,
        Response::Error { .. }
    ));
    assert!(c
        .locks()
        .await
        .relays
        .iter()
        .all(|r| r.from != Sender::Slot { slot: 2 }));
}

#[tokio::test]
async fn move_notice_queues_a_relay() {
    let s = stub_sources("move-relay", false);
    let d = s.daemon().await;
    let mut c = Client::connect(&d.socket).await;
    c.refresh().await;
    assert_eq!(c.start(1).await, Response::Ok);
    assert_eq!(c.assign("DEMO-40", 1, false, false).await, Response::Ok);
    assert_eq!(c.assign("DEMO-40", 2, false, true).await, Response::Ok);
    let v = c.locks_until(|v| !pending(v).is_empty()).await;
    assert_eq!(
        pending(&v),
        vec![(1, "DEMO-40 was moved to S2".to_string())]
    );
    assert_eq!(v.relays[0].from, Sender::Overseer);
    // S2 has no session, so its notice is dropped, never delivered later.
    assert_eq!(c.assign("DEMO-40", 3, false, true).await, Response::Ok);
    let v = c.locks_until(|v| v.relays.len() == 2).await;
    assert_eq!(
        relay_states(&v)[0],
        (2, t("DEMO-40 was moved to S3"), RelayState::Dropped)
    );
}

#[tokio::test]
async fn label_failure_queues_no_relay() {
    let s = stub_sources("label-fail-relay", false);
    write_script(
        &s.dir.join("bin/bd"),
        &format!(
            "if [ \"$1\" = list ]; then cat '{FIXTURES}/bd-list.json'; exit 0; fi\n\
             echo 'database is locked' >&2; exit 1\n"
        ),
    );
    let d = s.daemon().await;
    let mut c = Client::connect(&d.socket).await;
    c.refresh().await;
    assert_eq!(c.assign("ov-3", 2, false, false).await, Response::Ok);
    for _ in 0..250 {
        if c.work().await.notices.iter().any(|n| n.slot == 2) {
            break;
        }
        sleep(Duration::from_millis(20)).await;
    }
    assert!(c.work().await.notices.iter().any(|n| n.slot == 2));
    assert!(c.locks().await.relays.is_empty());
}

// ---- Courier ----

/// A stub `claude` for the courier: it draws Claude Code's input box (empty,
/// or holding the text in `box_file`), redraws on SIGUSR1, and logs every
/// byte it reads from the PTY to `reads`.
struct BoxStub {
    dir: PathBuf,
    bin: PathBuf,
    reads: PathBuf,
    box_file: PathBuf,
    draws: PathBuf,
}

fn box_stub(name: &str) -> BoxStub {
    box_stub_with(name, true)
}

/// `box_stub`, turning bracketed paste on only when `brackets` is set.
fn box_stub_with(name: &str, brackets: bool) -> BoxStub {
    let dir = std::env::temp_dir().join(format!("ovs-courier-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("work")).unwrap();
    let stub = BoxStub {
        bin: dir.join("claude"),
        reads: dir.join("reads.log"),
        box_file: dir.join("box.txt"),
        draws: dir.join("draws.log"),
        dir,
    };
    write_script(
        &stub.bin,
        &format!(
            r#"if [ "$1" = --session-id ] && [ -n "$STUB_SAVES" ]; then
  mkdir -p "$CLAUDE_CONFIG_DIR/projects/p" && : > "$CLAUDE_CONFIG_DIR/projects/p/$2.jsonl"
fi
stty raw -echo
exec 3<&0
cat <&3 >> '{reads}' &
reader=$!
draw() {{
  cols=$(stty size | cut -d' ' -f2)
  rule=$(awk -v n="$cols" 'BEGIN {{ for (i = 0; i < n; i++) printf "\342\224\200" }}')
  b=$(cat '{boxf}' 2>/dev/null)
  if [ "$b" = @dialog ]; then
    printf '\033[2J\033[H{mode}Bash command\r\n  kubectl apply -f demo-api.yaml\r\nDo you want to proceed?\r\n\342\235\257 1. Yes\r\n  2. No\r\n'
    echo drawn >> '{draws}'
    return
  fi
  if [ "$b" = @working ]; then
    printf '\033[2J\033[H{mode}\342\234\273 Churning\342\200\246 (4s \302\267 \342\206\223 323 tokens)\r\n\r\n%s\r\n' "$rule"
    printf '\342\235\257 \033[7mT\033[0m\033[2mry "fix the failing test"\033[0m\r\n'
    printf '%s\r\n  ? for shortcuts' "$rule"
    echo drawn >> '{draws}'
    return
  fi
  printf '\033[2J\033[H{mode}Working on DEMO-12 in demo-api\r\n\r\n%s\r\n' "$rule"
  if [ -s '{boxf}' ]; then
    printf '\342\235\257 %s\r\n' "$(cat '{boxf}')"
  else
    printf '\342\235\257 \033[7mT\033[0m\033[2mry "fix the failing test"\033[0m\r\n'
  fi
  printf '%s\r\n  ? for shortcuts' "$rule"
  echo drawn >> '{draws}'
}}
trap draw USR1
draw
i=0
while [ $i -lt 600 ]; do sleep 0.1; i=$((i + 1)); done
kill $reader
"#,
            reads = stub.reads.display(),
            boxf = stub.box_file.display(),
            draws = stub.draws.display(),
            mode = if brackets { "\\033[?2004h" } else { "" },
        ),
    );
    stub
}

impl BoxStub {
    fn env(&self) -> overseer_daemon::EnvSource {
        claude_env(&self.dir, true)
    }

    fn reads(&self) -> String {
        String::from_utf8_lossy(&std::fs::read(&self.reads).unwrap_or_default()).into_owned()
    }

    fn draws(&self) -> usize {
        std::fs::read_to_string(&self.draws)
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    async fn drawn(&self, at_least: usize) {
        for _ in 0..250 {
            if self.draws() >= at_least {
                return;
            }
            sleep(Duration::from_millis(20)).await;
        }
        panic!("stub never drew {at_least} times");
    }

    /// Waits for the PTY to have been sent `needle`; returns all it read.
    async fn read_until(&self, needle: &str) -> String {
        for _ in 0..250 {
            let got = self.reads();
            if got.contains(needle) {
                return got;
            }
            sleep(Duration::from_millis(20)).await;
        }
        panic!("stub never read {needle:?}; read {:?}", self.reads());
    }

    /// Asserts nothing reaches the PTY for `secs`.
    async fn reads_nothing_for(&self, secs: u64) {
        sleep(Duration::from_secs(secs)).await;
        assert_eq!(self.reads(), "", "typed into the session");
    }

    /// Sets the input box (`None` is empty) and has the stub redraw it.
    /// `@dialog` draws a permission dialog in its place, `@working` an empty
    /// box under Claude Code's working line.
    async fn set_box(&self, text: Option<&str>, pid: u32) {
        std::fs::write(&self.box_file, text.unwrap_or("")).unwrap();
        let before = self.draws();
        // SAFETY: a plain signal to the stub's own process.
        assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGUSR1) }, 0);
        self.drawn(before + 1).await;
    }
}

impl Drop for BoxStub {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The paste a relay of `text` arrives as, Return included.
fn pasted(text: &str) -> String {
    format!("\x1b[200~{}\x1b[201~\r", text.replace('\n', "\r"))
}

impl Client {
    /// Starts `stub` in slot `id`, waits for its first screen and returns
    /// its pid.
    async fn start_stub(&mut self, id: u8, stub: &BoxStub) -> u32 {
        let start = Request::StartSlot {
            id,
            label: format!("t{id}"),
            cwd: stub.dir.join("work"),
            command: Some(vec![stub.bin.to_string_lossy().into_owned()]),
            prompt: None,
        };
        assert_eq!(self.request(start).await, Response::Ok);
        stub.drawn(1).await;
        match self.list().await[id as usize].state {
            SlotState::Running { pid, .. } => pid,
            ref other => panic!("not running: {other:?}"),
        }
    }

    async fn notification(&mut self, slot: u8, kind: &str, message: &str) {
        self.hook(HookEvent {
            notification: Some(message.into()),
            notification_type: Some(kind.into()),
            ..hook(slot, HookKind::Notification)
        })
        .await;
    }

    /// Waits until `to` has a pending relay held for `reason`.
    async fn held_for(&mut self, to: u8, reason: &str) {
        self.locks_until(|v| {
            v.relays.iter().any(|r| {
                r.to == to && r.state == RelayState::Pending && r.held.as_deref() == Some(reason)
            })
        })
        .await;
    }

    /// Waits until `to` has no pending relay left.
    async fn all_delivered(&mut self, to: u8) -> LockView {
        self.locks_until(|v| {
            v.relays
                .iter()
                .filter(|r| r.to == to)
                .all(|r| r.state == RelayState::Delivered && r.held.is_none())
        })
        .await
    }
}

const HELLO: &str = "Message from Jonathan via Overseer: hello";

#[tokio::test]
async fn idle_slot_gets_the_relay_with_bracketed_paste_then_enter() {
    let stub = box_stub("idle");
    let d = start_daemon_with(temp_socket(), stub.env()).await;
    let mut c = Client::connect(&d.socket).await;
    c.start_stub(2, &stub).await;
    c.hook(hook(2, HookKind::SessionStart)).await;
    assert_eq!(c.relay(None, 2, "hello").await, queued_for(2));
    let got = stub.read_until(&pasted(HELLO)).await;
    assert_eq!(got, pasted(HELLO));
    let v = c.all_delivered(2).await;
    assert!(v.relays[0].delivered_at.is_some());
    // Still idle, nothing more to send: nothing more is typed.
    sleep(Duration::from_millis(1500)).await;
    assert_eq!(stub.reads(), pasted(HELLO));
}

#[tokio::test]
async fn working_slot_waits_until_stop() {
    let stub = box_stub("working");
    let d = start_daemon_with(temp_socket(), stub.env()).await;
    let mut c = Client::connect(&d.socket).await;
    c.start_stub(2, &stub).await;
    // No hook yet: the courier cannot know the session is ready.
    assert_eq!(c.relay(None, 2, "hello").await, queued_for(2));
    c.held_for(2, "no activity reported yet").await;
    c.hook(hook(2, HookKind::UserPromptSubmit)).await;
    c.held_for(2, "session is working").await;
    stub.reads_nothing_for(2).await;
    c.hook(hook(2, HookKind::Stop)).await;
    assert_eq!(stub.read_until(&pasted(HELLO)).await, pasted(HELLO));
    c.all_delivered(2).await;
}

#[tokio::test]
async fn permission_prompt_never_gets_a_relay() {
    let stub = box_stub("permission");
    let d = start_daemon_with(temp_socket(), stub.env()).await;
    let mut c = Client::connect(&d.socket).await;
    let pid = c.start_stub(2, &stub).await;
    c.hook(hook(2, HookKind::UserPromptSubmit)).await;
    stub.set_box(Some("@dialog"), pid).await;
    c.notification(
        2,
        "permission_prompt",
        "Claude needs your permission to use Bash",
    )
    .await;
    assert_eq!(c.relay(None, 2, "hello").await, queued_for(2));
    c.held_for(2, "waiting at a prompt for Jonathan").await;
    stub.reads_nothing_for(3).await;
    let v = c.locks().await;
    assert_eq!(
        v.relays[0].held.as_deref(),
        Some("waiting at a prompt for Jonathan")
    );
    assert_eq!(v.relays[0].state, RelayState::Pending);
    // Answered Yes: the tool runs under the working line, still no hook.
    stub.set_box(Some("@working"), pid).await;
    stub.reads_nothing_for(3).await;
    stub.set_box(None, pid).await;
    c.hook(hook(2, HookKind::Stop)).await;
    assert_eq!(stub.read_until(&pasted(HELLO)).await, pasted(HELLO));
    c.all_delivered(2).await;
}

/// Answering No ends the turn with no `Stop` hook (F6): once the empty box
/// is back with no working line, the relay goes.
#[tokio::test]
async fn prompt_answered_no_gets_the_relay() {
    let stub = box_stub("answered-no");
    let d = start_daemon_with(temp_socket(), stub.env()).await;
    let mut c = Client::connect(&d.socket).await;
    let pid = c.start_stub(2, &stub).await;
    c.hook(hook(2, HookKind::UserPromptSubmit)).await;
    stub.set_box(Some("@dialog"), pid).await;
    c.notification(
        2,
        "permission_prompt",
        "Claude needs your permission to use Bash",
    )
    .await;
    assert_eq!(c.relay(None, 2, "hello").await, queued_for(2));
    c.held_for(2, "waiting at a prompt for Jonathan").await;
    stub.reads_nothing_for(2).await;
    stub.set_box(None, pid).await;
    assert_eq!(stub.read_until(&pasted(HELLO)).await, pasted(HELLO));
    c.all_delivered(2).await;
}

#[tokio::test]
async fn idle_input_wait_gets_the_relay() {
    let stub = box_stub("idle-wait");
    let d = start_daemon_with(temp_socket(), stub.env()).await;
    let mut c = Client::connect(&d.socket).await;
    c.start_stub(2, &stub).await;
    c.hook(hook(2, HookKind::UserPromptSubmit)).await;
    assert_eq!(c.relay(None, 2, "hello").await, queued_for(2));
    c.held_for(2, "session is working").await;
    c.notification(2, "idle_prompt", "Claude is waiting for your input")
        .await;
    assert_eq!(stub.read_until(&pasted(HELLO)).await, pasted(HELLO));
    c.all_delivered(2).await;
}

#[tokio::test]
async fn typed_input_box_waits_until_empty() {
    let stub = box_stub("typed");
    let d = start_daemon_with(temp_socket(), stub.env()).await;
    let mut c = Client::connect(&d.socket).await;
    let pid = c.start_stub(2, &stub).await;
    stub.set_box(Some("half a draft for demo-api"), pid).await;
    c.hook(hook(2, HookKind::Stop)).await;
    assert_eq!(c.relay(None, 2, "hello").await, queued_for(2));
    c.held_for(2, "input box has text").await;
    stub.reads_nothing_for(2).await;
    // Jonathan clears his draft; the next look finds the box empty.
    stub.set_box(None, pid).await;
    assert_eq!(stub.read_until(&pasted(HELLO)).await, pasted(HELLO));
    c.all_delivered(2).await;
}

#[tokio::test]
async fn two_pending_relays_go_in_one_paste() {
    let stub = box_stub("two");
    let d = start_daemon_with(temp_socket(), stub.env()).await;
    let mut c = Client::connect(&d.socket).await;
    c.start_stub(2, &stub).await;
    c.hook(hook(2, HookKind::UserPromptSubmit)).await;
    assert_eq!(c.relay(Some(0), 2, "status?").await, queued_for(2));
    assert_eq!(c.relay(None, 2, "hello").await, queued_for(2));
    c.locks_until(|v| {
        v.relays
            .iter()
            .filter(|r| r.held.as_deref() == Some("session is working"))
            .count()
            == 2
    })
    .await;
    c.hook(hook(2, HookKind::Stop)).await;
    let both = format!(
        "Message from S0 via Overseer: status?\n(You can answer once with notify_session.)\n\n{HELLO}"
    );
    assert_eq!(stub.read_until(&pasted(&both)).await, pasted(&both));
    let v = c.all_delivered(2).await;
    assert_eq!(v.relays.len(), 2);
    // Delivered, so S2 may now answer S0 once.
    assert_eq!(c.start(0).await, Response::Ok);
    assert_eq!(c.relay(Some(2), 0, "on it").await, queued_for(0));
}

#[tokio::test]
async fn pending_relay_delivers_after_restart_and_resume() {
    let stub = box_stub("restart");
    let d = start_daemon_with(temp_socket(), stub.env()).await;
    let mut c = Client::connect(&d.socket).await;
    c.start_stub(3, &stub).await;
    c.hook(hook(3, HookKind::UserPromptSubmit)).await;
    assert_eq!(c.relay(None, 3, "hello").await, queued_for(3));
    c.held_for(3, "session is working").await;
    drop(c);

    let socket = shutdown(d).await;
    let draws = stub.draws();
    let d = start_daemon_with(socket, stub.env()).await;
    let mut c = Client::connect(&d.socket).await;
    assert!(matches!(
        c.list().await[3].state,
        SlotState::Resumable { .. }
    ));
    c.held_for(3, "slot is not running").await;
    assert_eq!(c.request(Request::ResumeSlot { id: 3 }).await, Response::Ok);
    stub.drawn(draws + 1).await;
    c.held_for(3, "no activity reported yet").await;
    c.hook(hook(3, HookKind::SessionStart)).await;
    assert_eq!(stub.read_until(&pasted(HELLO)).await, pasted(HELLO));
    c.all_delivered(3).await;
}

#[tokio::test]
async fn multi_line_relay_waits_without_bracketed_paste() {
    let stub = box_stub_with("no-brackets", false);
    let d = start_daemon_with(temp_socket(), stub.env()).await;
    let mut c = Client::connect(&d.socket).await;
    c.start_stub(2, &stub).await;
    c.hook(hook(2, HookKind::Stop)).await;
    assert_eq!(
        c.relay(None, 2, "first line\nsecond line").await,
        queued_for(2)
    );
    c.held_for(2, "bracketed paste is off").await;
    stub.reads_nothing_for(2).await;
    let v = c.locks().await;
    assert_eq!(v.relays[0].state, RelayState::Pending);
    assert_eq!(v.relays[0].held.as_deref(), Some("bracketed paste is off"));
}

#[tokio::test]
async fn one_line_relay_is_typed_without_bracketed_paste() {
    let stub = box_stub_with("no-brackets-one", false);
    let d = start_daemon_with(temp_socket(), stub.env()).await;
    let mut c = Client::connect(&d.socket).await;
    c.start_stub(2, &stub).await;
    c.hook(hook(2, HookKind::Stop)).await;
    // The escape is stripped, never typed as a key.
    assert_eq!(c.relay(None, 2, "hel\x1blo").await, queued_for(2));
    let want = format!("{HELLO}\r");
    assert_eq!(stub.read_until(&want).await, want);
    c.all_delivered(2).await;
}

// Session end: a slot's waits, unused grants and pending relays belong to
// the session in it and go when that session ends, however it ends.

/// `(to, text, state)` of every relay, newest first.
fn relay_states(v: &LockView) -> Vec<(u8, String, RelayState)> {
    v.relays
        .iter()
        .map(|r| (r.to, r.text.clone(), r.state))
        .collect()
}

impl Client {
    /// Waits until slot `id` reads as exited.
    async fn exited(&mut self, id: u8) {
        for _ in 0..250 {
            if matches!(
                self.list().await[id as usize].state,
                SlotState::Exited { .. }
            ) {
                return;
            }
            sleep(Duration::from_millis(20)).await;
        }
        panic!("S{id} never exited");
    }
}

#[tokio::test]
async fn stop_then_start_at_once_starts_clean() {
    let stub = box_stub("stop-start");
    let d = start_daemon_full(
        temp_socket(),
        stub.env(),
        offline_settings(),
        lock_patterns(),
    )
    .await;
    let mut c = Client::connect(&d.socket).await;
    c.start_stub(3, &stub).await;
    c.hook(hook(3, HookKind::UserPromptSubmit)).await;
    c.request(claim_target(1, "custom:a", None)).await;
    assert!(matches!(
        c.request(claim_target(3, "custom:a", None)).await,
        Response::Queued { .. }
    ));
    assert_eq!(c.relay(None, 3, "hello").await, queued_for(3));
    c.held_for(3, "session is working").await;

    assert_eq!(c.request(Request::StopSlot { id: 3 }).await, Response::Ok);
    c.exited(3).await;
    let draws = stub.draws();
    let start = Request::StartSlot {
        id: 3,
        label: "t3".into(),
        cwd: stub.dir.join("work"),
        command: Some(vec![stub.bin.to_string_lossy().into_owned()]),
        prompt: None,
    };
    assert_eq!(c.request(start).await, Response::Ok);
    // Settled by the start itself, whatever the old exit event does.
    let v = c.locks().await;
    assert!(v.waits.is_empty(), "{v:?}");
    assert_eq!(
        relay_states(&v),
        [(3, t("hello"), RelayState::Dropped)],
        "{v:?}"
    );
    stub.drawn(draws + 1).await;
    c.hook(hook(3, HookKind::SessionStart)).await;
    // The new session is idle with an empty box, and S1's release finds no
    // one waiting: nothing is typed in.
    assert_eq!(
        c.request(Request::ReleaseTarget {
            slot: 1,
            target: t("custom:a")
        })
        .await,
        Response::Text {
            text: t("Released custom:a.")
        }
    );
    stub.reads_nothing_for(2).await;
    assert!(pending(&c.locks().await).is_empty());
}

#[tokio::test]
async fn declining_a_resumable_slot_hands_its_grant_on() {
    let stub = box_stub("decline");
    let socket = temp_socket();
    let start = |socket| start_daemon_full(socket, stub.env(), offline_settings(), lock_patterns());
    let d = start(socket).await;
    let mut c = Client::connect(&d.socket).await;
    c.start_stub(2, &stub).await;
    c.hook(hook(2, HookKind::UserPromptSubmit)).await;
    let api = "deploy:demo/api";
    assert_eq!(c.check(1, "tu-1", "demo-deploy api").await, None);
    assert!(c.check(2, "tu-2", "demo-deploy api").await.is_some());
    c.request(claim_target(4, "custom:b", None)).await;
    c.request(claim_target(2, "custom:b", None)).await;
    c.hook(post_tool_use(1, "tu-1")).await;
    assert_eq!(c.relay(None, 2, "hello").await, queued_for(2));
    let v = c.locks().await;
    assert_eq!(
        holds(&v),
        [
            (t("custom:b"), 4, ClaimKind::Claim),
            (t(api), 2, ClaimKind::Grant)
        ]
    );
    assert_eq!(queued(&v), [(t("custom:b"), 2)]);
    drop(c);

    let socket = shutdown(d).await;
    let d = start(socket).await;
    let mut c = Client::connect(&d.socket).await;
    assert!(matches!(
        c.list().await[2].state,
        SlotState::Resumable { .. }
    ));
    // The resumable session keeps all of it.
    let v = c.locks().await;
    assert_eq!(holds(&v)[1], (t(api), 2, ClaimKind::Grant));
    assert_eq!(queued(&v), [(t("custom:b"), 2)]);
    assert!(matches!(
        c.request(claim_target(5, api, None)).await,
        Response::Queued { .. }
    ));

    assert_eq!(c.request(Request::StopSlot { id: 2 }).await, Response::Ok);
    let v = c.locks().await;
    assert_eq!(
        holds(&v),
        [
            (t("custom:b"), 4, ClaimKind::Claim),
            (t(api), 5, ClaimKind::Claim)
        ]
    );
    assert!(v.waits.is_empty(), "{v:?}");
    let states = relay_states(&v);
    assert!(
        states.contains(&(
            5,
            t("S2 has released deploy:demo/api. You now hold it."),
            RelayState::Pending
        )),
        "{states:?}"
    );
    assert!(
        states.contains(&(2, t("hello"), RelayState::Dropped)),
        "{states:?}"
    );
    assert!(
        states.contains(&(
            2,
            t("S1 has released deploy:demo/api. You now hold it."),
            RelayState::Dropped
        )),
        "{states:?}"
    );
}

#[tokio::test]
async fn restart_ends_the_sessions_that_cannot_resume() {
    let socket = temp_socket();
    let d = lock_daemon_at(socket).await;
    let mut c = Client::connect(&d.socket).await;
    // S1 runs a command that is not claude, so it cannot be resumed.
    assert_eq!(c.start(1).await, Response::Ok);
    assert_eq!(c.check(1, "tu-1", "demo-deploy api").await, None);
    c.request(claim_target(1, "custom:a", None)).await;
    c.request(claim_target(2, "custom:b", None)).await;
    c.request(claim_target(1, "custom:b", None)).await;
    drop(c);
    let socket = shutdown(d).await;
    let d = lock_daemon_at(socket).await;
    let mut c = Client::connect(&d.socket).await;
    assert!(matches!(c.list().await[1].state, SlotState::Exited { .. }));
    let v = c.locks().await;
    // The command claim and the wait went with the session; tool claims
    // stay until released.
    assert_eq!(
        holds(&v),
        [
            (t("custom:a"), 1, ClaimKind::Claim),
            (t("custom:b"), 2, ClaimKind::Claim)
        ]
    );
    assert!(v.waits.is_empty(), "{v:?}");
}

#[tokio::test]
async fn send_to_a_slot_with_no_session_is_refused() {
    let d = lock_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    assert!(is_error(
        &c.relay(None, 4, "hello").await,
        "S4 has no session to message."
    ));
    assert!(is_error(
        &c.relay(Some(0), 4, "status?").await,
        "S4 has no session to message."
    ));
    assert_eq!(c.start(4).await, Response::Ok);
    assert_eq!(c.relay(None, 4, "hello").await, queued_for(4));
    assert_eq!(c.request(Request::StopSlot { id: 4 }).await, Response::Ok);
    c.exited(4).await;
    let v = c
        .locks_until(|v| v.relays.iter().all(|r| r.state == RelayState::Dropped))
        .await;
    assert_eq!(relay_states(&v), [(4, t("hello"), RelayState::Dropped)]);
    assert!(is_error(
        &c.relay(None, 4, "again").await,
        "S4 has no session to message."
    ));
}

#[tokio::test]
async fn notice_for_a_holder_with_no_session_is_queued_dropped() {
    let d = lock_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    // S3 never ran; its tool claim stays until released.
    c.request(claim_target(3, "custom:z", None)).await;
    assert_eq!(
        c.request(Request::ForceRelease {
            target: t("custom:z"),
            holder: None,
        })
        .await,
        Response::Text {
            text: t("Released custom:z.")
        }
    );
    let v = c.locks().await;
    assert_eq!(
        relay_states(&v),
        [(
            3,
            t("Jonathan released custom:z, which you held."),
            RelayState::Dropped
        )]
    );
    assert!(v.relays[0].dropped_at.is_some());
}

#[tokio::test]
async fn relay_right_after_a_delivery_says_why_it_waits() {
    let stub = box_stub("cooldown");
    let d = start_daemon_with(temp_socket(), stub.env()).await;
    let mut c = Client::connect(&d.socket).await;
    c.start_stub(2, &stub).await;
    c.hook(hook(2, HookKind::SessionStart)).await;
    assert_eq!(c.relay(None, 2, "hello").await, queued_for(2));
    stub.read_until(&pasted(HELLO)).await;
    c.all_delivered(2).await;
    // No hook since: the session has not reacted to the first paste yet.
    assert_eq!(c.relay(None, 2, "and the tests").await, queued_for(2));
    c.held_for(2, "just delivered, waiting for the session")
        .await;
}

/// A throwaway git vault and a daemon with the 2ndBrain writer `on` or off.
/// Nothing under the real home is touched: the vault, `CURRENT_SESSION.md`
/// and the state folder are all under the temp dir.
async fn brain_daemon(on: bool) -> (Daemon, PathBuf) {
    let socket = temp_socket();
    let base = socket.parent().unwrap().to_path_buf();
    let vault = base.join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    let git = |args: &[&str]| {
        let o = std::process::Command::new("git")
            .args(args)
            .current_dir(&vault)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(o.status.success(), "git {args:?}");
    };
    git(&["init", "-b", "main"]);
    let vault = vault.canonicalize().unwrap();
    let settings = overseer_daemon::Config {
        brain_writer: on,
        brain_dir: vault.clone(),
        current_session: base.join("home/.claude/CURRENT_SESSION.md"),
        push: false,
        backup_mins: 0,
        sync_command: vec![],
        ..offline_settings()
    };
    let d = start_daemon_cfg(socket, overseer_daemon::EnvSource::Inherit, settings).await;
    (d, vault)
}

fn write_check(slot: u8, tool: &str, path: PathBuf) -> Request {
    Request::CheckWrite(overseer_proto::WriteCheck {
        slot,
        tool: tool.into(),
        paths: vec![path],
        cwd: None,
    })
}

fn verdict(resp: Response) -> (Option<String>, Option<String>) {
    match resp {
        Response::Verdict { deny, rewrite } => (deny, rewrite),
        other => panic!("unexpected {other:?}"),
    }
}

const JOURNAL: &str = "06-Daily/2026/11-November/2026-11-13-journal.md";
const JOURNAL_DENY: &str = "06-Daily/2026/11-November/2026-11-13-journal.md is a shared 2ndBrain file. Use append_journal for the journal or edit_shared_file for an exact replacement.";

#[tokio::test]
async fn vault_write_checks_deny_shared_files_and_git_and_rewrite_bd() {
    let (d, vault) = brain_daemon(true).await;
    let mut c = Client::connect(&d.socket).await;
    for tool in ["Edit", "Write", "MultiEdit"] {
        let r = c.request(write_check(1, tool, vault.join(JOURNAL))).await;
        assert_eq!(verdict(r), (Some(JOURNAL_DENY.to_string()), None), "{tool}");
    }
    // A free file is allowed.
    let r = c
        .request(write_check(1, "Write", vault.join("free.md")))
        .await;
    assert_eq!(verdict(r), (None, None));
    // A Bash redirect into the journal is the same write.
    let cmd = format!("echo x >> {}/{JOURNAL}", vault.display());
    let r = c.request(bash_in(1, &cmd, "/tmp")).await;
    assert_eq!(verdict(r), (Some(JOURNAL_DENY.to_string()), None));
    // Reading it is never blocked.
    let cmd = format!("cat {}/{JOURNAL} | grep x", vault.display());
    let r = c.request(bash_in(1, &cmd, "/tmp")).await;
    assert_eq!(verdict(r), (None, None));
    // Git in the vault, and a recursive delete over shared files.
    let cmd = format!("git -C {} push", vault.display());
    let (deny, _) = verdict(c.request(bash_in(1, &cmd, "/tmp")).await);
    assert_eq!(
        deny.as_deref(),
        Some("use the commit_2ndbrain tool; Overseer commits and pushes 2ndBrain")
    );
    let cmd = format!("git -C {} log", vault.display());
    assert_eq!(
        verdict(c.request(bash_in(1, &cmd, "/tmp")).await),
        (None, None)
    );
    std::fs::create_dir_all(vault.join("06-Daily")).unwrap();
    let cmd = format!("rm -rf {}/06-Daily", vault.display());
    let (deny, _) = verdict(c.request(bash_in(1, &cmd, "/tmp")).await);
    assert!(deny.unwrap().contains("holds shared 2ndBrain files"));
    // bd writes are rewritten, compound ones denied, reads untouched.
    let (deny, rewrite) = verdict(c.request(bash_in(1, "bd close ov-3", "/tmp")).await);
    assert_eq!(deny, None);
    // The daemon names the binary by its absolute path (hook_program).
    assert_eq!(rewrite.as_deref(), Some("/bin/true ctl bd -- close ov-3"));
    let (deny, rewrite) = verdict(
        c.request(bash_in(1, "bd close ov-3 && git push", "/tmp"))
            .await,
    );
    assert_eq!(
        deny.as_deref(),
        Some("Overseer: run bd writes on their own, without $(...), backticks, redirects or env prefixes, so the daemon can run them under the vault lock")
    );
    assert_eq!(rewrite, None);
    assert_eq!(
        verdict(c.request(bash_in(1, "bd ready --json", "/tmp")).await),
        (None, None)
    );
}

#[tokio::test]
async fn fresh_start_clears_the_slots_section_before_answering() {
    let (d, vault) = brain_daemon(true).await;
    let cs = vault
        .parent()
        .unwrap()
        .join("home/.claude/CURRENT_SESSION.md");
    let mut c = Client::connect(&d.socket).await;
    let r = c
        .request(Request::UpdateCurrentSession {
            slot: 2,
            text: "old work on DEMO-12".into(),
        })
        .await;
    assert_eq!(r, Response::Ok);
    assert!(std::fs::read_to_string(&cs).unwrap().contains("old work"));
    assert_eq!(c.start(2).await, Response::Ok);
    // Read at once: a session's first update cannot be cleared after this.
    let file = std::fs::read_to_string(&cs).unwrap();
    assert!(!file.contains("old work"), "{file}");
}

#[tokio::test]
async fn writer_off_allows_everything() {
    let (d, vault) = brain_daemon(false).await;
    let mut c = Client::connect(&d.socket).await;
    let r = c.request(write_check(1, "Edit", vault.join(JOURNAL))).await;
    assert_eq!(verdict(r), (None, None));
    for cmd in [
        format!("echo x >> {}/{JOURNAL}", vault.display()),
        format!("git -C {} push", vault.display()),
        format!("rm -rf {}/06-Daily", vault.display()),
        "bd close ov-3".to_string(),
        "bd close ov-3 && git push".to_string(),
    ] {
        let r = c.request(bash_in(1, &cmd, "/tmp")).await;
        assert_eq!(verdict(r), (None, None), "{cmd}");
    }
}

fn bash_in(slot: u8, command: &str, cwd: &str) -> Request {
    Request::CheckBash(BashCheck {
        slot,
        session_id: None,
        tool_use_id: Some("tu-w".into()),
        command: command.into(),
        cwd: Some(PathBuf::from(cwd)),
        env: vec![],
    })
}

impl Client {
    async fn board(&mut self) -> BoardView {
        match self.request(Request::GetBoard).await {
            Response::Board(b) => b,
            other => panic!("unexpected {other:?}"),
        }
    }

    async fn coord(&mut self) -> CoordView {
        match self.request(Request::CoordStatus).await {
            Response::Coord(v) => v,
            other => panic!("unexpected {other:?}"),
        }
    }

    async fn set_summary(&mut self, from: Option<u8>, text: &str) -> Response {
        self.request(Request::SetBoardSummary {
            from,
            text: text.into(),
        })
        .await
    }

    async fn ask_status(&mut self, from: Option<u8>, slot: u8) -> Response {
        self.request(Request::RequestStatus { from, slot }).await
    }

    async fn report(&mut self, slot: u8, text: &str) -> Response {
        self.request(Request::ReportProgress {
            slot,
            text: text.into(),
        })
        .await
    }

    /// The next `CoordChanged` event.
    async fn coord_event(&mut self) -> CoordView {
        loop {
            if let Frame::Event(Event::CoordChanged(v)) =
                self.recv().await.expect("connection closed")
            {
                return v;
            }
        }
    }
}

#[tokio::test]
async fn summary_from_s0_shows_on_the_board() {
    let d = start_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    assert_eq!(c.board().await.coord.summary, None);
    let resp = c
        .set_summary(Some(0), "Two lanes \u{2014} both green")
        .await;
    assert_eq!(resp, Response::Ok);
    let event = c.coord_event().await;
    assert_eq!(event.summary.unwrap().text, "Two lanes - both green");
    let board = c.board().await;
    assert_eq!(board.coord.summary.unwrap().text, "Two lanes - both green");
    assert_eq!(board.slots.len(), 7);
    // Jonathan, through ctl, may set it too.
    assert_eq!(c.set_summary(None, "  Quiet  ").await, Response::Ok);
    assert_eq!(c.coord().await.summary.unwrap().text, "Quiet");
}

#[tokio::test]
async fn summary_from_a_worker_is_refused() {
    let d = start_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    let resp = c.set_summary(Some(3), "take over").await;
    assert!(
        is_error(&resp, "only the coordinator (S0) can set the board summary"),
        "{resp:?}"
    );
    assert_eq!(c.coord().await.summary, None);
}

#[tokio::test]
async fn summary_limits() {
    let d = start_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    let resp = c.set_summary(Some(0), "  ").await;
    assert!(is_error(&resp, "the summary is empty"), "{resp:?}");
    let resp = c.set_summary(Some(0), &"x".repeat(2001)).await;
    assert!(
        is_error(&resp, "the summary is longer than 2000 characters"),
        "{resp:?}"
    );
    assert_eq!(
        c.set_summary(Some(0), &"x".repeat(2000)).await,
        Response::Ok
    );
    let resp = c.report(3, "").await;
    assert!(is_error(&resp, "the report is empty"), "{resp:?}");
}

#[tokio::test]
async fn status_request_relays_and_the_answer_comes_back() {
    let (s0, s3) = (box_stub("status-s0"), box_stub("status-s3"));
    let d = start_daemon_with(temp_socket(), s0.env()).await;
    let mut c = Client::connect(&d.socket).await;
    c.start_stub(0, &s0).await;
    c.start_stub(3, &s3).await;
    c.hook(hook(0, HookKind::SessionStart)).await;
    c.hook(hook(3, HookKind::SessionStart)).await;
    let resp = c.ask_status(Some(0), 3).await;
    assert!(matches!(resp, Response::Text { .. }), "{resp:?}");
    let ask = "Overseer: S0 asks for a short status. Answer with the report_progress tool in two or three lines.";
    assert_eq!(s3.read_until(&pasted(ask)).await, pasted(ask));
    assert_eq!(c.coord().await.status_asked.len(), 1);
    assert_eq!(c.report(3, "Tests pass, PR next").await, Response::Ok);
    let reply = "Overseer: S3 reports: Tests pass, PR next";
    assert_eq!(s0.read_until(&pasted(reply)).await, pasted(reply));
    let coord = c.coord().await;
    assert!(coord.status_asked.is_empty(), "{:?}", coord.status_asked);
    let report = coord.reports_for(3).next().unwrap();
    assert_eq!(report.kind, ReportKind::Report);
    assert_eq!(report.text, "Tests pass, PR next");
}

#[tokio::test]
async fn second_status_request_sends_one_relay() {
    let (s0, s3) = (box_stub("again-s0"), box_stub("again-s3"));
    let d = start_daemon_with(temp_socket(), s0.env()).await;
    let mut c = Client::connect(&d.socket).await;
    c.start_stub(3, &s3).await;
    assert!(matches!(c.ask_status(None, 3).await, Response::Text { .. }));
    let asked = c.coord().await.status_asked[0].1;
    let hhmm = overseer_daemon::locks::local_hhmm(asked);
    let resp = c.ask_status(Some(0), 3).await;
    assert_eq!(
        resp,
        Response::Text {
            text: format!("already asked S3 at {hhmm}")
        }
    );
    let to_s3 = c
        .locks()
        .await
        .relays
        .into_iter()
        .filter(|r| r.to == 3)
        .count();
    assert_eq!(to_s3, 1);
    // A worker may not ask.
    let resp = c.ask_status(Some(2), 3).await;
    assert!(
        is_error(&resp, "only the coordinator (S0) can ask for a status"),
        "{resp:?}"
    );
    drop(s0);
}

#[tokio::test]
async fn status_of_a_stopped_slot_is_refused() {
    let d = start_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    let resp = c.ask_status(Some(0), 3).await;
    assert!(is_error(&resp, "S3 is not running"), "{resp:?}");
    assert!(c.coord().await.status_asked.is_empty());
}

#[tokio::test]
async fn timeline_records_state_changes() {
    let d = start_daemon().await;
    let mut c = Client::connect(&d.socket).await;
    assert_eq!(c.start(3).await, Response::Ok);
    c.hook(hook(3, HookKind::UserPromptSubmit)).await;
    c.hook(hook(3, HookKind::Stop)).await;
    let mut texts = Vec::new();
    for _ in 0..250 {
        let coord = c.coord().await;
        let mut rows: Vec<_> = coord.reports_for(3).collect();
        rows.reverse();
        texts = rows.iter().map(|r| r.text.clone()).collect();
        if texts.len() >= 3 {
            assert!(rows.iter().all(|r| r.kind == ReportKind::State));
            break;
        }
        sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(texts, ["started", "working", "idle"]);
}

/// A daemon whose default command is a stub `claude` that logs each start,
/// over stub work sources and a throwaway git repo `demo-api`.
struct PlanRig {
    s: Sources,
    repo: PathBuf,
    worktrees: PathBuf,
    work5: PathBuf,
    bin: PathBuf,
    log: PathBuf,
}

fn git_in(dir: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.test"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

impl PlanRig {
    fn new(name: &str) -> PlanRig {
        let s = stub_sources(&format!("plan-{name}"), false);
        let (bin, log) = stub_claude(&s.dir.join("stub"));
        let repo = s.dir.join("repos/demo-api");
        std::fs::create_dir_all(&repo).unwrap();
        let repo = repo.canonicalize().unwrap();
        git_in(&repo, &["init", "-q", "-b", "main"]);
        git_in(&repo, &["commit", "-q", "--allow-empty", "-m", "start"]);
        let worktrees = s.dir.join("worktrees");
        std::fs::create_dir_all(&worktrees).unwrap();
        let work5 = s.dir.join("work5");
        std::fs::create_dir_all(&work5).unwrap();
        PlanRig {
            repo,
            worktrees: worktrees.canonicalize().unwrap(),
            work5: work5.canonicalize().unwrap(),
            s,
            bin,
            log,
        }
    }

    fn worktree(&self) -> PathBuf {
        self.worktrees.join("demo-api-DEMO-12")
    }

    async fn daemon_at(&self, socket: PathBuf) -> Daemon {
        let state_dir = socket.parent().unwrap().join("state");
        let settings = overseer_daemon::Config {
            worktree_root: self.worktrees.clone(),
            current_session: self.s.dir.join("CURRENT_SESSION.md"),
            ..self.s.settings()
        };
        let cfg = DaemonConfig {
            socket: socket.clone(),
            default_command: vec![self.bin.to_string_lossy().into_owned()],
            env: self.s.env(),
            state_dir: state_dir.clone(),
            hook_program: PathBuf::from("/bin/true"),
            settings,
            patterns: default_patterns(),
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

    /// A daemon and a client that has loaded the work items.
    async fn start(&self) -> (Daemon, Client) {
        let d = self.daemon_at(temp_socket()).await;
        let mut c = Client::connect(&d.socket).await;
        c.refresh().await;
        (d, c)
    }

    /// Everything the stub `claude` logged.
    fn calls(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// How many sessions the default command started.
    fn starts(&self) -> usize {
        self.calls().matches("|--session-id").count()
    }

    fn slot4(&self) -> PlanSlot {
        PlanSlot {
            slot: 4,
            label: String::new(),
            items: vec!["DEMO-12".into()],
            targets: vec![PlanTarget {
                target: "repo:demo-api:main".into(),
                reason: Some("merge".into()),
            }],
            worktree: Some(PlanWorktree {
                repo: self.repo.clone(),
                ticket: "DEMO-12".into(),
            }),
            cwd: None,
            kickoff: "Start with the parser.".into(),
            rules: vec![],
        }
    }

    fn slot3(&self) -> PlanSlot {
        PlanSlot {
            slot: 3,
            label: String::new(),
            items: vec!["ov-4".into()],
            targets: vec![],
            worktree: None,
            cwd: None,
            kickoff: "Pick this up next.".into(),
            rules: vec![],
        }
    }

    fn slot5(&self) -> PlanSlot {
        PlanSlot {
            slot: 5,
            label: "lane five".into(),
            items: vec![],
            targets: vec![],
            worktree: None,
            cwd: Some(self.work5.clone()),
            kickoff: "Look at the logs.".into(),
            rules: vec![],
        }
    }
}

fn plan_of(slots: Vec<PlanSlot>) -> Plan {
    Plan {
        summary: "Two lanes".into(),
        slots,
    }
}

impl Client {
    async fn propose(&mut self, from: Option<u8>, plan: Plan) -> Response {
        self.request(Request::ProposePlan { from, plan }).await
    }

    async fn approve(&mut self, id: i64, plan: Plan) -> Response {
        self.request(Request::DecidePlan {
            id,
            from: None,
            decision: PlanDecision::Approve { plan },
        })
        .await
    }

    async fn reject(&mut self, id: i64, reason: &str) -> Response {
        self.request(Request::DecidePlan {
            id,
            from: None,
            decision: PlanDecision::Reject {
                reason: reason.into(),
            },
        })
        .await
    }

    async fn plan_record(&mut self, id: i64) -> overseer_proto::PlanRecord {
        self.coord()
            .await
            .plans
            .into_iter()
            .find(|p| p.id == id)
            .unwrap_or_else(|| panic!("no plan {id}"))
    }
}

/// S0 and S3 running as stubs that draw Claude's input box, both idle.
async fn live_s0_and_s3(c: &mut Client, name: &str) -> (BoxStub, BoxStub) {
    let (s0, s3) = (
        box_stub(&format!("{name}-s0")),
        box_stub(&format!("{name}-s3")),
    );
    c.start_stub(0, &s0).await;
    c.start_stub(3, &s3).await;
    c.hook(hook(0, HookKind::SessionStart)).await;
    c.hook(hook(3, HookKind::SessionStart)).await;
    (s0, s3)
}

#[tokio::test]
async fn propose_from_a_worker_is_refused() {
    let rig = PlanRig::new("worker");
    let (_d, mut c) = rig.start().await;
    let resp = c.propose(Some(3), plan_of(vec![rig.slot4()])).await;
    assert!(
        is_error(&resp, "only the coordinator (S0) can propose a plan"),
        "{resp:?}"
    );
    assert!(c.coord().await.plans.is_empty());
}

#[tokio::test]
async fn proposed_plan_is_pending_and_announced() {
    let rig = PlanRig::new("announce");
    let (_d, mut c) = rig.start().await;
    // S2 owns the ticket, so a bead under it is a warning, not a refusal.
    assert_eq!(c.assign("DEMO-12", 2, false, false).await, Response::Ok);
    let mut s4 = rig.slot4();
    s4.items = vec!["ov-1".into()];
    let resp = c.propose(Some(0), plan_of(vec![s4])).await;
    assert_eq!(
        resp,
        Response::Text {
            text: "Plan 1 is waiting for Jonathan.".into()
        }
    );
    let event = c.coord_event().await;
    assert_eq!(event.plans.len(), 1);
    assert_eq!(event.plans[0].state, PlanState::Pending);
    assert_eq!(
        event.plans[0].warnings,
        vec!["This bead is under DEMO-12, owned by S2. Assign to S4 anyway?".to_string()]
    );
    assert_eq!(event.pending_plan().unwrap().id, 1);
}

#[tokio::test]
async fn new_plan_supersedes_the_old() {
    let rig = PlanRig::new("supersede");
    let (_d, mut c) = rig.start().await;
    let first = c.propose(None, plan_of(vec![rig.slot4()])).await;
    assert_eq!(
        first,
        Response::Text {
            text: "Plan 1 is waiting for Jonathan.".into()
        }
    );
    let second = c.propose(Some(0), plan_of(vec![rig.slot5()])).await;
    match second {
        Response::Text { text } => assert!(
            text == "Plan 2 is waiting for Jonathan. It replaces plan 1.",
            "{text}"
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(c.plan_record(1).await.state, PlanState::Superseded);
    assert_eq!(c.plan_record(2).await.state, PlanState::Pending);
}

#[tokio::test]
async fn approve_runs_every_step() {
    let rig = PlanRig::new("approve");
    let (_d, mut c) = rig.start().await;
    let (s0, s3) = live_s0_and_s3(&mut c, "approve").await;
    let plan = plan_of(vec![rig.slot4(), rig.slot3()]);
    assert!(matches!(
        c.propose(Some(0), plan.clone()).await,
        Response::Text { .. }
    ));
    let resp = c.approve(1, plan).await;
    let text = match resp {
        Response::Text { text } => text,
        other => panic!("{other:?}"),
    };
    let wt = rig.worktree();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines,
        [
            "S3: assigned ov-4".to_string(),
            "S3: kickoff queued".to_string(),
            "S4: assigned DEMO-12".to_string(),
            format!("S4: worktree {} created", wt.display()),
            "S4: holds repo:demo-api:main".to_string(),
            format!("S4: started in {}", wt.display()),
        ]
    );

    let work = c.work().await;
    assert_eq!(item(&work, "DEMO-12").owner, Some(4));
    assert_eq!(item(&work, "ov-4").owner, Some(3));
    let locks = c.locks().await;
    assert!(
        holds(&locks).contains(&("repo:demo-api:main".to_string(), 4, ClaimKind::Claim)),
        "{:?}",
        holds(&locks)
    );
    assert!(wt.is_dir());

    rig.starts_reach(1).await;
    let calls = rig.calls();
    assert!(calls.starts_with(&format!("{}|", wt.display())), "{calls}");
    let prompt = calls.split_once(" -- ").unwrap().1;
    assert!(prompt.contains("DEMO-12"), "{prompt}");
    assert!(prompt.contains(&wt.display().to_string()), "{prompt}");
    assert!(prompt.contains("Start with the parser."), "{prompt}");
    assert!(
        prompt.contains("You hold these targets: repo:demo-api:main"),
        "{prompt}"
    );

    s3.read_until("Pick this up next.").await;
    let told = s0.read_until("Jonathan approved plan 1.").await;
    assert!(told.contains("S4: holds repo:demo-api:main"), "{told}");
    assert!(told.contains("S3: kickoff queued"), "{told}");

    let record = c.plan_record(1).await;
    assert_eq!(record.state, PlanState::Approved);
    assert!(!record.edited);
    assert_eq!(record.outcome, lines);
}

#[tokio::test]
async fn approve_with_edits_says_so() {
    let rig = PlanRig::new("edits");
    let (_d, mut c) = rig.start().await;
    let (s0, s3) = live_s0_and_s3(&mut c, "edits").await;
    let proposed = plan_of(vec![rig.slot4(), rig.slot3()]);
    assert!(matches!(
        c.propose(Some(0), proposed).await,
        Response::Text { .. }
    ));
    let edited = plan_of(vec![rig.slot4()]);
    let resp = c.approve(1, edited.clone()).await;
    assert!(matches!(resp, Response::Text { .. }), "{resp:?}");
    s0.read_until("Jonathan approved plan 1 with edits.").await;
    let record = c.plan_record(1).await;
    assert!(record.edited);
    assert_eq!(record.plan, edited);
    // Only S4 ran: S3 got no kickoff and ov-4 has no owner.
    rig.starts_reach(1).await;
    assert_eq!(rig.starts(), 1);
    assert_eq!(item(&c.work().await, "ov-4").owner, None);
    s3.reads_nothing_for(1).await;
}

#[tokio::test]
async fn approval_revalidates() {
    let rig = PlanRig::new("revalidate");
    let (_d, mut c) = rig.start().await;
    let plan = plan_of(vec![rig.slot4()]);
    assert!(matches!(
        c.propose(Some(0), plan.clone()).await,
        Response::Text { .. }
    ));
    // Jonathan assigns the ticket by hand before approving.
    assert_eq!(c.assign("DEMO-12", 2, false, false).await, Response::Ok);
    let resp = c.approve(1, plan).await;
    assert_eq!(
        resp,
        Response::Error {
            message: "DEMO-12 is owned by S2".into()
        }
    );
    assert_eq!(c.plan_record(1).await.state, PlanState::Pending);
    assert!(matches!(c.list().await[4].state, SlotState::Empty));
    assert_eq!(rig.starts(), 0);
    assert!(!rig.worktree().exists());
}

#[tokio::test]
async fn second_approval_is_refused() {
    let rig = PlanRig::new("twice");
    let (d, mut c1) = rig.start().await;
    let mut c2 = Client::connect(&d.socket).await;
    let plan = plan_of(vec![rig.slot4()]);
    assert!(matches!(
        c1.propose(Some(0), plan.clone()).await,
        Response::Text { .. }
    ));
    let (a, b) = tokio::join!(c1.approve(1, plan.clone()), c2.approve(1, plan));
    let (oks, errs): (Vec<_>, Vec<_>) = [a, b]
        .into_iter()
        .partition(|r| matches!(r, Response::Text { .. }));
    assert_eq!(oks.len(), 1, "{oks:?} {errs:?}");
    assert_eq!(
        errs,
        [Response::Error {
            message: "plan 1 is no longer pending".into()
        }]
    );
    rig.starts_reach(1).await;
    sleep(Duration::from_millis(500)).await;
    assert_eq!(rig.starts(), 1);
}

impl PlanRig {
    async fn starts_reach(&self, n: usize) {
        for _ in 0..100 {
            if self.starts() >= n {
                return;
            }
            sleep(Duration::from_millis(50)).await;
        }
        panic!("stub started {} times, wanted {n}", self.starts());
    }
}

#[tokio::test]
async fn a_failing_slot_does_not_stop_the_others() {
    let rig = PlanRig::new("failing");
    let (_d, mut c) = rig.start().await;
    // A plain folder where S4's worktree would go.
    std::fs::create_dir_all(rig.worktree()).unwrap();
    let plan = plan_of(vec![rig.slot4(), rig.slot5()]);
    assert!(matches!(
        c.propose(Some(0), plan.clone()).await,
        Response::Text { .. }
    ));
    let text = match c.approve(1, plan).await {
        Response::Text { text } => text,
        other => panic!("{other:?}"),
    };
    let wt = rig.worktree();
    let failed = format!(
        "S4: worktree {0} failed: {0} exists and is not a worktree of {1}; the rest of S4's plan was skipped",
        wt.display(),
        rig.repo.display()
    );
    assert!(text.lines().any(|l| l == failed), "{text}");
    assert!(
        text.lines()
            .any(|l| l == format!("S5: started in {}", rig.work5.display())),
        "{text}"
    );
    // S4 stopped after its items; S5 started.
    assert!(matches!(c.list().await[4].state, SlotState::Empty));
    rig.starts_reach(1).await;
    assert!(rig
        .calls()
        .starts_with(&format!("{}|", rig.work5.display())));
    assert_eq!(c.locks().await.claims.len(), 0);
}

#[tokio::test]
async fn reject_tells_s0() {
    let rig = PlanRig::new("reject");
    let (_d, mut c) = rig.start().await;
    let (s0, _s3) = live_s0_and_s3(&mut c, "reject").await;
    assert!(matches!(
        c.propose(Some(0), plan_of(vec![rig.slot4()])).await,
        Response::Text { .. }
    ));
    assert_eq!(c.reject(1, "not today").await, Response::Ok);
    s0.read_until("Jonathan rejected plan 1: not today").await;
    assert_eq!(c.plan_record(1).await.state, PlanState::Rejected);
    assert_eq!(rig.starts(), 0);
    // A decided plan cannot be decided again.
    let resp = c.reject(1, "").await;
    assert!(is_error(&resp, "plan 1 is no longer pending"), "{resp:?}");
}

#[tokio::test]
async fn decide_from_a_slot_is_refused() {
    let rig = PlanRig::new("slotdecide");
    let (_d, mut c) = rig.start().await;
    let plan = plan_of(vec![rig.slot4()]);
    assert!(matches!(
        c.propose(Some(0), plan.clone()).await,
        Response::Text { .. }
    ));
    for decision in [
        PlanDecision::Approve { plan: plan.clone() },
        PlanDecision::Reject {
            reason: String::new(),
        },
    ] {
        let resp = c
            .request(Request::DecidePlan {
                id: 1,
                from: Some(0),
                decision,
            })
            .await;
        assert!(
            is_error(&resp, "only Jonathan can approve or reject a plan"),
            "{resp:?}"
        );
    }
    assert_eq!(c.plan_record(1).await.state, PlanState::Pending);
    assert_eq!(rig.starts(), 0);
}

#[tokio::test]
async fn pending_plan_survives_a_restart_and_approved_never_reruns() {
    let rig = PlanRig::new("restart");
    let (d, mut c) = rig.start().await;
    assert_eq!(c.assign("DEMO-12", 2, false, false).await, Response::Ok);
    let mut s4 = rig.slot4();
    s4.items = vec!["ov-1".into()];
    let plan = plan_of(vec![s4]);
    assert!(matches!(
        c.propose(Some(0), plan.clone()).await,
        Response::Text { .. }
    ));
    let warnings = c.plan_record(1).await.warnings;
    assert_eq!(warnings.len(), 1);
    drop(c);

    let socket = shutdown(d).await;
    let d = rig.daemon_at(socket).await;
    let mut c = Client::connect(&d.socket).await;
    let record = c.plan_record(1).await;
    assert_eq!(record.state, PlanState::Pending);
    assert_eq!(record.warnings, warnings);
    c.refresh().await;
    let resp = c.approve(1, plan).await;
    assert!(matches!(resp, Response::Text { .. }), "{resp:?}");
    rig.starts_reach(1).await;
    drop(c);

    let socket = shutdown(d).await;
    let d = rig.daemon_at(socket).await;
    let mut c = Client::connect(&d.socket).await;
    let record = c.plan_record(1).await;
    assert_eq!(record.state, PlanState::Approved);
    assert!(!record.outcome.is_empty());
    sleep(Duration::from_millis(500)).await;
    assert_eq!(rig.starts(), 1);
    let resp = c.approve(1, plan_of(vec![rig.slot4()])).await;
    assert!(is_error(&resp, "plan 1 is no longer pending"), "{resp:?}");
}

async fn kickoff_text_of(c: &mut Client, slot: u8) -> String {
    match c.request(Request::Kickoff { slot }).await {
        Response::Text { text } => text,
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn manual_kickoff_names_targets_and_current_session() {
    let rig = PlanRig::new("manual-kickoff");
    let (_d, mut c) = rig.start().await;
    assert_eq!(c.assign("DEMO-12", 3, false, false).await, Response::Ok);
    assert!(matches!(
        c.request(claim_target(3, "repo:demo-api:main", None)).await,
        Response::Ok | Response::Text { .. }
    ));
    let text = kickoff_text_of(&mut c, 3).await;
    assert!(text.contains("DEMO-12"), "{text}");
    assert!(
        text.contains("You hold these targets: repo:demo-api:main"),
        "{text}"
    );
    assert!(text.contains("Keep your section of"), "{text}");
    assert!(
        text.contains(&rig.s.dir.join("CURRENT_SESSION.md").display().to_string()),
        "{text}"
    );
}

/// A slot working in a folder under `worktree_root` is told to work there; a
/// slot working anywhere else is not.
#[tokio::test]
async fn manual_kickoff_names_the_worktree_only_under_the_root() {
    let rig = PlanRig::new("manual-kickoff-wt");
    let (_d, mut c) = rig.start().await;
    let wt = rig.worktrees.join("demo-api-DEMO-12");
    std::fs::create_dir_all(&wt).unwrap();
    assert_eq!(c.assign("DEMO-12", 3, false, false).await, Response::Ok);
    let start = |cwd: PathBuf| Request::StartSlot {
        id: 3,
        label: "t3".into(),
        cwd,
        command: None,
        prompt: None,
    };
    assert_eq!(c.request(start(rig.work5.clone())).await, Response::Ok);
    let text = kickoff_text_of(&mut c, 3).await;
    assert!(!text.contains("Work in "), "{text}");
    assert_eq!(c.request(Request::StopSlot { id: 3 }).await, Response::Ok);
    assert_eq!(c.request(start(wt.clone())).await, Response::Ok);
    let text = kickoff_text_of(&mut c, 3).await;
    assert!(
        text.contains(&format!("Work in {}.", wt.display())),
        "{text}"
    );
}

/// The plan view re-checks an edited plan: dropping the slot a warning is
/// about drops the warning, and a plan with nothing left is refused.
#[tokio::test]
async fn check_plan_recomputes_warnings_for_an_edited_plan() {
    let rig = PlanRig::new("check");
    let (_d, mut c) = rig.start().await;
    assert_eq!(c.assign("DEMO-12", 2, false, false).await, Response::Ok);
    let mut s4 = rig.slot4();
    s4.items = vec!["ov-1".into()];
    let check = |plan: Plan| Request::CheckPlan { plan };
    assert_eq!(
        c.request(check(plan_of(vec![s4, rig.slot5()]))).await,
        Response::PlanCheck {
            warnings: vec![
                "This bead is under DEMO-12, owned by S2. Assign to S4 anyway?".to_string()
            ],
            refusal: None,
        }
    );
    assert_eq!(
        c.request(check(plan_of(vec![rig.slot5()]))).await,
        Response::PlanCheck {
            warnings: vec![],
            refusal: None,
        }
    );
    assert_eq!(
        c.request(check(plan_of(vec![]))).await,
        Response::PlanCheck {
            warnings: vec![],
            refusal: Some("the plan has no slots".into()),
        }
    );
    // Checking records nothing.
    assert!(c.coord().await.plans.is_empty());
}
