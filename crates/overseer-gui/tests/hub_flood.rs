//! A live output burst reaches a view's route intact, without the daemon
//! dropping the GUI connection as a slow client.

use std::sync::Arc;
use std::time::Duration;

use overseer_daemon::{run, DaemonConfig};
use overseer_gui::FrameHub;
use overseer_proto::{Client, Request, Response};
use tokio::time::timeout;

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fake-claude.sh");

/// Runs `command` in slot 1 behind a hub route, types `input`, and returns
/// the byte count once `end` arrives.
async fn burst(tag: &str, command: Option<Vec<String>>, input: &[u8], end: &str) -> usize {
    // Short path: Unix socket paths are limited to 104 bytes on macOS.
    let dir = std::path::PathBuf::from(format!("/tmp/ovgf-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("s.sock");
    let task = tokio::spawn(run(DaemonConfig {
        socket: socket.clone(),
        default_command: vec![SCRIPT.to_string()],
    }));
    let client = Arc::new(Client::connect_or_start(&socket, || Ok(())).await.unwrap());
    let hub = FrameHub::start(&client, tokio::runtime::Handle::current());
    let (gen, mut out) = hub.route(1);

    let start = Request::StartSlot {
        id: 1,
        label: "t".into(),
        cwd: "/tmp".into(),
        command,
    };
    assert_eq!(client.request(start).await.unwrap(), Response::Ok);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(hub.attach(&client, 1, gen).await.unwrap(), Response::Ok);
    if !input.is_empty() {
        client.send_input(1, input).await.unwrap();
    }

    let mut total = 0usize;
    let mut tail = Vec::new();
    let done = timeout(Duration::from_secs(20), async {
        while let Some(data) = out.recv().await {
            total += data.len();
            tail.extend_from_slice(&data);
            if tail.len() > 64 {
                tail.drain(..tail.len() - 64);
            }
            if String::from_utf8_lossy(&tail).contains(end) {
                return true;
            }
        }
        false
    })
    .await
    .expect("timed out waiting for the burst");
    assert!(
        done,
        "route closed after {total} bytes (overflowed: {})",
        client.inbox_overflowed()
    );

    let _ = client.request(Request::Shutdown).await;
    let _ = timeout(Duration::from_secs(2), task).await;
    let _ = std::fs::remove_dir_all(&dir);
    total
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn block_buffered_flood_reaches_route() {
    let total = burst("block", None, b"flood\r", "FLOOD-END").await;
    assert!(total > 6 * 1024 * 1024, "only {total} bytes");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn line_at_a_time_burst_reaches_route() {
    let cmd = "sleep 0.5; seq 1 1000000; echo BURST-END";
    let command = vec!["/bin/sh".into(), "-c".into(), cmd.into()];
    let total = burst("lines", Some(command), b"", "BURST-END").await;
    assert!(total > 6_000_000, "only {total} bytes");
}
