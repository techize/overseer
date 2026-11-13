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
        // No real beads or Jira.
        patterns: overseer_daemon::locks::targets::default_patterns(),
        settings: overseer_daemon::Config {
            beads_dir: "/nonexistent/overseer-test-beads".into(),
            brain_dir: "/nonexistent/overseer-test-brain".into(),
            jira_command: vec![],
            ..overseer_daemon::Config::default()
        },
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
/// and holds the connection open for 500 ms.
fn fake_daemon(
    home: &std::path::Path,
    reply: bool,
) -> (
    std::sync::mpsc::Receiver<Frame>,
    std::thread::JoinHandle<()>,
) {
    let answer = reply.then_some(Response::Ok);
    fake_daemon_with(home, answer, Duration::from_millis(500))
}

/// `fake_daemon` answering `answer`, or silent for `hold` when `None`.
fn fake_daemon_with(
    home: &std::path::Path,
    answer: Option<Response>,
    hold: Duration,
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
            match answer {
                Some(resp) => write_frame(&mut s, &Frame::Response { seq, resp })
                    .await
                    .unwrap(),
                None => tokio::time::sleep(hold).await,
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

const BASH: &[u8] = br#"{"session_id":"abc","hook_event_name":"PreToolUse","tool_name":"Bash","tool_use_id":"tu-1","cwd":"/tmp","tool_input":{"command":"git push"}}"#;

/// Runs `overseer hook PreToolUse` with `stdin`; checks it exits 0 with no
/// stderr and returns stdout and how long it took.
fn run_pre_tool_use(home: &std::path::Path, stdin: &[u8]) -> (String, Duration) {
    let _ = Command::new(env!("CARGO_BIN_EXE_overseer"))
        .arg("hook")
        .stdin(Stdio::null())
        .output();
    let start = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_overseer"))
        .args(["hook", "PreToolUse"])
        .env("OVERSEER_HOME", home)
        .env("OVERSEER_SLOT", "S1")
        .env("ARGOCD_SERVER", "uat")
        .env("NOT_ALLOWED", "x")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    let out = child.wait_with_output().unwrap();
    let took = start.elapsed();
    assert!(out.status.success(), "status {:?}", out.status);
    assert!(out.stderr.is_empty(), "stderr: {:?}", out.stderr);
    (String::from_utf8(out.stdout).unwrap(), took)
}

fn temp_home(tag: &str) -> std::path::PathBuf {
    let home = std::env::temp_dir().join(format!("ovhook-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    home
}

#[test]
fn pre_tool_use_allows_silently_without_a_daemon() {
    let home = temp_home("pnod");
    let (out, took) = run_pre_tool_use(&home, BASH);
    assert!(out.is_empty(), "stdout: {out:?}");
    assert!(took < Duration::from_millis(1500), "took {took:?}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn pre_tool_use_allows_when_the_daemon_never_answers() {
    let home = temp_home("psil");
    let (rx, t) = fake_daemon_with(&home, None, Duration::from_secs(3));
    let (out, took) = run_pre_tool_use(&home, BASH);
    assert!(out.is_empty(), "stdout: {out:?}");
    assert!(took < Duration::from_millis(1500), "took {took:?}");
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        Frame::Request {
            req: Request::CheckBash(_),
            ..
        }
    ));
    t.join().unwrap();
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn pre_tool_use_prints_the_deny() {
    let home = temp_home("pdeny");
    let reason = "Overseer: repo:demo-api:main is held by S2. Wait.";
    let answer = Response::Verdict {
        deny: Some(reason.into()),
        rewrite: None,
    };
    let (rx, t) = fake_daemon_with(&home, Some(answer), Duration::ZERO);
    let (out, _) = run_pre_tool_use(&home, BASH);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v,
        serde_json::json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }})
    );
    let Frame::Request {
        req: Request::CheckBash(check),
        ..
    } = rx.recv_timeout(Duration::from_secs(2)).unwrap()
    else {
        panic!("not a bash check");
    };
    assert_eq!(check.slot, 1);
    assert_eq!(check.command, "git push");
    assert_eq!(check.session_id.as_deref(), Some("abc"));
    assert_eq!(check.tool_use_id.as_deref(), Some("tu-1"));
    assert_eq!(check.cwd.as_deref(), Some(std::path::Path::new("/tmp")));
    // Only the allowlisted variables go.
    assert!(check.env.contains(&("ARGOCD_SERVER".into(), "uat".into())));
    assert!(check.env.iter().all(|(k, _)| k != "NOT_ALLOWED"));
    t.join().unwrap();
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn pre_tool_use_allows_when_the_daemon_allows() {
    let home = temp_home("pallow");
    let (rx, t) = fake_daemon_with(
        &home,
        Some(Response::Verdict {
            deny: None,
            rewrite: None,
        }),
        Duration::ZERO,
    );
    let (out, _) = run_pre_tool_use(&home, BASH);
    assert!(out.is_empty(), "stdout: {out:?}");
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
    t.join().unwrap();
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn pre_tool_use_ignores_other_tools() {
    let home = temp_home("pother");
    let answer = Response::Verdict {
        deny: Some("no".into()),
        rewrite: None,
    };
    // Never joined: it waits for a connection that must not come.
    let (rx, _t) = fake_daemon_with(&home, Some(answer), Duration::ZERO);
    let grep = br#"{"tool_name":"Grep","tool_use_id":"tu-2","tool_input":{"pattern":"a"}}"#;
    let (out, took) = run_pre_tool_use(&home, grep);
    assert!(out.is_empty(), "stdout: {out:?}");
    assert!(took < Duration::from_millis(200), "took {took:?}");
    assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn pre_tool_use_asks_about_file_writes_and_prints_the_deny() {
    let home = temp_home("pedit");
    let reason = "j.md is a shared 2ndBrain file. Use append_journal.";
    let answer = Response::Verdict {
        deny: Some(reason.into()),
        rewrite: None,
    };
    let (rx, t) = fake_daemon_with(&home, Some(answer), Duration::ZERO);
    let edit = br#"{"tool_name":"Edit","tool_use_id":"tu-2","cwd":"/w","tool_input":{"file_path":"/v/j.md"}}"#;
    let (out, _) = run_pre_tool_use(&home, edit);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
    assert_eq!(v["hookSpecificOutput"]["permissionDecisionReason"], reason);
    let Frame::Request {
        req: Request::CheckWrite(check),
        ..
    } = rx.recv_timeout(Duration::from_secs(2)).unwrap()
    else {
        panic!("not a write check");
    };
    assert_eq!(check.slot, 1);
    assert_eq!(check.tool, "Edit");
    assert_eq!(check.paths, [std::path::PathBuf::from("/v/j.md")]);
    t.join().unwrap();
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn pre_tool_use_prints_a_rewrite_as_updated_input() {
    let home = temp_home("prewrite");
    let answer = Response::Verdict {
        deny: None,
        rewrite: Some("overseer ctl bd -- close ov-3".into()),
    };
    let (_rx, t) = fake_daemon_with(&home, Some(answer), Duration::ZERO);
    let (out, _) = run_pre_tool_use(&home, BASH);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v,
        serde_json::json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow",
            "updatedInput": {"command": "overseer ctl bd -- close ov-3"},
        }})
    );
    t.join().unwrap();
    let _ = std::fs::remove_dir_all(&home);
}
