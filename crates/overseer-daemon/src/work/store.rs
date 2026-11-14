//! The work-item cache and its assignments, shared by every connection.
//!
//! Two sources fill the cache: `bd list --json` in the beads folder and the
//! configured Jira command. Each runs off the request path with a 30 s
//! timeout, so a slow source never stalls a slot request. Ownership lives in
//! the ledger; every decision and its writes happen in one ledger
//! transaction, so two clients assigning the same item cannot both win.
//!
//! Lock order: `change_turn`, then the ledger's lock, then the cache's.
//! None is held across an await.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use overseer_proto::{
    owned_by_message, slot_name, Event, ItemKind, Response, Sender, SlotId, SourceStatus, WorkItem,
    WorkView, JIRA_OFF,
};
use regex::Regex;
use tokio::sync::{broadcast, mpsc};
use tokio::task::{JoinHandle, JoinSet};

use super::ownership::{decide, promotions, Decision};
use super::{compile_pattern, group, parse_beads, parse_jira, run_source, Bead, Ticket};
use crate::config::Config;
use crate::env::BaseEnv;
use crate::ledger::{self, Ledger};
use crate::manager::valid_id;

/// How long `bd` or the Jira command may run.
pub const SOURCE_TIMEOUT: Duration = Duration::from_secs(30);

/// `bd list` stops at 50 rows unless told otherwise; `--limit 0` lists all.
/// Run after the configured `bd_command`.
pub const BD_LIST: [&str; 4] = ["list", "--json", "--limit", "0"];

/// Runs one `bd` write (the arguments after `bd`) somewhere else: the
/// 2ndBrain writer, which holds the vault lock while bd runs.
pub type LabelWriter = Box<
    dyn Fn(
            Vec<String>,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>
        + Send
        + Sync,
>;

/// How long shutdown waits for queued bead labels to reach `bd`.
pub const LABEL_DRAIN: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Beads,
    Jira,
}

pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// What the sources last returned.
struct Cache {
    beads: Vec<Bead>,
    tickets: Vec<Ticket>,
    /// `group` of the two lists, without owners or folders.
    items: Vec<WorkItem>,
    beads_status: SourceStatus,
    jira_status: SourceStatus,
    /// Ticket folders under `<brain_dir>/Sprints/*/`, as (name, path),
    /// sorted by path. Listed again on every refresh.
    folders: Vec<(String, PathBuf)>,
}

/// A label change to make on a bead with `bd update`.
struct Label {
    id: String,
    add: Option<SlotId>,
    remove: Option<SlotId>,
}

/// The label queue: one worker task runs the changes in order.
#[derive(Default)]
struct LabelQueue {
    tx: Option<mpsc::UnboundedSender<Label>>,
    worker: Option<JoinHandle<()>>,
    /// Set by `shutdown`; later changes are not queued.
    closed: bool,
}

pub struct WorkStore {
    beads_dir: PathBuf,
    /// The configured `bd` program and its leading arguments.
    bd_command: Vec<String>,
    /// Set when the 2ndBrain writer is on: label writes go through it.
    label_writer: OnceLock<LabelWriter>,
    brain_dir: PathBuf,
    /// `None` when the configured command is empty: Jira is off.
    jira_command: Option<Vec<String>>,
    jql: String,
    beads_poll: Duration,
    jira_poll: Duration,
    /// The ticket pattern, or one that never matches when the configured
    /// pattern is bad (beads then stay ungrouped).
    pattern: Regex,
    pattern_error: Option<String>,
    ledger: Arc<Ledger>,
    events: broadcast::Sender<Event>,
    env: BaseEnv,
    cache: Mutex<Cache>,
    /// Held for the length of one source's refresh, so refreshes of the same
    /// source run one after another.
    beads_turn: tokio::sync::Mutex<()>,
    jira_turn: tokio::sync::Mutex<()>,
    /// Bead label changes, run in order by one task (started on first use)
    /// so a quick assign and move cannot reach `bd` swapped.
    labels: Mutex<LabelQueue>,
    /// Held across an assign or release and the queueing of its label, so
    /// labels are queued in the order the ledger committed the changes.
    change_turn: Mutex<()>,
    relay_hook: OnceLock<Box<dyn Fn() + Send + Sync>>,
    /// Every task the store runs (polls, requested refreshes, the label
    /// worker), so `shutdown` can stop them and the source children they own.
    tasks: Mutex<Tasks>,
}

struct Tasks {
    set: JoinSet<()>,
    closed: bool,
}

fn status(name: &str, message: Option<String>) -> SourceStatus {
    SourceStatus {
        name: name.into(),
        ok: false,
        count: 0,
        at: None,
        message,
    }
}

/// Joins a source error and a settings problem into one status message.
fn join_messages(a: Option<String>, b: Option<&String>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) => Some(format!("{a}; {b}")),
        (Some(a), None) => Some(a),
        (None, b) => b.cloned(),
    }
}

