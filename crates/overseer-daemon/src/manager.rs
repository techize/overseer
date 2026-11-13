//! The seven slots (S0 to S6) and their lifecycle.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use overseer_proto::{
    slot_name, Activity, Event, HookEvent, HookKind, SlotId, SlotInfo, SlotState,
};
use tokio::sync::broadcast;

use crate::activity::apply;
use crate::env::BaseEnv;
use crate::launch::{
    claude_argv, claude_projects_dir, first_prompt, has_transcript, is_claude, write_slot_files,
    RoleDirs, Start,
};
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
    /// From hook events; cleared when the slot exits.
    activity: Option<Activity>,
    /// Claude's current session id, empty for commands that are not claude.
    /// Kept so a hook only touches the ledger when the id really changed.
    session_id: String,
    /// The command as the client gave it, before Overseer's flags. This is
    /// what clients see and send back on a restart: reporting the flagged
    /// argv would make every restart add a second set of flags.
    command: Vec<String>,
    /// The ledger's session note for the slot, relative to the vault.
    session_note: Option<String>,
}

impl Entry {
    fn new(slot: Arc<Slot>, session_id: String, command: Vec<String>) -> Self {
        Entry {
            slot,
            activity: None,
            session_id,
            command,
            session_note: None,
        }
    }

    fn info(&self) -> SlotInfo {
        SlotInfo {
            activity: self.activity.clone(),
            command: Some(self.command.clone()),
            session_note: self.session_note.clone(),
            ..self.slot.info()
        }
    }
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
    /// Sessions found live in the ledger at startup, for slots that have no
    /// process. Only ever set for a slot with no entry in `slots`; a start
    /// or resume replaces it.
    restored: [Option<Restored>; SLOT_COUNT],
    /// `session_note` from the config: a fresh start fixes the slot's note
    /// from it in the ledger; a resume keeps the one it had.
    session_note: Option<String>,
}

/// A slot as the ledger left it: no process, but remembered.
struct Restored {
    info: SlotInfo,
    /// The session to continue, empty when the slot is not resumable.
    session_id: String,
}

