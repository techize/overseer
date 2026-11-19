//! Frames exchanged between a holder and the daemon.
//!
//! Wire format: a 4-byte big-endian length (covering the type byte plus the
//! payload), a 1-byte type, then the payload. Hello, DaemonHello, Resize and
//! Exit carry JSON; Output, Replay and Input carry raw bytes; Kill and
//! ExitSeen have no payload. Blocking std I/O.

use std::io::{self, Read, Write};
use std::path::PathBuf;

use overseer_proto::SlotId;
use serde::{Deserialize, Serialize};

pub const HOLDER_PROTOCOL: u32 = 1;
pub const SUPPORTED: &[u32] = &[1];
/// Largest accepted frame length (type byte plus payload).
pub const MAX_FRAME: usize = 1 << 20;

const T_HELLO: u8 = 1;
const T_DAEMON_HELLO: u8 = 2;
const T_REPLAY: u8 = 3;
const T_OUTPUT: u8 = 4;
const T_INPUT: u8 = 5;
const T_RESIZE: u8 = 6;
const T_KILL: u8 = 7;
const T_EXIT: u8 = 8;
const T_EXIT_SEEN: u8 = 9;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct HolderHello {
    pub version: u32,
    pub slot: SlotId,
    pub pid: u32,
    pub child_pid: u32,
    pub run: String,
    pub label: String,
    pub cwd: PathBuf,
    pub argv: Vec<String>,
    pub cols: u16,
    pub rows: u16,
    pub since: i64,
}

#[derive(Debug, PartialEq)]
pub enum Frame {
    /// Holder to daemon, first frame on every connection.
    Hello(HolderHello),
    DaemonHello {
        version: u32,
    },
    Replay(Vec<u8>),
    Output(Vec<u8>),
    Input(Vec<u8>),
    Resize {
        cols: u16,
        rows: u16,
    },
    Kill,
    Exit {
        code: Option<i32>,
        at: i64,
    },
    ExitSeen,
    /// Any other type; the payload is skipped.
    Unknown(u8),
}

#[derive(Serialize, Deserialize)]
struct DaemonHelloBody {
    version: u32,
}

#[derive(Serialize, Deserialize)]
struct ResizeBody {
    cols: u16,
    rows: u16,
}

