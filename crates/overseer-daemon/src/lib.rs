//! Overseer daemon: owns the PTY slots and serves them over a Unix socket.

pub mod env;
pub mod ledger;
pub mod manager;
pub mod ring;
pub mod server;
pub mod slot;
pub mod vt;

pub use env::EnvSource;
pub use server::{run, AlreadyRunning, DaemonConfig};
