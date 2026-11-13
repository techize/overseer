//! The 2ndBrain writer: the daemon is the only committer to the vault.
//!
//! `BrainService` owns the commit queue (in the ledger), runs it one entry
//! at a time under the vault mutex, pushes after each commit, runs the sync
//! once the queue has drained (Ruling 8), the timed backup (Ruling 9) and
//! `bd` writes (Ruling 10), and announces a `BrainView` after each change.

pub mod bash;
pub mod current;
pub mod git;
pub mod paths;
pub mod shared;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use overseer_proto::msg::{
    BashCheck, BrainRun, BrainView, Event, QueueEntry, QueueState, Response, Sender, SlotId,
    WriteCheck,
};
use tokio::sync::{broadcast, oneshot, Notify, OwnedMutexGuard};
use tokio::time::Instant;

use crate::config::Config;
use crate::env::BaseEnv;
use crate::ledger::Ledger;
use git::{GitError, VaultGit, VaultGuard};
use paths::{
    classify, fill_template, locate, shared_match, vault_rel, Owners, PathClass, PathContext,
};
use shared::FileLocks;

/// The answer to every writer request while `brain_writer` is off (Ruling 1).
pub const WRITER_OFF: &str = "the 2ndBrain writer is off (brain_writer in config.toml)";

const CURRENT_SESSION_DENY: &str =
    "CURRENT_SESSION.md is written by Overseer. Use update_current_session for your section.";

/// Every daemon commit message ends with this trailer and the entry id.
const TRAILER: &str = "Overseer-Queue";
/// A first try and three retries (Ruling 7).
const MAX_ATTEMPTS: i64 = 4;
/// Queue entries the board shows besides the queued ones.
const RECENT: usize = 20;
/// The longest the worker sleeps with nothing due.
const IDLE: Duration = Duration::from_secs(60);
/// How long a caller waits for its entry before being told it is still queued.
const REPLY_WAIT: Duration = Duration::from_secs(600);
const BD_TIMEOUT: Duration = Duration::from_secs(120);
const SYNC_TIMEOUT: Duration = Duration::from_secs(600);

const GIT_DENY: &str = "use the commit_2ndbrain tool; Overseer commits and pushes 2ndBrain";
const BD_COMPOUND_DENY: &str = "Overseer: run bd writes on their own, without $(...), backticks, redirects or env prefixes, so the daemon can run them under the vault lock";

fn deny_shared(rel: &str) -> String {
    format!("{rel} is a shared 2ndBrain file. Use append_journal for the journal or edit_shared_file for an exact replacement.")
}

