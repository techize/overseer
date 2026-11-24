//! Unix socket server: one reader task, one writer task and one handler per
//! connection.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use bytes::Bytes;
use overseer_proto::{
    read_frame,
    settings::{SettingEdit, TokenStatus},
    slot_name, write_frame, BoardView, DaemonInfo, Event, Frame, HookEvent, Request, Response,
    SlotId, SlotState, TokenKind,
};
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, watch, Notify};
use tokio::task::{JoinHandle, JoinSet};

use crate::attention::AttentionService;
use crate::brain::{BrainContext, BrainOptions, BrainService, WRITER_OFF};
use crate::coord::plan::PlanCtx;
use crate::coord::CoordService;
use crate::env::{BaseEnv, EnvSource};
use crate::launch::RoleDirs;
use crate::ledger::Ledger;
use crate::locks::{owned_items, LockService};
use crate::manager::{valid_id, Attachment, SlotManager};
use crate::relay::courier::Courier;
use crate::settings::SettingsFiles;
use crate::slot::CatchUp;
use crate::timeline_service::{LogSource, TimelineService};
use crate::work::store::{unix_now, KickoffFacts};
use crate::work::token::{save_token, token_status};
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
    /// The binary run as `<holder_program> hold <state_dir>` to keep each
    /// session's terminal in its own process. `None` runs sessions in the
    /// daemon, which then ends them when it stops.
    pub holder_program: Option<PathBuf>,
    /// Settings from `config.toml`.
    pub settings: crate::config::Config,
    /// Write target patterns, from `targets.toml` and the defaults.
    pub patterns: Vec<crate::locks::targets::Pattern>,
    /// The files the Settings view reads and saves.
    pub files: SettingsFiles,
}

/// What `GetSettings` and `SaveSettings` work with. `running` is the
/// config this daemon started with; saves are one at a time.
struct SettingsCtx {
    files: SettingsFiles,
    running: crate::config::Config,
    home: PathBuf,
    save: Mutex<()>,
    env: BaseEnv,
    /// The last token read, used for a save's reply.
    tokens: Mutex<Option<Tokens>>,
}

impl SettingsCtx {
    fn view(&self, info: &DaemonInfo) -> Response {
        Response::Settings(crate::settings::read(
            &self.files,
            &self.running,
            &self.home,
            info.clone(),
        ))
    }

    fn save(&self, info: &DaemonInfo, base_hash: &str, edits: &[SettingEdit]) -> Response {
        let _one = self.save.lock().unwrap();
        match crate::settings::save(&self.files, &self.home, base_hash, edits) {
            Ok(()) => self.view(info),
            Err(message) => Response::Error { message },
        }
    }

    /// Where each token is for the running `jira_email`, read now. Both
    /// reads run at once, each waiting at most `STATUS_TIMEOUT` on the
    /// Keychain, so a locked Keychain costs Settings a few seconds, not 30.
    async fn read_tokens(&self) -> Tokens {
        let email = self.running.jira_email.trim();
        if email.is_empty() {
            return Tokens::default();
        }
        let vars = self.env.get();
        let security = &self.files.security;
        let (jira, bitbucket) = tokio::join!(
            token_status(TokenKind::Jira, email, &vars, security),
            token_status(TokenKind::Bitbucket, email, &vars, security),
        );
        let read = Tokens { jira, bitbucket };
        *self.tokens.lock().unwrap() = Some(read);
        read
    }

    /// `resp` with the token states filled in when it is a Settings view.
    /// `fresh` reads them again; otherwise the last read is used (a settings
    /// save cannot change a token), and only the first view reads them.
    async fn with_token_state(&self, resp: Response, fresh: bool) -> Response {
        match resp {
            Response::Settings(mut v) => {
                let known = *self.tokens.lock().unwrap();
                let t = match known {
                    Some(t) if !fresh => t,
                    _ => self.read_tokens().await,
                };
                v.jira_token = t.jira;
                v.bitbucket_token = t.bitbucket;
                Response::Settings(v)
            }
            other => other,
        }
    }

    /// Stores the `kind` token in the Keychain. It is not logged or
    /// answered. The save reads the item back, so a success is recorded as
    /// stored without asking the Keychain again.
    async fn set_token(&self, kind: TokenKind, token: &str) -> Response {
        let email = self.running.jira_email.trim();
        if email.is_empty() {
            return Response::Error {
                message: "set jira_email first".into(),
            };
        }
        match save_token(kind, email, token, &self.files.security).await {
            Ok(()) => {
                if let Some(t) = self.tokens.lock().unwrap().as_mut() {
                    match kind {
                        TokenKind::Jira => t.jira = TokenStatus::Keychain,
                        TokenKind::Bitbucket => t.bitbucket = TokenStatus::Keychain,
                    }
                }
                Response::Ok
            }
            Err(message) => Response::Error { message },
        }
    }
}

