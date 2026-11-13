//! Unix socket server: one reader task, one writer task and one handler per
//! connection.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use bytes::Bytes;
use overseer_proto::{
    read_frame, slot_name, write_frame, Event, Frame, Request, Response, SlotId, SlotState,
};
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, watch, Notify};
use tokio::task::{JoinHandle, JoinSet};

use crate::brain::{BrainContext, BrainService, WRITER_OFF};
use crate::env::{BaseEnv, EnvSource};
use crate::launch::RoleDirs;
use crate::ledger::Ledger;
use crate::locks::{owned_items, LockService};
use crate::manager::{valid_id, Attachment, SlotManager};
use crate::relay::courier::Courier;
use crate::slot::CatchUp;
use crate::work::store::unix_now;
use crate::work::WorkStore;

/// Frames queued per connection before it is considered too slow.
const OUTBOUND_QUEUE: usize = 256;
const INBOUND_QUEUE: usize = 64;
const EVENT_CHANNEL_CAP: usize = 256;
/// Lets the `Shutdown` reply reach its client before connections are dropped.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(100);

pub struct DaemonConfig {
    pub socket: PathBuf,
    /// Command used when `StartSlot` does not name one. Normally `["claude"]`.
    pub default_command: Vec<String>,
    /// Where sessions get their base environment.
    pub env: EnvSource,
    /// Holds `overseer.db`, `slots/` (generated claude files) and `roles/`.
    pub state_dir: PathBuf,
    /// The binary the generated hooks call (`<hook_program> hook <Event>`).
    pub hook_program: PathBuf,
    /// Settings from `config.toml`.
    pub settings: crate::config::Config,
    /// Write target patterns, from `targets.toml` and the defaults.
    pub patterns: Vec<crate::locks::targets::Pattern>,
}

/// `run` found another daemon serving the socket. Not a failure: the daemon
/// that was asked for is running, so the process should exit 0 (launchd's
/// KeepAlive would otherwise respawn it every 10 s).
#[derive(Debug)]
pub struct AlreadyRunning;

impl std::fmt::Display for AlreadyRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("daemon already running")
    }
}

impl std::error::Error for AlreadyRunning {}

type Manager = Arc<Mutex<SlotManager>>;

async fn bind(path: &PathBuf) -> anyhow::Result<UnixListener> {
    if path.exists() {
        match UnixStream::connect(path).await {
            Ok(_) => return Err(AlreadyRunning.into()),
            Err(e) => {
                tracing::info!("removing stale socket {}: {e}", path.display());
                std::fs::remove_file(path).context("remove stale socket")?;
            }
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).context("create socket dir")?;
    }
    let listener = UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .context("chmod socket")?;
    }
    Ok(listener)
}

