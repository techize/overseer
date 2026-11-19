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
use crate::holder::{
    activity_path, client, end_by_pid, exit_path, holder_alive, pid_path, socket_path, ExitFile,
    HolderHello, PidFile,
};
use crate::launch::{
    claude_argv, claude_projects_dir, first_prompt, has_transcript, is_claude, write_slot_files,
    RoleDirs, Start,
};
use crate::ledger::{Ledger, SessionRow};
use crate::slot::{spawn_spec, Attached, Slot};

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
    /// The program run as `<program> hold <state_dir>` to keep each
    /// session's terminal; `None` runs sessions in the daemon itself.
    holder: Option<PathBuf>,
}

/// A slot as the ledger left it: no process, but remembered.
struct Restored {
    info: SlotInfo,
    /// The session to continue, empty when the slot is not resumable.
    session_id: String,
    /// Set when the slot's holder is alive but could not be attached.
    unreachable: Option<Unreachable>,
}

/// A session whose holder is alive but that this daemon could not attach
/// to, or whose holder is an older build's. Its card shows it running;
/// Stop ends it by pid.
struct Unreachable {
    pids: PidFile,
    /// Set when the holder is an older build's: Stop then offers its
    /// session for resume rather than ending it.
    older: Option<Older>,
    /// Set by Stop, under the slots lock, so a later retry leaves it alone.
    given_up: Arc<AtomicBool>,
    /// When the session was seen to end, shared with the retry thread so
    /// its exit is recorded once, whoever notices first.
    ended: Ended,
}

/// What an older build's session goes back to when Stop ends it.
#[derive(Clone)]
struct Older {
    /// The label without the older-build note.
    label: String,
    /// The session to offer for resume, from the ledger.
    session_id: Option<String>,
    /// When the session started, from the ledger.
    started: i64,
}

type Ended = Arc<Mutex<Option<i64>>>;

/// How often the retry thread, done retrying, checks the holder is alive.
const HOLDER_WATCH: Duration = Duration::from_secs(1);

/// Marks an unreachable session ended, recording its exit in the ledger
/// the first time. Returns when it ended and whether this call ended it.
fn end_once(ended: &Ended, ledger: Option<&Ledger>, id: SlotId) -> (i64, bool) {
    let mut at = ended.lock().unwrap();
    if let Some(at) = *at {
        return (at, false);
    }
    let now = unix_now();
    *at = Some(now);
    if let Some(ledger) = ledger {
        if let Err(e) = ledger.record_exit(id, None, now) {
            tracing::warn!("{} ledger exit not recorded: {e:#}", slot_name(id));
        }
    }
    (now, true)
}

/// A placeholder's card once its session has ended.
fn ended_info(info: &SlotInfo, at: i64) -> SlotInfo {
    SlotInfo {
        state: SlotState::Exited { code: None, at },
        held: false,
        ..info.clone()
    }
}

