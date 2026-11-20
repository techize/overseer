//! The terminal half of a slot: a child on a PTY, its output pump and its
//! exit watcher. Knows nothing about rings, screens or broadcasts; the
//! owner passes an output callback.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context};
use overseer_proto::{slot_name, SlotId};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

/// `Some(Some(code))` or `Some(None)` (killed by signal) once exited.
type ExitValue = Option<Option<i32>>;

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Called on the reader thread with each chunk of output.
pub type OnOutput = Box<dyn FnMut(&[u8]) + Send>;

/// Everything needed to start a session's terminal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnSpec {
    pub slot: SlotId,
    pub label: String,
    pub cwd: PathBuf,
    pub argv: Vec<String>,
    /// The child's whole environment, already built (see [`session_env`]).
    pub env: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
    /// This spawn's `OVERSEER_RUN`.
    pub run: String,
}

/// The environment a session starts with: `env` less Overseer's API tokens
/// (`env::TOKEN_VARS`), plus `OVERSEER_SLOT`, `OVERSEER_RUN`, `TERM` and
/// `COLORTERM`. A variable that is not valid UTF-8 is skipped with a warning.
pub fn session_env(env: &[(OsString, OsString)], slot: SlotId, run: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = crate::env::for_session(env)
        .into_iter()
        .filter_map(|(k, v)| match (k.into_string(), v.into_string()) {
            (Ok(k), Ok(v)) => Some((k, v)),
            (Ok(k), Err(_)) => {
                tracing::warn!("skipping environment variable {k}: value is not UTF-8");
                None
            }
            (Err(k), _) => {
                tracing::warn!("skipping environment variable {k:?}: name is not UTF-8");
                None
            }
        })
        .collect();
    out.push(("OVERSEER_SLOT".into(), slot_name(slot)));
    out.push(("OVERSEER_RUN".into(), run.to_string()));
    out.push(("TERM".into(), "xterm-256color".into()));
    out.push(("COLORTERM".into(), "truecolor".into()));
    out
}

/// A child on a PTY.
pub struct LocalPty {
    pid: u32,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    exit_rx: watch::Receiver<ExitValue>,
    exited_at: Arc<Mutex<Option<i64>>>,
}

impl LocalPty {
    /// Starts `spec.argv` with exactly `spec.env`. `on_output` runs on the
    /// reader thread for every chunk of output; the thread drops it when the
    /// stream ends, so dropping it is how the owner learns of the end.
    pub fn spawn(spec: &SpawnSpec, mut on_output: OnOutput) -> anyhow::Result<LocalPty> {
        let (program, args) = spec
            .argv
            .split_first()
            .ok_or_else(|| anyhow!("empty command"))?;
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: spec.rows,
                cols: spec.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("open pty")?;

        let mut cmd = CommandBuilder::new(program);
        cmd.args(args);
        cmd.cwd(&spec.cwd);
        cmd.env_clear();
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("spawn {program}"))?;
        drop(pair.slave);

        let pid = child.process_id().unwrap_or(0);
        let killer = child.clone_killer();
        let mut reader = pair.master.try_clone_reader().context("clone pty reader")?;
        let writer = pair.master.take_writer().context("take pty writer")?;

        std::thread::Builder::new()
            .name(format!("pty-read-{}", slot_name(spec.slot)))
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => on_output(&buf[..n]),
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
            })
            .context("spawn reader thread")?;

        let (exit_tx, exit_rx) = watch::channel::<ExitValue>(None);
        let exited_at = Arc::new(Mutex::new(None));
        let at = exited_at.clone();
        std::thread::Builder::new()
            .name(format!("pty-wait-{}", slot_name(spec.slot)))
            .spawn(move || {
                let code = match child.wait() {
                    Ok(status) => {
                        let c = status.exit_code() as i32;
                        // portable-pty reports signal deaths as a signal name with code 1.
                        if status.signal().is_some() {
                            None
                        } else {
                            Some(c)
                        }
                    }
                    Err(e) => {
                        tracing::warn!("wait on child failed: {e}");
                        None
                    }
                };
                *at.lock().unwrap() = Some(now_secs());
                let _ = exit_tx.send(Some(code));
            })
            .context("spawn wait thread")?;

        Ok(LocalPty {
            pid,
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            killer: Mutex::new(killer),
            exit_rx,
            exited_at,
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn write(&self, data: &[u8]) -> io::Result<()> {
        let mut w = self.writer.lock().unwrap();
        w.write_all(data)?;
        w.flush()
    }

    pub fn resize(&self, cols: u16, rows: u16) -> anyhow::Result<()> {
        self.master
            .lock()
            .unwrap()
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("resize pty")
    }

    /// Kill the child. Does nothing harmful if it has already exited.
    pub fn kill(&self) {
        if let Err(e) = self.killer.lock().unwrap().kill() {
            tracing::debug!("kill: {e}");
        }
    }

    /// When the child exited, in Unix seconds, once it has. Set before
    /// [`LocalPty::exit`] reports the exit.
    pub fn exited_at(&self) -> Option<i64> {
        *self.exited_at.lock().unwrap()
    }

    /// `Some(Some(code))` or `Some(None)` (killed by signal) once exited.
    pub fn exit(&self) -> watch::Receiver<Option<Option<i32>>> {
        self.exit_rx.clone()
    }
}
