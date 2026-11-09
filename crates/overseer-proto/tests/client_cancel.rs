use std::time::Duration;

use overseer_proto::{read_frame, write_frame, Client, Event, Frame, Request, Response, SlotId};
use overseer_proto::{SlotInfo, SlotState};
use tokio::net::UnixListener;
use tokio::time::timeout;

fn sock(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ovpc-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("s.sock")
}

fn info(id: SlotId) -> Event {
    Event::SlotChanged(SlotInfo {
        id,
        label: String::new(),
        cwd: None,
        command: None,
        state: SlotState::Empty,
        cols: 80,
        rows: 24,
    })
}

#[tokio::test]
async fn cancelled_request_leaves_stream_usable() {
    let path = sock("cancel");
    let listener = UnixListener::bind(&path).unwrap();
    // Server ignores the first request and answers every later one.
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut first = true;
        while let Ok(Some(frame)) = read_frame(&mut s).await {
            let Frame::Request { seq, .. } = frame else {
                continue;
            };
            if std::mem::take(&mut first) {
                continue;
            }
            write_frame(
                &mut s,
                &Frame::Response {
                    seq,
                    resp: Response::Ok,
                },
            )
            .await
            .unwrap();
        }
    });
    let client = Client::connect(&path).await.unwrap();
    let r = timeout(
        Duration::from_millis(150),
        client.request(Request::ListSlots),
    )
    .await;
    assert!(r.is_err(), "first request should time out");
    let r = timeout(Duration::from_secs(2), client.request(Request::Shutdown))
        .await
        .expect("second request timed out")
        .unwrap();
    assert_eq!(r, Response::Ok);
    // Input after a cancelled request still frames correctly.
    client.send_input(1, b"x").await.unwrap();
    let r = timeout(Duration::from_secs(2), client.request(Request::ListSlots))
        .await
        .expect("third request timed out")
        .unwrap();
    assert_eq!(r, Response::Ok);
}

#[tokio::test]
async fn inbox_overflow_ends_connection() {
    let path = sock("overflow");
    let listener = UnixListener::bind(&path).unwrap();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        // Wait for the in-flight request, never answer it, then flood events.
        let _ = read_frame(&mut s).await;
        for i in 0..20 {
            if write_frame(&mut s, &Frame::Event(info(i % 7)))
                .await
                .is_err()
            {
                break;
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    let client = Client::connect_with_inbox_cap(&path, 2).await.unwrap();
    // Nobody drains the inbox, so the flood overflows the cap of 2.
    let r = timeout(Duration::from_secs(3), client.request(Request::ListSlots))
        .await
        .expect("in-flight request must fail, not hang");
    assert!(r.is_err());
    assert!(client.inbox_overflowed());
    // The connection is over: later requests fail fast.
    let r = timeout(Duration::from_secs(1), client.request(Request::ListSlots))
        .await
        .expect("late request must fail fast");
    assert!(r.is_err());
    // Frames queued before the cut are still delivered, then the stream ends.
    let mut rx = client.subscribe();
    let mut n = 0;
    while timeout(Duration::from_secs(1), rx.recv())
        .await
        .unwrap()
        .is_some()
    {
        n += 1;
    }
    assert!(n >= 1 && n <= 2, "got {n} frames");
}