/// Further tries at attaching a holder that is alive, after the first one
/// at start failed.
const ATTACH_RETRIES: u32 = 3;
const ATTACH_RETRY_PAUSE: Duration = Duration::from_secs(2);

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
            holder: None,
        }
    }

    /// Picks up the sessions the last daemon left. First the holders in
    /// `<state>/holders`: a live one is attached again and its slot runs on;
    /// an exit file is recorded in the ledger and the slot shows as exited
    /// with its code; a socket nobody listens on is deleted. Then the
    /// sessions the ledger still has as live with no holder (the daemon went
    /// down under them) become `Resumable` slots. Anything that cannot be
    /// resumed (not claude, no session id, unreadable command) shows as
    /// exited and is marked exited in the ledger so it is not considered
    /// again.
    pub fn restore(&mut self) {
        let Some(launch) = self.launch.clone() else {
            return;
        };
        let rows = match launch.ledger.live_at_shutdown() {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("ledger not readable, nothing to resume: {e:#}");
                Vec::new()
            }
        };
        for id in 0..SLOT_COUNT as SlotId {
            let row = rows.iter().find(|r| r.slot == id);
            self.restore_held(&launch, id, row);
        }
        for row in rows.into_iter().filter(|r| valid_id(r.slot)) {
            let filled = self.slots.lock().unwrap()[row.slot as usize].is_some()
                || self.restored[row.slot as usize].is_some();
            if filled {
                continue;
            }
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
                held: false,
            };
            let session_id = if resumable {
                row.session_id
            } else {
                String::new()
            };
            self.restored[row.slot as usize] = Some(Restored {
                info,
                session_id,
                unreachable: None,
            });
        }
    }

    /// The holder side of [`SlotManager::restore`] for slot `id`, whose
    /// live ledger row is `row`, if any.
    fn restore_held(&mut self, launch: &Launch, id: SlotId, row: Option<&SessionRow>) {
        let state_dir = &launch.state_dir;
        let saved = take_activity(state_dir, id);
        // An exit file comes first: its holder is gone or about to go.
        let exit = exit_path(state_dir, id);
        if let Some(file) = read_json::<ExitFile>(&exit) {
            tracing::info!(
                "{} exited (code {:?}) while the daemon was down",
                slot_name(id),
                file.code
            );
            if let Err(e) = launch.ledger.record_exit(id, file.code, file.at) {
                tracing::warn!("{} ledger exit not recorded: {e:#}", slot_name(id));
            }
            let info = SlotInfo {
                id,
                label: row.map(|r| r.label.clone()).unwrap_or_default(),
                cwd: row.map(|r| r.cwd.clone()),
                command: row.map(|r| r.command.clone()).filter(|c| !c.is_empty()),
                state: SlotState::Exited {
                    code: file.code,
                    at: file.at,
                },
                cols: DEFAULT_COLS,
                rows: DEFAULT_ROWS,
                activity: None,
                session_note: launch.ledger.session_note(id).ok().flatten(),
                held: false,
            };
            self.restored[id as usize] = Some(Restored {
                info,
                session_id: String::new(),
                unreachable: None,
            });
            remove_file(&exit);
            return;
        }
        let socket = socket_path(state_dir, id);
        if !socket.exists() {
            forget_stale_pid_file(state_dir, id);
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!("{} holder not attached: no runtime", slot_name(id));
            return;
        };
        let ctx = Reattach {
            slots: self.slots.clone(),
            events: self.events.clone(),
            ledger: launch.ledger.clone(),
            shutting_down: self.shutting_down.clone(),
            runtime,
        };
        let session = Session {
            command: row.map(|r| r.command.clone()).filter(|c| !c.is_empty()),
            session_id: row.map(|r| r.session_id.clone()).unwrap_or_default(),
            note: launch.ledger.session_note(id).ok().flatten(),
            activity: saved,
        };
        let why = match try_attach(&socket, id) {
            Attempt::Got(slot, monitor_rx, trust_rx) => {
                let mut slots = self.slots.lock().unwrap();
                install(&ctx, &mut slots, id, slot, monitor_rx, trust_rx, session);
                return;
            }
            Attempt::Gone => {
                tracing::info!("{} holder is gone; removing its socket", slot_name(id));
                remove_file(&socket);
                forget_stale_pid_file(state_dir, id);
                return;
            }
            Attempt::Unsupported(hello) => {
                self.keep_older(ctx, state_dir, id, row, session, hello.as_ref());
                return;
            }
            Attempt::Failed(why) => why,
        };
        // A holder that is still alive keeps its session: never offer a
        // resume over it, which its live pid file would refuse anyway.
        let pids = client::read_pid_file(&pid_path(state_dir, id)).filter(holder_alive);
        let Some(pids) = pids else {
            tracing::info!(
                "{} holder is gone ({why}); removing its socket",
                slot_name(id)
            );
            remove_file(&socket);
            forget_stale_pid_file(state_dir, id);
            return;
        };
        tracing::warn!(
            "{} holder (pid {}) is alive but not attached ({why}); trying again",
            slot_name(id),
            pids.holder
        );
        let since = row.map_or_else(unix_now, |r| r.started);
        let label = row.map(|r| r.label.clone()).unwrap_or_default();
        let child = pids.child;
        let watch = self.keep_placeholder(id, row, &session, pids, child, since, label, None);
        retry_attach(ctx, socket, id, session, watch, ATTACH_RETRIES);
    }

    /// Keeps a card for a session whose holder is an older build's
    /// (`hello` is `None` when its Hello could not be read at all): it shows
    /// as running, with the older-build note on its label, until Stop ends
    /// it by pid or the holder dies.
    fn keep_older(
        &mut self,
        ctx: Reattach,
        state_dir: &Path,
        id: SlotId,
        row: Option<&SessionRow>,
        session: Session,
        hello: Option<&HolderHello>,
    ) {
        let from_hello = hello.map(|h| PidFile {
            holder: h.pid,
            child: h.child_pid,
            version: h.version,
        });
        let pids = client::read_pid_file(&pid_path(state_dir, id))
            .or(from_hello)
            .filter(holder_alive);
        let Some(pids) = pids else {
            // It answered, so something listens there: leave it alone, but
            // never signal what cannot be shown to be a holder.
            tracing::warn!(
                "{} holder of another build answered, but no live holder is named for it; left alone",
                slot_name(id)
            );
            return;
        };
        tracing::warn!(
            "{} holder (pid {}) is an older build's (protocol {}); Stop ends it",
            slot_name(id),
            pids.holder,
            hello.map_or_else(|| "unreadable".to_string(), |h| h.version.to_string())
        );
        let label = row
            .map(|r| r.label.clone())
            .or_else(|| hello.map(|h| h.label.clone()))
            .unwrap_or_default();
        let since = row
            .map(|r| r.started)
            .or_else(|| hello.map(|h| h.since))
            .unwrap_or_else(unix_now);
        let resumable = row
            .filter(|r| !r.session_id.is_empty() && !r.command.is_empty() && is_claude(&r.command));
        let older = Older {
            label: label.clone(),
            session_id: resumable.map(|r| r.session_id.clone()),
            started: since,
        };
        let child = hello.map_or(pids.child, |h| h.child_pid);
        let watch = self.keep_placeholder(
            id,
            row,
            &session,
            pids,
            child,
            since,
            format!("{label} (older build)"),
            Some(older),
        );
        let socket = socket_path(state_dir, id);
        retry_attach(ctx, socket, id, session, watch, 0);
    }

    /// Puts a running card in slot `id` for a live holder this daemon does
    /// not attach. Returns what the watching thread needs.
    #[allow(clippy::too_many_arguments)]
    fn keep_placeholder(
        &mut self,
        id: SlotId,
        row: Option<&SessionRow>,
        session: &Session,
        pids: PidFile,
        child: u32,
        since: i64,
        label: String,
        older: Option<Older>,
    ) -> Watch {
        let given_up = Arc::new(AtomicBool::new(false));
        let ended: Ended = Arc::default();
        let info = SlotInfo {
            id,
            label,
            cwd: row.map(|r| r.cwd.clone()),
            command: session.command.clone(),
            state: SlotState::Running { pid: child, since },
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            activity: None,
            session_note: session.note.clone(),
            held: true,
        };
        let watch = Watch {
            pids: pids.clone(),
            given_up: given_up.clone(),
            ended: ended.clone(),
            info: info.clone(),
        };
        self.restored[id as usize] = Some(Restored {
            info,
            session_id: String::new(),
            unreachable: Some(Unreachable {
                pids,
                older,
                given_up,
                ended,
            }),
        });
        watch
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

    /// Runs every session started from now on under a holder process,
    /// `<program> hold <state_dir>`. Needs [`SlotManager::with_launch`] for
    /// the state dir; without it sessions run in the daemon.
    pub fn with_holder(mut self, program: PathBuf) -> Self {
        self.holder = Some(program);
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
        if slots[id as usize].is_none() {
            if let Some(u) = self.unreachable(id) {
                return Err(u.refusal(id));
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
        // Only claude asks the folder-trust question.
        let trust_rx = (!session_id.is_empty()).then(|| tx.subscribe());
        let spawned = match (&self.holder, &self.launch) {
            (Some(program), Some(launch)) => {
                let spec = spawn_spec(id, label, cwd, &argv, DEFAULT_COLS, DEFAULT_ROWS, &env);
                Slot::spawn_held(spec, program, &launch.state_dir, tx)
            }
            _ => Slot::spawn_in(id, label, cwd, &argv, DEFAULT_COLS, DEFAULT_ROWS, tx, &env),
        };
        let slot = spawned.map_err(|e| {
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
            if let Err(e) = launch.ledger.mark_resumed(id, start_time) {
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
        if let Some(rx) = trust_rx {
            tokio::spawn(watch_trust(
                slot.clone(),
                rx,
                self.events.clone(),
                self.slots.clone(),
            ));
        }
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
        // A late event from the run before (killed, then started again) must
        // not set this run's activity. An event with no run is from a hook
        // older than the token, or a hand-run `overseer hook`: applied.
        if ev.run.as_deref().is_some_and(|run| run != entry.slot.run()) {
            return;
        }
        // A subagent's or teammate's events describe its own work, not the
        // main conversation's: a background one keeps calling tools after
        // the main turn's Stop, and a PostToolUse from it would mark an idle
        // session working until its next turn (#58).
        if ev.agent_id.is_some() {
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
        if let Some(r) = self.unreachable(id) {
            let slots = self.slots.lock().unwrap();
            // Unless a retry has attached it meanwhile, end it by pid.
            if slots[id as usize].is_none() {
                r.given_up.store(true, Ordering::SeqCst);
                end_in_background(id, r.pids.clone());
                let older = r.older.clone();
                // An older build's session is offered for resume, so its
                // ledger row stays live; any other session has ended.
                let resume = older
                    .as_ref()
                    .and_then(|o| Some((o.session_id.clone()?, o.started)));
                let ended = resume
                    .is_none()
                    .then(|| end_once(&r.ended, self.launch.as_ref().map(|l| &*l.ledger), id).0);
                drop(slots);
                let restored = self.restored[id as usize].as_mut().unwrap();
                restored.unreachable = None;
                if let Some(o) = older {
                    restored.info.label = o.label;
                }
                restored.info = match (resume, ended) {
                    (Some((session_id, at)), _) => {
                        restored.session_id = session_id.clone();
                        SlotInfo {
                            state: SlotState::Resumable { session_id, at },
                            held: false,
                            ..restored.info.clone()
                        }
                    }
                    (None, at) => ended_info(&restored.info, at.unwrap_or_else(unix_now)),
                };
                let _ = self.events.send(Event::SlotChanged(restored.info.clone()));
                return Ok(());
            }
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
                    // A holder that died since shows as exited.
                    Some(Restored {
                        info,
                        unreachable: Some(u),
                        ..
                    }) if self.unreachable(id).is_none() => {
                        ended_info(info, u.ended.lock().unwrap().unwrap_or_else(unix_now))
                    }
                    Some(r) => r.info.clone(),
                    None => Self::empty_info(id),
                },
            })
            .collect()
    }

    /// The slot's holder, if it is alive but not attached. A holder found
    /// dead ends its session here, if the retry thread has not already:
    /// its exit is recorded once and its card changes to exited, so the
    /// slot can take a new session.
    fn unreachable(&self, id: SlotId) -> Option<&Unreachable> {
        let restored = self.restored[id as usize].as_ref()?;
        let u = restored.unreachable.as_ref()?;
        if u.ended.lock().unwrap().is_some() {
            return None;
        }
        if holder_alive(&u.pids) {
            return Some(u);
        }
        let (at, now) = end_once(&u.ended, self.launch.as_ref().map(|l| &*l.ledger), id);
        if now {
            tracing::info!("{} unreachable holder is gone", slot_name(id));
            let _ = self
                .events
                .send(Event::SlotChanged(ended_info(&restored.info, at)));
        }
        None
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
            held: false,
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
        let entry = slots[id as usize].as_ref().ok_or_else(|| {
            if let Some(u) = self.unreachable(id) {
                u.refusal(id)
            } else {
                format!("{} is empty", slot_name(id))
            }
        })?;
        let (snapshot, pos, live) = entry.slot.attach_at();
        Ok(Attachment {
            slot: entry.slot.clone(),
            live,
            snapshot,
            pos,
        })
    }

    /// Called on daemon shutdown. Held sessions keep running under their
    /// holders for the next daemon: each one's activity is saved to its
    /// activity file and its connection closed. N4: local children are
    /// killed. Either way the ledger rows stay live.
    pub fn shutdown_all(&mut self) {
        // Before any kill or close: the exit monitors must see it when the
        // children die or their connections end.
        self.shutting_down.store(true, Ordering::SeqCst);
        let entries: Vec<Entry> = self
            .slots
            .lock()
            .unwrap()
            .iter_mut()
            .filter_map(Option::take)
            .collect();
        for entry in &entries {
            if !entry.slot.is_held() {
                entry.slot.kill();
                continue;
            }
            let running = matches!(entry.slot.info().state, SlotState::Running { .. });
            if let (Some(launch), Some(activity), true) = (&self.launch, &entry.activity, running) {
                let id = entry.slot.info().id;
                let path = activity_path(&launch.state_dir, id);
                if let Err(e) = crate::holder::write_json_atomic(&path, activity) {
                    tracing::warn!("{} activity not saved: {e}", slot_name(id));
                }
            }
            entry.slot.let_go();
        }
    }
}

impl Unreachable {
    /// Why a start or an attach on this slot is refused.
    fn refusal(&self, id: SlotId) -> String {
        if self.older.is_some() {
            return OLDER_BUILD.to_string();
        }
        format!(
            "{} has a session this daemon cannot reach; stop it first",
            slot_name(id)
        )
    }
}

/// Why an older build's session cannot be attached or replaced.
const OLDER_BUILD: &str = "this session runs on an older Overseer build; stop it to use the slot";

/// Removes slot `id`'s pid file when it names no live holder: what it
/// names may be some other process by now.
fn forget_stale_pid_file(state_dir: &Path, id: SlotId) {
    let path = pid_path(state_dir, id);
    match crate::holder::load_pid_file(&path) {
        Ok(Some(p)) if holder_alive(&p) => {}
        Ok(None) => {}
        _ => {
            tracing::info!("{} removing a stale holder pid file", slot_name(id));
            remove_file(&path);
        }
    }
}

/// What a reattach needs beyond the slot, shared with a retry thread.
#[derive(Clone)]
struct Reattach {
    slots: Slots,
    events: broadcast::Sender<Event>,
    ledger: Arc<Ledger>,
    shutting_down: Arc<AtomicBool>,
    runtime: tokio::runtime::Handle,
}

/// The slot's session as the ledger and the last daemon left it.
struct Session {
    /// The base command from the ledger; `None` takes the holder's argv.
    command: Option<Vec<String>>,
    session_id: String,
    note: Option<String>,
    activity: Option<Activity>,
}

enum Attempt {
    /// With the exit monitor's receiver and the trust watch's.
    Got(
        Box<Slot>,
        broadcast::Receiver<Bytes>,
        broadcast::Receiver<Bytes>,
    ),
    /// Nothing listens on the socket.
    Gone,
    /// The holder is another build's: its Hello, or `None` when it could
    /// not be read.
    Unsupported(Option<HolderHello>),
    Failed(String),
}

/// One try at attaching the holder at `socket` as slot `id`.
fn try_attach(socket: &Path, id: SlotId) -> Attempt {
    let (tx, monitor_rx) = broadcast::channel(OUTPUT_CHANNEL_CAP);
    // For the trust watch, subscribed once the replay is in: a trust
    // question in the replay was asked before the restart, and the saved
    // activity already says whether it is still open. Dropped after, so
    // the channel still closes when the stream ends.
    let live = tx.clone();
    let attached = Slot::attach_held(socket, tx);
    let trust_rx = live.subscribe();
    drop(live);
    match attached {
        Ok(Attached::Slot(slot)) if slot.info().id == id => {
            Attempt::Got(Box::new(slot), monitor_rx, trust_rx)
        }
        Ok(Attached::Slot(slot)) => {
            let other = slot.info().id;
            slot.let_go();
            Attempt::Failed(format!("its holder keeps {}", slot_name(other)))
        }
        Ok(Attached::Unsupported(hello)) => Attempt::Unsupported(Some(hello)),
        Ok(Attached::Unreadable) => Attempt::Unsupported(None),
        Err(e) if nobody_listens(&e) => Attempt::Gone,
        Err(e) => Attempt::Failed(format!("{e:#}")),
    }
}

/// Puts a slot attached again into `slots` (the locked slots) and starts
/// its watchers as a start does. Returns its info.
fn install(
    ctx: &Reattach,
    slots: &mut [Option<Entry>; SLOT_COUNT],
    id: SlotId,
    slot: Box<Slot>,
    monitor_rx: broadcast::Receiver<Bytes>,
    trust_rx: broadcast::Receiver<Bytes>,
    session: Session,
) -> SlotInfo {
    let slot = Arc::new(*slot);
    let command = session
        .command
        .unwrap_or_else(|| slot.info().command.unwrap_or_default());
    let mut entry = Entry::new(slot.clone(), session.session_id, command.clone());
    entry.session_note = session.note;
    entry.activity = session.activity;
    if let SlotState::Running { pid, .. } = slot.info().state {
        tracing::info!(
            "{} attached again to its session (pid {pid})",
            slot_name(id)
        );
    }
    let info = entry.info();
    slots[id as usize] = Some(entry);
    slot.nudge_redraw();
    if is_claude(&command) {
        ctx.runtime.spawn(watch_trust(
            slot.clone(),
            trust_rx,
            ctx.events.clone(),
            ctx.slots.clone(),
        ));
    }
    ctx.runtime.spawn(monitor_exit(
        slot,
        monitor_rx,
        ctx.events.clone(),
        ctx.slots.clone(),
        Some(ctx.ledger.clone()),
        ctx.shutting_down.clone(),
    ));
    info
}

/// What the retry thread needs to notice an unreachable holder's death.
struct Watch {
    pids: PidFile,
    given_up: Arc<AtomicBool>,
    ended: Ended,
    /// The placeholder's card.
    info: SlotInfo,
}

/// Tries again, in the background, to attach a holder that is alive but
/// was not attached at start, up to `retries` times. Unless it attaches,
/// it then watches the holder: when the holder dies, the session's exit is
/// recorded and its card changes to exited. Stops early when Stop ends the
/// session or on shutdown.
fn retry_attach(
    ctx: Reattach,
    socket: PathBuf,
    id: SlotId,
    session: Session,
    watch: Watch,
    retries: u32,
) {
    let spawned = std::thread::Builder::new()
        .name(format!("reattach-{}", slot_name(id)))
        .spawn(move || {
            let stopped = || {
                watch.given_up.load(Ordering::SeqCst)
                    || ctx.shutting_down.load(Ordering::SeqCst)
                    || watch.ended.lock().unwrap().is_some()
            };
            for _ in 0..retries {
                std::thread::sleep(ATTACH_RETRY_PAUSE);
                if stopped() || !holder_alive(&watch.pids) {
                    break;
                }
                match try_attach(&socket, id) {
                    Attempt::Got(slot, monitor_rx, trust_rx) => {
                        let mut slots = ctx.slots.lock().unwrap();
                        if stopped() || slots[id as usize].is_some() {
                            slot.let_go();
                            return;
                        }
                        let info =
                            install(&ctx, &mut slots, id, slot, monitor_rx, trust_rx, session);
                        let _ = ctx.events.send(Event::SlotChanged(info));
                        return;
                    }
                    Attempt::Failed(why) => {
                        tracing::warn!("{} holder still not attached: {why}", slot_name(id));
                    }
                    Attempt::Gone | Attempt::Unsupported(_) => break,
                }
            }
            if retries > 0 && !stopped() && holder_alive(&watch.pids) {
                tracing::warn!(
                    "{} holder not attached after {ATTACH_RETRIES} more tries; Stop ends it",
                    slot_name(id)
                );
            }
            while !stopped() {
                if !holder_alive(&watch.pids) {
                    // Under the slots lock, as Stop ends it, and only while
                    // no session has taken the slot.
                    let slots = ctx.slots.lock().unwrap();
                    if slots[id as usize].is_none() && !stopped() {
                        let (at, now) = end_once(&watch.ended, Some(&ctx.ledger), id);
                        if now {
                            tracing::info!("{} unreachable holder is gone", slot_name(id));
                            let info = ended_info(&watch.info, at);
                            let _ = ctx.events.send(Event::SlotChanged(info));
                        }
                    }
                    return;
                }
                std::thread::sleep(HOLDER_WATCH);
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("{} attach not retried: {e}", slot_name(id));
    }
}

/// Ends a session this daemon cannot reach by the pids in its holder's
/// pid file ([`end_by_pid`]), on a thread of its own since that waits for
/// the holder to go.
fn end_in_background(id: SlotId, pids: PidFile) {
    tracing::info!(
        "{} ending a session by pid (holder {}, child {})",
        slot_name(id),
        pids.holder,
        pids.child
    );
    let spawned = std::thread::Builder::new()
        .name(format!("end-{}", slot_name(id)))
        .spawn(move || end_by_pid(&pids));
    if let Err(e) = spawned {
        tracing::warn!("{} not ended: {e}", slot_name(id));
    }
}

/// The activity the last daemon saved for slot `id`, if any. The file is
/// removed either way: it only ever describes the session it was saved for.
fn take_activity(state_dir: &Path, id: SlotId) -> Option<Activity> {
    let path = activity_path(state_dir, id);
    let activity = read_json(&path);
    remove_file(&path);
    activity
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!("{} not readable: {e}", path.display());
            return None;
        }
    };
    serde_json::from_slice(&bytes)
        .inspect_err(|e| tracing::warn!("{} not readable: {e}", path.display()))
        .ok()
}

fn remove_file(path: &Path) {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            tracing::warn!("{} not removed: {e}", path.display());
        }
        _ => {}
    }
}

/// Whether connecting to a holder's socket failed because nothing listens
/// there any more.
fn nobody_listens(e: &anyhow::Error) -> bool {
    e.chain()
        .filter_map(|c| c.downcast_ref::<std::io::Error>())
        .any(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            )
        })
}