pub async fn run(cfg: DaemonConfig) -> anyhow::Result<()> {
    let listener = bind(&cfg.socket).await?;
    // Resolved in the background so clients can connect at once; a
    // StartSlot waits for it.
    let env = BaseEnv::start(&cfg.env);
    let (events, _) = broadcast::channel::<Event>(EVENT_CHANNEL_CAP);
    let db = cfg.state_dir.join("overseer.db");
    let ledger = match Ledger::open(&db) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("ledger {} unusable, not persisting: {e:#}", db.display());
            Ledger::in_memory()
        }
    };
    let ledger = Arc::new(ledger);
    let store = WorkStore::new(&cfg.settings, ledger.clone(), events.clone(), env.clone());
    let locks = LockService::new(
        ledger.clone(),
        cfg.patterns,
        events.clone(),
        env.clone(),
        Duration::from_secs(cfg.settings.lock_idle_mins.saturating_mul(60)),
    );
    let brain = BrainService::new(&cfg.settings, ledger.clone(), events.clone());
    // Subscribed before any slot can start, so no exit is missed.
    let exit_events = events.subscribe();
    let courier_events = events.subscribe();
    let state_dir = cfg.state_dir.clone();
    let mut mgr = SlotManager::new(cfg.default_command, events)
        .with_env(env.clone())
        .with_session_note(cfg.settings.session_note.clone())
        .with_launch(
            cfg.state_dir,
            cfg.hook_program,
            ledger.clone(),
            RoleDirs {
                brain: cfg.settings.brain_dir.clone(),
                beads: cfg.settings.beads_dir.clone(),
            },
        );
    mgr.restore();
    // A session that did not survive the restart, or whose exit was never
    // seen, has ended: only a resumable one keeps its waits and relays.
    let ended = sessionless(&mgr.list());
    locks.reconcile_exits(&ended, unix_now());
    let manager: Manager = Arc::new(Mutex::new(mgr));
    if let Some(b) = &brain {
        let (manager, store, ledger) = (manager.clone(), store.clone(), ledger.clone());
        let dir = cfg.settings.brain_dir.clone();
        b.set_context(move || brain_context(&manager, &store, &ledger, &dir));
    }
    locks.on_sessionless({
        let manager = manager.clone();
        move || sessionless(&manager.lock().unwrap().list())
    });
    let courier = Courier::spawn(ledger, manager.clone(), locks.clone(), courier_events);
    store.on_relays_queued({
        let (locks, courier) = (locks.clone(), courier.clone());
        move || {
            // A notice for a slot with no session is never delivered.
            locks.drop_sessionless_relays(unix_now());
            locks.announce();
            courier.poke();
        }
    });
    let exits = tokio::spawn(watch_exits(
        exit_events,
        locks.clone(),
        manager.clone(),
        courier.clone(),
    ));
    store.start_polling();
    let shutdown = Arc::new(Notify::new());
    let mut conns = JoinSet::new();

    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    conns.spawn(handle_conn(
                        stream,
                        Services {
                            manager: manager.clone(),
                            store: store.clone(),
                            locks: locks.clone(),
                            courier: courier.clone(),
                            state_dir: state_dir.clone(),
                            brain: brain.clone(),
                        },
                        shutdown.clone(),
                        env.clone(),
                    ));
                }
                Err(e) => tracing::warn!("accept failed: {e}"),
            },
            _ = shutdown.notified() => break,
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
        }
    }

    store.shutdown();
    // The sessions are killed next; they keep their locks for the restart.
    locks.shutdown();
    manager.lock().unwrap().shutdown_all();
    // A label change just before a restart should still reach the bead.
    store.drain_labels(crate::work::store::LABEL_DRAIN).await;
    let _ = std::fs::remove_file(&cfg.socket);
    tokio::time::sleep(SHUTDOWN_GRACE).await;
    // Dropping `conns` aborts every connection task, closing the sockets.
    drop(conns);
    exits.abort();
    Ok(())
}

/// The slots in `infos` with no session: empty or exited. A resumable
/// slot still has one, waiting to be resumed.
fn sessionless(infos: &[overseer_proto::SlotInfo]) -> Vec<SlotId> {
    infos
        .iter()
        .filter(|i| matches!(i.state, SlotState::Empty | SlotState::Exited { .. }))
        .map(|i| i.id)
        .collect()
}

/// Who owns what in the vault and which slots are running or busy, from
/// the slots, the work store and the session notes in the ledger.
fn brain_context(
    manager: &Manager,
    store: &WorkStore,
    ledger: &Ledger,
    brain: &Path,
) -> BrainContext {
    let infos = manager.lock().unwrap().list();
    let running: Vec<SlotId> = infos
        .iter()
        .filter(|i| matches!(i.state, SlotState::Running { .. }))
        .map(|i| i.id)
        .collect();
    let live: Vec<SlotId> = infos
        .iter()
        .filter(|i| {
            matches!(
                i.state,
                SlotState::Running { .. } | SlotState::Resumable { .. }
            )
        })
        .map(|i| i.id)
        .collect();
    // A running slot is quiet only once it says it is idle.
    let busy = infos
        .iter()
        .filter(|i| running.contains(&i.id))
        .filter(|i| {
            i.activity
                .as_ref()
                .is_none_or(|a| a.kind != overseer_proto::ActivityKind::Idle)
        })
        .map(|i| i.id)
        .collect();
    let notes: Vec<(SlotId, String)> = live
        .iter()
        .filter_map(|&s| ledger.session_note(s).ok().flatten().map(|n| (s, n)))
        .collect();
    BrainContext {
        owners: crate::brain::paths::owners(&store.view(), &notes, &live, brain),
        running,
        busy,
    }
}