/// Every folder one level under `<brain>/Sprints/*/`.
fn list_folders(brain: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(sprints) = std::fs::read_dir(brain.join("Sprints")) else {
        return out;
    };
    for sprint in sprints.flatten() {
        if !sprint.path().is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(sprint.path()) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.is_dir() {
                out.push((e.file_name().to_string_lossy().into_owned(), path));
            }
        }
    }
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

/// The folder named `key` or starting with `key-`. When several sprints have
/// one, the last by path (normally the latest sprint) wins.
fn folder_for(folders: &[(String, PathBuf)], key: &str) -> Option<PathBuf> {
    let prefix = format!("{key}-");
    folders
        .iter()
        .rev()
        .find(|(name, _)| name == key || name.starts_with(&prefix))
        .map(|(_, p)| p.clone())
}

// Short-lived, built and matched on one call path: boxing buys nothing.
#[allow(clippy::large_enum_variant)]
enum Outcome {
    Reply(Response),
    /// The flag says a relay was queued, so lock watchers need telling.
    Changed(Option<Label>, bool),
}

impl WorkStore {
    pub fn new(
        cfg: &Config,
        ledger: Arc<Ledger>,
        events: broadcast::Sender<Event>,
        env: BaseEnv,
    ) -> Arc<WorkStore> {
        let (pattern, pattern_error) = match compile_pattern(&cfg.ticket_pattern) {
            Ok(p) => (p, None),
            Err(e) => {
                tracing::warn!("{e}; beads will not be grouped under tickets");
                // `\b\B` never matches.
                (Regex::new(r"\b\B").expect("static regex"), Some(e))
            }
        };
        let jira_command = (!cfg.jira_command.is_empty()).then(|| cfg.jira_command.clone());
        let jira_status = status(
            "jira",
            Some(if jira_command.is_some() {
                "not read yet".into()
            } else {
                JIRA_OFF.into()
            }),
        );
        Arc::new(WorkStore {
            beads_dir: cfg.beads_dir.clone(),
            bd_command: cfg.bd_command.clone(),
            label_writer: OnceLock::new(),
            brain_dir: cfg.brain_dir.clone(),
            jira_command,
            jql: cfg.jql.clone(),
            beads_poll: Duration::from_secs(cfg.beads_poll_secs.max(1)),
            jira_poll: Duration::from_secs(cfg.jira_poll_secs.max(1)),
            pattern,
            pattern_error,
            ledger,
            events,
            env,
            cache: Mutex::new(Cache {
                beads: Vec::new(),
                tickets: Vec::new(),
                items: Vec::new(),
                beads_status: status("beads", Some("not read yet".into())),
                jira_status,
                folders: Vec::new(),
            }),
            beads_turn: tokio::sync::Mutex::new(()),
            jira_turn: tokio::sync::Mutex::new(()),
            labels: Mutex::new(LabelQueue::default()),
            change_turn: Mutex::new(()),
            relay_hook: OnceLock::new(),
            tasks: Mutex::new(Tasks {
                set: JoinSet::new(),
                closed: false,
            }),
        })
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, Cache> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn brain_dir(&self) -> &Path {
        &self.brain_dir
    }

    /// Sends the label writes to `f` instead of running `bd` here. Set once
    /// at start-up, when the 2ndBrain writer is on.
    pub fn route_label_writes(&self, f: LabelWriter) {
        let _ = self.label_writer.set(f);
    }

    /// `bd_command` followed by `args`.
    fn bd(&self, args: &[String]) -> Vec<String> {
        let mut cmd = self.bd_command.clone();
        cmd.extend(args.iter().cloned());
        cmd
    }

    /// The cached items with their owners and folders, owned items that have
    /// left the cache (marked `in_list: false`), the source statuses and all
    /// notices.
    pub fn view(&self) -> WorkView {
        let read = self
            .ledger
            .transaction(|conn| Ok((ledger::assignments_on(conn)?, ledger::notices_on(conn)?)));
        let (assigned, notices) = read.unwrap_or_else(|e| {
            tracing::warn!("ledger read failed: {e:#}");
            (Vec::new(), Vec::new())
        });
        let owners: HashMap<&str, SlotId> =
            assigned.iter().map(|(k, s)| (k.as_str(), *s)).collect();
        let cache = self.cache();
        let mut items = cache.items.clone();
        for it in &mut items {
            it.owner = owners.get(it.key.as_str()).copied();
            it.folder = folder_for(&cache.folders, &it.key);
        }
        for (key, slot) in &assigned {
            if items.iter().any(|i| &i.key == key) {
                continue;
            }
            items.push(WorkItem {
                key: key.clone(),
                kind: self.guess_kind(key),
                title: key.clone(),
                status: overseer_proto::UNKNOWN_STATUS.into(),
                priority: None,
                parent: None,
                owner: Some(*slot),
                in_list: false,
                folder: folder_for(&cache.folders, key),
            });
        }
        WorkView {
            items,
            sources: vec![cache.beads_status.clone(), cache.jira_status.clone()],
            notices,
        }
    }

    /// A key not in the cache is a ticket when the whole key matches the
    /// ticket pattern, else a bead.
    fn guess_kind(&self, key: &str) -> ItemKind {
        match self.pattern.find(key) {
            Some(m) if m.as_str() == key => ItemKind::Ticket,
            _ => ItemKind::Bead,
        }
    }

    fn kind_of(&self, key: &str) -> ItemKind {
        let known = self
            .cache()
            .items
            .iter()
            .find(|i| i.key == key)
            .map(|i| i.kind.clone());
        known.unwrap_or_else(|| self.guess_kind(key))
    }

    fn announce(&self) {
        let _ = self.events.send(Event::WorkChanged(self.view()));
    }

    /// Sets what runs after a notice also queued a relay, so lock watchers
    /// see it. Set once at start-up.
    pub fn on_relays_queued(&self, f: impl Fn() + Send + Sync + 'static) {
        let _ = self.relay_hook.set(Box::new(f));
    }

    fn relays_queued(&self) {
        if let Some(f) = self.relay_hook.get() {
            f();
        }
    }

    /// Reads one source, regroups, records promotions and tells clients.
    /// Waits for a refresh of the same source that is already running.
    pub async fn refresh(&self, which: Source) {
        let _turn = match which {
            Source::Beads => self.beads_turn.lock().await,
            Source::Jira => self.jira_turn.lock().await,
        };
        // The login shell's PATH is what finds `bd` and `acli`.
        let mut env = self.env.clone();
        env.wait().await;
        let vars = env.get();
        match which {
            Source::Beads => {
                let cmd = self.bd(&BD_LIST.map(String::from));
                let got = run_source(&cmd, &self.beads_dir, SOURCE_TIMEOUT, &vars)
                    .await
                    .and_then(|out| parse_beads(&out));
                let now = unix_now();
                let mut cache = self.cache();
                match got {
                    Ok(beads) => {
                        cache.beads_status = SourceStatus {
                            name: "beads".into(),
                            ok: true,
                            count: beads.len(),
                            at: Some(now),
                            message: self.pattern_error.clone(),
                        };
                        cache.beads = beads;
                    }
                    Err(e) => {
                        tracing::warn!("beads source: {e}");
                        let s = &mut cache.beads_status;
                        s.ok = false;
                        s.message = join_messages(Some(e), self.pattern_error.as_ref());
                    }
                }
            }
            Source::Jira => {
                let Some(cmd) = &self.jira_command else {
                    self.announce();
                    return;
                };
                let cmd: Vec<String> = cmd.iter().map(|a| a.replace("{jql}", &self.jql)).collect();
                let got = run_source(&cmd, Path::new("/"), SOURCE_TIMEOUT, &vars)
                    .await
                    .and_then(|out| parse_jira(&out));
                let now = unix_now();
                let mut cache = self.cache();
                match got {
                    Ok(tickets) => {
                        cache.jira_status = SourceStatus {
                            name: "jira".into(),
                            ok: true,
                            count: tickets.len(),
                            at: Some(now),
                            message: None,
                        };
                        cache.tickets = tickets;
                    }
                    Err(e) => {
                        tracing::warn!("jira source: {e}");
                        cache.jira_status.ok = false;
                        cache.jira_status.message = Some(e);
                    }
                }
            }
        }
        self.regroup(unix_now());
        self.announce();
    }

    /// Both sources at once; returns when both are done.
    pub async fn refresh_all(&self) {
        tokio::join!(self.refresh(Source::Beads), self.refresh(Source::Jira));
    }

    /// Rebuilds the grouped list. A bead that was standalone and owned and is
    /// now under a ticket owned by another slot is a promotion: it is logged
    /// and both slots get a notice.
    fn regroup(&self, now: i64) {
        let folders = list_folders(&self.brain_dir);
        let mut folders = Some(folders);
        let mut told = false;
        let done = self.ledger.transaction(|conn| {
            let owners: HashMap<String, SlotId> =
                ledger::assignments_on(conn)?.into_iter().collect();
            let mut cache = self.cache();
            let after = group(&cache.tickets, &cache.beads, &self.pattern);
            for (bead, was, ticket_owner) in promotions(&cache.items, &after, &owners) {
                let parent = after
                    .iter()
                    .find(|i| i.key == bead)
                    .and_then(|i| i.parent.clone())
                    .unwrap_or_default();
                let text = format!(
                    "{bead} (owned by {}) is now under {parent}, owned by {}",
                    slot_name(was),
                    slot_name(ticket_owner)
                );
                ledger::log_on(conn, now, &bead, Some(was), "promoted", &text)?;
                tell_on(conn, was, now, &text)?;
                tell_on(conn, ticket_owner, now, &text)?;
                told = true;
            }
            cache.items = after;
            if let Some(f) = folders.take() {
                cache.folders = f;
            }
            Ok(())
        });
        if told && done.is_ok() {
            self.relays_queued();
        }
        if let Err(e) = done {
            // The list still updates; only the promotion notices are lost.
            tracing::warn!("recording promotions failed: {e:#}");
            let mut cache = self.cache();
            cache.items = group(&cache.tickets, &cache.beads, &self.pattern);
            if let Some(f) = folders.take() {
                cache.folders = f;
            }
        }
    }

    /// Decides and records an assignment in one ledger transaction.
    pub fn assign(
        self: &Arc<Self>,
        key: &str,
        slot: SlotId,
        confirm: bool,
        move_owner: bool,
        now: i64,
    ) -> Response {
        if !valid_id(slot) {
            return Response::Error {
                message: format!("invalid slot id {slot}"),
            };
        }
        let _turn = self.change_turn.lock().unwrap_or_else(|e| e.into_inner());
        let done = self.ledger.transaction(|conn| {
            let owners: HashMap<String, SlotId> =
                ledger::assignments_on(conn)?.into_iter().collect();
            let decision = {
                let cache = self.cache();
                decide(&cache.items, &owners, key, slot, confirm, move_owner)
            };
            let decision = match decision {
                Ok(d) => d,
                Err(message) => return Ok(Outcome::Reply(Response::Error { message })),
            };
            let label = |remove| Label {
                id: key.to_owned(),
                add: Some(slot),
                remove,
            };
            Ok(match decision {
                Decision::AlreadyYours => Outcome::Reply(Response::Ok),
                Decision::Refuse { owner } => Outcome::Reply(Response::Error {
                    message: owned_by_message(key, owner),
                }),
                Decision::Warn { message, .. } if !confirm => {
                    Outcome::Reply(Response::NeedsConfirm { message })
                }
                Decision::Warn {
                    message,
                    tell,
                    from,
                } => {
                    ledger::assign_on(conn, key, slot, now)?;
                    let (action, detail) = match from {
                        Some(f) => (
                            "moved",
                            format!("from {}; confirmed: {message}", slot_name(f)),
                        ),
                        None => ("assigned", format!("confirmed: {message}")),
                    };
                    ledger::log_on(conn, now, key, Some(slot), action, &detail)?;
                    let text = format!("{} took {key}, which overlaps your work", slot_name(slot));
                    for t in tell {
                        tell_on(conn, t, now, &text)?;
                    }
                    if let Some(f) = from {
                        tell_on(conn, f, now, &moved_text(key, slot))?;
                    }
                    Outcome::Changed(Some(label(from)), true)
                }
                Decision::Assign => {
                    ledger::assign_on(conn, key, slot, now)?;
                    ledger::log_on(conn, now, key, Some(slot), "assigned", "")?;
                    Outcome::Changed(Some(label(None)), false)
                }
                Decision::Move { from } => {
                    ledger::assign_on(conn, key, slot, now)?;
                    ledger::log_on(
                        conn,
                        now,
                        key,
                        Some(slot),
                        "moved",
                        &format!("from {}", slot_name(from)),
                    )?;
                    tell_on(conn, from, now, &moved_text(key, slot))?;
                    Outcome::Changed(Some(label(Some(from))), true)
                }
            })
        });
        self.finish(done)
    }

    /// Removes `key`'s assignment and logs it.
    pub fn release(self: &Arc<Self>, key: &str, now: i64) -> Response {
        let _turn = self.change_turn.lock().unwrap_or_else(|e| e.into_inner());
        let done = self.ledger.transaction(|conn| {
            Ok(match ledger::release_on(conn, key)? {
                None => Outcome::Reply(Response::Error {
                    message: format!("{key} is not assigned"),
                }),
                Some(old) => {
                    ledger::log_on(conn, now, key, Some(old), "released", "")?;
                    Outcome::Changed(
                        Some(Label {
                            id: key.to_owned(),
                            add: None,
                            remove: Some(old),
                        }),
                        false,
                    )
                }
            })
        });
        self.finish(done)
    }

    /// After a committed change: labels a bead in the background and tells
    /// clients. The caller holds `change_turn`.
    fn finish(self: &Arc<Self>, done: anyhow::Result<Outcome>) -> Response {
        match done {
            Err(e) => Response::Error {
                message: format!("ledger: {e:#}"),
            },
            Ok(Outcome::Reply(r)) => r,
            Ok(Outcome::Changed(label, relayed)) => {
                #[cfg(test)]
                if tests::WIDEN_RACE.load(std::sync::atomic::Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_micros(200));
                }
                // Only a directly assigned bead is labelled, never a ticket.
                if let Some(l) = label.filter(|l| self.kind_of(&l.id) == ItemKind::Bead) {
                    self.queue_label(l);
                }
                self.announce();
                if relayed {
                    self.relays_queued();
                }
                Response::Ok
            }
        }
    }

