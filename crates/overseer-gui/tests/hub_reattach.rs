//! N11: re-attaching a slot on the same connection (a view recreated for the
//! same slot) must deliver exactly the new snapshot followed by live output,
//! never stale live output from the earlier attach.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use overseer_daemon::{run, DaemonConfig};
use overseer_gui::FrameHub;
use overseer_proto::{Client, Request, Response};
use tokio::sync::mpsc;
use tokio::time::timeout;

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fake-claude.sh");

async fn attach(hub: &FrameHub, client: &Client, gen: u64) {
    assert_eq!(hub.attach(client, 1, gen).await.unwrap(), Response::Ok);
}

/// Reads `out` until it holds at least `want` bytes.
async fn collect(out: &mut mpsc::UnboundedReceiver<Bytes>, want: usize) -> Vec<u8> {
    let mut buf = Vec::new();
    timeout(Duration::from_secs(20), async {
        while buf.len() < want {
            match out.recv().await {
                Some(d) => buf.extend_from_slice(&d),
                None => panic!("route closed after {} bytes", buf.len()),
            }
        }
    })
    .await
    .expect("timed out collecting output");
    buf
}

/// Checks that the complete lines of `buf` (after the first, which the
/// snapshot may cut) are consecutive integers. Returns how many were checked.
fn assert_consecutive(buf: &[u8]) -> usize {
    let text = String::from_utf8_lossy(buf);
    let mut lines = text.split('\n');
    lines.next(); // possibly partial
    let mut prev: Option<u64> = None;
    let mut checked = 0;
    let all: Vec<&str> = lines.collect();
    // The last piece may be partial too.
    for line in &all[..all.len().saturating_sub(1)] {
        let n: u64 = line
            .trim_end_matches('\r')
            .parse()
            .unwrap_or_else(|_| panic!("not a number: {line:?}"));
        if let Some(p) = prev {
            assert_eq!(n, p + 1, "stream jumped from {p} to {n}");
        }
        prev = Some(n);
        checked += 1;
    }
    checked
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reattach_delivers_only_the_new_snapshot_and_live_output() {
    let dir = std::path::PathBuf::from(format!("/tmp/ovgr-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("s.sock");
    let task = tokio::spawn(run(DaemonConfig {
        socket: socket.clone(),
        default_command: vec![SCRIPT.to_string()],
    }));
    let client = Arc::new(Client::connect_or_start(&socket, || Ok(())).await.unwrap());
    let hub = FrameHub::start(&client, tokio::runtime::Handle::current());

    // Endless numbered lines, so any replayed or stale chunk shows as a jump.
    let start = Request::StartSlot {
        id: 1,
        label: "t".into(),
        cwd: "/tmp".into(),
        command: Some(vec!["/usr/bin/seq".into(), "1".into(), "1000000000".into()]),
    };
    assert_eq!(client.request(start).await.unwrap(), Response::Ok);

    // First view attaches and sees live output.
    let (gen_a, mut out_a) = hub.route(1);
    attach(&hub, &client, gen_a).await;
    collect(&mut out_a, 64 * 1024).await;

    // Recreate the view several times while output is streaming at full speed.
    for round in 0..5 {
        let (gen_b, mut out_b) = hub.route(1);
        attach(&hub, &client, gen_b).await;
        let buf = collect(&mut out_b, 512 * 1024).await;
        let checked = assert_consecutive(&buf);
        assert!(checked > 1000, "round {round}: only {checked} lines");
    }

    let _ = client.request(Request::Shutdown).await;
    let _ = timeout(Duration::from_secs(2), task).await;
    let _ = std::fs::remove_dir_all(&dir);
}
