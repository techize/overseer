//! `overseer attach`: a raw-mode terminal client that mirrors one slot.

use std::io::{IsTerminal, Read, Write};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use crossterm::terminal;
use overseer_proto::{slot_name, Client, Event, Frame, Request, Response, SlotId, SlotState};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;

use crate::ctl;

/// Ctrl-] as a raw byte.
const DETACH_KEY: u8 = 0x1d;
/// Ctrl-] as a terminal using the kitty keyboard protocol reports it.
const DETACH_KITTY: &[u8] = b"\x1b[93;5u";
/// How long to wait for the daemon to acknowledge Detach or Resize.
const DETACH_GRACE: Duration = Duration::from_secs(1);

/// Returns the bytes before the detach key and whether it was seen. The key
/// is Ctrl-] (`0x1d`), or its kitty-protocol spelling `CSI 93;5u`, which a
/// terminal sends once the session has asked for enhanced keyboard reporting.
pub fn split_detach(input: &[u8]) -> (Vec<u8>, bool) {
    for i in 0..input.len() {
        if input[i] == DETACH_KEY || input[i..].starts_with(DETACH_KITTY) {
            return (input[..i].to_vec(), true);
        }
    }
    (input.to_vec(), false)
}

/// Restores the terminal on drop, so every exit path (including a panic
/// unwinding through `run`) leaves the user's shell usable. The session's TUI
/// may have switched on mouse reporting, bracketed paste, application cursor
/// keys, the kitty keyboard protocol or the alternate screen; those are
/// switched off again here.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> anyhow::Result<TerminalGuard> {
        terminal::enable_raw_mode().context("enable raw mode")?;
        Ok(TerminalGuard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let mut out = std::io::stdout();
        let _ = out.write_all(
            b"\x1b[<u\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1l\x1b>\x1b[?1049l\x1b[0m\x1b[?25h\n",
        );
        let _ = out.flush();
    }
}

/// Blocking stdin reader; the thread ends with the process.
fn spawn_stdin_reader() -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel(64);
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    rx
}

enum Ending {
    Detached,
    Exited(Option<i32>),
    Lost(anyhow::Error),
}

async fn slot_state(client: &Client, id: SlotId) -> anyhow::Result<SlotState> {
    match client.request(Request::ListSlots).await? {
        Response::Slots(slots) => slots
            .into_iter()
            .find(|s| s.id == id)
            .map(|s| s.state)
            .ok_or_else(|| anyhow!("invalid slot id {id}")),
        Response::Error { message } => bail!("{message}"),
        other => bail!("unexpected reply: {other:?}"),
    }
}

fn expect_ok(resp: Response) -> anyhow::Result<()> {
    match resp {
        Response::Ok => Ok(()),
        Response::Error { message } => bail!("{message}"),
        other => bail!("unexpected reply: {other:?}"),
    }
}

fn exited_message(id: SlotId, code: Option<i32>) -> String {
    match code {
        Some(c) => format!("{} exited with code {c}.", slot_name(id)),
        None => format!("{} exited.", slot_name(id)),
    }
}

pub async fn run(id: SlotId) -> anyhow::Result<()> {
    let name = slot_name(id);
    let client = std::sync::Arc::new(ctl::connect().await?);
    let mut frames = client.subscribe();

    match slot_state(&client, id).await? {
        SlotState::Empty => {
            eprintln!("{name} is empty. Start it with: overseer ctl start {id} --cwd <dir>");
            std::process::exit(1);
        }
        SlotState::Exited { code, .. } => bail!("{}", exited_message(id, code)),
        SlotState::Running { .. } => {}
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("attach needs an interactive terminal");
    }

    let (cols, rows) = terminal::size().context("read terminal size")?;
    expect_ok(client.request(Request::Resize { id, cols, rows }).await?)?;
    expect_ok(client.request(Request::Attach { id }).await?)?;
    // An exit between the first check and Attach would send no event.
    if let SlotState::Exited { code, .. } = slot_state(&client, id).await? {
        bail!("{}", exited_message(id, code));
    }

    let ending = {
        let _guard = TerminalGuard::enter()?;
        let ending = pump(id, &mut frames, &client).await;
        if matches!(ending, Ending::Detached) {
            let _ =
                tokio::time::timeout(DETACH_GRACE, client.request(Request::Detach { id })).await;
        }
        ending
    };

    match ending {
        Ending::Detached => {
            println!("Detached from {name}. The session is still running.");
            Ok(())
        }
        Ending::Exited(code) => {
            println!("{}", exited_message(id, code));
            Ok(())
        }
        Ending::Lost(e) => Err(e),
    }
}

async fn pump(id: SlotId, frames: &mut mpsc::Receiver<Frame>, client: &Client) -> Ending {
    let mut stdin = spawn_stdin_reader();
    let mut winch = match signal(SignalKind::window_change()) {
        Ok(s) => s,
        Err(e) => return Ending::Lost(anyhow!(e).context("watch for terminal resizes")),
    };
    let mut stdout = std::io::stdout();
    loop {
        tokio::select! {
            frame = frames.recv() => match frame {
                Some(Frame::Output { id: fid, data }) if fid == id => {
                    if let Err(e) = stdout.write_all(&data).and_then(|()| stdout.flush()) {
                        return Ending::Lost(anyhow!(e).context("write to terminal"));
                    }
                }
                Some(Frame::Event(Event::SlotChanged(info))) if info.id == id => {
                    if let SlotState::Exited { code, .. } = info.state {
                        return Ending::Exited(code);
                    }
                }
                Some(_) => {}
                None if client.inbox_overflowed() => {
                    return Ending::Lost(anyhow!(
                        "terminal fell too far behind the daemon, so the connection was dropped"
                    ));
                }
                None => return Ending::Lost(anyhow!("connection to the daemon was lost")),
            },
            bytes = stdin.recv() => match bytes {
                Some(bytes) => {
                    let (before, detach) = split_detach(&bytes);
                    // Queues the frame without awaiting the socket, so the select
                    // can never cancel a half-written frame (N7).
                    if !before.is_empty() && client.send_input(id, &before).await.is_err() {
                        return Ending::Lost(anyhow!("connection to the daemon was lost"));
                    }
                    if detach {
                        return Ending::Detached;
                    }
                }
                None => return Ending::Lost(anyhow!("terminal input closed")),
            },
            _ = winch.recv() => {
                if let Ok((cols, rows)) = terminal::size() {
                    let resize = client.request(Request::Resize { id, cols, rows });
                    let _ = tokio::time::timeout(DETACH_GRACE, resize).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_detach_none() {
        assert_eq!(split_detach(b"abc"), (b"abc".to_vec(), false));
    }

    #[test]
    fn split_detach_mid() {
        assert_eq!(split_detach(b"ab\x1dcd"), (b"ab".to_vec(), true));
    }

    #[test]
    fn split_detach_start_and_empty() {
        assert_eq!(split_detach(b"\x1d"), (Vec::new(), true));
        assert_eq!(split_detach(b""), (Vec::new(), false));
    }

    #[test]
    fn split_detach_kitty_spelling() {
        assert_eq!(split_detach(b"ab\x1b[93;5ucd"), (b"ab".to_vec(), true));
        assert_eq!(split_detach(b"\x1b[93;6u"), (b"\x1b[93;6u".to_vec(), false));
    }
}