/// Where each API token was last found, for the Settings view.
#[derive(Clone, Copy, Default)]
struct Tokens {
    jira: TokenStatus,
    bitbucket: TokenStatus,
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

/// The work source a saved token is for.
fn source_for(kind: TokenKind) -> crate::work::store::Source {
    match kind {
        TokenKind::Jira => crate::work::store::Source::Jira,
        TokenKind::Bitbucket => crate::work::store::Source::Prs,
    }
}

/// The notes folder notice goes to S0, whose role works in that folder.
const NOTES_NOTICE_SLOT: SlotId = 0;

/// Leaves `text` for `slot` unless the same notice is still waiting, so a
/// restart does not stack copies.
fn notice_once(ledger: &Ledger, slot: SlotId, text: &str) {
    let waiting = ledger
        .notices()
        .map(|n| n.iter().any(|n| n.slot == slot && n.text == text))
        .unwrap_or(false);
    if !waiting {
        if let Err(e) = ledger.add_notice(slot, unix_now(), text) {
            tracing::warn!("could not store the notes notice: {e:#}");
        }
    }
}

/// Runs the daemon until a `Shutdown` or `StopAll` request. Signals are
/// left alone: an embedding process (the tests) keeps its own. The daemon
/// proper runs [`run_until`] with [`stop_signal`].
pub async fn run(cfg: DaemonConfig) -> anyhow::Result<()> {
    run_until(cfg, std::future::pending()).await
}

/// SIGTERM or SIGINT: launchd stopping the job (`bootout`, `kickstart -k`)
/// or Ctrl-C on a foreground daemon. The handlers are in place once this
/// returns, so a signal sent from then on is never the default kill.
/// Needs a tokio runtime.
pub fn stop_signal() -> std::io::Result<impl std::future::Future<Output = ()>> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    Ok(async move {
        tokio::select! {
            _ = term.recv() => tracing::info!("SIGTERM: shutting down"),
            _ = int.recv() => tracing::info!("SIGINT: shutting down"),
        }
    })
}