    pub fn clear_notices(&self, slot: SlotId) -> Response {
        if !valid_id(slot) {
            return Response::Error {
                message: format!("invalid slot id {slot}"),
            };
        }
        match self.ledger.clear_notices(slot) {
            Ok(()) => {
                self.announce();
                Response::Ok
            }
            Err(e) => Response::Error {
                message: format!("ledger: {e:#}"),
            },
        }
    }

    fn queue_label(self: &Arc<Self>, label: Label) {
        let mut q = self.labels.lock().unwrap_or_else(|e| e.into_inner());
        if q.closed {
            tracing::warn!("stopping: label change for {} not written", label.id);
            return;
        }
        let tx = match &q.tx {
            Some(tx) => tx.clone(),
            None => {
                let (tx, rx) = mpsc::unbounded_channel();
                q.worker = Some(tokio::spawn(label_worker(Arc::downgrade(self), rx)));
                q.tx = Some(tx.clone());
                tx
            }
        };
        let _ = tx.send(label);
    }

    async fn run_label(&self, l: Label) {
        let mut cmd = vec!["update".to_string(), l.id.clone()];
        if let Some(s) = l.add {
            cmd.extend(["--add-label".into(), format!("overseer:{}", slot_name(s))]);
        }
        if let Some(s) = l.remove {
            cmd.extend([
                "--remove-label".into(),
                format!("overseer:{}", slot_name(s)),
            ]);
        }
        let result = match self.label_writer.get() {
            Some(write) => write(cmd).await,
            None => {
                // Right after start the login shell's PATH may not be in yet.
                let mut env = self.env.clone();
                env.wait().await;
                let vars = env.get();
                run_source(&self.bd(&cmd), &self.beads_dir, SOURCE_TIMEOUT, &vars)
                    .await
                    .map(|_| ())
            }
        };
        if let Err(e) = result {
            let text = format!("could not label {} in beads: {e}", l.id);
            tracing::warn!("{text}");
            let Some(slot) = l.add.or(l.remove) else {
                return;
            };
            if let Err(e) = self.ledger.add_notice(slot, unix_now(), &text) {
                tracing::warn!("could not store the notice: {e:#}");
            }
            self.announce();
        }
    }