#[derive(Serialize, Deserialize)]
struct ExitBody {
    code: Option<i32>,
    at: i64,
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn json<T: Serialize>(v: &T) -> io::Result<Vec<u8>> {
    serde_json::to_vec(v).map_err(|e| invalid(e.to_string()))
}

fn from_json<'a, T: Deserialize<'a>>(b: &'a [u8]) -> io::Result<T> {
    serde_json::from_slice(b).map_err(|e| invalid(e.to_string()))
}

pub fn write_frame(w: &mut impl Write, f: &Frame) -> io::Result<()> {
    let (ty, payload): (u8, Vec<u8>) = match f {
        Frame::Hello(h) => (T_HELLO, json(h)?),
        Frame::DaemonHello { version } => (
            T_DAEMON_HELLO,
            json(&DaemonHelloBody { version: *version })?,
        ),
        Frame::Replay(b) => (T_REPLAY, b.clone()),
        Frame::Output(b) => (T_OUTPUT, b.clone()),
        Frame::Input(b) => (T_INPUT, b.clone()),
        Frame::Resize { cols, rows } => (
            T_RESIZE,
            json(&ResizeBody {
                cols: *cols,
                rows: *rows,
            })?,
        ),
        Frame::Kill => (T_KILL, Vec::new()),
        Frame::Exit { code, at } => (
            T_EXIT,
            json(&ExitBody {
                code: *code,
                at: *at,
            })?,
        ),
        Frame::ExitSeen => (T_EXIT_SEEN, Vec::new()),
        Frame::Unknown(t) => (*t, Vec::new()),
    };
    let len = payload.len() + 1;
    if len > MAX_FRAME {
        return Err(invalid("frame too large"));
    }
    let mut buf = Vec::with_capacity(4 + len);
    buf.extend_from_slice(&(len as u32).to_be_bytes());
    buf.push(ty);
    buf.extend_from_slice(&payload);
    w.write_all(&buf)
}

/// Read one frame. `Ok(None)` means a clean EOF between frames.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Frame>> {
    let mut header = [0u8; 4];
    let mut got = 0;
    while got < header.len() {
        match r.read(&mut header[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let len = u32::from_be_bytes(header) as usize;
    if len == 0 {
        return Err(invalid("frame has no type byte"));
    }
    if len > MAX_FRAME {
        return Err(invalid("frame too large"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    let ty = body[0];
    let payload = &body[1..];
    let frame = match ty {
        T_HELLO => Frame::Hello(from_json(payload)?),
        T_DAEMON_HELLO => Frame::DaemonHello {
            version: from_json::<DaemonHelloBody>(payload)?.version,
        },
        T_REPLAY => Frame::Replay(payload.to_vec()),
        T_OUTPUT => Frame::Output(payload.to_vec()),
        T_INPUT => Frame::Input(payload.to_vec()),
        T_RESIZE => {
            let b: ResizeBody = from_json(payload)?;
            Frame::Resize {
                cols: b.cols,
                rows: b.rows,
            }
        }
        T_KILL => Frame::Kill,
        T_EXIT => {
            let b: ExitBody = from_json(payload)?;
            Frame::Exit {
                code: b.code,
                at: b.at,
            }
        }
        T_EXIT_SEEN => Frame::ExitSeen,
        other => Frame::Unknown(other),
    };
    Ok(Some(frame))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello() -> HolderHello {
        HolderHello {
            version: HOLDER_PROTOCOL,
            slot: 3,
            pid: 100,
            child_pid: 101,
            run: "demo-run".into(),
            label: "demo".into(),
            cwd: PathBuf::from("/tmp/demo"),
            argv: vec!["claude".into(), "--demo".into()],
            cols: 120,
            rows: 40,
            since: 1_700_000_000,
        }
    }

    fn round_trip(f: Frame) {
        let mut buf = Vec::new();
        write_frame(&mut buf, &f).unwrap();
        let back = read_frame(&mut buf.as_slice()).unwrap().unwrap();
        assert_eq!(back, f);
    }

    #[test]
    fn every_frame_round_trips() {
        round_trip(Frame::Hello(hello()));
        round_trip(Frame::DaemonHello { version: 1 });
        round_trip(Frame::Replay(b"replayed \x1b[0m bytes".to_vec()));
        round_trip(Frame::Output(vec![0, 1, 2, 255]));
        round_trip(Frame::Input(b"ls\r".to_vec()));
        round_trip(Frame::Resize { cols: 80, rows: 24 });
        round_trip(Frame::Kill);
        round_trip(Frame::Exit {
            code: Some(-1),
            at: 42,
        });
        round_trip(Frame::Exit { code: None, at: 7 });
        round_trip(Frame::ExitSeen);
    }

    #[test]
    fn an_unknown_type_is_skipped() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&4u32.to_be_bytes());
        buf.push(200);
        buf.extend_from_slice(b"abc");
        write_frame(&mut buf, &Frame::Output(b"x".to_vec())).unwrap();
        let mut r = buf.as_slice();
        assert_eq!(read_frame(&mut r).unwrap(), Some(Frame::Unknown(200)));
        assert_eq!(
            read_frame(&mut r).unwrap(),
            Some(Frame::Output(b"x".to_vec()))
        );
    }

    #[test]
    fn an_oversized_frame_is_an_error() {
        let header = ((MAX_FRAME + 1) as u32).to_be_bytes();
        let err = read_frame(&mut header.as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_zero_length_frame_is_an_error() {
        let header = 0u32.to_be_bytes();
        let err = read_frame(&mut header.as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn eof_between_frames_is_none() {
        let mut empty: &[u8] = &[];
        assert_eq!(read_frame(&mut empty).unwrap(), None);
        let mut cut: &[u8] = &[0, 0];
        let err = read_frame(&mut cut).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