/// [`run`], also stopping, by the same path as a `Shutdown` request, when
/// `stop` resolves.
pub async fn run_until(
    mut cfg: DaemonConfig,
    stop: impl std::future::Future<Output = ()>,
) -> anyhow::Result<()> {
    let listener = bind(&cfg.socket).await?;
    // Before anything reads the settings: a refused folder turns the notes
    // writer and the sprint sync off.
    let notes_refused = crate::notes::prepare(&mut cfg.settings);
    let info = Arc::new(DaemonInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        build: overseer_proto::current_build().to_string(),
        pid: std::process::id(),
        started_at: unix_now(),
        exe: std::env::current_exe().unwrap_or_default(),
    });
    // Resolved in the background so clients can connect at once; a
    // StartSlot waits for it.
    let env = BaseEnv::start(&cfg.env);
    let settings = Arc::new(SettingsCtx {
        files: cfg.files,
        running: cfg.settings.clone(),
        // The same home `main` expands `config.toml` against.
        home: crate::config::home_from_env(),
        save: Mutex::new(()),
        env: env.clone(),
        tokens: Mutex::new(None),
    });
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
    if let Some(text) = &notes_refused {
        notice_once(&ledger, NOTES_NOTICE_SLOT, text);
    }
    let store = WorkStore::new(&cfg.settings, ledger.clone(), events.clone(), env.clone());
    let locks = LockService::new(
        ledger.clone(),
        cfg.patterns,
        events.clone(),
        env.clone(),
        Duration::from_secs(cfg.settings.lock_idle_mins.saturating_mul(60)),
        cfg.settings.user().to_string(),
    );
    let coord = CoordService::new(
        ledger.clone(),
        events.clone(),
        cfg.settings.user().to_string(),
    );
    let timeline = coord.watch(events.subscribe());
    let brain = BrainService::with_options(
        &cfg.settings,
        ledger.clone(),
        events.clone(),
        BrainOptions {
            program: cfg.hook_program.clone(),
            ..BrainOptions::from_config(&cfg.settings)
        },
    );
    // Subscribed before any slot can start, so no exit is missed.
    let exit_events = events.subscribe();
    let courier_events = events.subscribe();
    let log_events = events.subscribe();
    let state_dir = cfg.state_dir.clone();
    let holder_program = cfg.holder_program;
    let mut mgr = SlotManager::new(cfg.default_command, events.clone())
        .with_env(env.clone())
        .with_session_note(cfg.settings.session_note.clone())
        .with_launch(
            cfg.state_dir,
            cfg.hook_program,
            ledger.clone(),
            RoleDirs {
                brain: cfg.settings.brain_dir.clone(),
                beads: cfg.settings.beads_dir.clone(),
                brain_writer: cfg.settings.brain_writer,
                current_session: cfg.settings.current_session.clone(),
                user: cfg.settings.user().to_string(),
            },
        );
    if let Some(program) = holder_program {
        mgr = mgr.with_holder(program);
    }
    mgr.restore();
    // A session that did not survive the restart, or whose exit was never
    // seen, has ended: only a resumable one keeps its waits and relays.
    let ended = sessionless(&mgr.list());
    locks.reconcile_exits(&ended, unix_now());
    // Items and status requests too, for a session that ended while the
    // daemon was down (Exited). An Empty slot may own items assigned ahead
    // of its start, so it keeps them.
    for slot in exited(&mgr.list()) {
        session_over_rest(&coord, &store, slot, unix_now());
    }
    let manager: Manager = Arc::new(Mutex::new(mgr));
    // Hooks that found no daemon were spooled. They come after the restore,
    // so what happened while down wins over the saved activity; an event of
    // a run that is no longer there is dropped by the same run check as a
    // live one. A spool that cannot be read never blocks startup.
    match overseer_proto::spool::take(&state_dir) {
        Ok(events) => {
            if !events.is_empty() {
                tracing::info!("applying {} hook events spooled while down", events.len());
            }
            for ev in &events {
                apply_hook(&manager, &locks, ev);
            }
        }
        Err(e) => tracing::warn!("hook spool not read: {e:#}"),
    }
    let (seen_tx, seen_rx) = mpsc::unbounded_channel();
    manager.lock().unwrap().feed_stretches(seen_tx);
    // Poked whenever a stretch or wait row is written.
    let timeline_nudge = Arc::new(Notify::new());
    if let Some(b) = &brain {
        {
            let (manager, store, ledger) = (manager.clone(), store.clone(), ledger.clone());
            let dir = cfg.settings.brain_dir.clone();
            b.set_context(move || brain_context(&manager, &store, &ledger, &dir));
        }
        b.set_env(env.clone());
        // Label writes take the vault lock like a session's bd write. Weak:
        // the brain's context already holds the store.
        let weak = Arc::downgrade(b);
        store.route_label_writes(Box::new(move |args| {
            let weak = weak.clone();
            Box::pin(async move {
                match weak.upgrade() {
                    Some(b) => b.write_label(args).await,
                    None => Err("the notes writer has stopped".into()),
                }
            })
        }));
    }
    locks.on_sessionless({
        let manager = manager.clone();
        move || sessionless(&manager.lock().unwrap().list())
    });
    let courier = Courier::spawn(
        ledger.clone(),
        manager.clone(),
        locks.clone(),
        courier_events,
        cfg.settings.user().to_string(),
    );
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
        coord.clone(),
        store.clone(),
        manager.clone(),
        courier.clone(),
    ));
    let log_check = tokio::spawn(watch_session_logs(
        log_events,
        seen_rx,
        LogCheck {
            manager: manager.clone(),
            ledger: ledger.clone(),
            store: store.clone(),
            brain: brain.clone(),
            brain_dir: cfg.settings.brain_dir.clone(),
            home: crate::config::home_from_env(),
            patterns: cfg.settings.session_logs.clone(),
            runs_file: cfg.settings.slot_runs.clone(),
            min_secs: cfg.settings.session_log_min_mins.saturating_mul(60) as i64,
            timeline_nudge: timeline_nudge.clone(),
        },
    ));
    store.start_polling();
    let attention = AttentionService::spawn(
        ledger.clone(),
        manager.clone(),
        locks.clone(),
        store.clone(),
        events.clone(),
        !cfg.settings.pr_author.trim().is_empty(),
    );
    let day_timeline = TimelineService::spawn(
        ledger.clone(),
        (!cfg.settings.session_logs.is_empty()).then(|| LogSource {
            brain_dir: cfg.settings.brain_dir.clone(),
            home: crate::config::home_from_env(),
            patterns: cfg.settings.session_logs.clone(),
        }),
        cfg.settings.session_log_min_mins.saturating_mul(60) as i64,
        timeline_nudge.clone(),
        events.clone(),
    );
    let plans = Arc::new(PlanCtx {
        manager: manager.clone(),
        store: store.clone(),
        locks: locks.clone(),
        brain: brain.clone(),
        env: env.clone(),
        worktree_root: cfg.settings.worktree_root.clone(),
        brain_dir: cfg.settings.brain_dir.clone(),
        current_session: cfg.settings.current_session.clone(),
        state_dir: state_dir.clone(),
    });
    let shutdown = Arc::new(Notify::new());
    let mut conns = JoinSet::new();
    tokio::pin!(stop);

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
                            coord: coord.clone(),
                            plans: plans.clone(),
                            courier: courier.clone(),
                            state_dir: state_dir.clone(),
                            brain: brain.clone(),
                            info: info.clone(),
                            settings: settings.clone(),
                            attention: attention.clone(),
                            timeline: day_timeline.clone(),
                        },
                        shutdown.clone(),
                        env.clone(),
                    ));
                }
                Err(e) => tracing::warn!("accept failed: {e}"),
            },
            _ = shutdown.notified() => break,
            _ = &mut stop => break,
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
        }
    }

    // First stop taking connections: a hook sent from now on finds no
    // daemon and spools its event for the next one, rather than queueing
    // on a listener nobody will accept from again. Connections already
    // accepted run on until `conns` is dropped.
    drop(listener);
    let _ = std::fs::remove_file(&cfg.socket);
    store.shutdown();
    // Sessions ended by the shutdown are not checked.
    log_check.abort();
    // The sessions are let go next; they keep their locks for the restart.
    locks.shutdown();
    manager.lock().unwrap().shutdown_all();
    // A label change just before a restart should still reach the bead.
    store.drain_labels(crate::work::store::LABEL_DRAIN).await;
    tokio::time::sleep(SHUTDOWN_GRACE).await;
    // Dropping `conns` aborts every connection task, closing the sockets.
    drop(conns);
    exits.abort();
    timeline.abort();
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

