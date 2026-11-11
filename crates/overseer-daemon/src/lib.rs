//! Overseer daemon: owns the PTY slots and serves them over a Unix socket.

pub mod activity;
pub mod config;
pub mod env;
pub mod launch;
pub mod ledger;
pub mod manager;
pub mod ring;
pub mod server;
pub mod slot;
pub mod vt;
pub mod work;

pub use config::Config;
pub use env::EnvSource;
pub use server::{run, AlreadyRunning, DaemonConfig};