fn deny_owned(rel: &str, slot: SlotId, what: &str) -> String {
    format!("{rel} belongs to S{slot} ({what}). Ask Jonathan before writing it.")
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// `2026-11-13 09:30` in local time, from `localtime_r`.
fn local_stamp(now: i64) -> String {
    // SAFETY: localtime_r only writes the `tm` we pass it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t = now as libc::time_t;
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return "1970-01-01 00:00".into();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

/// The resolved form of `p` (its parent's when `p` does not exist yet), so
/// one file always gets one `FileLocks` key.
fn canon(p: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(p) {
        return c;
    }
    match (p.parent(), p.file_name()) {
        (Some(dir), Some(name)) => std::fs::canonicalize(dir)
            .map(|d| d.join(name))
            .unwrap_or_else(|_| p.to_path_buf()),
        _ => p.to_path_buf(),
    }
}

/// A session's message as given, without em dashes or a forged trailer.
fn clean_message(m: &str) -> String {
    m.replace('\u{2014}', "-")
        .lines()
        .filter(|l| !l.trim_start().starts_with(&format!("{TRAILER}:")))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn prefix(from: Sender) -> String {
    match from {
        Sender::Slot { slot } => format!("S{slot}: "),
        Sender::You | Sender::Overseer => String::new(),
    }
}

fn last_line(stdout: &str, stderr: &str) -> String {
    [stdout, stderr]
        .iter()
        .find_map(|s| s.lines().map(str::trim).rfind(|l| !l.is_empty()))
        .unwrap_or("")
        .to_string()
}

/// Timings and the home folder; the production values come from the config.
#[derive(Debug, Clone)]
pub struct BrainOptions {
    /// How long the queue stays empty after a change before the sync runs.
    pub drain: Duration,
    /// Time between backups; `None` turns them off.
    pub backup: Option<Duration>,
    /// The wait before an entry that found `index.lock` held is tried again.
    pub retry: Duration,
    /// How long a commit waits for `index.lock` to go.
    pub lock_wait: Duration,
    /// What `~` means in a path a session names.
    pub home: PathBuf,
    /// The overseer binary a rewritten `bd` write runs (`<program> ctl bd`).
    pub program: PathBuf,
}

impl BrainOptions {
    pub fn from_config(cfg: &Config) -> Self {
        BrainOptions {
            drain: Duration::from_secs(10),
            backup: (cfg.backup_mins > 0)
                .then(|| Duration::from_secs(cfg.backup_mins.saturating_mul(60))),
            retry: Duration::from_secs(10),
            lock_wait: Duration::from_secs(5),
            home: std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default(),
            program: PathBuf::from("overseer"),
        }
    }
}

/// What the service needs to know about the slots: who owns what, which
/// slots are running and which of those are working or waiting on Jonathan.
#[derive(Debug, Clone, Default)]
pub struct BrainContext {
    pub owners: Owners,
    pub running: Vec<SlotId>,
    pub busy: Vec<SlotId>,
}

type ContextFn = Arc<dyn Fn() -> BrainContext + Send + Sync>;

struct Settings {
    brain: PathBuf,
    beads: PathBuf,
    shared_files: Vec<String>,
    journal_path: String,
    current_session: PathBuf,
    push: bool,
    sync_command: Vec<String>,
    bd_command: Vec<String>,
}

#[derive(Default)]
struct Runtime {
    /// When the last commit or `bd` write finished; cleared once a drain ran.
    last_change: Option<Instant>,
    sync_pending: bool,
    bd_sync_pending: bool,
    /// The last sync attempt was held back by a running slot's changes;
    /// the backup tick may try it again (Ruling 8).
    sync_held: bool,
    push_problem: Option<String>,
    /// `push_problem` was raised for unmerged paths found before a commit,
    /// not by a push, so it clears once they are resolved.
    conflict_problem: bool,
    /// The queue waits until then after `index.lock` was held.
    retry_at: Option<Instant>,
    next_backup: Option<Instant>,
    /// Entries queued when the service started: one may have been committed
    /// by the daemon that went down, so its trailer is looked for first.
    recovering: HashSet<i64>,
    /// Commits found by trailer that may not have been pushed.
    need_push: bool,
}

pub struct BrainService {
    cfg: Settings,
    opts: BrainOptions,
    git: VaultGit,
    ledger: Arc<Ledger>,
    events: broadcast::Sender<Event>,
    files: FileLocks,
    waiters: Mutex<HashMap<i64, Vec<oneshot::Sender<Response>>>>,
    wake: Arc<Notify>,
    rt: Mutex<Runtime>,
    context: Mutex<Option<ContextFn>>,
    env: std::sync::OnceLock<BaseEnv>,
}

impl Drop for BrainService {
    fn drop(&mut self) {
        // Lets the worker see the service has gone.
        self.wake.notify_one();
    }
}

impl BrainService {
    /// `None` when the writer is off.
    pub fn new(
        cfg: &Config,
        ledger: Arc<Ledger>,
        events: broadcast::Sender<Event>,
    ) -> Option<Arc<BrainService>> {
        Self::with_options(cfg, ledger, events, BrainOptions::from_config(cfg))
    }

    /// As `new`, with the timings given (tests). Starts the worker, which
    /// first retries whatever the ledger still has queued, so both must be
    /// called inside a tokio runtime.
    pub fn with_options(
        cfg: &Config,
        ledger: Arc<Ledger>,
        events: broadcast::Sender<Event>,
        opts: BrainOptions,
    ) -> Option<Arc<BrainService>> {
        if !cfg.brain_writer {
            return None;
        }
        let git = VaultGit::new(cfg.brain_dir.clone(), cfg.push).with_lock_wait(opts.lock_wait);
        let recovering = ledger
            .queue_recent(0)
            .unwrap_or_default()
            .into_iter()
            .filter(|e| e.state == QueueState::Queued)
            .map(|e| e.id)
            .collect();
        let push_problem = match ledger.brain_run("push") {
            Ok(Some((_, false, detail))) => Some(detail),
            _ => None,
        };
        let rt = Runtime {
            recovering,
            push_problem,
            next_backup: opts.backup.map(|d| Instant::now() + d),
            ..Runtime::default()
        };
        let svc = Arc::new(BrainService {
            cfg: Settings {
                brain: cfg.brain_dir.clone(),
                beads: cfg.beads_dir.clone(),
                shared_files: cfg.shared_files.clone(),
                journal_path: cfg.journal_path.clone(),
                current_session: cfg.current_session.clone(),
                push: cfg.push,
                sync_command: cfg.sync_command.clone(),
                bd_command: cfg.bd_command.clone(),
            },
            opts,
            git,
            ledger,
            events,
            files: FileLocks::new(),
            waiters: Mutex::new(HashMap::new()),
            wake: Arc::new(Notify::new()),
            rt: Mutex::new(rt),
            context: Mutex::new(None),
            env: std::sync::OnceLock::new(),
        });
        tokio::spawn(worker(Arc::downgrade(&svc), svc.wake.clone()));
        Some(svc)
    }

    /// The view the board shows while the writer is off.
    pub fn off_view() -> BrainView {
        BrainView {
            on: false,
            entries: Vec::new(),
            last_backup: None,
            last_sync: None,
            push_problem: None,
            brain_dir: None,
        }
    }

    fn rt(&self) -> std::sync::MutexGuard<'_, Runtime> {
        self.rt.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Where the service learns about the slots (the daemon's manager and
    /// work store). Without one, nobody owns anything and no slot runs.
    pub fn set_context(&self, f: impl Fn() -> BrainContext + Send + Sync + 'static) {
        *self.context.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(f));
    }

    pub fn context(&self) -> BrainContext {
        let f = self
            .context
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        f.map(|f| f()).unwrap_or_default()
    }

    /// The vault's one git runner; its mutex is the vault lock.
    pub fn git(&self) -> &VaultGit {
        &self.git
    }

    /// Holds the per-file lock the shared-file writers take for `p`.
    pub async fn lock_file(&self, p: &Path) -> OwnedMutexGuard<()> {
        self.files.lock(&canon(p)).await
    }

    pub fn view(&self) -> BrainView {
        let run = |kind: &str| {
            self.ledger
                .brain_run(kind)
                .ok()
                .flatten()
                .map(|(at, ok, detail)| BrainRun { at, ok, detail })
        };
        BrainView {
            on: true,
            entries: self.ledger.queue_recent(RECENT).unwrap_or_else(|e| {
                tracing::warn!("2ndBrain queue unreadable: {e:#}");
                Vec::new()
            }),
            last_backup: run("backup"),
            last_sync: run("sync"),
            push_problem: self.rt().push_problem.clone(),
            brain_dir: Some(self.cfg.brain.clone()),
        }
    }

    fn announce(&self) {
        let _ = self.events.send(Event::BrainChanged(self.view()));
    }

    fn path_ctx<'a>(&'a self, owners: &'a Owners, cwd: Option<&'a Path>) -> PathContext<'a> {
        PathContext {
            brain: &self.cfg.brain,
            home: &self.opts.home,
            cwd,
            shared_files: &self.cfg.shared_files,
            current_session: &self.cfg.current_session,
            owners,
        }
    }

    /// The deny text for a slot writing `p`, if any (Ruling 13).
    fn deny_for(
        &self,
        p: &Path,
        slot: SlotId,
        owners: &Owners,
        cwd: Option<&Path>,
    ) -> Option<String> {
        let ctx = self.path_ctx(owners, cwd);
        let rel = || {
            locate(&self.cfg.brain, cwd, p, &self.opts.home)
                .map(|(r, _)| r)
                .unwrap_or_else(|_| p.display().to_string())
        };
        match classify(p, &ctx) {
            PathClass::Outside | PathClass::Free => None,
            PathClass::CurrentSession => Some(CURRENT_SESSION_DENY.to_string()),
            PathClass::Shared => Some(deny_shared(&rel())),
            PathClass::Owned(list) => {
                if list.iter().any(|(s, _)| *s == slot) {
                    None
                } else {
                    list.first().map(|(s, what)| deny_owned(&rel(), *s, what))
                }
            }
        }
    }

    /// A folder is judged by what it holds, not only by itself: writing
    /// it as a whole (a recursive delete) touches every file under it.
    fn folder_deny(&self, folder: &str, slot: SlotId, owners: &Owners) -> Option<String> {
        let holds = |child: &str| {
            folder.is_empty()
                || child == folder
                || child
                    .strip_prefix(folder)
                    .is_some_and(|r| r.starts_with('/'))
        };
        for (s, note) in &owners.notes {
            if *s != slot && holds(note) {
                return Some(deny_owned(note, *s, "session note"));
            }
        }
        for (s, f, key) in &owners.folders {
            if *s != slot && holds(f) {
                return Some(deny_owned(f, *s, key));
            }
        }
        let depth = if folder.is_empty() {
            0
        } else {
            folder.split('/').count()
        };
        let holds_shared = self.cfg.shared_files.iter().any(|g| {
            let segs: Vec<&str> = g.split('/').collect();
            depth < segs.len() && (depth == 0 || shared_match(&[segs[..depth].join("/")], folder))
        });
        if holds_shared {
            return Some(format!("{} holds shared 2ndBrain files. Use append_journal for the journal or edit_shared_file for an exact replacement.", if folder.is_empty() { "2ndBrain" } else { folder }));
        }
        let cs = canon(&self.cfg.current_session);
        let root = canon(&self.cfg.brain).join(folder);
        if cs.starts_with(&root) {
            return Some(CURRENT_SESSION_DENY.to_string());
        }
        None
    }

    /// The hook's question: may slot `check.slot` write these paths?
    /// `None` allows; otherwise the deny text (Ruling 13).
    pub fn check_write(&self, check: &WriteCheck, owners: &Owners) -> Option<String> {
        let cwd = check.cwd.as_deref();
        for p in &check.paths {
            if let Some(d) = self.deny_for(p, check.slot, owners, cwd) {
                return Some(d);
            }
            if let Ok((rel, true)) = locate(&self.cfg.brain, cwd, p, &self.opts.home) {
                if let Some(d) = self.folder_deny(&rel, check.slot, owners) {
                    return Some(d);
                }
            }
        }
        None
    }

    /// The `PreToolUse` check of a Bash command (Rulings 10 to 12): a `bd`
    /// write is rewritten to run through the daemon (or denied when joined
    /// with other commands), git in the vault other than a read is denied,
    /// and a redirect or writer aimed at a vault path is judged like the
    /// file tools. `None` leaves the command to the lock check.
    pub fn check_bash(&self, check: &BashCheck, owners: &Owners) -> Option<Response> {
        let verdict = |deny: Option<String>, rewrite: Option<String>| {
            Some(Response::Verdict { deny, rewrite })
        };
        match bash::bd_call(&check.command) {
            // Another repo's own beads: bd there is none of the vault's business (I1).
            bash::BdCall::Write(_) | bash::BdCall::Compound if !self.bd_uses_beads_dir(check) => {}
            bash::BdCall::Write(args) => {
                let line = bash::bd_rewrite(&self.opts.program, &check.command, &args);
                return verdict(None, Some(line));
            }
            bash::BdCall::Compound => return verdict(Some(BD_COMPOUND_DENY.into()), None),
            bash::BdCall::None | bash::BdCall::Read => {}
        }
        let cwd = check.cwd.clone().unwrap_or_else(|| PathBuf::from("/"));
        if bash::vault_git(&check.command, &cwd, &self.cfg.brain).is_some() {
            return verdict(Some(GIT_DENY.into()), None);
        }
        for w in bash::writes(&check.command) {
            if let Some(d) = self.deny_bash_write(&w, check.slot, owners, &cwd) {
                return verdict(Some(d), None);
            }
        }
        None
    }

    /// Whether `bd`, run as this line from the session's cwd, would use
    /// `beads_dir`: `BEADS_DIR` (in the line or the session's environment)
    /// or `--db` names it, or the cwd is under it, or no other `.beads`
    /// exists walking up from the cwd to the home folder or `/`. Without a
    /// cwd it cannot be told, and the line is left alone.
    fn bd_uses_beads_dir(&self, check: &BashCheck) -> bool {
        let beads = canon(&self.cfg.beads);
        let cwd = check.cwd.as_deref();
        let resolve = |raw: &str| -> PathBuf {
            let p = match raw.strip_prefix('~') {
                Some(rest) if rest.is_empty() || rest.starts_with('/') => {
                    self.opts.home.join(rest.trim_start_matches('/'))
                }
                _ => PathBuf::from(raw),
            };
            match cwd {
                Some(c) if p.is_relative() => c.join(p),
                _ => p,
            }
        };
        let names_beads = |raw: &str| {
            let p = canon(&resolve(raw));
            p == beads || p.parent() == Some(beads.as_path())
        };
        if let Some(dir) = bash::assigned(&check.command, "BEADS_DIR") {
            return names_beads(&dir);
        }
        if let Some((_, dir)) = check.env.iter().find(|(k, _)| k == "BEADS_DIR") {
            return names_beads(dir);
        }
        if let bash::BdCall::Write(args) = bash::bd_call(&check.command) {
            if let Some(db) = bash::bd_db(&args) {
                // The rewrite runs bd in beads_dir, where a relative path
                // means the same file only from beads_dir itself.
                let same = db.starts_with('/')
                    || db.starts_with('~')
                    || cwd.is_some_and(|c| canon(c) == beads);
                return same && canon(&resolve(db)).starts_with(&beads);
            }
        }
        let Some(cwd) = cwd else {
            return false;
        };
        let cwd = canon(cwd);
        if cwd.starts_with(&beads) {
            return true;
        }
        let home = canon(&self.opts.home);
        let mut dir = Some(cwd.as_path());
        while let Some(d) = dir {
            if d.join(".beads").exists() {
                return false;
            }
            if d == home {
                break;
            }
            dir = d.parent();
        }
        true
    }

    /// The paths a Bash word names: its glob matches against `cwd`, or the
    /// word as written when it has none or matches nothing.
    fn expand_word(&self, w: &str, cwd: &Path) -> Vec<PathBuf> {
        if bash::has_glob(w) {
            let hits = bash::glob(w, cwd, &self.opts.home);
            if !hits.is_empty() {
                return hits;
            }
        }
        vec![PathBuf::from(w)]
    }

    fn is_vault_folder(&self, p: &Path, cwd: &Path) -> bool {
        matches!(
            locate(&self.cfg.brain, Some(cwd), p, &self.opts.home),
            Ok((_, true))
        )
    }

    /// Judges one write of a Bash command. Only a recursive delete is
    /// judged as a folder; `cp` and `mv` into a folder are judged by the
    /// files that land in it.
    fn deny_bash_write(
        &self,
        w: &bash::Write,
        slot: SlotId,
        owners: &Owners,
        cwd: &Path,
    ) -> Option<String> {
        use bash::Write;
        let file = |p: &Path| self.deny_for(p, slot, owners, Some(cwd));
        match w {
            Write::File(p) | Write::Delete(p) => {
                self.expand_word(p, cwd).iter().find_map(|p| file(p))
            }
            Write::DeleteTree(p) => self.expand_word(p, cwd).iter().find_map(|p| {
                file(p).or_else(|| {
                    let (rel, dir) = locate(&self.cfg.brain, Some(cwd), p, &self.opts.home).ok()?;
                    dir.then(|| self.folder_deny(&rel, slot, owners)).flatten()
                })
            }),
            Write::Copy { target, sources } => self.expand_word(target, cwd).iter().find_map(|t| {
                if !self.is_vault_folder(t, cwd) {
                    return file(t);
                }
                sources
                    .iter()
                    .flat_map(|s| self.expand_word(s, cwd))
                    .find_map(|s| file(&t.join(s.file_name()?)))
            }),
        }
    }

    /// Queues a pathspec commit of `paths` and waits for it to be committed
    /// (not pushed). A slot may not commit what another slot owns, a shared
    /// file or `CURRENT_SESSION.md`; Jonathan may commit anything inside.
    pub async fn commit(
        &self,
        from: Sender,
        paths: Vec<String>,
        message: String,
        owners: &Owners,
    ) -> Response {
        let err = |message: String| Response::Error { message };
        let message = clean_message(&message);
        if message.is_empty() {
            return err("the commit message is empty".into());
        }
        if paths.is_empty() {
            return err("name at least one path to commit".into());
        }
        let mut rels: Vec<String> = Vec::new();
        for p in &paths {
            let rel = match vault_rel(&self.cfg.brain, None, Path::new(p), &self.opts.home) {
                Ok(r) => r,
                Err(e) => return err(e),
            };
            if let Sender::Slot { slot } = from {
                if let Some(d) = self.deny_for(Path::new(&rel), slot, owners, None) {
                    return err(d);
                }
            }
            if !rels.contains(&rel) {
                rels.push(rel);
            }
        }
        self.enqueue(from, rels, message).await
    }

    /// Adds `text` to `section` of today's journal and commits it.
    pub async fn append_journal(&self, from: Sender, section: &str, text: &str) -> Response {
        let err = |message: String| Response::Error { message };
        let section = section.trim();
        if section.is_empty() || section.contains('\n') {
            return err("the section must be one line, e.g. Work Log".into());
        }
        let now = unix_now();
        let rel = fill_template(&self.cfg.journal_path, now, 0, "");
        let abs = self.cfg.brain.join(&rel);
        {
            let _g = self.files.lock(&canon(&abs)).await;
            let file = match std::fs::read_to_string(&abs) {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => shared::journal_header(now),
                Err(e) => return err(format!("cannot read {rel}: {e}")),
            };
            let next = shared::append_section(&file, section, text);
            if let Some(dir) = abs.parent() {
                if let Err(e) = std::fs::create_dir_all(dir) {
                    return err(format!("cannot create the folder for {rel}: {e}"));
                }
            }
            if let Err(e) = shared::write_atomic(&abs, &next) {
                return err(format!("cannot write {rel}: {e}"));
            }
        }
        let message = format!("{}journal, {section}", prefix(from));
        self.enqueue(from, vec![rel], message).await
    }

    /// Replaces the one occurrence of `old` in a shared file and commits it.
    pub async fn edit_shared(&self, from: Sender, rel: &str, old: &str, new: &str) -> Response {
        let err = |message: String| Response::Error { message };
        let rel = match vault_rel(&self.cfg.brain, None, Path::new(rel), &self.opts.home) {
            Ok(r) => r,
            Err(e) => return err(e),
        };
        if !shared_match(&self.cfg.shared_files, &rel) {
            return err(format!(
                "{rel} is not a shared 2ndBrain file; write it directly and commit it with commit_2ndbrain"
            ));
        }
        let abs = self.cfg.brain.join(&rel);
        if let Err(e) = shared::edit_file(&canon(&abs), old, new, &self.files).await {
            return err(e);
        }
        let name = Path::new(&rel)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| rel.clone());
        let message = format!("{}edit {name}", prefix(from));
        self.enqueue(from, vec![rel], message).await
    }

    /// Sets slot `slot`'s section of `CURRENT_SESSION.md` (Ruling 14).
    pub async fn update_current_session(&self, slot: SlotId, label: &str, text: &str) -> Response {
        let cs = &self.cfg.current_session;
        let _g = self.files.lock(&canon(cs)).await;
        let file = match std::fs::read_to_string(cs) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                return Response::Error {
                    message: format!("cannot read CURRENT_SESSION.md: {e}"),
                }
            }
        };
        let next = current::set_section(&file, slot, label, text);
        if let Some(dir) = canon(cs).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match shared::write_atomic(cs, &next) {
            Ok(()) => Response::Ok,
            Err(e) => Response::Error {
                message: format!("cannot write CURRENT_SESSION.md: {e}"),
            },
        }
    }

    /// Removes slot `slot`'s section, for a fresh session in that slot.
    pub async fn clear_current_session(&self, slot: SlotId) {
        let cs = &self.cfg.current_session;
        let _g = self.files.lock(&canon(cs)).await;
        let Ok(file) = std::fs::read_to_string(cs) else {
            return;
        };
        let next = current::clear_section(&file, slot);
        if next != file {
            if let Err(e) = shared::write_atomic(cs, &next) {
                tracing::warn!("S{slot} section of CURRENT_SESSION.md not cleared: {e}");
            }
        }
    }

    /// Runs `bd_command` with `args` in `beads_dir`, under the vault lock
    /// when the beads live in the vault, then queues a `bd sync` for the
    /// next drain (Ruling 10).
    pub async fn run_bd(&self, args: Vec<String>, stdin: Option<String>) -> Response {
        let out = self.bd(&args, stdin).await;
        let now = unix_now();
        let shown = format!("bd {}", args.join(" "));
        let resp = match out {
            Ok(o) => {
                let ok = o.code == 0;
                let _ = self.ledger.set_brain_run(
                    "bd",
                    now,
                    ok,
                    &format!("{shown}: {}", last_line(&o.stdout, &o.stderr)),
                );
                if ok && args.first().map(String::as_str) != Some("sync") {
                    let mut rt = self.rt();
                    rt.bd_sync_pending = true;
                    rt.last_change = Some(Instant::now());
                }
                Response::BdOutput {
                    code: o.code,
                    stdout: o.stdout,
                    stderr: o.stderr,
                }
            }
            Err(e) => {
                let _ = self
                    .ledger
                    .set_brain_run("bd", now, false, &format!("{shown}: {e}"));
                Response::Error { message: e }
            }
        };
        self.wake.notify_one();
        self.announce();
        resp
    }

    fn beads_in_vault(&self) -> bool {
        canon(&self.cfg.beads).starts_with(canon(&self.cfg.brain))
    }

    async fn bd(&self, args: &[String], stdin: Option<String>) -> Result<RunOut, String> {
        let Some((program, base)) = self.cfg.bd_command.split_first() else {
            return Err("bd_command is empty".into());
        };
        let mut argv: Vec<String> = base.to_vec();
        argv.extend(args.iter().cloned());
        // The login shell's PATH is what finds bd under a LaunchAgent.
        let env = self.env.get().cloned();
        let vars = match env {
            Some(mut e) => {
                e.wait().await;
                Some(e.get())
            }
            None => None,
        };
        let _g = if self.beads_in_vault() {
            Some(self.git.lock().await)
        } else {
            None
        };
        run_program(
            Path::new(program),
            &argv,
            &self.cfg.beads,
            BD_TIMEOUT,
            stdin,
            vars.as_deref().map(Vec::as_slice),
        )
        .await
    }

    /// Where `bd` and the sync find their programs: the daemon's base
    /// environment (the login shell's). Set once at start-up; without it
    /// they inherit the daemon's own.
    pub fn set_env(&self, env: BaseEnv) {
        let _ = self.env.set(env);
    }

    /// A bead label change from the work store, run like a session's `bd`
    /// write: under the vault lock, followed by a `bd sync` at the next drain.
    pub async fn write_label(&self, args: Vec<String>) -> Result<(), String> {
        match self.run_bd(args, None).await {
            Response::BdOutput { code: 0, .. } => Ok(()),
            Response::BdOutput { code, stderr, .. } => {
                let first = stderr.lines().next().unwrap_or("").trim().to_string();
                Err(if first.is_empty() {
                    format!("bd exited with {code}")
                } else {
                    format!("bd exited with {code}: {first}")
                })
            }
            Response::Error { message } => Err(message),
            other => Err(format!("unexpected reply: {other:?}")),
        }
    }

    /// Commits whatever is changed and free, or owned only by slots that
    /// are not working, and not under a per-file lock (Ruling 9), as
    /// `vault backup: <local time>`, then pushes.
    pub async fn backup_now(&self) {
        let ctx = self.context();
        let g = self.git.lock().await;
        let now = unix_now();
        let changed = match self.git.changed_paths(&g).await {
            Ok(c) => c,
            Err(e) => {
                let _ = self
                    .ledger
                    .set_brain_run("backup", now, false, &e.to_string());
                drop(g);
                self.announce();
                return;
            }
        };
        // A queued entry commits its own paths with its own message.
        let queued: HashSet<String> = match self.ledger.queue_recent(0) {
            Ok(entries) => entries
                .into_iter()
                .filter(|e| e.state == QueueState::Queued)
                .flat_map(|e| e.paths)
                .collect(),
            Err(e) => {
                let _ = self.ledger.set_brain_run(
                    "backup",
                    now,
                    false,
                    &format!("queue unreadable: {e:#}"),
                );
                drop(g);
                self.announce();
                return;
            }
        };
        // A conflicted file holds conflict markers: never backed up (C1).
        let unmerged = match self.git.unmerged(&g).await {
            Ok(u) => u,
            Err(e) => {
                let _ = self.ledger.set_brain_run(
                    "backup",
                    now,
                    false,
                    &format!("cannot list unmerged paths: {e}"),
                );
                drop(g);
                self.announce();
                return;
            }
        };
        self.note_unmerged(&unmerged);
        let mut keep: Vec<String> = Vec::new();
        for rel in changed {
            if !keep.contains(&rel)
                && !queued.contains(&rel)
                && !unmerged.contains(&rel)
                && self.backup_takes(&rel, &ctx)
            {
                keep.push(rel);
            }
        }
        if keep.is_empty() {
            let _ = self
                .ledger
                .set_brain_run("backup", now, true, "nothing to back up");
            if self.cfg.push && !self.ledger.queue_unpushed().unwrap_or_default().is_empty() {
                self.push_and_record(&g, None).await;
            }
        } else {
            let message = format!("vault backup: {}", local_stamp(now));
            match self
                .ledger
                .queue_push(now, &Sender::Overseer, &keep, &message)
            {
                Ok(id) => {
                    let entry = QueueEntry {
                        id,
                        at: now,
                        from: Sender::Overseer,
                        paths: keep.clone(),
                        message,
                        state: QueueState::Queued,
                        commit: None,
                        pushed: None,
                        error: None,
                    };
                    let state = self.attempt(&g, &entry).await;
                    let paths = format!("{} paths", keep.len());
                    let (ok, detail) = match state {
                        QueueState::Committed | QueueState::NothingToCommit => (true, paths),
                        // Retried by the queue, like any held entry; the
                        // worker is told so it waits only for the retry.
                        QueueState::Queued => {
                            self.wake.notify_one();
                            (true, format!("{paths}, queued: {}", self.entry_error(id)))
                        }
                        QueueState::Failed => (false, self.entry_error(id)),
                    };
                    let _ = self.ledger.set_brain_run("backup", now, ok, &detail);
                }
                Err(e) => {
                    let _ = self
                        .ledger
                        .set_brain_run("backup", now, false, &e.to_string());
                }
            }
        }
        drop(g);
        self.announce();
    }

    /// The error recorded on queue entry `id`, or a stand-in.
    fn entry_error(&self, id: i64) -> String {
        self.ledger
            .queue_recent(RECENT)
            .ok()
            .and_then(|es| es.into_iter().find(|e| e.id == id))
            .and_then(|e| e.error)
            .unwrap_or_else(|| "the commit did not finish".into())
    }

    fn backup_takes(&self, rel: &str, ctx: &BrainContext) -> bool {
        if self.files.is_locked(&canon(&self.cfg.brain.join(rel))) {
            return false;
        }
        match classify(Path::new(rel), &self.path_ctx(&ctx.owners, None)) {
            PathClass::Outside => false,
            // In the vault on some machines (R7): committed like a free file.
            PathClass::CurrentSession | PathClass::Shared | PathClass::Free => true,
            PathClass::Owned(list) => list.iter().all(|(s, _)| !ctx.busy.contains(s)),
        }
    }

    /// Adds an entry and waits for the worker to commit it.
    async fn enqueue(&self, from: Sender, rels: Vec<String>, message: String) -> Response {
        let (id, rx) = {
            // Held across the insert so the worker cannot answer the entry
            // before its waiter is registered.
            let mut waiters = self.waiters.lock().unwrap_or_else(|e| e.into_inner());
            let id = match self.ledger.queue_push(unix_now(), &from, &rels, &message) {
                Ok(id) => id,
                Err(e) => {
                    return Response::Error {
                        message: format!("cannot queue the commit: {e:#}"),
                    }
                }
            };
            let (tx, rx) = oneshot::channel();
            waiters.entry(id).or_default().push(tx);
            (id, rx)
        };
        self.announce();
        self.wake.notify_one();
        match tokio::time::timeout(REPLY_WAIT, rx).await {
            Ok(Ok(r)) => r,
            _ => Response::Queued {
                message: format!(
                    "still queued as entry {id}; the board shows when it is committed"
                ),
            },
        }
    }

    fn reply(&self, id: i64, resp: Response) {
        let txs = self
            .waiters
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id)
            .unwrap_or_default();
        for tx in txs {
            let _ = tx.send(resp.clone());
        }
    }

    /// One pass of the worker; returns how long it may sleep.
    async fn tick(&self) -> Duration {
        let mut wait = IDLE;
        if let Some(w) = self.run_queue().await {
            wait = wait.min(w);
        }
        let need_push = std::mem::take(&mut self.rt().need_push);
        if need_push && self.cfg.push {
            let g = self.git.lock().await;
            self.push_and_record(&g, None).await;
        }
        let queue_empty =
            self.rt().retry_at.is_none() && matches!(self.ledger.queue_next(), Ok(None));
        let now = Instant::now();
        if queue_empty {
            let due = {
                let rt = self.rt();
                (rt.sync_pending || rt.bd_sync_pending)
                    .then_some(rt.last_change)
                    .flatten()
                    .map(|t| t + self.opts.drain)
            };
            match due {
                Some(due) if due <= now => self.drain().await,
                Some(due) => wait = wait.min(due - now),
                None => {}
            }
        }
        let backup_due = self.rt().next_backup;
        if let (Some(at), Some(every)) = (backup_due, self.opts.backup) {
            let now = Instant::now();
            if at <= now {
                self.backup_now().await;
                self.rt().next_backup = Some(Instant::now() + every);
                wait = wait.min(every);
                // A sync held back at the last drain is tried again now,
                // but only after the usual quiet period.
                let retry = {
                    let rt = self.rt();
                    rt.sync_held
                        && rt
                            .last_change
                            .is_none_or(|t| t.elapsed() >= self.opts.drain)
                };
                if retry && queue_empty {
                    self.drain().await;
                }
            } else {
                wait = wait.min(at - now);
            }
        }
        wait
    }

    /// Commits the queued entries in order. Returns a wait when an entry
    /// is held back until a retry.
    async fn run_queue(&self) -> Option<Duration> {
        loop {
            let retry_at = self.rt().retry_at;
            if let Some(at) = retry_at {
                let now = Instant::now();
                if at > now {
                    return Some(at - now);
                }
                self.rt().retry_at = None;
            }
            // The lock first, then the read: a backup may commit an entry
            // of its own meanwhile.
            let g = self.git.lock().await;
            let entry = match self.ledger.queue_next() {
                Ok(Some(e)) => e,
                Ok(None) => return None,
                Err(e) => {
                    tracing::warn!("2ndBrain queue unreadable: {e:#}");
                    return Some(self.opts.retry);
                }
            };
            self.process(&g, entry).await;
        }
    }

    async fn process(&self, g: &VaultGuard<'_>, e: QueueEntry) {
        let recovering = self.rt().recovering.remove(&e.id);
        if recovering {
            match self.find_trailer(g, &e).await {
                Ok(Some(sha)) => {
                    tracing::info!(
                        "2ndBrain queue entry {} was committed before a restart",
                        e.id
                    );
                    if !self.record(e.id, QueueState::Committed, Some(&sha), None, None) {
                        return;
                    }
                    {
                        let mut rt = self.rt();
                        rt.need_push = true;
                        rt.sync_pending = true;
                        rt.last_change = Some(Instant::now());
                    }
                    self.announce();
                    // A caller may still be waiting, after a failed write.
                    self.reply(e.id, committed_text(&sha, &e.message));
                    return;
                }
                Ok(None) => {}
                Err(err) => {
                    // Committing without knowing could commit it twice.
                    tracing::warn!("2ndBrain queue entry {}: git log failed: {err}", e.id);
                    let mut rt = self.rt();
                    rt.recovering.insert(e.id);
                    rt.retry_at = Some(Instant::now() + self.opts.retry);
                    return;
                }
            }
        }
        self.attempt(g, &e).await;
    }

    /// The commit carrying `e`'s trailer, if one exists from after it was queued.
    async fn find_trailer(
        &self,
        g: &VaultGuard<'_>,
        e: &QueueEntry,
    ) -> Result<Option<String>, GitError> {
        let grep = format!("--grep=^{TRAILER}: {}$", e.id);
        let out = self
            .git
            .run(g, &["log", "-E", &grep, "--format=%H %ct"])
            .await?;
        Ok(out.lines().find_map(|l| {
            let (sha, ct) = l.split_once(' ')?;
            let ct: i64 = ct.trim().parse().ok()?;
            // Clock slack; an older commit is another ledger's entry.
            (ct + 60 >= e.at).then(|| sha.to_string())
        }))
    }

    /// Commits one entry and pushes; records and answers the outcome.
    async fn attempt(&self, g: &VaultGuard<'_>, e: &QueueEntry) -> QueueState {
        // Unmerged paths hold conflict markers. While any exist the queue
        // waits (C1); a backup only leaves them out, since it picks its
        // paths again each time.
        let unmerged = match self.git.unmerged(g).await {
            Ok(u) => u,
            Err(err) => {
                let msg = format!("held: cannot list unmerged paths: {err}");
                return self.hold_queued(e.id, &msg);
            }
        };
        self.note_unmerged(&unmerged);
        let mut e = e.clone();
        if is_backup(&e) {
            let ctx = self.context();
            e.paths
                .retain(|p| !unmerged.contains(p) && self.backup_takes(p, &ctx));
        } else if !unmerged.is_empty() {
            let msg = format!(
                "held: 2ndBrain has unresolved conflicts in {}; committed once they are resolved",
                list_paths(&unmerged)
            );
            return self.hold_queued(e.id, &msg);
        }
        let e = &e;
        let attempts = match self.ledger.queue_begin(e.id) {
            Ok(n) => n,
            Err(err) => {
                // Without a count the attempt cap cannot work: do not try.
                tracing::warn!(
                    "2ndBrain queue entry {}: attempt not recorded: {err:#}",
                    e.id
                );
                self.hold(e.id);
                return QueueState::Queued;
            }
        };
        if attempts > MAX_ATTEMPTS {
            let msg = format!("gave up after {} attempts", attempts - 1);
            return self.fail(e.id, &msg);
        }
        let message = format!("{}\n\n{TRAILER}: {}", e.message, e.id);
        let result = self.git.commit_paths(g, &e.paths, &message).await;
        if self.git.take_removed_stale_lock(g) {
            let text = "removed a stale .git/index.lock (older than 60 s)";
            tracing::warn!("2ndBrain: {text}");
            let _ = self
                .ledger
                .log(unix_now(), "2ndBrain", None, "index.lock", text);
        }
        match result {
            Ok(Some(sha)) => {
                {
                    let mut rt = self.rt();
                    rt.sync_pending = true;
                    rt.last_change = Some(Instant::now());
                }
                if !self.record(e.id, QueueState::Committed, Some(&sha), None, None) {
                    // Found by its trailer on the next pass, then pushed.
                    return QueueState::Queued;
                }
                self.announce();
                self.reply(e.id, committed_text(&sha, &e.message));
                self.push_and_record(g, Some(e.id)).await;
                QueueState::Committed
            }
            Ok(None) => {
                if !self.record(e.id, QueueState::NothingToCommit, None, None, None) {
                    return QueueState::Queued;
                }
                self.announce();
                self.reply(
                    e.id,
                    Response::Text {
                        text: "nothing to commit".into(),
                    },
                );
                QueueState::NothingToCommit
            }
            Err(GitError::IndexLocked) if attempts < MAX_ATTEMPTS => {
                let msg = GitError::IndexLocked.to_string();
                self.rt().retry_at = Some(Instant::now() + self.opts.retry);
                if !self.record(e.id, QueueState::Queued, None, None, Some(&msg)) {
                    return QueueState::Queued;
                }
                self.announce();
                self.reply(e.id, Response::Queued { message: msg });
                QueueState::Queued
            }
            Err(GitError::IndexLocked) => self.fail(
                e.id,
                &format!("index.lock still held after {attempts} attempts"),
            ),
            Err(err) => self.fail(e.id, &err.to_string()),
        }
    }

    /// Leaves an entry queued with `msg` as its reason and tries again
    /// after `retry`. Not an attempt.
    fn hold_queued(&self, id: i64, msg: &str) -> QueueState {
        self.rt().retry_at = Some(Instant::now() + self.opts.retry);
        if !self.record(id, QueueState::Queued, None, None, Some(msg)) {
            return QueueState::Queued;
        }
        self.announce();
        self.reply(
            id,
            Response::Queued {
                message: msg.to_string(),
            },
        );
        QueueState::Queued
    }

    /// Keeps `push_problem` set while unmerged paths exist, and clears the
    /// one this raised once they are gone.
    fn note_unmerged(&self, unmerged: &[String]) {
        let text = {
            let mut rt = self.rt();
            if unmerged.is_empty() {
                if !std::mem::take(&mut rt.conflict_problem) {
                    return;
                }
                rt.push_problem = None;
                None
            } else {
                if rt.push_problem.is_some() {
                    return;
                }
                let text = format!(
                    "2ndBrain push needs you: unresolved conflicts in {}. Resolve them in 2ndBrain (git status); Overseer holds its commits, backup and sync of them until then.",
                    list_paths(unmerged)
                );
                rt.push_problem = Some(text.clone());
                rt.conflict_problem = true;
                Some(text)
            }
        };
        let now = unix_now();
        match text {
            Some(t) => {
                tracing::warn!("{t}");
                let _ = self.ledger.set_brain_run("push", now, false, &t);
            }
            None => {
                let _ = self.ledger.set_brain_run("push", now, true, "");
            }
        }
        self.announce();
    }

    /// Writes an entry's state. On failure the entry is held: the queue
    /// waits `retry` and then looks for its trailer before committing again,
    /// so a commit the ledger missed is never made twice.
    fn record(
        &self,
        id: i64,
        state: QueueState,
        sha: Option<&str>,
        pushed: Option<bool>,
        error: Option<&str>,
    ) -> bool {
        match self.ledger.queue_set(id, state, sha, pushed, error) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!("2ndBrain queue entry {id}: state {state:?} not recorded: {e:#}");
                self.hold(id);
                false
            }
        }
    }

    /// The push result of a committed entry. Its state is already recorded,
    /// so a failed write only leaves the push column stale until the next push.
    fn note_push(&self, id: i64, sha: Option<&str>, pushed: bool, error: Option<&str>) {
        if let Err(e) = self
            .ledger
            .queue_set(id, QueueState::Committed, sha, Some(pushed), error)
        {
            tracing::warn!("2ndBrain queue entry {id}: push result not recorded: {e:#}");
        }
    }

    fn hold(&self, id: i64) {
        let mut rt = self.rt();
        rt.recovering.insert(id);
        rt.retry_at = Some(Instant::now() + self.opts.retry);
    }

    fn fail(&self, id: i64, msg: &str) -> QueueState {
        if !self.record(id, QueueState::Failed, None, None, Some(msg)) {
            return QueueState::Queued;
        }
        self.announce();
        self.reply(
            id,
            Response::Error {
                message: msg.to_string(),
            },
        );
        QueueState::Failed
    }

    /// Pushes and records the result on `id` (and, on success, on every
    /// earlier entry still waiting). HEAD is read after the push, since a
    /// rebase gives the commit a new hash (Ruling 6).
    async fn push_and_record(&self, g: &VaultGuard<'_>, id: Option<i64>) {
        if !self.cfg.push {
            return;
        }
        // A pull would stop on the unmerged index; wait for Jonathan.
        match self.git.unmerged(g).await {
            Ok(u) if !u.is_empty() => {
                self.note_unmerged(&u);
                if let Some(id) = id {
                    let head = self.git.run(g, &["rev-parse", "HEAD"]).await.ok();
                    let line = format!("held: unresolved conflicts in {}", list_paths(&u));
                    self.note_push(id, head.as_deref().map(str::trim), false, Some(&line));
                }
                self.announce();
                return;
            }
            _ => {}
        }
        let result = self.git.push(g).await;
        let head = self
            .git
            .run(g, &["rev-parse", "HEAD"])
            .await
            .map(|h| h.trim().to_string())
            .ok();
        let now = unix_now();
        match result {
            Ok(()) => {
                if let Some(id) = id {
                    self.note_push(id, head.as_deref(), true, None);
                }
                for e in self.ledger.queue_unpushed().unwrap_or_default() {
                    self.note_push(e.id, e.commit.as_deref(), true, None);
                }
                let had = {
                    let mut rt = self.rt();
                    rt.conflict_problem = false;
                    rt.push_problem.take().is_some()
                };
                if had || id.is_none() {
                    let _ = self.ledger.set_brain_run("push", now, true, "");
                }
            }
            Err(err) => {
                let line = match &err {
                    GitError::Conflict(m) | GitError::Failed(m) => m.clone(),
                    GitError::IndexLocked => err.to_string(),
                };
                let text = match &err {
                    GitError::Conflict(m) if m.contains("stash") => format!(
                        "2ndBrain push needs you: {m}. Resolve the conflicted files in 2ndBrain (git status), then drop that stash (git stash list --format='%gd %H' shows its stash@{{n}}); Overseer holds its commits, backup and sync until then."
                    ),
                    GitError::Conflict(m) => format!("2ndBrain push needs you: {m}"),
                    _ => format!(
                        "2ndBrain push failed: {line}; it is tried again after the next commit or backup"
                    ),
                };
                tracing::warn!("{text}");
                if let Some(id) = id {
                    self.note_push(id, head.as_deref(), false, Some(&line));
                }
                let _ = self.ledger.set_brain_run("push", now, false, &text);
                let mut rt = self.rt();
                rt.push_problem = Some(text);
                rt.conflict_problem = false;
            }
        }
        self.announce();
    }

    /// Runs the queued `bd sync`, then `sync_command` unless a running
    /// slot has an uncommitted path of its own (Ruling 8).
    async fn drain(&self) {
        let (bd_sync, sync) = {
            let mut rt = self.rt();
            rt.last_change = None;
            (std::mem::take(&mut rt.bd_sync_pending), rt.sync_pending)
        };
        if bd_sync {
            let r = self.bd(&["sync".to_string()], None).await;
            let (ok, detail) = match r {
                Ok(o) => (o.code == 0, last_line(&o.stdout, &o.stderr)),
                Err(e) => (false, e),
            };
            let _ = self
                .ledger
                .set_brain_run("bd", unix_now(), ok, &format!("bd sync: {detail}"));
        }
        if sync {
            self.sync().await;
        }
        self.announce();
    }

    /// Every exit is recorded, so the board shows a sync that did not run.
    async fn sync(&self) {
        let record = |ok: bool, detail: &str| {
            let _ = self.ledger.set_brain_run("sync", unix_now(), ok, detail);
        };
        let Some((first, args)) = self.cfg.sync_command.split_first() else {
            record(true, "skipped: sync_command is empty");
            let mut rt = self.rt();
            rt.sync_pending = false;
            rt.sync_held = false;
            return;
        };
        let program = self.cfg.brain.join(first);
        if !program.is_file() {
            // Not set up on this machine (Ruling 2).
            record(true, &format!("skipped: {first} not found"));
            let mut rt = self.rt();
            rt.sync_pending = false;
            rt.sync_held = false;
            return;
        }
        let ctx = self.context();
        let g = self.git.lock().await;
        let changed = match self.git.changed_paths(&g).await {
            Ok(c) => c,
            Err(e) => {
                // Tried again at the next backup tick.
                let detail = format!("held: git status failed: {e}");
                tracing::warn!("2ndBrain sync {detail}");
                record(false, &detail);
                self.rt().sync_held = true;
                return;
            }
        };
        // The sync script commits what it finds, conflict markers too (C1).
        match self.git.unmerged(&g).await {
            Ok(u) if u.is_empty() => {}
            r => {
                let detail = match r {
                    Ok(u) => {
                        self.note_unmerged(&u);
                        format!("held: unresolved conflicts in {}", list_paths(&u))
                    }
                    Err(e) => format!("held: cannot list unmerged paths: {e}"),
                };
                tracing::warn!("2ndBrain sync {detail}");
                record(false, &detail);
                self.rt().sync_held = true;
                return;
            }
        }
        let pctx = self.path_ctx(&ctx.owners, None);
        let held = changed.iter().any(|rel| {
            matches!(classify(Path::new(rel), &pctx),
                PathClass::Owned(list) if list.iter().any(|(s, _)| ctx.running.contains(s)))
        });
        if held {
            // Not a failure: the sync waits for the slot (Ruling 8).
            let detail = "waiting: a running slot has uncommitted changes";
            tracing::info!("2ndBrain sync {detail}");
            record(true, detail);
            self.rt().sync_held = true;
            return;
        }
        let r = run_program(&program, args, &self.cfg.brain, SYNC_TIMEOUT, None, None).await;
        drop(g);
        let (ok, detail) = match r {
            Ok(o) => (o.code == 0, last_line(&o.stdout, &o.stderr)),
            Err(e) => (false, e),
        };
        record(ok, &detail);
        let mut rt = self.rt();
        rt.sync_pending = false;
        rt.sync_held = false;
    }
}

