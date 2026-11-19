//! The session holder: a small process that owns one Claude PTY so the
//! session survives a daemon restart. This module holds the wire format the
//! holder and the daemon speak over a Unix socket.

pub mod frame;

pub use frame::{
    read_frame, write_frame, Frame, HolderHello, HOLDER_PROTOCOL, MAX_FRAME, SUPPORTED,
};