/// Emits `SlotChanged` once the child has exited AND its output stream has
/// ended (N5). `Slot::wait_exit` can fire before the reader thread has pushed
/// the final bytes, so we wait for the output channel to close (the reader
/// thread dropping its sender) before announcing the exit. Clients that attach
/// after the event therefore see the complete tail in the snapshot. The wait is
/// bounded: a grandchild that inherited the PTY could keep the stream open.
/// How long after a start the folder-trust question is looked for. Claude
/// asks it before anything else, so a session past this has answered it.
const TRUST_WINDOW: Duration = Duration::from_secs(120);

/// Marks a claude slot Needs you while Claude asks whether to trust its
/// folder (F6): the question comes before the session starts, so no hook
/// reports it. Stops at the first hook event, at exit, or after
/// [`TRUST_WINDOW`]. Answering it starts the session, whose `SessionStart`
/// sets the card Idle.
async fn watch_trust(
    slot: Arc<Slot>,
    mut rx: broadcast::Receiver<Bytes>,
    events: broadcast::Sender<Event>,
    slots: Slots,
) {
    let id = slot.info().id as usize;
    // Whether this run is still in its slot with no activity reported yet.
    let unreported = |slots: &Slots| {
        slots.lock().unwrap()[id]
            .as_ref()
            .is_some_and(|e| Arc::ptr_eq(&e.slot, &slot) && e.activity.is_none())
    };
    let mut scan = crate::trust::TrustScan::default();
    let asked = tokio::time::timeout(TRUST_WINDOW, async {
        loop {
            match rx.recv().await {
                Ok(bytes) => {
                    if scan.feed(&bytes) {
                        return true;
                    }
                    if !unreported(&slots) {
                        return false;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return false,
            }
        }
    })
    .await
    .unwrap_or(false);
    if !asked {
        return;
    }
    let mut slots = slots.lock().unwrap();
    if let Some(e) = slots[id]
        .as_mut()
        .filter(|e| Arc::ptr_eq(&e.slot, &slot) && e.activity.is_none())
    {
        tracing::info!(
            "{} is waiting on Claude's folder-trust question",
            slot_name(e.slot.info().id)
        );
        e.activity = Some(crate::trust::activity(unix_now()));
        let _ = events.send(Event::SlotChanged(e.info()));
    }
}

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

    /// A state folder of the test's own, removed when it ends.
    struct StateDir(PathBuf);

    impl StateDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("ovm-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for StateDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn manager(ledger: Arc<Ledger>, state: &StateDir) -> SlotManager {
        let (events, _) = broadcast::channel(16);
        SlotManager::new(vec!["claude".into()], events).with_launch(
            state.0.clone(),
            PathBuf::from("/bin/true"),
            ledger,
            RoleDirs::default(),
        )
    }

    fn role_note(state: &StateDir, slot: SlotId) -> String {
        let role =
            std::fs::read_to_string(state.0.join("slots").join(slot_name(slot)).join("role.md"))
                .unwrap();
        role.lines()
            .find_map(|l| l.split("session note is ").nth(1))
            .unwrap_or("")
            .to_string()
    }

    #[tokio::test]
    async fn the_role_names_the_ledgers_note_fresh_and_resumed() {
        let ledger = Arc::new(Ledger::in_memory());
        let state = StateDir::new();
        let mut m =
            manager(ledger.clone(), &state).with_session_note("Sessions/{slot}-{label}.md".into());
        // The role needs the writer on to name a note.
        m.launch.as_mut().unwrap().dirs.brain_writer = true;
        m.launch.as_mut().unwrap().dirs.brain = PathBuf::from("/tmp/demo-brain");
        // Only a claude command gets a role file: a stand-in named claude.
        let bin = state.0.join("bin");
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
        assert_eq!(role_note(&state, 6), format!("/tmp/demo-brain/{fresh}. Write it, and any ticket folder you own (for example Sprints/sprint-26-01/DEMO-12-demo-lane/), directly with your file tools."));
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
            role_note(&state, 6)
                .starts_with("/tmp/demo-brain/Sessions/2020/01-January/2020-01-02-s6-old.md."),
            "{}",
            role_note(&state, 6)
        );
        m.shutdown_all();
    }

    #[tokio::test]
    async fn a_fresh_start_fixes_the_session_note() {
        let ledger = Arc::new(Ledger::in_memory());
        let state = StateDir::new();
        let mut m =
            manager(ledger.clone(), &state).with_session_note("Sessions/{slot}-{label}.md".into());
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
        ledger.mark_resumed(3, 1).unwrap();
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
        let state = StateDir::new();
        // A command that reads back as empty, as a corrupt row does.
        ledger.record_start(&row(1, "abc", &[])).unwrap();
        ledger.record_start(&row(2, "def", &["claude"])).unwrap();
        let mut m = manager(ledger.clone(), &state);
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
