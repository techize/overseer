//! The seven slots (S0 to S6) and their lifecycle.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use overseer_proto::{slot_name, Event, SlotId, SlotInfo, SlotState};
use tokio::sync::broadcast;

use crate::env::BaseEnv;
use crate::launch::{claude_argv, is_claude, write_slot_files, Start};
use crate::ledger::{Ledger, SessionRow};
use crate::slot::Slot;

pub const SLOT_COUNT: usize = 7;
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;
/// Chunks buffered per output subscriber before it starts lagging.
const OUTPUT_CHANNEL_CAP: usize = 1024;
/// How long to wait for the output stream to end after the child exits.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// N2: slot ids are validated here, in the daemon layer.
pub fn valid_id(id: SlotId) -> bool {
    (id as usize) < SLOT_COUNT
}

fn invalid_id(id: SlotId) -> String {
    format!("invalid slot id {id} (expected 0..={})", SLOT_COUNT - 1)
}

struct Entry {
    slot: Arc<Slot>,
}

type Slots = Arc<Mutex<[Option<Entry>; SLOT_COUNT]>>;

pub struct SlotManager {
    /// Shared with the exit monitors so they can tell whether their slot is
    /// still the current occupant before announcing anything.
    slots: Slots,
    default_command: Vec<String>,
    events: broadcast::Sender<Event>,
    /// Environment every slot starts from.
    env: BaseEnv,
    /// Where generated files go and the ledger lives; `None` runs commands
    /// as given and records nothing.
    launch: Option<Launch>,
    /// Set before daemon shutdown kills the children, so their exits are not
    /// recorded and those sessions stay live for resume.
    shutting_down: Arc<AtomicBool>,
}

#[derive(Clone)]
struct Launch {
    state_dir: PathBuf,
    /// The binary the generated hooks call.
    hook_program: PathBuf,
    ledger: Arc<Ledger>,
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// What a client needs to attach to a slot.
pub struct Attachment {
    pub slot: Arc<Slot>,
    /// Live output, gap-free and duplicate-free after `snapshot`. `None` when the
    /// slot's output stream has already ended (the snapshot is all there is).
    pub live: Option<broadcast::Receiver<Bytes>>,
    pub snapshot: Vec<u8>,
    /// Stream position `live` starts at, for catching up after a lag.
    pub pos: u64,
}

impl SlotManager {
    pub fn new(default_command: Vec<String>, events: broadcast::Sender<Event>) -> Self {
        SlotManager {
            slots: Default::default(),
            default_command,
            events,
            env: BaseEnv::ready(crate::env::own_env()),
            launch: None,
            shutting_down: Default::default(),
        }
    }

    /// Generates claude's per-slot files under `state_dir` and records every
    /// start (and exit) in `ledger`.
    pub fn with_launch(
        mut self,
        state_dir: PathBuf,
        hook_program: PathBuf,
        ledger: Arc<Ledger>,
    ) -> Self {
        self.launch = Some(Launch {
            state_dir,
            hook_program,
            ledger,
        });
        self
    }

