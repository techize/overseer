//! `overseer hook` must be silent and fast whether or not a daemon is there.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use overseer_daemon::{run, DaemonConfig};
use overseer_proto::{read_frame, write_frame, Client, Frame, HookKind, Request, Response};
use tokio::net::UnixListener;

const STOP: &[u8] = br#"{"session_id":"abc","hook_event_name":"Stop"}"#;

fn run_hook(home: &std::path::Path) {
    // The first exec of a fresh binary can be slow on macOS (signature
    // checks); that is not the hook's own latency, so warm it up untimed.
    static WARM: std::sync::Once = std::sync::Once::new();
    WARM.call_once(|| {
        let _ = Command::new(env!("CARGO_BIN_EXE_overseer"))
            .arg("hook")
            .stdin(Stdio::null())
            .output();
    });
    let start = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_overseer"))
        .args(["hook", "Stop"])
        .env("OVERSEER_HOME", home)
        .env("OVERSEER_SLOT", "S1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(STOP).unwrap();
    let out = child.wait_with_output().unwrap();
    let took = start.elapsed();
    assert!(out.status.success(), "status {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout: {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr: {:?}", out.stderr);
    assert!(took < Duration::from_millis(200), "took {took:?}");
}

#[test]
fn silent_and_fast_without_a_daemon() {
    let home = std::env::temp_dir().join(format!("ovhook-nod-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    run_hook(&home);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn silent_and_fast_with_a_daemon() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let home = std::env::temp_dir().join(format!("ovhook-d-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let socket = home.join("overseer.sock");
    let task = rt.spawn(run(DaemonConfig {
        socket: socket.clone(),
        default_command: vec!["/bin/cat".to_string()],
        env: overseer_daemon::EnvSource::Inherit,
        state_dir: home.join("state"),
        hook_program: std::path::PathBuf::from("/bin/true"),
        settings: overseer_daemon::Config::default(),
    }));
    rt.block_on(async {
        let c = Client::connect_or_start(&socket, || Ok(())).await.unwrap();
        drop(c);
    });
    run_hook(&home);
    rt.block_on(async {
        let c = Client::connect(&socket).await.unwrap();
        let _ = c.request(overseer_proto::Request::Shutdown).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
    });
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn bare_hook_without_event_is_silent_and_exits_zero() {
    let out = Command::new(env!("CARGO_BIN_EXE_overseer"))
        .arg("hook")
        .env("OVERSEER_SLOT", "S1")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "status {:?}", out.status);
    assert!(out.stdout.is_empty());
    assert!(out.stderr.is_empty(), "stderr: {:?}", out.stderr);
}

/// A listener at `home/overseer.sock` that reads one frame, hands it to the
/// returned channel and, if `reply`, answers Ok. Otherwise it stays silent
/// and holds the connection open.
fn fake_daemon(
    home: &std::path::Path,
    reply: bool,
) -> (
    std::sync::mpsc::Receiver<Frame>,
    std::thread::JoinHandle<()>,
) {
    let (tx, rx) = std::sync::mpsc::channel();
    let socket = home.join("overseer.sock");
    let ready = std::sync::mpsc::channel();
    let t = std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let l = UnixListener::bind(&socket).unwrap();
            ready.0.send(()).unwrap();
            let (mut s, _) = l.accept().await.unwrap();
            let Some(f) = read_frame(&mut s).await.unwrap() else {
                return;
            };
            let seq = match &f {
                Frame::Request { seq, .. } => *seq,
                _ => 0,
            };
            tx.send(f).unwrap();
            if reply {
                write_frame(
                    &mut s,
                    &Frame::Response {
                        seq,
                        resp: Response::Ok,
                    },
                )
                .await
                .unwrap();
            } else {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        });
    });
    ready.1.recv().unwrap();
    (rx, t)
}

#[test]
fn delivers_the_event_to_a_listener() {
    let home = std::env::temp_dir().join(format!("ovhook-l-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let (rx, t) = fake_daemon(&home, true);
    run_hook(&home);
    let Frame::Request {
        req: Request::Hook(ev),
        ..
    } = rx.recv_timeout(Duration::from_secs(2)).unwrap()
    else {
        panic!("not a hook request");
    };
    assert_eq!(ev.slot, 1);
    assert_eq!(ev.event, HookKind::Stop);
    assert_eq!(ev.session_id.as_deref(), Some("abc"));
    t.join().unwrap();
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_listener_that_never_replies_does_not_hold_the_hook() {
    let home = std::env::temp_dir().join(format!("ovhook-s-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let (rx, t) = fake_daemon(&home, false);
    run_hook(&home);
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        Frame::Request {
            req: Request::Hook(_),
            ..
        }
    ));
    t.join().unwrap();
    let _ = std::fs::remove_dir_all(&home);
}