/// The timed backup's own entries are the only ones Overseer sends.
fn is_backup(e: &QueueEntry) -> bool {
    e.from == Sender::Overseer
}

/// `a.md, b.md` and, past three, `(+2 more)`.
fn list_paths(paths: &[String]) -> String {
    let shown = paths.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
    match paths.len() {
        n if n > 3 => format!("{shown} (+{} more)", n - 3),
        _ => shown,
    }
}

fn committed_text(sha: &str, message: &str) -> Response {
    let subject = message.lines().next().unwrap_or("");
    Response::Text {
        text: format!("committed {}: {subject}", &sha[..sha.len().min(7)]),
    }
}

async fn worker(svc: Weak<BrainService>, wake: Arc<Notify>) {
    loop {
        let wait = match svc.upgrade() {
            Some(s) => s.tick().await,
            None => return,
        };
        tokio::select! {
            _ = wake.notified() => {}
            _ = tokio::time::sleep(wait) => {}
        }
    }
}

struct RunOut {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Runs `program args` in `cwd` with `OVERSEER_COMMIT=1`, killed after `limit`.
async fn run_program(
    program: &Path,
    args: &[String],
    cwd: &Path,
    limit: Duration,
    stdin: Option<String>,
    env: Option<&[(std::ffi::OsString, std::ffi::OsString)]>,
) -> Result<RunOut, String> {
    let mut cmd = tokio::process::Command::new(program);
    if let Some(vars) = env {
        cmd.env_clear().envs(vars.iter().map(|(k, v)| (k, v)));
    }
    cmd.args(args)
        .current_dir(cwd)
        .env("OVERSEER_COMMIT", "1")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let name = program.display();
    let run = async {
        let mut child = cmd.spawn()?;
        if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
            use tokio::io::AsyncWriteExt;
            // Written beside the output drain: a child that fills its
            // stdout pipe before it reads would otherwise deadlock. A child
            // that exits without reading closes the pipe early; that is its
            // answer, not an error. Dropping `pipe` is the EOF bd waits for.
            tokio::spawn(async move {
                let _ = pipe.write_all(text.as_bytes()).await;
            });
        }
        child.wait_with_output().await
    };
    match tokio::time::timeout(limit, run).await {
        Ok(Ok(o)) => Ok(RunOut {
            code: o.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        }),
        Ok(Err(e)) => Err(format!("could not run {name}: {e}")),
        Err(_) => Err(format!("{name} timed out after {} s", limit.as_secs())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_lose_em_dashes_and_forged_trailers() {
        assert_eq!(
            clean_message("S1: a \u{2014} b\n\nOverseer-Queue: 7\nmore"),
            "S1: a - b\n\nmore"
        );
        assert_eq!(clean_message("  \n"), "");
    }

    #[test]
    fn backup_stamp_is_date_and_minutes() {
        let s = local_stamp(1_763_026_200);
        let re = regex::Regex::new(r"^\d{4}-\d{2}-\d{2} \d{2}:\d{2}$").unwrap();
        assert!(re.is_match(&s), "{s}");
    }

    #[test]
    fn last_line_prefers_stdout() {
        assert_eq!(last_line("a\nb\n\n", "err"), "b");
        assert_eq!(last_line("", "e1\ne2\n"), "e2");
        assert_eq!(last_line("", ""), "");
    }
}