/// Ends the locks of each slot whose session exits. After a lag, which
/// may have lost exit events, every slot with no session is reconciled.
async fn watch_exits(
    mut events: broadcast::Receiver<Event>,
    locks: Arc<LockService>,
    manager: Manager,
    courier: Arc<Courier>,
) {
    loop {
        match events.recv().await {
            Ok(Event::SlotChanged(info)) if matches!(info.state, SlotState::Exited { .. }) => {
                // A start may have replaced the session since this was
                // sent, and the start already ended the old one.
                let ended = sessionless(&manager.lock().unwrap().list());
                if !ended.contains(&info.id) {
                    continue;
                }
                locks.on_session_ended(info.id, unix_now());
                // A handoff may have queued a relay.
                courier.poke();
            }
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!("lock exit watcher lagged by {n} events; reconciling");
                let ended = sessionless(&manager.lock().unwrap().list());
                locks.reconcile_exits(&ended, unix_now());
                courier.poke();
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Bounded outbound queue for one connection. Sending never waits: if the
/// queue is full the client is too slow and the connection is dropped.
#[derive(Clone)]
struct Outbox {
    tx: mpsc::Sender<Frame>,
    dead: Arc<watch::Sender<bool>>,
}

impl Outbox {
    fn send(&self, frame: Frame) -> bool {
        match self.tx.try_send(frame) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                if !self.dead.send_replace(true) {
                    tracing::warn!("client too slow, disconnected");
                }
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.dead.send_replace(true);
                false
            }
        }
    }
}

/// One connection's output streams, one per slot.
///
/// `abort` does not wait, so a stream task that is mid-poll could still queue
/// a chunk after the reply to a re-attach or detach, and the client would
/// take it as new output. Each stream therefore carries the epoch it was
/// started in and only sends while that epoch is current, checking and
/// queueing under one lock. `cut` bumps the epoch before the reply is
/// queued, so nothing from an earlier stream can follow that reply.
struct Streams {
    epochs: Arc<Mutex<[u64; crate::manager::SLOT_COUNT]>>,
    tasks: [Option<JoinHandle<()>>; crate::manager::SLOT_COUNT],
}

/// A stream's right to send `Output` for one slot.
#[derive(Clone)]
struct StreamGate {
    epochs: Arc<Mutex<[u64; crate::manager::SLOT_COUNT]>>,
    id: SlotId,
    epoch: u64,
}

impl StreamGate {
    /// Queues `data` unless a later `cut` ended this stream. Returns false
    /// when the stream should stop.
    fn send(&self, outbox: &Outbox, data: Bytes) -> bool {
        let epochs = self.epochs.lock().unwrap();
        if epochs[self.id as usize] != self.epoch {
            return false;
        }
        outbox.send(Frame::Output { id: self.id, data })
    }

    /// `send` into a queue slot already reserved, so it cannot find the
    /// queue full.
    fn send_with(&self, permit: mpsc::Permit<'_, Frame>, data: Bytes) -> bool {
        let epochs = self.epochs.lock().unwrap();
        if epochs[self.id as usize] != self.epoch {
            return false;
        }
        permit.send(Frame::Output { id: self.id, data });
        true
    }
}

impl Streams {
    fn new() -> Streams {
        Streams {
            epochs: Arc::new(Mutex::new([0; crate::manager::SLOT_COUNT])),
            tasks: Default::default(),
        }
    }

    /// Ends `id`'s stream: from now on its task can queue nothing. Returns
    /// the gate for a stream started after this point.
    fn cut(&mut self, id: SlotId) -> StreamGate {
        let epoch = {
            let mut epochs = self.epochs.lock().unwrap();
            epochs[id as usize] += 1;
            epochs[id as usize]
        };
        if let Some(old) = self.tasks[id as usize].take() {
            old.abort();
        }
        StreamGate {
            epochs: self.epochs.clone(),
            id,
            epoch,
        }
    }

    fn abort_all(&mut self) {
        for h in self.tasks.iter_mut().filter_map(Option::take) {
            h.abort();
        }
    }
}

/// What every connection serves requests from.
struct Services {
    manager: Manager,
    store: Arc<WorkStore>,
    locks: Arc<LockService>,
    courier: Arc<Courier>,
    state_dir: PathBuf,
    brain: Option<Arc<BrainService>>,
}

async fn handle_conn(
    stream: UnixStream,
    services: Services,
    shutdown: Arc<Notify>,
    mut env: BaseEnv,
) {
    let Services {
        manager,
        store,
        locks,
        courier,
        state_dir,
        brain,
    } = services;
    let (mut rd, mut wr) = stream.into_split();
    let (out_tx, mut out_rx) = mpsc::channel::<Frame>(OUTBOUND_QUEUE);
    let (dead_tx, mut dead_rx) = watch::channel(false);
    let dead_tx = Arc::new(dead_tx);
    let outbox = Outbox {
        tx: out_tx,
        dead: dead_tx.clone(),
    };

    // N1: read_frame is not cancel-safe, so it lives in its own task and is
    // never polled inside a select!. The handler only selects on channels.
    let (in_tx, mut in_rx) = mpsc::channel::<Frame>(INBOUND_QUEUE);
    let reader = tokio::spawn(async move {
        loop {
            match read_frame(&mut rd).await {
                Ok(Some(frame)) => {
                    if in_tx.send(frame).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::debug!("read error: {e}");
                    break;
                }
            }
        }
    });

    let writer_dead = dead_tx.clone();
    let writer = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if let Err(e) = write_frame(&mut wr, &frame).await {
                tracing::debug!("write error: {e}");
                break;
            }
        }
        let _ = wr.shutdown().await;
        writer_dead.send_replace(true);
    });

    let mut events = manager.lock().unwrap().subscribe_events();
    let mut streams = Streams::new();

    loop {
        tokio::select! {
            frame = in_rx.recv() => match frame {
                Some(f) => {
                    // The first starts after launch wait for the login
                    // shell's environment (bounded by its timeout).
                    if matches!(
                        &f,
                        Frame::Request {
                            req: Request::StartSlot { .. } | Request::ResumeSlot { .. },
                            ..
                        }
                    ) {
                        env.wait().await;
                    }
                    let ctx = Ctx {
                        manager: &manager,
                        store: &store,
                        locks: &locks,
                        courier: &courier,
                        state_dir: &state_dir,
                        brain: brain.as_ref(),
                    };
                    handle_frame(f, &ctx, &outbox, &shutdown, &mut streams)
                }
                None => break, // client hung up
            },
            ev = events.recv() => match ev {
                Ok(ev) => {
                    if !outbox.send(Frame::Event(ev)) { break; }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("event stream lagged by {n}");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            _ = dead_rx.changed() => break,
        }
    }

    streams.abort_all();
    reader.abort();
    writer.abort();
}

/// What a request handler works with.
struct Ctx<'a> {
    manager: &'a Manager,
    store: &'a Arc<WorkStore>,
    locks: &'a Arc<LockService>,
    courier: &'a Arc<Courier>,
    state_dir: &'a Path,
    brain: Option<&'a Arc<BrainService>>,
}