    /// Runs `fut` as one of the store's tasks. After `shutdown` it is
    /// dropped instead.
    pub fn spawn<F>(&self, fut: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        if tasks.closed {
            return;
        }
        // Reap finished tasks so the set does not grow.
        while tasks.set.try_join_next().is_some() {}
        tasks.set.spawn(fut);
    }

    /// Stops every task the store runs, except that label changes already
    /// queued are left for `drain_labels`. Dropping a running source kills
    /// its child (`kill_on_drop`).
    pub fn shutdown(&self) {
        let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        tasks.closed = true;
        tasks.set.abort_all();
        drop(tasks);
        let mut q = self.labels.lock().unwrap_or_else(|e| e.into_inner());
        q.closed = true;
        // The worker ends once the queue is empty.
        q.tx = None;
    }

    /// After `shutdown`: gives the queued label changes up to `limit` to
    /// reach `bd`, so a release just before a restart is not lost, then
    /// stops the worker (and any `bd` it is running).
    pub async fn drain_labels(&self, limit: Duration) {
        let worker = self
            .labels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .worker
            .take();
        let Some(mut worker) = worker else { return };
        if tokio::time::timeout(limit, &mut worker).await.is_err() {
            tracing::warn!("label changes still queued after {limit:?}; dropping them");
            worker.abort();
        }
    }

