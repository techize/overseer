use std::io;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::msg::{Event, Request, Response, SlotId};

/// Largest accepted frame body (kind byte plus payload), 8 MiB.
pub const MAX_FRAME: usize = 8 * 1024 * 1024;

const KIND_REQUEST: u8 = 0;
const KIND_RESPONSE: u8 = 1;
const KIND_EVENT: u8 = 2;
const KIND_OUTPUT: u8 = 3;
const KIND_INPUT: u8 = 4;

#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Request { seq: u32, req: Request },
    Response { seq: u32, resp: Response },
    Event(Event),
    Output { id: SlotId, data: Bytes },
    Input { id: SlotId, data: Bytes },
}

#[derive(Serialize)]
struct ReqOut<'a> {
    seq: u32,
    req: &'a Request,
}
#[derive(Deserialize)]
struct ReqIn {
    seq: u32,
    req: Request,
}
#[derive(Serialize)]
struct RespOut<'a> {
    seq: u32,
    resp: &'a Response,
}
#[derive(Deserialize)]
struct RespIn {
    seq: u32,
    resp: Response,
}

fn invalid(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

fn json<T: Serialize>(v: &T) -> io::Result<Vec<u8>> {
    serde_json::to_vec(v).map_err(invalid)
}

fn encode(frame: &Frame) -> io::Result<(u8, Vec<u8>)> {
    Ok(match frame {
        Frame::Request { seq, req } => (KIND_REQUEST, json(&ReqOut { seq: *seq, req })?),
        Frame::Response { seq, resp } => (KIND_RESPONSE, json(&RespOut { seq: *seq, resp })?),
        Frame::Event(ev) => (KIND_EVENT, json(ev)?),
        Frame::Output { id, data } | Frame::Input { id, data } => {
            let kind = if matches!(frame, Frame::Output { .. }) {
                KIND_OUTPUT
            } else {
                KIND_INPUT
            };
            let mut p = Vec::with_capacity(1 + data.len());
            p.push(*id);
            p.extend_from_slice(data);
            (kind, p)
        }
    })
}

fn decode(kind: u8, payload: &[u8]) -> io::Result<Frame> {
    match kind {
        KIND_REQUEST => {
            let r: ReqIn = serde_json::from_slice(payload).map_err(invalid)?;
            Ok(Frame::Request {
                seq: r.seq,
                req: r.req,
            })
        }
        KIND_RESPONSE => {
            let r: RespIn = serde_json::from_slice(payload).map_err(invalid)?;
            Ok(Frame::Response {
                seq: r.seq,
                resp: r.resp,
            })
        }
        KIND_EVENT => Ok(Frame::Event(
            serde_json::from_slice(payload).map_err(invalid)?,
        )),
        KIND_OUTPUT | KIND_INPUT => {
            let (id, data) = payload
                .split_first()
                .ok_or_else(|| invalid("missing slot id"))?;
            let data = Bytes::copy_from_slice(data);
            Ok(if kind == KIND_OUTPUT {
                Frame::Output { id: *id, data }
            } else {
                Frame::Input { id: *id, data }
            })
        }
        k => Err(invalid(format!("unknown frame kind {k}"))),
    }
}

/// Writes one frame: u32 BE length, u8 kind, payload.
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, frame: &Frame) -> io::Result<()> {
    let (kind, payload) = encode(frame)?;
    let len = 1 + payload.len();
    if len > MAX_FRAME {
        return Err(invalid("frame too large"));
    }
    let mut buf = Vec::with_capacity(4 + len);
    buf.extend_from_slice(&(len as u32).to_be_bytes());
    buf.push(kind);
    buf.extend_from_slice(&payload);
    w.write_all(&buf).await?;
    w.flush().await
}

/// Reads one frame. `Ok(None)` is a clean EOF at a frame boundary.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Option<Frame>> {
    let mut hdr = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        let n = r.read(&mut hdr[got..]).await?;
        if n == 0 {
            return if got == 0 {
                Ok(None)
            } else {
                Err(io::ErrorKind::UnexpectedEof.into())
            };
        }
        got += n;
    }
    let len = u32::from_be_bytes(hdr) as usize;
    if len > MAX_FRAME {
        return Err(invalid("frame exceeds 8 MiB"));
    }
    if len == 0 {
        return Err(invalid("empty frame"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    decode(body[0], &body[1..]).map(Some)
}