fn handle_frame(
    frame: Frame,
    ctx: &Ctx,
    outbox: &Outbox,
    shutdown: &Notify,
    streams: &mut Streams,
) {
    let (manager, store) = (ctx.manager, ctx.store);
    match frame {
        // Runs the sources, which can take up to their timeout, so it is
        // answered from a task of its own and the connection keeps serving.
        Frame::Request {
            seq,
            req: Request::RefreshWorkItems,
        } => {
            let task_store = store.clone();
            let outbox = outbox.clone();
            store.spawn(async move {
                task_store.refresh_all().await;
                outbox.send(Frame::Response {
                    seq,
                    resp: Response::Work(task_store.view()),
                });
            });
        }
        // Resolving targets can run git or kubectl (bounded by
        // `RESOLVE_WITHIN`), so it too is answered from a task.
        Frame::Request {
            seq,
            req: Request::CheckBash(check),
        } => {
            let owned = owned_items(&store.view(), check.slot);
            let locks = ctx.locks.clone();
            let outbox = outbox.clone();
            store.spawn(async move {
                let resp = locks.check_bash(check, owned).await;
                outbox.send(Frame::Response { seq, resp });
            });
        }
        // A commit waits for its turn in the queue and `bd` for the vault
        // lock, so the writer's requests are answered from a task.
        Frame::Request {
            seq,
            req:
                req @ (Request::Commit { .. }
                | Request::AppendJournal { .. }
                | Request::EditShared { .. }
                | Request::UpdateCurrentSession { .. }
                | Request::RunBd { .. }),
        } => {
            let Some(brain) = ctx.brain.cloned() else {
                outbox.send(Frame::Response {
                    seq,
                    resp: Response::Error {
                        message: WRITER_OFF.into(),
                    },
                });
                return;
            };
            let label = match &req {
                Request::UpdateCurrentSession { slot, .. } if valid_id(*slot) => {
                    manager.lock().unwrap().list()[*slot as usize].label.clone()
                }
                _ => String::new(),
            };
            let outbox = outbox.clone();
            store.spawn(async move {
                let resp = brain_request(&brain, req, &label).await;
                outbox.send(Frame::Response { seq, resp });
            });
        }
        Frame::Request { seq, req } => {
            let is_shutdown = matches!(req, Request::Shutdown);
            // Each of these can queue a relay or make one deliverable.
            let pokes = matches!(
                req,
                Request::SendRelay { .. }
                    | Request::ClaimTarget { .. }
                    | Request::ReleaseTarget { .. }
                    | Request::ForceRelease { .. }
                    | Request::Release { .. }
                    | Request::Assign { .. }
                    | Request::StartSlot { .. }
                    | Request::StopSlot { .. }
                    | Request::Hook(_)
            );
            let mut pending_attach = None;
            let resp = handle_request(req, ctx, streams, &mut pending_attach);
            if pokes {
                ctx.courier.poke();
            }
            // A re-attach ends the earlier stream before its Ok is queued.
            let gate = pending_attach.as_ref().map(|(id, _)| streams.cut(*id));
            outbox.send(Frame::Response { seq, resp });
            // Ok first, then the snapshot, then live output.
            if let (Some((_, att)), Some(gate)) = (pending_attach, gate) {
                start_stream(gate, att, outbox, streams);
            }
            if is_shutdown {
                shutdown.notify_one();
            }
        }
        Frame::Input { id, data } => {
            // N2: a bad id on a fire-and-forget frame is logged and dropped.
            if !valid_id(id) {
                tracing::warn!("ignoring input frame for invalid slot id {id}");
                return;
            }
            let slot = manager.lock().unwrap().slot(id);
            match slot {
                Some(slot) => {
                    // The PTY write is a short blocking call; the kernel
                    // buffer absorbs keystroke-sized input.
                    if let Err(e) = slot.write(&data) {
                        tracing::debug!("input to {} failed: {e}", slot_name(id));
                    }
                }
                None => tracing::debug!("ignoring input for empty slot {}", slot_name(id)),
            }
        }
        other => tracing::debug!("ignoring unexpected frame from client: {other:?}"),
    }
}