/// The slots whose session exited and has not been replaced. Unlike
/// `sessionless`, an Empty slot is left out: it may own items assigned
/// ahead of its start.
fn exited(infos: &[overseer_proto::SlotInfo]) -> Vec<SlotId> {
    infos
        .iter()
        .filter(|i| matches!(i.state, SlotState::Exited { .. }))
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

/// What the session-log check needs (see [`crate::session_logs`]).
struct LogCheck {
    manager: Manager,
    ledger: Arc<Ledger>,
    store: Arc<WorkStore>,
    brain: Option<Arc<BrainService>>,
    brain_dir: PathBuf,
    home: PathBuf,
    patterns: Vec<String>,
    runs_file: String,
    /// Shorter runs are not checked: a session stopped at once has
    /// nothing to log.
    min_secs: i64,
    /// Poked after each stretch or wait row written.
    timeline_nudge: Arc<Notify>,
}

/// Stretch rows older than this are deleted at daemon start.
const STRETCH_KEEP_SECS: i64 = 7 * 86400;
/// A running stretch's row is written again once its last work has moved
/// on this far.
const STRETCH_WRITE_SECS: i64 = 60;

/// The slots' stretches and idle waits, and what of them the ledger has.
#[derive(Default)]
struct Tracker {
    stretches: crate::stretches::Stretches,
    /// Per slot, the open stretch as last written: session, first, last.
    written: std::collections::HashMap<SlotId, (String, i64, i64)>,
    /// Slots with a wait open in the ledger.
    waiting: std::collections::HashSet<SlotId>,
}

impl Tracker {
    /// Picks up from what the last daemon stored: old rows go, a stretch
    /// whose slot still runs its transcript carries on, and any other
    /// ends at its last work. Waits of slots no longer running close now.
    fn reconcile(&mut self, c: &LogCheck, now: i64) {
        if let Err(e) = c.ledger.prune_stretches(now - STRETCH_KEEP_SECS) {
            tracing::warn!("old stretches not pruned: {e:#}");
        }
        let open = c.ledger.open_stretches().unwrap_or_else(|e| {
            tracing::warn!("open stretches not read: {e:#}");
            Vec::new()
        });
        for row in open {
            let s = crate::stretches::Stretch {
                slot: row.slot,
                session: row.session,
                first: row.first,
                last: row.last,
            };
            let running = c.manager.lock().unwrap().running_session(s.slot);
            if running.as_deref() == Some(s.session.as_str()) {
                // Two open rows for one transcript: the earlier one ends.
                if let Some(before) = self.stretches.open(s.slot) {
                    self.end(c, &before, before.last);
                }
                self.stretches.seed(&s);
                self.written
                    .insert(s.slot, (s.session.clone(), s.first, s.last));
            } else {
                self.end(c, &s, s.last);
            }
        }
        let waits = c.ledger.waits_since(i64::MAX).unwrap_or_else(|e| {
            tracing::warn!("open waits not read: {e:#}");
            Vec::new()
        });
        for w in waits.into_iter().filter(|w| w.end.is_none()) {
            if c.manager.lock().unwrap().running_session(w.slot).is_some() {
                self.waiting.insert(w.slot);
            } else {
                store_wait_end(c, w.slot, now);
            }
        }
    }

    /// Takes in one hook event: a stretch it ended is stored and checked,
    /// and the slot's open stretch is stored when it is new or has moved on.
    async fn seen(&mut self, c: &LogCheck, s: &crate::stretches::Seen) {
        if let Some(done) = self.stretches.seen(s) {
            self.end(c, &done, s.at);
            stretch_ended(c, &done, true).await;
        }
        let Some(open) = self.stretches.open(s.slot) else {
            return;
        };
        let stored = self
            .written
            .get(&s.slot)
            .is_some_and(|(session, first, last)| {
                *session == open.session
                    && *first == open.first
                    && open.last - last < STRETCH_WRITE_SECS
            });
        if stored {
            return;
        }
        match c
            .ledger
            .put_stretch(open.slot, &open.session, open.first, open.last)
        {
            Ok(()) => {
                self.written
                    .insert(open.slot, (open.session, open.first, open.last));
                c.timeline_nudge.notify_one();
            }
            Err(e) => tracing::warn!("S{} stretch not stored: {e:#}", open.slot),
        }
    }

    /// Stores `s` as ended at `ended`.
    fn end(&mut self, c: &LogCheck, s: &crate::stretches::Stretch, ended: i64) {
        if self
            .written
            .get(&s.slot)
            .is_some_and(|(session, first, _)| *session == s.session && *first == s.first)
        {
            self.written.remove(&s.slot);
        }
        match c
            .ledger
            .end_stretch(s.slot, &s.session, s.first, s.last, ended)
        {
            Ok(()) => c.timeline_nudge.notify_one(),
            Err(e) => tracing::warn!("S{} stretch end not stored: {e:#}", s.slot),
        }
    }

    /// Opens or closes the slot's idle wait from its new state: an idle
    /// wait opens one when its activity starts; anything else, or the
    /// slot exiting, closes it.
    fn slot_changed(&mut self, c: &LogCheck, info: &overseer_proto::SlotInfo) {
        let activity = info.activity.as_ref();
        let idle_wait =
            activity.filter(|a| a.kind == overseer_proto::ActivityKind::Idle && a.idle_wait);
        match (&info.state, idle_wait) {
            (SlotState::Running { .. }, Some(a)) => {
                if self.waiting.contains(&info.id) {
                    return;
                }
                let session = self
                    .stretches
                    .open(info.id)
                    .map(|s| s.session)
                    .or_else(|| c.manager.lock().unwrap().running_session(info.id))
                    .unwrap_or_default();
                match c.ledger.open_wait(info.id, &session, a.since) {
                    Ok(()) => {
                        self.waiting.insert(info.id);
                        c.timeline_nudge.notify_one();
                    }
                    Err(e) => tracing::warn!("S{} wait not stored: {e:#}", info.id),
                }
            }
            (SlotState::Exited { at, .. }, _) => self.close_wait(c, info.id, *at),
            (_, _) => {
                let end = activity.map_or_else(unix_now, |a| a.since);
                self.close_wait(c, info.id, end);
            }
        }
    }

    /// Closes the slot's wait at `end`, if it has one open.
    fn close_wait(&mut self, c: &LogCheck, slot: SlotId, end: i64) {
        if self.waiting.remove(&slot) {
            store_wait_end(c, slot, end);
        }
    }
}

/// Ends the wait the ledger has open for `slot` at `end`.
fn store_wait_end(c: &LogCheck, slot: SlotId, end: i64) {
    match c.ledger.close_wait(slot, end) {
        Ok(()) => c.timeline_nudge.notify_one(),
        Err(e) => tracing::warn!("S{slot} wait end not stored: {e:#}"),
    }
}

/// Checks a slot's session logs each time its session really ends, and
/// tracks each transcript's stretch of work: one a `/clear` ends is
/// recorded, its logs are checked and the slot is told to log the next
/// stretch apart (#80). Stretches and idle waits are stored in the ledger
/// as they happen, for the day timeline (#84).
async fn watch_session_logs(
    mut events: broadcast::Receiver<Event>,
    mut seen: mpsc::UnboundedReceiver<crate::stretches::Seen>,
    c: LogCheck,
) {
    let mut t = Tracker::default();
    t.reconcile(&c, unix_now());
    loop {
        tokio::select! {
            s = seen.recv() => {
                let Some(s) = s else { return };
                t.seen(&c, &s).await;
            }
            ev = events.recv() => match ev {
                Ok(Event::SlotChanged(info)) => {
                    t.slot_changed(&c, &info);
                    let SlotState::Exited { at, .. } = info.state else {
                        continue;
                    };
                    // A start may have replaced the session since this was sent.
                    if !sessionless(&c.manager.lock().unwrap().list()).contains(&info.id) {
                        continue;
                    }
                    // Events still queued belong to the session that ended.
                    while let Ok(s) = seen.try_recv() {
                        t.seen(&c, &s).await;
                    }
                    if let Some(done) = t.stretches.ended(info.id) {
                        t.end(&c, &done, at);
                        stretch_ended(&c, &done, false).await;
                    }
                    check_session_logs(&c, info.id, at).await;
                }
                Ok(_) => {}
                // A missed exit is not checked: the next one is.
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("session log check lagged by {n} events");
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
        }
    }
}

/// The logs a run wrote, after a notice (to the slot and S0) of what they
/// miss. `record` names the runs file the notice points to.
async fn check_run(
    c: &LogCheck,
    run: crate::session_logs::Run,
    record: Option<&str>,
) -> Vec<crate::session_logs::Log> {
    use crate::session_logs as logs;
    let (brain_dir, home, patterns) = (c.brain_dir.clone(), c.home.clone(), c.patterns.clone());
    let found = tokio::task::spawn_blocking(move || {
        logs::read_logs(&logs::find_logs(&brain_dir, &home, &patterns, &run))
    })
    .await
    .unwrap_or_default();
    if let Some(text) = logs::notice(&run, &found, record) {
        // S0 follows up; its own logs are only flagged on its card.
        let relay_to = (run.slot != 0).then_some(0);
        c.store.notify(run.slot, relay_to, &text, unix_now());
    }
    found
}

/// The runs file to record in, when the notes writer is on.
fn runs_record(c: &LogCheck) -> Option<&Arc<BrainService>> {
    c.brain.as_ref().filter(|_| !c.runs_file.is_empty())
}

async fn append_run_line(b: &BrainService, slot: SlotId, file: &str, line: &str, what: &str) {
    let message = format!("{} {what}", overseer_proto::slot_name(slot));
    if let Response::Error { message } = b.append_line(file, line, &message).await {
        tracing::warn!("S{slot} {what} not recorded: {message}")
    }
}

/// A transcript's stretch of work ended: it is recorded and, when a
/// `/clear` ended a stretch long enough to log, its logs are checked and
/// the slot is told to log what comes next apart.
async fn stretch_ended(c: &LogCheck, s: &crate::stretches::Stretch, cleared: bool) {
    use crate::session_logs as logs;
    let run = logs::Run {
        slot: s.slot,
        started: s.first,
        ended: s.last,
    };
    let file = logs::runs_file(&c.runs_file, &run);
    let record = runs_record(c);
    if cleared && !c.patterns.is_empty() && s.last - s.first >= c.min_secs {
        check_run(c, run, record.map(|_| file.as_str())).await;
        c.store.tell(s.slot, &logs::cleared_text(s), unix_now());
    }
    if let Some(b) = record {
        append_run_line(b, s.slot, &file, &logs::stretch_line(s), "activity").await;
    }
}

async fn check_session_logs(c: &LogCheck, slot: SlotId, ended: i64) {
    use crate::session_logs as logs;
    if c.patterns.is_empty() {
        return;
    }
    let started = match c.ledger.session_times(slot) {
        Ok(Some((started, _))) => started,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!("session log check for S{slot}: {e:#}");
            return;
        }
    };
    if ended - started < c.min_secs {
        return;
    }
    let run = logs::Run {
        slot,
        started,
        ended,
    };
    let file = logs::runs_file(&c.runs_file, &run);
    let record = runs_record(c);
    let found = check_run(c, run, record.map(|_| file.as_str())).await;
    if let Some(b) = record {
        append_run_line(b, slot, &file, &logs::run_line(&run, &found), "run times").await;
    }
}

/// The rest of a session's end, after its locks: the slot's items are
/// released and a status request it never answered goes. Callers skip it
/// once the daemon is stopping, so a session that outlives it keeps them.
fn session_over_rest(coord: &CoordService, store: &Arc<WorkStore>, slot: SlotId, now: i64) {
    coord.session_ended(slot);
    store.release_slot(slot, now);
}

/// A slot's session ended (it exited, or a resumable one was declined):
/// its locks, items and status request go. Nothing once the daemon is
/// stopping.
fn session_over(
    locks: &LockService,
    coord: &CoordService,
    store: &Arc<WorkStore>,
    slot: SlotId,
    now: i64,
) {
    if locks.is_stopping() {
        return;
    }
    locks.on_session_ended(slot, now);
    session_over_rest(coord, store, slot, now);
}

/// Ends the locks, items and status request of each slot whose session
/// exits. After a lag, which may have lost exit events, every slot with no
/// session is reconciled.
async fn watch_exits(
    mut events: broadcast::Receiver<Event>,
    locks: Arc<LockService>,
    coord: Arc<CoordService>,
    store: Arc<WorkStore>,
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
                session_over(&locks, &coord, &store, info.id, unix_now());
                // A handoff may have queued a relay.
                courier.poke();
            }
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!("lock exit watcher lagged by {n} events; reconciling");
                let ended = sessionless(&manager.lock().unwrap().list());
                locks.reconcile_exits(&ended, unix_now());
                if !locks.is_stopping() {
                    for slot in exited(&manager.lock().unwrap().list()) {
                        session_over_rest(&coord, &store, slot, unix_now());
                    }
                }
                courier.poke();
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// The answer to a request frame this daemon cannot decode.
pub const UNKNOWN_REQUEST: &str =
    "the daemon does not understand this request; restart the daemon to finish an upgrade";

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
    coord: Arc<CoordService>,
    plans: Arc<PlanCtx>,
    courier: Arc<Courier>,
    state_dir: PathBuf,
    brain: Option<Arc<BrainService>>,
    info: Arc<DaemonInfo>,
    settings: Arc<SettingsCtx>,
    attention: Arc<AttentionService>,
    timeline: Arc<TimelineService>,
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
        coord,
        plans,
        courier,
        state_dir,
        brain,
        info,
        settings,
        attention,
        timeline,
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
    let reader_outbox = outbox.clone();
    // Aborts on drop: the reader holds an outbox clone, and the writer only
    // ends once every clone is gone, so a handler that is itself dropped (at
    // shutdown) must not leave the reader behind.
    let reader = AbortOnDrop(tokio::spawn(async move {
        loop {
            match read_frame(&mut rd).await {
                Ok(Some(frame)) => {
                    if in_tx.send(frame).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    if let Some(seq) = overseer_proto::undecodable_request(&e) {
                        tracing::debug!("undecodable request {seq}: {e}");
                        reader_outbox.send(Frame::Response {
                            seq,
                            resp: Response::Error {
                                message: UNKNOWN_REQUEST.into(),
                            },
                        });
                        continue;
                    }
                    tracing::debug!("read error: {e}");
                    break;
                }
            }
        }
    }));

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
                    // A fresh session starts with no section of its own.
                    // Cleared before the start, so its first update cannot
                    // be lost to the clear.
                    if let (
                        Frame::Request {
                            req: Request::StartSlot { id, .. },
                            ..
                        },
                        Some(b),
                    ) = (&f, &brain)
                    {
                        let id = *id;
                        let running = valid_id(id)
                            && matches!(
                                manager.lock().unwrap().list()[id as usize].state,
                                SlotState::Running { .. }
                            );
                        if valid_id(id) && !running {
                            b.clear_current_session(id).await;
                        }
                    }
                    let ctx = Ctx {
                        manager: &manager,
                        store: &store,
                        locks: &locks,
                        coord: &coord,
                        plans: &plans,
                        courier: &courier,
                        state_dir: &state_dir,
                        brain: brain.as_ref(),
                        info: &info,
                        settings: &settings,
                        attention: &attention,
                        timeline: &timeline,
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
    drop(reader);
    writer.abort();
}

/// A task that is aborted when this handle is dropped.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// What a request handler works with.
struct Ctx<'a> {
    manager: &'a Manager,
    store: &'a Arc<WorkStore>,
    locks: &'a Arc<LockService>,
    coord: &'a Arc<CoordService>,
    plans: &'a Arc<PlanCtx>,
    courier: &'a Arc<Courier>,
    state_dir: &'a Path,
    brain: Option<&'a Arc<BrainService>>,
    info: &'a DaemonInfo,
    settings: &'a Arc<SettingsCtx>,
    attention: &'a Arc<AttentionService>,
    timeline: &'a Arc<TimelineService>,
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
        // An item's detail can wait on the Keychain, a Jira call or `bd`, so
        // it is answered from a task like a refresh.
        Frame::Request {
            seq,
            req: Request::GetItemDetail { key, refresh },
        } => {
            let task_store = store.clone();
            let outbox = outbox.clone();
            store.spawn(async move {
                let resp = match task_store.item_detail(&key, refresh).await {
                    Ok(d) => Response::ItemDetail(d),
                    Err(message) => Response::Error { message },
                };
                outbox.send(Frame::Response { seq, resp });
            });
        }
        // The Settings view says whether a token is stored, which reads the
        // Keychain (bounded, but not instant), so the settings requests are
        // answered from a task. Saves stay one at a time inside `save`.
        Frame::Request {
            seq,
            req: Request::GetSettings,
        } => {
            let (settings, info) = (ctx.settings.clone(), ctx.info.clone());
            let outbox = outbox.clone();
            store.spawn(async move {
                let resp = settings.with_token_state(settings.view(&info), true).await;
                outbox.send(Frame::Response { seq, resp });
            });
        }
        Frame::Request {
            seq,
            req: Request::SaveSettings { base_hash, edits },
        } => {
            let (settings, info) = (ctx.settings.clone(), ctx.info.clone());
            let outbox = outbox.clone();
            store.spawn(async move {
                let saved = settings.save(&info, &base_hash, &edits);
                let resp = settings.with_token_state(saved, false).await;
                outbox.send(Frame::Response { seq, resp });
            });
        }
        Frame::Request {
            seq,
            req: Request::SetToken { kind, token },
        } => {
            let settings = ctx.settings.clone();
            let outbox = outbox.clone();
            let task_store = store.clone();
            store.spawn(async move {
                let resp = settings.set_token(kind, token.expose()).await;
                let saved = resp == Response::Ok;
                outbox.send(Frame::Response { seq, resp });
                // The source that uses the token reads again now, not at
                // its next poll.
                if saved {
                    task_store.refresh(source_for(kind)).await;
                }
            });
        }
        // Approving a plan creates worktrees and starts sessions, so it is
        // answered from a task. Two approvals at once race in the ledger;
        // only one wins.
        Frame::Request {
            seq,
            req: Request::DecidePlan { id, from, decision },
        } => {
            let (coord, plans, courier) =
                (ctx.coord.clone(), ctx.plans.clone(), ctx.courier.clone());
            let outbox = outbox.clone();
            store.spawn(async move {
                let resp = coord.decide(id, from, decision, &plans, unix_now()).await;
                courier.poke();
                outbox.send(Frame::Response { seq, resp });
            });
        }
        // Resolving targets can run git or kubectl (bounded by
        // `RESOLVE_WITHIN`), so it too is answered from a task.
        Frame::Request {
            seq,
            req: Request::CheckBash(check),
        } => {
            // The writer's own checks come first and need no lookups; a
            // rewrite or deny answers at once.
            if let Some(b) = ctx.brain {
                if let Some(resp) = b.check_bash(&check, &b.context().owners) {
                    outbox.send(Frame::Response { seq, resp });
                    return;
                }
            }
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
                | Request::SyncNotes { .. }
                | Request::RestoreNotesFile { .. }
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
            let is_shutdown = matches!(req, Request::Shutdown | Request::StopAll);
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
                    | Request::RequestStatus { .. }
                    | Request::ProposePlan { .. }
                    | Request::ReportProgress { .. }
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
    let sender = |from: Option<SlotId>| -> Result<overseer_proto::Sender, String> {
        match from {
            None => Ok(overseer_proto::Sender::You),
            Some(slot) if valid_id(slot) => Ok(overseer_proto::Sender::Slot { slot }),
            Some(slot) => Err(format!("invalid slot id {slot}")),
        }
    };
    let from = match &req {
        Request::Commit { from, .. }
        | Request::AppendJournal { from, .. }
        | Request::EditShared { from, .. }
        | Request::SyncNotes { from, .. }
        | Request::RestoreNotesFile { from, .. }
        | Request::RunBd { from, .. } => match sender(*from) {
            Ok(s) => s,
            Err(message) => return Response::Error { message },
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
        Request::RunBd { args, stdin, .. } => brain.run_bd(args, stdin).await,
        Request::SyncNotes { confirm, .. } => brain.sync_notes(confirm).await,
        Request::RestoreNotesFile { path, rev, .. } => {
            let owners = brain.context().owners;
            brain.restore_file(from, &path, &rev, &owners).await
        }
        other => Response::Error {
            message: format!("not a notes writer request: {other:?}"),
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
        Request::PrSeen { repo, id } => store.mark_pr_seen(&repo, id),
        Request::Attention => Response::Attention(ctx.attention.current()),
        Request::Timeline => Response::Timeline(ctx.timeline.current()),
        Request::ListSlots => Response::Slots(manager.lock().unwrap().list()),
        Request::DaemonInfo => Response::DaemonInfo(ctx.info.clone()),
        Request::SlotScreen { id } if !valid_id(id) => err(format!("invalid slot id {id}")),
        Request::SlotScreen { id } => match manager.lock().unwrap().slot(id) {
            Some(slot) => Response::Text {
                text: crate::relay::screen::describe(&slot.screen().0),
            },
            None => err(format!("{} has no session", overseer_proto::slot_name(id))),
        },
        // RefreshWorkItems, GetItemDetail and the settings requests are answered in
        // `handle_frame` from a task of their own (they can wait on the
        // Keychain); this arm only keeps the match whole.
        Request::ListWorkItems | Request::RefreshWorkItems => Response::Work(store.view()),
        Request::GetItemDetail { .. } => Response::Error {
            message: "item details are answered by the connection".into(),
        },
        Request::GetSettings | Request::SaveSettings { .. } | Request::SetToken { .. } => {
            Response::Error {
                message: "settings requests are answered by the connection".into(),
            }
        }
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
        Request::DropRelays { from, id, to } => locks.drop_relays(from, id, to, unix_now()),
        Request::TakeStaleRelays { slot } => {
            let running = valid_id(slot)
                && matches!(
                    manager.lock().unwrap().list()[slot as usize].state,
                    SlotState::Running { .. }
                );
            if !running {
                return Response::Text {
                    text: String::new(),
                };
            }
            match ctx.courier.take_stale(slot, unix_now()) {
                Ok(text) => Response::Text { text },
                Err(message) => err(message),
            }
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
        Request::CheckPlan { plan } => {
            let owned = ctx.coord.gather(ctx.plans);
            match crate::coord::plan::validate(&plan, &owned.facts()) {
                Ok(warnings) => Response::PlanCheck {
                    warnings,
                    refusal: None,
                },
                Err(refusal) => Response::PlanCheck {
                    warnings: Vec::new(),
                    refusal: Some(refusal),
                },
            }
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
        | Request::SyncNotes { .. }
        | Request::RestoreNotesFile { .. }
        | Request::RunBd { .. } => err(WRITER_OFF.to_string()),
        Request::BrainStatus => Response::Brain(ctx.brain.map_or_else(
            || BrainService::off_view(&ctx.settings.running.brain_dir),
            |b| b.view(),
        )),
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
            let facts = KickoffFacts {
                held: locks
                    .view()
                    .claims
                    .iter()
                    .filter(|c| c.slot == slot)
                    .map(|c| c.target.clone())
                    .collect(),
                worktree: slot_worktree(ctx.plans, ctx.manager, slot),
                rules: Vec::new(),
                current_session: ctx.plans.current_session.clone(),
            };
            match crate::work::store::kickoff_text(
                &template,
                slot,
                &items,
                store.brain_dir(),
                &facts,
            ) {
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
                ctx.coord.session_ended(id);
            }
            match m.start(id, label, &cwd, command, prompt) {
                // CURRENT_SESSION.md was cleared before this (`handle_conn`).
                Ok(()) => Response::Ok,
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
                        session_over(locks, ctx.coord, store, id, unix_now());
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
        // A terminal that reports no size (a bare pty) must not shrink the
        // slot to nothing: the relay courier reads the screen at this size.
        Request::Resize { cols, rows, .. } if cols == 0 || rows == 0 => {
            err(format!("a {cols}x{rows} terminal has no usable size"))
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
        // The daemon stops after answering (see `handle_frame`): it has
        // been told to end its sessions, so it must not go on serving.
        Request::StopAll => match manager.lock().unwrap().end_all() {
            Ok(()) => Response::Ok,
            Err(e) => err(e),
        },
        // Coordinator requests.
        Request::GetBoard => Response::Board(BoardView {
            slots: manager.lock().unwrap().list(),
            work: store.view(),
            locks: locks.view(),
            coord: ctx.coord.view(),
        }),
        Request::CoordStatus => Response::Coord(ctx.coord.view()),
        Request::SetBoardSummary { from, text } => ctx.coord.set_summary(from, &text, unix_now()),
        Request::RequestStatus { from, slot } => {
            let running = valid_id(slot)
                && matches!(
                    manager.lock().unwrap().list()[slot as usize].state,
                    SlotState::Running { .. }
                );
            ctx.coord
                .request_status(from, slot, running, locks, unix_now())
        }
        Request::ReportProgress { slot, text } => {
            let live_s0 = matches!(
                manager.lock().unwrap().list()[0].state,
                SlotState::Running { .. }
            );
            ctx.coord.report(slot, &text, live_s0, locks, unix_now())
        }
        Request::ProposePlan { from, plan } => {
            let owned = ctx.coord.gather(ctx.plans);
            ctx.coord.propose(from, plan, &owned.facts(), unix_now())
        }
        // Answered in `handle_frame` from a task (it runs git and starts
        // sessions); this arm only keeps the match whole.
        Request::DecidePlan { .. } => err("not answered here".into()),
        // Always Ok: a hook runs inside Claude and must never see a failure
        // because of the daemon's state.
        Request::Hook(ev) => {
            apply_hook(manager, locks, &ev);
            Response::Ok
        }
    }
}

/// Applies one hook event to its slot and to the lock table. Shared by the
/// `Hook` request and the replay of the events spooled while the daemon was
/// down.
fn apply_hook(manager: &Manager, locks: &LockService, ev: &HookEvent) {
    manager.lock().unwrap().hook(ev);
    locks.on_hook(ev, unix_now());
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

/// The slot's cwd when it lies under `worktree_root`, else `None`. The guard
/// is dropped before the caller awaits anything.
fn slot_worktree(plans: &PlanCtx, manager: &Mutex<SlotManager>, slot: SlotId) -> Option<PathBuf> {
    let cwd = manager
        .lock()
        .unwrap()
        .list()
        .get(slot as usize)?
        .cwd
        .clone()?;
    let root = &plans.worktree_root;
    let under = |cwd: &Path, root: &Path| cwd != root && cwd.starts_with(root);
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    (under(&cwd, root) || under(&canon(&cwd), &canon(root))).then_some(cwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_token_refreshes_its_own_source() {
        use crate::work::store::Source;
        assert_eq!(source_for(TokenKind::Jira), Source::Jira);
        assert_eq!(source_for(TokenKind::Bitbucket), Source::Prs);
    }

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
