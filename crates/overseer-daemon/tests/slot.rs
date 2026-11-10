use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use overseer_daemon::slot::Slot;
use tokio::sync::broadcast;
use tokio::time::timeout;

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fake-claude.sh");

fn spawn_script(id: u8) -> (Slot, broadcast::Receiver<Bytes>) {
    let (tx, rx) = broadcast::channel(1024);
    let slot = Slot::spawn(
        id,
        "test".into(),
        Path::new("/tmp"),
        &[SCRIPT.to_string()],
        80,
        24,
        tx,
    )
    .unwrap();
    (slot, rx)
}

/// Collect broadcast output until `needle` appears or `dur` elapses.
async fn collect_until(rx: &mut broadcast::Receiver<Bytes>, needle: &str, dur: Duration) -> String {
    let mut acc = String::new();
    let _ = timeout(dur, async {
        loop {
            match rx.recv().await {
                Ok(b) => {
                    acc.push_str(&String::from_utf8_lossy(&b));
                    if acc.contains(needle) {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
    .await;
    acc
}

#[tokio::test]
async fn slot_echoes_input() {
    let (slot, mut rx) = spawn_script(0);
    let out = collect_until(&mut rx, "READY", Duration::from_secs(2)).await;
    assert!(out.contains("READY"), "got {out:?}");
    slot.write(b"hi\r").unwrap();
    let out = collect_until(&mut rx, "ECHO:hi", Duration::from_secs(2)).await;
    assert!(out.contains("ECHO:hi"), "got {out:?}");
    slot.kill();
}

#[tokio::test]
async fn slot_reports_exit_code() {
    let (slot, _rx) = spawn_script(1);
    slot.write(b"quit\r").unwrap();
    let code = timeout(Duration::from_secs(2), slot.wait_exit())
        .await
        .unwrap();
    assert_eq!(code, Some(3));
    // Late callers still get the code.
    assert_eq!(slot.wait_exit().await, Some(3));
}

#[tokio::test]
async fn slot_sets_env() {
    let (tx, mut rx) = broadcast::channel(1024);
    let _slot = Slot::spawn(
        4,
        "env".into(),
        Path::new("/tmp"),
        &["/bin/sh".into(), "-c".into(), "echo $OVERSEER_SLOT".into()],
        80,
        24,
        tx,
    )
    .unwrap();
    let out = collect_until(&mut rx, "S4", Duration::from_secs(2)).await;
    assert!(out.contains("S4"), "got {out:?}");
}

#[tokio::test]
async fn ring_caps_flood() {
    let (slot, mut rx) = spawn_script(2);
    let drain = tokio::spawn(async move {
        while !matches!(rx.recv().await, Err(broadcast::error::RecvError::Closed)) {}
    });
    slot.write(b"flood\r").unwrap();
    let code = timeout(Duration::from_secs(20), slot.wait_exit())
        .await
        .unwrap();
    assert_eq!(code, Some(0));
    // Let the reader thread finish pushing the tail.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let snap = slot.snapshot();
    assert!(snap.len() <= 4 * 1024 * 1024, "len {}", snap.len());
    let tail = String::from_utf8_lossy(&snap[snap.len().saturating_sub(64)..]).into_owned();
    assert!(tail.contains("FLOOD-END"), "tail {tail:?}");
    drain.abort();
}

#[tokio::test]
async fn attach_mid_stream_has_no_gap_or_duplicate() {
    let (tx, _keep) = broadcast::channel(65536);
    drop(_keep);
    // ~3.4 MB of numbered lines, under the 4 MiB ring so nothing wraps.
    let slot = Slot::spawn(
        5,
        "seq".into(),
        Path::new("/tmp"),
        &[
            "/usr/bin/awk".into(),
            "BEGIN { for (i = 0; i < 500000; i++) print i }".into(),
        ],
        80,
        24,
        tx,
    )
    .unwrap();
    // Attach as soon as some output exists, i.e. mid-stream.
    let (snap, rx) = loop {
        let (snap, rx) = slot.attach();
        if !snap.is_empty() {
            break (snap, rx);
        }
        tokio::task::yield_now().await;
    };
    let mut rx = rx.expect("stream still open");
    let mut all = snap;
    timeout(Duration::from_secs(20), async {
        loop {
            match rx.recv().await {
                Ok(b) => all.extend_from_slice(&b),
                Err(broadcast::error::RecvError::Lagged(n)) => panic!("lagged {n}"),
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
    .await
    .unwrap();
    let text = String::from_utf8(all).unwrap();
    let mut expect = 0u32;
    for line in text.lines() {
        assert_eq!(
            line.trim().parse::<u32>().unwrap(),
            expect,
            "seam at {expect}"
        );
        expect += 1;
    }
    assert_eq!(expect, 500000);
}