/// The writer's requests that wait (see `handle_frame`).
async fn brain_request(brain: &BrainService, req: Request, label: &str) -> Response {
    let sender = |from: Option<SlotId>| -> Result<overseer_proto::Sender, Response> {
        match from {
            None => Ok(overseer_proto::Sender::You),
            Some(slot) if valid_id(slot) => Ok(overseer_proto::Sender::Slot { slot }),
            Some(slot) => Err(Response::Error {
                message: format!("invalid slot id {slot}"),
            }),
        }
    };
    let from = match &req {
        Request::Commit { from, .. }
        | Request::AppendJournal { from, .. }
        | Request::EditShared { from, .. }
        | Request::RunBd { from, .. } => match sender(*from) {
            Ok(s) => s,
            Err(r) => return r,
        },
        _ => overseer_proto::Sender::You,
    };
    match req {
        Request::Commit { paths, message, .. } => {
            let owners = brain.context().owners;
            brain.commit(from, paths, message, &owners).await
        }
        Request::AppendJournal { section, text, .. } => {
            brain.append_journal(from, &section, &text).await
        }
        Request::EditShared {
            path,
            old_text,
            new_text,
            ..
        } => brain.edit_shared(from, &path, &old_text, &new_text).await,
        Request::UpdateCurrentSession { slot, .. } if !valid_id(slot) => Response::Error {
            message: format!("invalid slot id {slot}"),
        },
        Request::UpdateCurrentSession { slot, text } => {
            brain.update_current_session(slot, label, &text).await
        }
        Request::RunBd { args, .. } => brain.run_bd(args).await,
        other => Response::Error {
            message: format!("not a 2ndBrain writer request: {other:?}"),
        },
    }
}

