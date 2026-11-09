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
    assert!((1..=2).contains(&n), "got {n} frames");
}

#[tokio::test]
async fn marked_reply_is_ordered_in_the_stream() {
    let path = sock("marked");
    let listener = UnixListener::bind(&path).unwrap();
    // Each request gets: output "before", its reply, output "after".
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        while let Ok(Some(frame)) = read_frame(&mut s).await {
            let Frame::Request { seq, .. } = frame else {
                continue;
            };
            let out = |data: &'static [u8]| Frame::Output {
                id: 1,
                data: bytes::Bytes::from_static(data),
            };
            write_frame(&mut s, &out(b"before")).await.unwrap();
            let resp = Response::Ok;
            write_frame(&mut s, &Frame::Response { seq, resp })
                .await
                .unwrap();
            write_frame(&mut s, &out(b"after")).await.unwrap();
        }
    });
    let client = Client::connect(&path).await.unwrap();
    let mut rx = client.subscribe();
    let mut seen_seq = None;
    let r = client
        .request_marked(Request::Attach { id: 1 }, |s| {
            seen_seq = Some(s);
            true
        })
        .await
        .unwrap();
    assert_eq!(r, Response::Ok);
    // A plain request puts no reply into the stream.
    client.request(Request::ListSlots).await.unwrap();

    let mut got = Vec::new();
    for _ in 0..5 {
        let f = timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        got.push(match f {
            Frame::Output { data, .. } => String::from_utf8(data.to_vec()).unwrap(),
            Frame::Response { seq, resp } => {
                assert_eq!(Some(seq), seen_seq);
                assert_eq!(resp, Response::Ok);
                "reply".into()
            }
            other => panic!("unexpected {other:?}"),
        });
    }
    assert_eq!(got, ["before", "reply", "after", "before", "after"]);
}

#[tokio::test]
async fn declined_marked_request_is_never_sent() {
    let path = sock("declined");
    let listener = UnixListener::bind(&path).unwrap();
    let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        while let Ok(Some(frame)) = read_frame(&mut s).await {
            let Frame::Request { seq, req } = frame else {
                continue;
            };
            let _ = seen_tx.send(req);
            let resp = Response::Ok;
            write_frame(&mut s, &Frame::Response { seq, resp })
                .await
                .unwrap();
        }
    });
    let client = Client::connect(&path).await.unwrap();
    let mut rx = client.subscribe();
    let err = client
        .request_marked(Request::Attach { id: 1 }, |_| false)
        .await
        .unwrap_err();
    assert!(err.is::<overseer_proto::client::Cancelled>(), "{err:#}");
    // The next request is the first thing the server sees.
    client.request(Request::ListSlots).await.unwrap();
    assert_eq!(seen_rx.recv().await, Some(Request::ListSlots));
    assert!(seen_rx.try_recv().is_err());
    // And no mark for the declined request reached the stream.
    assert!(timeout(Duration::from_millis(100), rx.recv())
        .await
        .is_err());
}
