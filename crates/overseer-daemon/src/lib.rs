//! Overseer daemon: owns the PTY slots and serves them over a Unix socket.

pub mod activity;
pub mod attention;
pub mod brain;
pub mod config;
pub mod coord;
pub mod env;
pub mod holder;
pub mod launch;
pub mod ledger;
pub mod locks;
pub mod manager;
pub mod notes;
pub mod pty;
pub mod relay;
pub mod ring;
pub mod server;
pub mod session_logs;
pub mod settings;
pub mod slot;
pub mod stretches;
pub mod timeline;
pub mod trust;
pub mod vt;
pub mod work;

pub use config::Config;
pub use env::EnvSource;
pub use server::{run, run_until, stop_signal, AlreadyRunning, DaemonConfig};
pub use settings::SettingsFiles;