fn handle_request(
    req: Request,
    ctx: &Ctx,
    streams: &mut Streams,
    pending_attach: &mut Option<(SlotId, Attachment)>,
) -> Response {
    let (manager, store, locks, state_dir) = (ctx.manager, ctx.store, ctx.locks, ctx.state_dir);
    let err = |message: String| Response::Error { message };
    match req {
        Request::Hello { client } => {
            tracing::debug!("hello from {client}");
            Response::Ok
        }
        Request::ListSlots => Response::Slots(manager.lock().unwrap().list()),
        // RefreshWorkItems is answered in `handle_frame` once the sources
        // have run; this arm only keeps the match whole.
        Request::ListWorkItems | Request::RefreshWorkItems => Response::Work(store.view()),
        Request::Assign {
            key,
            slot,
            confirm,
            move_owner,
        } => {
            let owner = |v: overseer_proto::WorkView| {
                v.items.iter().find(|i| i.key == key).and_then(|i| i.owner)
            };
            let before = owner(store.view());
            let now = unix_now();
            let resp = store.assign(&key, slot, confirm, move_owner, now);
            // A move: the targets claimed for the item go with the old owner.
            if resp == Response::Ok && before.is_some_and(|b| b != slot) {
                locks.on_item_released(&key, now);
            }
            resp
        }
        Request::Release { key } => {
            let now = unix_now();
            let resp = store.release(&key, now);
            if resp == Response::Ok {
                locks.on_item_released(&key, now);
            }
            resp
        }
        Request::ClearNotices { slot } => store.clear_notices(slot),
        Request::ListLocks => Response::Locks(locks.view()),
        Request::ClaimTarget {
            slot,
            target,
            reason,
            item,
        } => locks.claim(
            slot,
            &target,
            reason.as_deref(),
            item.as_deref(),
            &owned_items(&store.view(), slot),
            unix_now(),
        ),
        Request::ReleaseTarget { slot, target } => locks.release(slot, &target, unix_now()),
        Request::ForceRelease { target, holder } => {
            locks.force_release(&target, holder, unix_now())
        }
        Request::SendRelay { from, to, text } => {
            // An empty or exited slot has no session to type into; a
            // resumable one gets it once resumed.
            let live = valid_id(to)
                && !matches!(
                    manager.lock().unwrap().list()[to as usize].state,
                    SlotState::Empty | SlotState::Exited { .. }
                );
            crate::relay::send(locks, from, to, &text, live, unix_now())
        }
        // Answered in `handle_frame` from a task; this arm only keeps the
        // match whole.
        Request::CheckBash(_) => Response::Verdict {
            deny: None,
            rewrite: None,
        },
        // Answered in `handle_frame` from a task; this arm only keeps the
        // match whole.
        Request::Commit { .. }
        | Request::AppendJournal { .. }
        | Request::EditShared { .. }
        | Request::UpdateCurrentSession { .. }
        | Request::RunBd { .. } => err(WRITER_OFF.to_string()),
        Request::BrainStatus => {
            Response::Brain(ctx.brain.map_or_else(BrainService::off_view, |b| b.view()))
        }
        // While the writer is off the hook applies none of its checks.
        Request::CheckWrite(check) => Response::Verdict {
            deny: ctx
                .brain
                .and_then(|b| b.check_write(&check, &b.context().owners)),
            rewrite: None,
        },
        Request::Kickoff { slot } if !valid_id(slot) => err(format!("invalid slot id {slot}")),
        Request::Kickoff { slot } => {
            let items = store.view().items;
            let template = match crate::launch::assignment_template(state_dir) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("assignment template unreadable, using the default: {e}");
                    crate::launch::ASSIGNMENT.to_string()
                }
            };
            match crate::work::store::kickoff_text(&template, slot, &items, store.brain_dir()) {
                Some(text) => Response::Text { text },
                None => err(format!("{} owns no work items", slot_name(slot))),
            }
        }
        Request::StartSlot {
            id,
            label,
            cwd,
            command,
            prompt,
        } => {
            // Held across both, so no other start or stop comes between.
            let mut m = manager.lock().unwrap();
            // A fresh start replaces whatever session the slot had (an
            // exited one, perhaps not announced yet, or a resumable one):
            // its waits, unused grants and pending relays are not the new
            // session's. A resume keeps them.
            let running =
                valid_id(id) && matches!(m.list()[id as usize].state, SlotState::Running { .. });
            if !running {
                locks.on_session_ended(id, unix_now());
            }
            match m.start(id, label, &cwd, command, prompt) {
                Ok(()) => {
                    // A new session starts with no section of its own.
                    if let Some(b) = ctx.brain.cloned() {
                        tokio::spawn(async move { b.clear_current_session(id).await });
                    }
                    Response::Ok
                }
                Err(e) => err(e),
            }
        }
        Request::StopSlot { id } => {
            let mut m = manager.lock().unwrap();
            // Stopping a resumable slot declines it: that session has ended
            // and no exit event will say so.
            let declined =
                valid_id(id) && matches!(m.list()[id as usize].state, SlotState::Resumable { .. });
            match m.stop(id) {
                Ok(()) => {
                    if declined {
                        locks.on_session_ended(id, unix_now());
                    }
                    Response::Ok
                }
                Err(e) => err(e),
            }
        }
        Request::ResumeSlot { id } => match manager.lock().unwrap().resume(id) {
            Ok(()) => Response::Ok,
            Err(m) => err(m),
        },
        Request::Attach { id } => match manager.lock().unwrap().attach(id) {
            Ok(a) => {
                *pending_attach = Some((id, a));
                Response::Ok
            }
            Err(m) => err(m),
        },
        Request::Detach { id } => {
            if !valid_id(id) {
                return err(format!("invalid slot id {id}"));
            }
            // Ends the stream before the Ok is queued.
            streams.cut(id);
            Response::Ok
        }
        Request::Resize { id, cols, rows } => match manager.lock().unwrap().slot(id) {
            Some(slot) => match slot.resize(cols, rows) {
                Ok(()) => Response::Ok,
                Err(e) => err(format!("{e:#}")),
            },
            None if !valid_id(id) => err(format!("invalid slot id {id}")),
            None => err(format!("{} is empty", slot_name(id))),
        },
        Request::Shutdown => Response::Ok,
        // Always Ok: a hook runs inside Claude and must never see a failure
        // because of the daemon's state.
        Request::Hook(ev) => {
            manager.lock().unwrap().hook(&ev);
            locks.on_hook(&ev, unix_now());
            Response::Ok
        }
    }
}

