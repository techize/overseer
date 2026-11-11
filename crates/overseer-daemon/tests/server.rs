use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use bytes::Bytes;
use overseer_daemon::{run, DaemonConfig};
use overseer_proto::{
    read_frame, write_frame, Activity, ActivityKind, Event, Frame, HookEvent, HookKind, ItemKind,
    Request, Response, SlotInfo, SlotState, SourceStatus, WorkItem, WorkView,
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
    let state_dir = socket.parent().unwrap().join("state");
    let cfg = DaemonConfig {
        socket: socket.clone(),
        default_command: vec![SCRIPT.to_string()],
        env,
        state_dir: state_dir.clone(),
        hook_program: PathBuf::from("/bin/true"),
        settings,
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
            "#!/bin/sh\nprintf '%s|%s\\n' \"$(pwd -P)\" \"$*\" >> '{}'\n\
             # Like claude, save the conversation for a new session id.\n\
             if [ \"$1\" = --session-id ] && [ -n \"$STUB_SAVES\" ]; then\n\
             mkdir -p \"$CLAUDE_CONFIG_DIR/projects/p\" && : > \"$CLAUDE_CONFIG_DIR/projects/p/$2.jsonl\"\n\
             fi\nexec sleep 30\n",
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
    // Kickoff ends in "what." so check the whole tail, not one word.
    assert!(
        lines[0].ends_with("propose who works on what."),
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
    for flag in ["--session-id", "--name", "--settings"] {
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
    for flag in ["--session-id", "--name", "--settings"] {
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
        ("exec sleep 10\n".to_string(), "exec sleep 10\n".to_string())
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
    assert_eq!(bd[0], format!("{}|list --json", s.beads_dir.display()));
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
    assert!(for_s1[0].text.contains("DEMO-40"), "{:?}", for_s1[0]);
    assert!(for_s1[0].text.contains("S2"), "{:?}", for_s1[0]);
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_assigns_one_wins() {
    let s = stub_sources("race", false);
    let d = s.daemon().await;
    let mut a = Client::connect(&d.socket).await;
    let mut b = Client::connect(&d.socket).await;
    a.refresh().await;
    for key in ["DEMO-12", "DEMO-40", "DEMO-7", "ov-3"] {
        let (ra, rb) = tokio::join!(
            a.assign(key, 1, false, false),
            b.assign(key, 2, false, false)
        );
        let oks = [&ra, &rb].iter().filter(|r| ***r == Response::Ok).count();
        assert_eq!(oks, 1, "{key}: {ra:?} {rb:?}");
        let loser = if ra == Response::Ok { &rb } else { &ra };
        assert!(
            matches!(loser, Response::Error { message } if message.starts_with(&format!("{key} is owned by S"))),
            "{loser:?}"
        );
    }
}
