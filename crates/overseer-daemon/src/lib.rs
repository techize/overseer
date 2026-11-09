//! Overseer daemon: owns the PTY slots and serves them over a Unix socket.

pub mod manager;
pub mod ring;
pub mod server;
pub mod slot;

pub use server::{run, DaemonConfig};