#[derive(Clone)]
struct Launch {
    state_dir: PathBuf,
    /// The binary the generated hooks call.
    hook_program: PathBuf,
    ledger: Arc<Ledger>,
    dirs: RoleDirs,
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
            restored: Default::default(),
            session_note: None,
        }
    }

    /// Turns the sessions the ledger still has as live (the daemon went down
    /// under them) into `Resumable` slots. Anything that cannot be resumed
    /// (not claude, no session id, unreadable command) shows as exited and is
    /// marked exited in the ledger so it is not considered again.
    pub fn restore(&mut self) {
        let Some(launch) = self.launch.clone() else {
            return;
        };
        let rows = match launch.ledger.live_at_shutdown() {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("ledger not readable, nothing to resume: {e:#}");
                return;
            }
        };
        for row in rows.into_iter().filter(|r| valid_id(r.slot)) {
            let resumable =
                !row.session_id.is_empty() && !row.command.is_empty() && is_claude(&row.command);
            let state = if resumable {
                SlotState::Resumable {
                    session_id: row.session_id.clone(),
                    at: row.started,
                }
            } else {
                if let Err(e) = launch.ledger.record_exit(row.slot, None, row.started) {
                    tracing::warn!("{} ledger exit not recorded: {e:#}", slot_name(row.slot));
                }
                SlotState::Exited {
                    code: None,
                    at: row.started,
                }
            };
            let info = SlotInfo {
                id: row.slot,
                label: row.label,
                cwd: Some(row.cwd),
                command: Some(row.command).filter(|c| !c.is_empty()),
                state,
                cols: DEFAULT_COLS,
                rows: DEFAULT_ROWS,
                activity: None,
                session_note: launch.ledger.session_note(row.slot).ok().flatten(),
            };
            let session_id = if resumable {
                row.session_id
            } else {
                String::new()
            };
            self.restored[row.slot as usize] = Some(Restored { info, session_id });
        }
    }

    /// Generates claude's per-slot files under `state_dir` and records every
    /// start (and exit) in `ledger`.
    pub fn with_launch(
        mut self,
        state_dir: PathBuf,
        hook_program: PathBuf,
        ledger: Arc<Ledger>,
        dirs: RoleDirs,
    ) -> Self {
        self.launch = Some(Launch {
            state_dir,
            hook_program,
            ledger,
            dirs,
        });
        self
    }

    /// Fixes each fresh session's note path from `template` (vault-relative,
    /// see `brain::paths::fill_template`).
    pub fn with_session_note(mut self, template: String) -> Self {
        self.session_note = Some(template);
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
        prompt: Option<String>,
    ) -> Result<(), String> {
        self.launch_slot(id, label, cwd, command, None, prompt)
    }

    /// Relaunches a `Resumable` slot with `claude --resume <id>`, in its own
    /// cwd with its own base command and fresh generated files.
    pub fn resume(&mut self, id: SlotId) -> Result<(), String> {
        if !valid_id(id) {
            return Err(invalid_id(id));
        }
        let offer = self.restored[id as usize]
            .as_ref()
            .filter(|r| matches!(r.info.state, SlotState::Resumable { .. }))
            .ok_or_else(|| format!("{} is not resumable", slot_name(id)))?;
        let (label, command, session_id) = (
            offer.info.label.clone(),
            offer.info.command.clone(),
            offer.session_id.clone(),
        );
        let cwd = offer.info.cwd.clone().unwrap_or_default();
        self.launch_slot(id, label, &cwd, command, Some(session_id), None)
    }

    /// The shared start path. `resume` is the session to continue, if any.
    fn launch_slot(
        &mut self,
        id: SlotId,
        label: String,
        cwd: &Path,
        command: Option<Vec<String>>,
        resume: Option<String>,
        prompt: Option<String>,
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
        // One clock reading and one note for the role, the row and the
        // ledger: a fresh start fixes the note from `start_time`, a resume keeps
        // the stored one (it may be from another day).
        let start_time = unix_now();
        let note = match (&resume, &self.launch) {
            (Some(_), Some(l)) => l.ledger.session_note(id).ok().flatten(),
            (None, _) => self
                .session_note
                .as_ref()
                .map(|t| crate::brain::paths::fill_template(t, start_time, id, &label)),
            _ => None,
        };
        // Claude gets Overseer's hooks, role prompt and a session id of our
        // choosing; anything else runs as given and has no session id.
        let mut session_id = String::new();
        let mut argv = command.clone();
        let env = self.env.get();
        if let Some(launch) = self.launch.as_ref().filter(|_| is_claude(&command)) {
            let files = write_slot_files(
                &launch.state_dir,
                id,
                &label,
                &launch.hook_program,
                &launch.dirs,
                note.as_deref(),
            )
            .map_err(|e| format!("cannot write {} launch files: {e}", slot_name(id)))?;
            let how = match &resume {
                // A session that never had a message has nothing to resume,
                // and `--resume` would exit at once, so it starts fresh under
                // the same id. If the folder cannot be found, resume as asked.
                Some(sid)
                    if claude_projects_dir(&env)
                        .is_some_and(|p| p.is_dir() && !has_transcript(&p, sid)) =>
                {
                    tracing::info!(
                        "{} session {sid} has no saved conversation; starting it fresh",
                        slot_name(id)
                    );
                    Start::New(sid.clone())
                }
                Some(sid) => Start::Resume(sid.clone()),
                None => Start::New(uuid::Uuid::new_v4().to_string()),
            };
            session_id = match &how {
                Start::New(sid) | Start::Resume(sid) => sid.clone(),
            };
            let prompt = first_prompt(&launch.state_dir, id, &how, prompt)
                .map_err(|e| format!("cannot read {} kickoff: {e}", slot_name(id)))?;
            argv = claude_argv(&command, id, &label, &files, how, prompt.as_deref());
        }
        let row_label = label.clone();
        let entry_session_id = session_id.clone();
        let (tx, monitor_rx) = broadcast::channel(OUTPUT_CHANNEL_CAP);
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
        let mut entry = Entry::new(slot.clone(), entry_session_id, command.clone());
        entry.session_note = note.clone().filter(|_| self.launch.is_some());
        let started = entry.info();
        if let Some(old) = slots[id as usize].replace(entry) {
            old.slot.kill();
        }
        // Ledger writes are single-row SQLite statements done synchronously
        // here: the start path already forks a process synchronously, so a
        // spawn_blocking hop would buy nothing. The base command is recorded
        // (not the flagged argv) so a resume can rebuild its own flags. A
        // resume keeps its row (same session id) and only goes live again.
        self.restored[id as usize] = None;
        if let (Some(launch), Some(_)) = (&self.launch, &resume) {
            if let Err(e) = launch.ledger.mark_resumed(id) {
                tracing::warn!("{} ledger resume not recorded: {e:#}", slot_name(id));
            }
        } else if let Some(launch) = &self.launch {
            let row = SessionRow {
                slot: id,
                session_id,
                label: row_label,
                cwd: cwd.to_path_buf(),
                command: command.clone(),
                started: start_time,
            };
            if let Err(e) = launch.ledger.record_start(&row) {
                tracing::warn!("{} ledger start not recorded: {e:#}", slot_name(id));
            } else if let Some(note) = &note {
                if let Err(e) = launch.ledger.set_session_note(id, note) {
                    tracing::warn!("{} session note not recorded: {e:#}", slot_name(id));
                }
            }
        }
        let _ = self.events.send(Event::SlotChanged(started));
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

    /// Applies a Claude Code hook event to its slot. Events for a slot that
    /// is not running are dropped: the hook process never sees an error, and
    /// a late event must not bring back the activity an exit cleared.
    pub fn hook(&self, ev: &HookEvent) {
        let mut slots = self.slots.lock().unwrap();
        let Some(entry) = slots.get_mut(ev.slot as usize).and_then(Option::as_mut) else {
            return;
        };
        if !matches!(entry.slot.info().state, SlotState::Running { .. }) {
            return;
        }
        // A resumed or compacted session can report an id other than the one
        // we generated; the ledger needs the real one to resume it. Written
        // synchronously for the same reason as the start write.
        if ev.event == HookKind::SessionStart {
            if let Some(id) = ev.session_id.as_ref().filter(|id| **id != entry.session_id) {
                if let Some(launch) = &self.launch {
                    if let Err(e) = launch.ledger.set_session_id(ev.slot, id) {
                        tracing::warn!(
                            "{} ledger session id not updated: {e:#}",
                            slot_name(ev.slot)
                        );
                    }
                }
                entry.session_id = id.clone();
            }
        }
        let next = apply(entry.activity.as_ref(), ev, unix_now());
        if next != entry.activity {
            entry.activity = next;
            let _ = self.events.send(Event::SlotChanged(entry.info()));
        }
    }

    pub fn stop(&mut self, id: SlotId) -> Result<(), String> {
        if !valid_id(id) {
            return Err(invalid_id(id));
        }
        // Stopping a resumable slot declines the offer: it becomes empty and
        // its session is not offered again after the next restart.
        if matches!(
            self.restored[id as usize].as_ref().map(|r| &r.info.state),
            Some(SlotState::Resumable { .. })
        ) {
            self.restored[id as usize] = None;
            if let Some(launch) = &self.launch {
                if let Err(e) = launch.ledger.record_exit(id, None, unix_now()) {
                    tracing::warn!("{} ledger exit not recorded: {e:#}", slot_name(id));
                }
            }
            let _ = self.events.send(Event::SlotChanged(Self::empty_info(id)));
            return Ok(());
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
                Some(e) => e.info(),
                None => match &self.restored[id as usize] {
                    Some(r) => r.info.clone(),
                    None => Self::empty_info(id),
                },
            })
            .collect()
    }

    fn empty_info(id: SlotId) -> SlotInfo {
        SlotInfo {
            id,
            label: String::new(),
            cwd: None,
            command: None,
            state: SlotState::Empty,
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            activity: None,
            session_note: None,
        }
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
    let mut slots = slots.lock().unwrap();
    // The entry's info, not the slot's: it carries the base command.
    let current = slots[info.id as usize]
        .as_mut()
        .filter(|e| Arc::ptr_eq(&e.slot, &slot))
        .map(|e| {
            e.activity = None;
            e.info()
        });
    if let Some(info) = current {
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
        mgr.slots.lock().unwrap()[1] = Some(Entry::new(old.clone(), String::new(), Vec::new()));
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
        mgr.start(1, "new".into(), Path::new("/tmp"), None, None)
            .unwrap();

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
        let err = mgr.start(2, "x".into(), missing, None, None).unwrap_err();
        assert!(
            err.contains("does not exist or is not a directory"),
            "{err}"
        );
        assert!(err.contains("overseer-no-such-dir-for-test"), "{err}");
        // A file is not a directory either.
        let err = mgr
            .start(2, "x".into(), Path::new("/bin/sh"), None, None)
            .unwrap_err();
        assert!(err.contains("not a directory"), "{err}");
        assert_eq!(mgr.list()[2].state, SlotState::Empty);
        assert!(ev_rx.try_recv().is_err(), "no event for a refused start");
    }
}

