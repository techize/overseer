//! `overseer hook` must be silent and fast whether or not a daemon is there.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use overseer_daemon::{run, DaemonConfig};
use overseer_proto::Client;

const STOP: &[u8] = br#"{"session_id":"abc","hook_event_name":"Stop"}"#;

fn run_hook(home: &std::path::Path) {
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