    /// Uses `env` as the base environment for slots started from now on.
    pub fn with_env(mut self, env: BaseEnv) -> Self {
        self.env = env;
        self
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn start(
        &mut self,
        id: SlotId,
        label: String,
        cwd: &Path,
        command: Option<Vec<String>>,
    ) -> Result<(), String> {
        if !valid_id(id) {
            return Err(invalid_id(id));
        }
        // I4: portable-pty quietly falls back to $HOME for a cwd that is not
        // a directory, which would run the session somewhere else than every
        // client shows. The daemon is the source of truth, so it checks.
        if !cwd.is_dir() {
            return Err(format!(
                "working directory {} does not exist or is not a directory",
                cwd.display()
            ));
        }
        // Held through replace and event send so a monitor's identity check
        // cannot interleave with them.
        let mut slots = self.slots.lock().unwrap();
        if let Some(entry) = &slots[id as usize] {
            if matches!(entry.slot.info().state, SlotState::Running { .. }) {
                return Err(format!("{} is already running", slot_name(id)));
            }
        }
        let command = command.unwrap_or_else(|| self.default_command.clone());
        // Claude gets Overseer's hooks, role prompt and a session id of our
        // choosing; anything else runs as given and has no session id.
        let mut session_id = String::new();
        let mut argv = command.clone();
        if let Some(launch) = self.launch.as_ref().filter(|_| is_claude(&command)) {
            let files = write_slot_files(&launch.state_dir, id, &label, &launch.hook_program)
                .map_err(|e| format!("cannot write {} launch files: {e}", slot_name(id)))?;
            session_id = uuid::Uuid::new_v4().to_string();
            argv = claude_argv(&command, id, &label, &files, Start::New(session_id.clone()));
        }
        let row_label = label.clone();
        let (tx, monitor_rx) = broadcast::channel(OUTPUT_CHANNEL_CAP);
        let env = self.env.get();
        let slot = Slot::spawn_in(id, label, cwd, &argv, DEFAULT_COLS, DEFAULT_ROWS, tx, &env)
            .map_err(|e| {
                tracing::warn!("{} failed to start {:?}: {e:#}", slot_name(id), argv);
                format!("{e:#}")
            })?;
        let slot = Arc::new(slot);
        if let SlotState::Running { pid, .. } = slot.info().state {
            tracing::info!(
                "{} started {:?} in {} (pid {pid})",
                slot_name(id),
                command,
                cwd.display()
            );
        }
        // N4: tear down whatever occupied the slot before (an exited child;
        // kill is a no-op then, but keeps the invariant in one place).
        if let Some(old) = slots[id as usize].replace(Entry { slot: slot.clone() }) {
            old.slot.kill();
        }
        // Ledger writes are single-row SQLite statements done synchronously
        // here: the start path already forks a process synchronously, so a
        // spawn_blocking hop would buy nothing. The base command is recorded
        // (not the flagged argv) so a resume can rebuild its own flags.
        if let Some(launch) = &self.launch {
            let row = SessionRow {
                slot: id,
                session_id,
                label: row_label,
                cwd: cwd.to_path_buf(),
                command: command.clone(),
                started: unix_now(),
            };
            if let Err(e) = launch.ledger.record_start(&row) {
                tracing::warn!("{} ledger start not recorded: {e:#}", slot_name(id));
            }
        }
        let _ = self.events.send(Event::SlotChanged(slot.info()));
        drop(slots);
        tokio::spawn(monitor_exit(
            slot,
            monitor_rx,
            self.events.clone(),
            self.slots.clone(),
            self.launch.as_ref().map(|l| l.ledger.clone()),
            self.shutting_down.clone(),
        ));
        Ok(())
    }

    pub fn stop(&self, id: SlotId) -> Result<(), String> {
        if !valid_id(id) {
            return Err(invalid_id(id));
        }
        match &self.slots.lock().unwrap()[id as usize] {
            Some(e) if matches!(e.slot.info().state, SlotState::Running { .. }) => {
                e.slot.kill();
                Ok(())
            }
            _ => Err(format!("{} is not running", slot_name(id))),
        }
    }

    pub fn list(&self) -> Vec<SlotInfo> {
        let slots = self.slots.lock().unwrap();
        (0..SLOT_COUNT as u8)
            .map(|id| match &slots[id as usize] {
                Some(e) => e.slot.info(),
                None => SlotInfo {
                    id,
                    label: String::new(),
                    cwd: None,
                    command: None,
                    state: SlotState::Empty,
                    cols: DEFAULT_COLS,
                    rows: DEFAULT_ROWS,
                },
            })
            .collect()
    }

    /// The slot, if the id is valid and occupied.
    pub fn slot(&self, id: SlotId) -> Option<Arc<Slot>> {
        self.slots
            .lock()
            .unwrap()
            .get(id as usize)?
            .as_ref()
            .map(|e| e.slot.clone())
    }

    pub fn attach(&self, id: SlotId) -> Result<Attachment, String> {
        if !valid_id(id) {
            return Err(invalid_id(id));
        }
        let slots = self.slots.lock().unwrap();
        let entry = slots[id as usize]
            .as_ref()
            .ok_or_else(|| format!("{} is empty", slot_name(id)))?;
        let (snapshot, pos, live) = entry.slot.attach_at();
        Ok(Attachment {
            slot: entry.slot.clone(),
            live,
            snapshot,
            pos,
        })
    }

    /// N4: kill every child. Called on daemon shutdown.
    pub fn shutdown_all(&mut self) {
        // Before any kill: the exit monitors must see it when the children die.
        self.shutting_down.store(true, Ordering::SeqCst);
        for entry in self
            .slots
            .lock()
            .unwrap()
            .iter_mut()
            .filter_map(Option::take)
        {
            entry.slot.kill();
        }
    }
}

/// Emits `SlotChanged` once the child has exited AND its output stream has
/// ended (N5). `Slot::wait_exit` can fire before the reader thread has pushed
/// the final bytes, so we wait for the output channel to close (the reader
/// thread dropping its sender) before announcing the exit. Clients that attach
/// after the event therefore see the complete tail in the snapshot. The wait is
/// bounded: a grandchild that inherited the PTY could keep the stream open.
async fn monitor_exit(
    slot: Arc<Slot>,
    mut rx: broadcast::Receiver<Bytes>,
    events: broadcast::Sender<Event>,
    slots: Slots,
    ledger: Option<Arc<Ledger>>,
    shutting_down: Arc<AtomicBool>,
) {
    slot.wait_exit().await;
    let _ = tokio::time::timeout(DRAIN_TIMEOUT, async {
        loop {
            match rx.recv().await {
                Err(broadcast::error::RecvError::Closed) => break,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
            }
        }
    })
    .await;
    // The drain can take up to DRAIN_TIMEOUT, and `start` treats an exited
    // slot as replaceable, so this slot may have been restarted meanwhile. A
    // stale Exited would make clients show the new slot as exited. Check
    // identity and send under the same lock `start` uses to replace and
    // announce.
    let info = slot.info();
    if let SlotState::Exited { code, .. } = &info.state {
        tracing::info!("{} exited (code {code:?})", slot_name(info.id));
    }
    let slots = slots.lock().unwrap();
    let current = slots[info.id as usize]
        .as_ref()
        .is_some_and(|e| Arc::ptr_eq(&e.slot, &slot));
    if current {
        // A daemon shutdown kills the children too, but those sessions must
        // stay live in the ledger so they can be resumed. Synchronous for the
        // same reason as the start write.
        if let (Some(ledger), SlotState::Exited { code, at }) = (&ledger, &info.state) {
            if !shutting_down.load(Ordering::SeqCst) {
                if let Err(e) = ledger.record_exit(info.id, *code, *at) {
                    tracing::warn!("{} ledger exit not recorded: {e:#}", slot_name(info.id));
                }
            }
        }
        let _ = events.send(Event::SlotChanged(info));
    } else {
        tracing::debug!("dropping stale exit event for replaced slot");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reproduces the window between "slot reads as Exited" (so `start` may
    /// replace it) and "its monitor finished draining" (so it announces the
    /// exit). The drain is held open by keeping the output sender alive.
    #[tokio::test]
    async fn restart_during_drain_drops_stale_exit_event() {
        let (events, mut ev_rx) = broadcast::channel(16);
        let mut mgr = SlotManager::new(vec!["/bin/sleep".into(), "30".into()], events.clone());

        // Old occupant: exits at once, but its output channel never closes.
        let (tx, rx) = broadcast::channel(16);
        let old = Arc::new(
            Slot::spawn(
                1,
                "old".into(),
                Path::new("/tmp"),
                &["/bin/sh".into(), "-c".into(), "exit 0".into()],
                80,
                24,
                tx.clone(),
            )
            .unwrap(),
        );
        mgr.slots.lock().unwrap()[1] = Some(Entry { slot: old.clone() });
        old.wait_exit().await;
        tokio::spawn(monitor_exit(
            old.clone(),
            rx,
            events,
            mgr.slots.clone(),
            None,
            Default::default(),
        ));

        // The old slot reads as Exited, so a restart is allowed mid-drain.
        mgr.start(1, "new".into(), Path::new("/tmp"), None).unwrap();

        // Wait past the drain timeout; the old monitor must stay silent.
        tokio::time::sleep(DRAIN_TIMEOUT + Duration::from_millis(500)).await;
        drop(tx);
        let mut seen = Vec::new();
        while let Ok(Event::SlotChanged(info)) = ev_rx.try_recv() {
            seen.push(info.state);
        }
        assert_eq!(seen.len(), 1, "events: {seen:?}");
        assert!(matches!(seen[0], SlotState::Running { .. }), "{seen:?}");
        assert!(matches!(mgr.list()[1].state, SlotState::Running { .. }));
        mgr.shutdown_all();
    }

    #[tokio::test]
    async fn start_rejects_a_missing_cwd() {
        let (events, mut ev_rx) = broadcast::channel(16);
        let mut mgr = SlotManager::new(vec!["/bin/sleep".into(), "30".into()], events);
        let missing = Path::new("/tmp/overseer-no-such-dir-for-test");
        let err = mgr.start(2, "x".into(), missing, None).unwrap_err();
        assert!(
            err.contains("does not exist or is not a directory"),
            "{err}"
        );
        assert!(err.contains("overseer-no-such-dir-for-test"), "{err}");
        // A file is not a directory either.
        let err = mgr
            .start(2, "x".into(), Path::new("/bin/sh"), None)
            .unwrap_err();
        assert!(err.contains("not a directory"), "{err}");
        assert_eq!(mgr.list()[2].state, SlotState::Empty);
        assert!(ev_rx.try_recv().is_err(), "no event for a refused start");
    }
}