    /// Polls each source on its own timer, once at start, until `shutdown`.
    pub fn start_polling(self: &Arc<Self>) {
        for (which, every) in [
            (Source::Beads, self.beads_poll),
            (Source::Jira, self.jira_poll),
        ] {
            let store = self.clone();
            self.spawn(async move {
                loop {
                    store.refresh(which).await;
                    tokio::time::sleep(every).await;
                }
            });
        }
    }
}

/// A notice that tells a session something: shown on the board and also
/// queued as a relay from Overseer, in the caller's transaction.
fn tell_on(conn: &rusqlite::Connection, slot: SlotId, now: i64, text: &str) -> anyhow::Result<()> {
    ledger::add_notice_on(conn, slot, now, text)?;
    crate::locks::table::queue_relay_on(conn, &Sender::Overseer, slot, text, None, now)?;
    Ok(())
}

fn moved_text(key: &str, to: SlotId) -> String {
    format!("{key} was moved to {}", slot_name(to))
}

async fn label_worker(store: Weak<WorkStore>, mut rx: mpsc::UnboundedReceiver<Label>) {
    while let Some(l) = rx.recv().await {
        let Some(store) = store.upgrade() else { return };
        store.run_label(l).await;
    }
}

/// What a kickoff says beyond the items: the targets the slot holds, the
/// worktree it works in, the rules that apply and where its section of
/// `CURRENT_SESSION.md` lives. Every part may be empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KickoffFacts {
    pub held: Vec<String>,
    pub worktree: Option<PathBuf>,
    pub rules: Vec<String>,
    pub current_session: PathBuf,
}

