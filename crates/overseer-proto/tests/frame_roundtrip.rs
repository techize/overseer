use std::path::PathBuf;

use bytes::Bytes;
use overseer_proto::{
    paths, read_frame, write_frame, Event, Frame, Request, Response, SlotInfo, SlotState,
};

fn sample_info() -> SlotInfo {
    SlotInfo {
        id: 3,
        label: "api".into(),
        cwd: Some(PathBuf::from("/tmp/work")),
        command: Some(vec!["claude".into(), "--resume".into()]),
        state: SlotState::Running {
            pid: 42,
            since: 1_700_000_000,
        },
        cols: 120,
        rows: 40,
        activity: None,
    }
}

#[tokio::test]
async fn roundtrip_every_frame_kind() {
    let frames = vec![
        Frame::Request {
            seq: 1,
            req: Request::Hello {
                client: "cli".into(),
            },
        },
        Frame::Request {
            seq: 2,
            req: Request::ListSlots,
        },
        Frame::Request {
            seq: 3,
            req: Request::StartSlot {
                id: 2,
                label: "x".into(),
                cwd: PathBuf::from("/tmp"),
                command: None,
                prompt: None,
            },
        },
        Frame::Request {
            seq: 4,
            req: Request::Resize {
                id: 1,
                cols: 80,
                rows: 24,
            },
        },
        Frame::Request {
            seq: 5,
            req: Request::Shutdown,
        },
        Frame::Response {
            seq: 1,
            resp: Response::Ok,
        },
        Frame::Response {
            seq: 2,
            resp: Response::Slots(vec![sample_info()]),
        },
        Frame::Response {
            seq: 3,
            resp: Response::Error {
                message: "nope".into(),
            },
        },
        Frame::Event(Event::SlotChanged(sample_info())),
        Frame::Output {
            id: 6,
            data: Bytes::from_static(&[0x00, 0x1b, b'[', b'A', 0xff]),
        },
        Frame::Input {
            id: 0,
            data: Bytes::from_static(b"ls\r"),
        },
    ];
    let mut buf: Vec<u8> = Vec::new();
    for f in &frames {
        write_frame(&mut buf, f).await.unwrap();
    }
    let mut rd = &buf[..];
    for f in &frames {
        assert_eq!(read_frame(&mut rd).await.unwrap().as_ref(), Some(f));
    }
    assert_eq!(read_frame(&mut rd).await.unwrap(), None);
}

#[tokio::test]
async fn eof_returns_none() {
    let mut rd: &[u8] = &[];
    assert_eq!(read_frame(&mut rd).await.unwrap(), None);
}

#[tokio::test]
async fn oversize_frame_rejected() {
    let len: u32 = 9 * 1024 * 1024;
    let mut buf = len.to_be_bytes().to_vec();
    buf.push(3);
    let mut rd = &buf[..];
    let err = read_frame(&mut rd).await.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn overseer_home_override() {
    std::env::set_var("OVERSEER_HOME", "/tmp/x");
    assert_eq!(paths::socket_path(), PathBuf::from("/tmp/x/overseer.sock"));
    assert_eq!(paths::log_dir(), PathBuf::from("/tmp/x/logs"));
    assert_eq!(paths::state_dir(), PathBuf::from("/tmp/x"));
    std::env::remove_var("OVERSEER_HOME");
}

#[tokio::test]
async fn truncated_frame_is_unexpected_eof() {
    // Header promises 10 bytes, only 3 follow.
    let mut buf = 10u32.to_be_bytes().to_vec();
    buf.extend_from_slice(&[3, 1, 2]);
    let mut rd = &buf[..];
    let err = read_frame(&mut rd).await.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);

    // A partial length header is also truncation, not a clean EOF.
    let mut rd: &[u8] = &[0, 0];
    let err = read_frame(&mut rd).await.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
}

#[tokio::test]
async fn length_just_over_max_rejected() {
    let len = (overseer_proto::MAX_FRAME + 1) as u32;
    let mut buf = len.to_be_bytes().to_vec();
    buf.push(3);
    let mut rd = &buf[..];
    let err = read_frame(&mut rd).await.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

#[tokio::test]
async fn unknown_kind_rejected() {
    let mut buf = 2u32.to_be_bytes().to_vec();
    buf.extend_from_slice(&[99, 0]);
    let mut rd = &buf[..];
    let err = read_frame(&mut rd).await.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(err.to_string().contains("unknown frame kind"), "{err}");
}