#[cfg(test)]
mod restore_tests {
    use super::*;
    use crate::ledger::Ledger;

    fn row(slot: SlotId, session_id: &str, command: &[&str]) -> SessionRow {
        SessionRow {
            slot,
            session_id: session_id.into(),
            label: "l".into(),
            cwd: PathBuf::from("/tmp"),
            command: command.iter().map(|s| s.to_string()).collect(),
            started: 77,
        }
    }

    fn manager(ledger: Arc<Ledger>) -> SlotManager {
        let (events, _) = broadcast::channel(16);
        SlotManager::new(vec!["claude".into()], events).with_launch(
            std::env::temp_dir(),
            PathBuf::from("/bin/true"),
            ledger,
            RoleDirs::default(),
        )
    }

    fn role_note(slot: SlotId) -> String {
        let role = std::fs::read_to_string(
            std::env::temp_dir()
                .join("slots")
                .join(slot_name(slot))
                .join("role.md"),
        )
        .unwrap();
        role.lines()
            .find_map(|l| l.split("session note is ").nth(1))
            .unwrap_or("")
            .to_string()
    }

    #[tokio::test]
    async fn the_role_names_the_ledgers_note_fresh_and_resumed() {
        let ledger = Arc::new(Ledger::in_memory());
        let mut m = manager(ledger.clone()).with_session_note("Sessions/{slot}-{label}.md".into());
        // The role needs the writer on to name a note.
        m.launch.as_mut().unwrap().dirs.brain_writer = true;
        m.launch.as_mut().unwrap().dirs.brain = PathBuf::from("/tmp/demo-brain");
        // Only a claude command gets a role file: a stand-in named claude.
        let bin = std::env::temp_dir().join(format!("ovm-{}-claude", std::process::id()));
        std::fs::create_dir_all(&bin).unwrap();
        let fake = bin.join("claude");
        std::fs::write(&fake, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let sleep = Some(vec![fake.to_string_lossy().to_string()]);
        m.start(
            6,
            "Demo Lane".into(),
            Path::new("/tmp"),
            sleep.clone(),
            None,
        )
        .unwrap();
        let fresh = ledger.session_note(6).unwrap().unwrap();
        assert_eq!(role_note(6), format!("/tmp/demo-brain/{fresh}. Write it, and any ticket folder you own (for example Sprints/sprint-26-01/DEMO-12-demo-lane/), directly with your file tools."));
        m.stop(6).unwrap();
        for _ in 0..100 {
            if !matches!(m.list()[6].state, SlotState::Running { .. }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // A resume the next day keeps the stored note, not today's.
        ledger
            .set_session_note(6, "Sessions/2020/01-January/2020-01-02-s6-old.md")
            .unwrap();
        m.launch_slot(
            6,
            "Demo Lane".into(),
            Path::new("/tmp"),
            sleep,
            Some("sid-6".into()),
            None,
        )
        .unwrap();
        assert_eq!(
            m.list()[6].session_note.as_deref(),
            Some("Sessions/2020/01-January/2020-01-02-s6-old.md")
        );
        assert!(
            role_note(6)
                .starts_with("/tmp/demo-brain/Sessions/2020/01-January/2020-01-02-s6-old.md."),
            "{}",
            role_note(6)
        );
        m.shutdown_all();
    }

    #[tokio::test]
    async fn a_fresh_start_fixes_the_session_note() {
        let ledger = Arc::new(Ledger::in_memory());
        let mut m = manager(ledger.clone()).with_session_note("Sessions/{slot}-{label}.md".into());
        let sleep = Some(vec!["/bin/sleep".to_string(), "30".to_string()]);
        m.start(
            3,
            "Demo Lane".into(),
            Path::new("/tmp"),
            sleep.clone(),
            None,
        )
        .unwrap();
        assert_eq!(
            ledger.session_note(3).unwrap().as_deref(),
            Some("Sessions/s3-demo-lane.md")
        );
        // The slot's info carries the note for the board.
        assert_eq!(
            m.list()[3].session_note.as_deref(),
            Some("Sessions/s3-demo-lane.md")
        );
        // A resume keeps the row, note included.
        ledger.mark_resumed(3).unwrap();
        assert!(ledger.session_note(3).unwrap().is_some());
        m.stop(3).unwrap();
        for _ in 0..100 {
            if !matches!(m.list()[3].state, SlotState::Running { .. }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        m.start(3, "other".into(), Path::new("/tmp"), sleep, None)
            .unwrap();
        assert_eq!(
            ledger.session_note(3).unwrap().as_deref(),
            Some("Sessions/s3-other.md")
        );
        m.shutdown_all();
    }

    #[test]
    fn corrupt_row_is_not_resumable() {
        let ledger = Arc::new(Ledger::in_memory());
        // A command that reads back as empty, as a corrupt row does.
        ledger.record_start(&row(1, "abc", &[])).unwrap();
        ledger.record_start(&row(2, "def", &["claude"])).unwrap();
        let mut m = manager(ledger.clone());
        m.restore();
        let slots = m.list();
        assert_eq!(slots[1].state, SlotState::Exited { code: None, at: 77 });
        assert_eq!(
            slots[2].state,
            SlotState::Resumable {
                session_id: "def".into(),
                at: 77
            }
        );
        assert!(m.resume(1).is_err());
        let live: Vec<_> = ledger
            .live_at_shutdown()
            .unwrap()
            .iter()
            .map(|r| r.slot)
            .collect();
        assert_eq!(live, vec![2], "unresumable row recorded as exited");
    }
}