/// The items of `mine` as lines: a bead whose ticket is also in `mine` is
/// nested under it, any other item is a top-level line.
fn item_lines(mine: &[&WorkItem]) -> Vec<String> {
    let line =
        |i: &WorkItem, indent: &str| format!("{indent}- {} ({}): {}", i.key, i.status, i.title);
    let in_set = |key: &str| mine.iter().any(|i| i.key == key);
    let mut lines = Vec::new();
    for top in mine {
        if top.parent.as_deref().is_some_and(in_set) {
            continue;
        }
        lines.push(line(top, ""));
        for child in mine
            .iter()
            .filter(|c| c.parent.as_deref() == Some(&top.key))
        {
            lines.push(line(child, "  "));
        }
    }
    lines
}

/// Expands `{name}` placeholders of `line` in one pass, so braces inside a
/// value are never expanded again. The flag says some placeholder in the
/// line expanded to nothing.
fn expand_line(line: &str, values: &[(&str, String)]) -> (String, bool) {
    let mut out = String::new();
    let mut empty = false;
    let mut rest = line;
    'scan: while let Some(at) = rest.find('{') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        for (name, value) in values {
            let tag = format!("{{{name}}}");
            if let Some(after) = rest.strip_prefix(tag.as_str()) {
                empty |= value.is_empty();
                out.push_str(value);
                rest = after;
                continue 'scan;
            }
        }
        out.push('{');
        rest = &rest[1..];
    }
    out.push_str(rest);
    (out, empty)
}