/// Largest Output frame built by merging queued chunks.
const COALESCE_MAX: usize = 64 * 1024;

/// Appends the chunks already waiting in `live` to `first`, up to
/// `COALESCE_MAX` bytes, so a program that writes one line per syscall (each
/// a tiny PTY read) costs one frame per wakeup instead of one per line. The
/// outbound queue counts frames, so without this a client that keeps up is
/// still cut off as slow during a burst.
fn coalesce(first: Bytes, live: &mut broadcast::Receiver<Bytes>) -> Merged {
    let mut buf: Option<Vec<u8>> = None;
    let mut end = None;
    while buf.as_ref().map_or(first.len(), Vec::len) < COALESCE_MAX {
        match live.try_recv() {
            Ok(more) => buf
                .get_or_insert_with(|| first.to_vec())
                .extend_from_slice(&more),
            Err(broadcast::error::TryRecvError::Lagged(_)) => {
                end = Some(StreamEnd::Lagged);
                break;
            }
            Err(broadcast::error::TryRecvError::Empty) => break,
            Err(broadcast::error::TryRecvError::Closed) => {
                end = Some(StreamEnd::Closed);
                break;
            }
        }
    }
    Merged {
        data: buf.map_or(first, Bytes::from),
        end,
    }
}

/// Output merged by `coalesce`, and why merging stopped early, if it did.
struct Merged {
    data: Bytes,
    end: Option<StreamEnd>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum StreamEnd {
    /// The receiver fell behind and lost chunks after `data`.
    Lagged,
    /// The output stream ended after `data`.
    Closed,
}

/// Free outbound slots a stream leaves for replies and events. A stream
/// waits for room above this instead of filling the queue, so a burst
/// cannot crowd out a reply and get the client cut off.
const STREAM_HEADROOM: usize = 32;
/// How long a stream waits for room in the outbound queue before the client
/// counts as too slow and is disconnected.
pub const SLOW_CLIENT_GRACE: Duration = Duration::from_secs(2);

/// Queues one `Output` frame for a live stream, waiting (up to
/// `SLOW_CLIENT_GRACE`) while the client drains its queue. Meanwhile the
/// slot's output piles up in the broadcast, and the next frame carries more
/// of it. Returns false when the stream should stop.
async fn send_live(gate: &StreamGate, outbox: &Outbox, data: Bytes) -> bool {
    let wait = outbox.tx.reserve_many(STREAM_HEADROOM + 1);
    match tokio::time::timeout(SLOW_CLIENT_GRACE, wait).await {
        Ok(Ok(mut permits)) => match permits.next() {
            Some(permit) => gate.send_with(permit, data),
            None => false,
        },
        Ok(Err(_)) => {
            outbox.dead.send_replace(true);
            false
        }
        Err(_) => {
            if !outbox.dead.send_replace(true) {
                tracing::warn!("client too slow, disconnected");
            }
            false
        }
    }
}

/// Sends the snapshot as one `Output` frame, then forwards live output until
/// the stream ends, the connection dies or the client detaches.
/// `gate` comes from the `cut` that ended any earlier stream for the slot.
fn start_stream(gate: StreamGate, att: Attachment, outbox: &Outbox, streams: &mut Streams) {
    let id = gate.id;
    if !gate.send(outbox, Bytes::from(att.snapshot)) {
        return;
    }
    let Some(mut live) = att.live else { return };
    let (slot, mut pos) = (att.slot, att.pos);
    let outbox = outbox.clone();
    streams.tasks[id as usize] = Some(tokio::spawn(async move {
        loop {
            let merged = match live.recv().await {
                Ok(first) => coalesce(first, &mut live),
                Err(broadcast::error::RecvError::Lagged(_)) => Merged {
                    data: Bytes::new(),
                    end: Some(StreamEnd::Lagged),
                },
                // Output ended (child exited and the PTY drained). Everything
                // buffered before the close has already been delivered.
                Err(broadcast::error::RecvError::Closed) => break,
            };
            pos += merged.data.len() as u64;
            if !merged.data.is_empty() && !send_live(&gate, &outbox, merged.data).await {
                break;
            }
            match merged.end {
                None => {}
                Some(StreamEnd::Closed) => break,
                // Chunks were lost: fill the gap from the ring and carry on
                // with a fresh receiver, so the client sees the exact stream.
                Some(StreamEnd::Lagged) => {
                    let (missed, end, rx) = slot.catch_up(pos);
                    let bytes = match missed {
                        CatchUp::Missed(bytes) => bytes,
                        CatchUp::Resync(bytes) => {
                            tracing::warn!(
                                "{} output lagged past the ring, resyncing",
                                slot_name(id)
                            );
                            bytes
                        }
                    };
                    pos = end;
                    if !bytes.is_empty() && !send_live(&gate, &outbox, Bytes::from(bytes)).await {
                        break;
                    }
                    match rx {
                        Some(rx) => live = rx,
                        // The stream has ended; the catch-up held all of it.
                        None => break,
                    }
                }
            }
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outbox() -> (Outbox, mpsc::Receiver<Frame>) {
        let (tx, rx) = mpsc::channel(16);
        let (dead, _) = watch::channel(false);
        let dead = Arc::new(dead);
        (Outbox { tx, dead }, rx)
    }

    fn output(rx: &mut mpsc::Receiver<Frame>) -> Vec<Bytes> {
        let mut got = Vec::new();
        while let Ok(f) = rx.try_recv() {
            match f {
                Frame::Output { data, .. } => got.push(data),
                other => panic!("unexpected {other:?}"),
            }
        }
        got
    }

    #[test]
    fn a_cut_stream_queues_nothing_more() {
        let (outbox, mut rx) = outbox();
        let mut streams = Streams::new();
        let old = streams.cut(1);
        assert!(old.send(&outbox, Bytes::from_static(b"old")));
        // A re-attach (or detach) cuts the stream; the old task may still
        // be running, but whatever it sends now is dropped and it stops.
        let new = streams.cut(1);
        assert!(!old.send(&outbox, Bytes::from_static(b"stale")));
        assert!(new.send(&outbox, Bytes::from_static(b"new")));
        // Other slots are untouched.
        let other = streams.cut(2);
        assert!(new.send(&outbox, Bytes::from_static(b"still")));
        assert!(other.send(&outbox, Bytes::from_static(b"two")));
        assert_eq!(output(&mut rx), ["old", "new", "still", "two"]);
    }
}