/// Fills `template` for `slot` from the items it owns (`mine`) and `facts`.
/// `items` is the whole view, for a bead's ticket folder. A line whose
/// placeholder expands to nothing is dropped, and so is a heading (a line
/// ending in a colon, with no placeholder) that the dropped line belonged to.
pub(crate) fn fill_kickoff(
    template: &str,
    slot: SlotId,
    mine: &[&WorkItem],
    items: &[WorkItem],
    brain_dir: &Path,
    facts: &KickoffFacts,
) -> String {
    let ticket_folder = |i: &WorkItem| {
        let parent = i.parent.as_deref()?;
        items.iter().find(|t| t.key == parent)?.folder.as_ref()
    };
    let mut folders: Vec<String> = Vec::new();
    for f in mine
        .iter()
        .flat_map(|i| [i.folder.as_ref(), ticket_folder(i)])
        .flatten()
    {
        let f = f.display().to_string();
        if !folders.contains(&f) {
            folders.push(f);
        }
    }
    let folders = if folders.is_empty() {
        "none found".to_string()
    } else {
        folders.join(", ")
    };
    let lines = item_lines(mine);
    let values = [
        ("slot", slot_name(slot)),
        (
            "items",
            if lines.is_empty() {
                "none yet".to_string()
            } else {
                lines.join("\n")
            },
        ),
        ("folders", folders),
        ("brain", brain_dir.display().to_string()),
        ("targets", facts.held.join(", ")),
        (
            "worktree",
            facts
                .worktree
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        ),
        (
            "current_session",
            if facts.current_session.as_os_str().is_empty() {
                String::new()
            } else {
                facts.current_session.display().to_string()
            },
        ),
        (
            "rules",
            facts
                .rules
                .iter()
                .map(|r| format!("- {r}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
    ];
    let mut out: Vec<String> = Vec::new();
    // The last template line was a heading kept only if what follows stays.
    let mut heading: Option<usize> = None;
    for line in template.lines() {
        let (text, empty) = expand_line(line, &values);
        if empty {
            if let Some(at) = heading.take() {
                out.remove(at);
            }
            continue;
        }
        // Dropped lines must not leave two blank lines side by side.
        if text.is_empty() && out.last().is_some_and(String::is_empty) {
            continue;
        }
        let is_heading = !line.contains('{') && line.trim_end().ends_with(':');
        heading = is_heading.then_some(out.len());
        out.push(text);
    }
    out.join("\n").trim().to_string()
}

/// The first prompt for a slot, built from `template` and the items in
/// `items` (the whole view) that it owns. `None` when it owns none. A bead
/// whose ticket it also owns is listed under it; any other item is a
/// top-level line. The folders are those of its items and, for a bead, of
/// the bead's ticket. `facts` adds held targets, worktree and rules.
pub fn kickoff_text(
    template: &str,
    slot: SlotId,
    items: &[WorkItem],
    brain_dir: &Path,
    facts: &KickoffFacts,
) -> Option<String> {
    let mine: Vec<&WorkItem> = items.iter().filter(|i| i.owner == Some(slot)).collect();
    if mine.is_empty() {
        return None;
    }
    Some(fill_kickoff(template, slot, &mine, items, brain_dir, facts))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Makes `finish` pause between the commit and the queueing, so a
    /// missing `change_turn` shows up as labels out of order.
    pub(super) static WIDEN_RACE: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    #[test]
    fn folder_matches_key_or_key_dash_prefix() {
        let f = vec![
            ("DEMO-1".to_string(), PathBuf::from("/b/Sprints/s1/DEMO-1")),
            (
                "DEMO-12-x".to_string(),
                PathBuf::from("/b/Sprints/s1/DEMO-12-x"),
            ),
            (
                "DEMO-12-y".to_string(),
                PathBuf::from("/b/Sprints/s2/DEMO-12-y"),
            ),
        ];
        assert_eq!(
            folder_for(&f, "DEMO-1"),
            Some(PathBuf::from("/b/Sprints/s1/DEMO-1"))
        );
        assert_eq!(
            folder_for(&f, "DEMO-12"),
            Some(PathBuf::from("/b/Sprints/s2/DEMO-12-y"))
        );
        assert_eq!(folder_for(&f, "DEMO-123"), None);
    }

    fn store(pattern: &str) -> Arc<WorkStore> {
        let cfg = Config {
            ticket_pattern: pattern.into(),
            jira_command: vec![],
            ..Config::default()
        };
        let (events, _) = broadcast::channel(4);
        WorkStore::new(
            &cfg,
            Arc::new(Ledger::in_memory()),
            events,
            BaseEnv::ready(vec![]),
        )
    }

    #[test]
    fn owned_item_missing_from_the_cache_is_listed() {
        let s = store(r"DEMO-\d+");
        s.ledger.assign("DEMO-5", 2, 1).unwrap();
        s.ledger.assign("ov-9", 3, 1).unwrap();
        let v = s.view();
        assert_eq!(v.items.len(), 2);
        assert!(v.items.iter().all(|i| !i.in_list));
        assert_eq!(v.items[0].kind, ItemKind::Ticket);
        assert_eq!(v.items[0].owner, Some(2));
        assert_eq!(v.items[1].kind, ItemKind::Bead);
        let jira = &v.sources[1];
        assert_eq!(
            jira.message.as_deref(),
            Some("jira_command is empty; Jira is off")
        );
    }

    #[test]
    fn bad_pattern_does_not_panic_and_groups_nothing() {
        let s = store("[DEMO");
        assert!(s.pattern_error.is_some());
        assert_eq!(s.guess_kind("DEMO-1"), ItemKind::Bead);
        assert!(s.pattern.find("DEMO-1 x").is_none());
    }

    #[test]
    fn promotion_logs_and_tells_both_slots() {
        let s = store(r"DEMO-\d+");
        let bead = |title: &str| Bead {
            id: "ov-3".into(),
            title: title.into(),
            status: "open".into(),
            priority: None,
            external_ref: None,
            labels: vec![],
        };
        s.cache().beads = vec![bead("plain")];
        s.regroup(1);
        s.ledger.assign("ov-3", 2, 1).unwrap();
        s.ledger.assign("DEMO-12", 5, 1).unwrap();
        s.cache().beads = vec![bead("now DEMO-12")];
        s.regroup(2);
        let n = s.ledger.notices().unwrap();
        assert_eq!(n.len(), 2, "{n:?}");
        assert_eq!(
            n[0].text,
            "ov-3 (owned by S2) is now under DEMO-12, owned by S5"
        );
        let slots: Vec<_> = n.iter().map(|n| n.slot).collect();
        assert_eq!(slots, [2, 5]);
        // Each notice also queued a relay for the same slot.
        let relays = s.ledger.lock_view(10, 0).unwrap().relays;
        let mut to: Vec<_> = relays.iter().map(|r| (r.to, r.text.clone())).collect();
        to.sort();
        assert_eq!(to[0], (2, n[0].text.clone()));
        assert_eq!(to[1].0, 5);
        // Seen once only.
        s.regroup(3);
        assert_eq!(s.ledger.notices().unwrap().len(), 2);
    }

    /// Many threads assign, move and release one bead. Each queued label
    /// must take off the label the one before it added, or `bd` would end
    /// up with a stale `overseer:S<n>`.
    #[test]
    fn labels_are_queued_in_commit_order() {
        let s = store(r"DEMO-\d+");
        s.cache().beads = vec![Bead {
            id: "ov-3".into(),
            title: "plain".into(),
            status: "open".into(),
            priority: None,
            external_ref: None,
            labels: vec![],
        }];
        s.regroup(1);
        let (tx, mut rx) = mpsc::unbounded_channel();
        s.labels.lock().unwrap().tx = Some(tx);
        WIDEN_RACE.store(true, std::sync::atomic::Ordering::Relaxed);
        std::thread::scope(|scope| {
            for slot in 1..=6u8 {
                let s = s.clone();
                scope.spawn(move || {
                    for round in 0..200 {
                        if round % 7 == 0 {
                            s.release("ov-3", 1);
                        } else {
                            s.assign("ov-3", slot, true, true, 1);
                        }
                    }
                });
            }
        });
        WIDEN_RACE.store(false, std::sync::atomic::Ordering::Relaxed);
        let mut on_bead = None;
        let mut n = 0;
        while let Ok(l) = rx.try_recv() {
            assert_eq!(l.remove, on_bead, "label {n} out of order");
            on_bead = l.add;
            n += 1;
        }
        assert!(n > 6, "only {n} labels");
        let owner = s.ledger.assignments().unwrap().first().map(|(_, o)| *o);
        assert_eq!(on_bead, owner);
    }
}

#[cfg(test)]
mod kickoff_tests {
    use super::*;
    use crate::launch::ASSIGNMENT;

    fn it(key: &str, kind: ItemKind, status: &str, title: &str, parent: Option<&str>) -> WorkItem {
        WorkItem {
            key: key.into(),
            kind,
            title: title.into(),
            status: status.into(),
            priority: None,
            parent: parent.map(String::from),
            owner: Some(3),
            in_list: true,
            folder: None,
        }
    }

    #[test]
    fn kickoff_lists_items_and_folders() {
        let mut t = it(
            "DEMO-12",
            ItemKind::Ticket,
            "In Progress",
            "Widget wiring",
            None,
        );
        t.folder = Some(PathBuf::from("/b/Sprints/s1/DEMO-12-widget"));
        let text = kickoff_text(
            ASSIGNMENT,
            3,
            &[t],
            Path::new("/b"),
            &KickoffFacts::default(),
        )
        .unwrap();
        assert!(text.starts_with("You are S3. Overseer has assigned you this work:\n\n"));
        assert!(
            text.contains("\n- DEMO-12 (In Progress): Widget wiring\n"),
            "{text}"
        );
        assert!(
            text.contains("Ticket folders in 2ndBrain: /b/Sprints/s1/DEMO-12-widget\n"),
            "{text}"
        );
        assert!(!text.contains('{'), "{text}");
    }

    #[test]
    fn kickoff_none_without_items() {
        assert_eq!(
            kickoff_text(
                ASSIGNMENT,
                3,
                &[],
                Path::new("/b"),
                &KickoffFacts::default()
            ),
            None
        );
    }

    #[test]
    fn kickoff_says_none_found_without_folders() {
        let b = it("ov-3", ItemKind::Bead, "open", "Fix {slot} thing", None);
        let text = kickoff_text(
            ASSIGNMENT,
            3,
            &[b],
            Path::new("/b"),
            &KickoffFacts::default(),
        )
        .unwrap();
        assert!(
            text.contains("Ticket folders in 2ndBrain: none found"),
            "{text}"
        );
        // Braces in a title are not expanded again.
        assert!(text.contains("- ov-3 (open): Fix {slot} thing"), "{text}");
    }

    #[test]
    fn bead_under_ticket_is_indented() {
        let items = [
            it(
                "DEMO-12",
                ItemKind::Ticket,
                "In Progress",
                "Widget wiring",
                None,
            ),
            it("ov-2", ItemKind::Bead, "open", "Wire it", Some("DEMO-12")),
            it(
                "ov-9",
                ItemKind::Bead,
                "open",
                "Loose bead",
                Some("DEMO-40"),
            ),
        ];
        let text = kickoff_text(
            "{items}",
            3,
            &items,
            Path::new("/b"),
            &KickoffFacts::default(),
        )
        .unwrap();
        assert_eq!(
            text,
            "- DEMO-12 (In Progress): Widget wiring\n  - ov-2 (open): Wire it\n- ov-9 (open): Loose bead"
        );
    }

    #[test]
    fn bead_kickoff_names_its_tickets_folder() {
        let mut ticket = it("DEMO-12", ItemKind::Ticket, "To Do", "Widget wiring", None);
        ticket.owner = Some(5);
        ticket.folder = Some(PathBuf::from("/b/Sprints/s1/DEMO-12-widget"));
        let bead = it("ov-3", ItemKind::Bead, "open", "Wire it", Some("DEMO-12"));
        let mut other = it("ov-4", ItemKind::Bead, "open", "Other", None);
        other.owner = Some(5);
        let items = [ticket.clone(), bead.clone(), other];
        let text = kickoff_text(
            "{items}|{folders}",
            3,
            &items,
            Path::new("/b"),
            &KickoffFacts::default(),
        )
        .unwrap();
        // Only S3's item is listed, with the ticket's folder.
        assert_eq!(text, "- ov-3 (open): Wire it|/b/Sprints/s1/DEMO-12-widget");
        // Owning the ticket too lists the folder once.
        ticket.owner = Some(3);
        let text = kickoff_text(
            "{folders}",
            3,
            &[ticket, bead],
            Path::new("/b"),
            &KickoffFacts::default(),
        )
        .unwrap();
        assert_eq!(text, "/b/Sprints/s1/DEMO-12-widget");
    }

    #[test]
    fn child_listed_before_its_ticket_still_nests() {
        let items = [
            it("ov-2", ItemKind::Bead, "open", "Wire it", Some("DEMO-12")),
            it("DEMO-12", ItemKind::Ticket, "To Do", "Widget wiring", None),
        ];
        let text = kickoff_text(
            "{items}",
            3,
            &items,
            Path::new("/b"),
            &KickoffFacts::default(),
        )
        .unwrap();
        assert_eq!(
            text,
            "- DEMO-12 (To Do): Widget wiring\n  - ov-2 (open): Wire it"
        );
    }
}
