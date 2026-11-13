//! The session ledger: which Claude Code session ran in each slot.
//!
//! The ledger is what lets the daemon offer `claude --resume <id>` for the
//! sessions that were live when it last went down. It holds one row per slot,
//! replaced on every start, in a small SQLite file.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Result;
use overseer_proto::msg::{LockView, Notice, QueueEntry, QueueState, Sender, SlotId};
use rusqlite::Connection;

use crate::locks::table;

/// What the ledger remembers about the session in a slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub slot: SlotId,
    pub session_id: String,
    pub label: String,
    pub cwd: PathBuf,
    pub command: Vec<String>,
    /// Unix seconds.
    pub started: i64,
}

/// A synchronous SQLite-backed ledger. It is `Send + Sync` so it can sit in
/// an `Arc` and be shared across tasks.
///
/// Every call takes a mutex and does disk I/O. The daemon calls it directly
/// on the async runtime, under the slot manager's mutex: each write is a
/// single-row statement, and the start path already forks a process
/// synchronously, so a `spawn_blocking` hop would buy nothing.
pub struct Ledger {
    conn: Mutex<Connection>,
}

/// The version 1 schema: sessions only.
const SESSIONS_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS sessions (
    slot INTEGER PRIMARY KEY,
    session_id TEXT NOT NULL,
    label TEXT,
    cwd TEXT,
    command TEXT,
    started INTEGER,
    exited INTEGER,
    exit_code INTEGER
)";

/// What version 2 adds: which slot owns which work item, a log of changes,
/// and notices waiting for a slot.
const V2_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS assignments (
    key TEXT PRIMARY KEY,
    slot INTEGER NOT NULL,
    at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS history (
    id INTEGER PRIMARY KEY,
    at INTEGER,
    key TEXT,
    slot INTEGER,
    event TEXT,
    detail TEXT
);
CREATE TABLE IF NOT EXISTS notices (
    id INTEGER PRIMARY KEY,
    slot INTEGER,
    at INTEGER,
    text TEXT
)";

/// What version 3 adds: claims on write targets, the slots queued for
/// them, and relays between sessions.
const V3_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS claims (
    target TEXT PRIMARY KEY,
    slot INTEGER NOT NULL,
    kind TEXT NOT NULL,
    reason TEXT,
    item TEXT,
    tool_use_id TEXT,
    pin_revision TEXT,
    relay_id INTEGER,
    back_on TEXT,
    since INTEGER NOT NULL,
    last_used INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS waits (
    id INTEGER PRIMARY KEY,
    target TEXT NOT NULL,
    slot INTEGER NOT NULL,
    via TEXT NOT NULL,
    reason TEXT,
    item TEXT,
    at INTEGER NOT NULL,
    UNIQUE(target, slot)
);
CREATE TABLE IF NOT EXISTS relays (
    id INTEGER PRIMARY KEY,
    at INTEGER NOT NULL,
    sender TEXT NOT NULL,
    from_slot INTEGER,
    to_slot INTEGER NOT NULL,
    text TEXT NOT NULL,
    reply_to INTEGER,
    delivered_at INTEGER,
    dropped_at INTEGER
)";

/// What version 4 adds: the 2ndBrain commit queue, the record of the last
/// backup and sync, and each session's note path. The `ALTER` is not
/// idempotent, so it runs only for files older than version 4.
const V4_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS brain_queue (
    id INTEGER PRIMARY KEY,
    at INTEGER NOT NULL,
    sender TEXT NOT NULL,
    paths TEXT NOT NULL,
    message TEXT NOT NULL,
    state TEXT NOT NULL,
    commit_sha TEXT,
    pushed INTEGER,
    error TEXT,
    attempts INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS brain_runs (
    kind TEXT PRIMARY KEY,
    at INTEGER NOT NULL,
    ok INTEGER NOT NULL,
    detail TEXT
);
ALTER TABLE sessions ADD COLUMN session_note TEXT";

const VERSION: i64 = 4;

/// Creates any missing tables and brings the file up to [`VERSION`]. An
/// older file keeps its rows; the new tables are added beside them. The
/// tables and the version bump commit together.
fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version >= VERSION {
        return Ok(());
    }
    conn.execute_batch(&format!(
        "BEGIN;\n{SESSIONS_SCHEMA};\n{V2_SCHEMA};\n{V3_SCHEMA};\n{V4_SCHEMA};\nPRAGMA user_version = {VERSION};\nCOMMIT;"
    ))?;
    Ok(())
}

fn queue_entry(r: &rusqlite::Row) -> rusqlite::Result<QueueEntry> {
    let sender: String = r.get(2)?;
    let paths: String = r.get(3)?;
    let state: String = r.get(5)?;
    Ok(QueueEntry {
        id: r.get(0)?,
        at: r.get(1)?,
        from: serde_json::from_str(&sender).unwrap_or(Sender::You),
        paths: serde_json::from_str(&paths).unwrap_or_default(),
        message: r.get(4)?,
        state: serde_json::from_str(&format!("\"{state}\"")).unwrap_or(QueueState::Failed),
        commit: r.get(6)?,
        pushed: r.get::<_, Option<i64>>(7)?.map(|p| p != 0),
        error: r.get(8)?,
    })
}

const QUEUE_COLS: &str = "id, at, sender, paths, message, state, commit_sha, pushed, error";

fn state_name(state: QueueState) -> String {
    serde_json::to_string(&state)
        .unwrap_or_default()
        .trim_matches('"')
        .to_string()
}

/// Adds a queued commit, returning its id.
pub(crate) fn queue_push_on(
    conn: &Connection,
    at: i64,
    from: &Sender,
    paths: &[String],
    message: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO brain_queue (at, sender, paths, message, state) VALUES (?1, ?2, ?3, ?4, 'Queued')",
        rusqlite::params![
            at,
            serde_json::to_string(from)?,
            serde_json::to_string(paths)?,
            message
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// The oldest entry still queued.
pub(crate) fn queue_next_on(conn: &Connection) -> Result<Option<QueueEntry>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {QUEUE_COLS} FROM brain_queue WHERE state = 'Queued' ORDER BY id LIMIT 1"
    ))?;
    let mut rows = stmt.query_map([], queue_entry)?;
    Ok(rows.next().transpose()?)
}

/// Records where an entry got to.
pub(crate) fn queue_set_on(
    conn: &Connection,
    id: i64,
    state: QueueState,
    sha: Option<&str>,
    pushed: Option<bool>,
    error: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE brain_queue SET state = ?2, commit_sha = ?3, pushed = ?4, error = ?5,
             attempts = attempts + 1 WHERE id = ?1",
        rusqlite::params![id, state_name(state), sha, pushed, error],
    )?;
    Ok(())
}

/// Committed entries whose push has not succeeded, oldest first.
pub(crate) fn queue_unpushed_on(conn: &Connection) -> Result<Vec<QueueEntry>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {QUEUE_COLS} FROM brain_queue
         WHERE state = 'Committed' AND pushed = 0 ORDER BY id"
    ))?;
    let rows = stmt.query_map([], queue_entry)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Every queued entry plus the `n` most recent others, newest first.
pub(crate) fn queue_recent_on(conn: &Connection, n: usize) -> Result<Vec<QueueEntry>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {QUEUE_COLS} FROM brain_queue WHERE state = 'Queued'
         UNION ALL
         SELECT {QUEUE_COLS} FROM (SELECT * FROM brain_queue WHERE state <> 'Queued'
                                   ORDER BY id DESC LIMIT ?1)
         ORDER BY id DESC"
    ))?;
    let rows = stmt.query_map(rusqlite::params![n as i64], queue_entry)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// Connection-level forms of the work-item calls, for use inside
// `Ledger::transaction`, whose closure already holds the ledger's lock (the
// `&self` methods would deadlock there).

/// Every work item that has a slot, as `(key, slot)`.
pub(crate) fn assignments_on(conn: &Connection) -> Result<Vec<(String, SlotId)>> {
    let mut stmt = conn.prepare("SELECT key, slot FROM assignments ORDER BY key")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Gives `key` to `slot` at `at`, replacing any earlier owner.
pub(crate) fn assign_on(conn: &Connection, key: &str, slot: SlotId, at: i64) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO assignments (key, slot, at) VALUES (?1, ?2, ?3)",
        rusqlite::params![key, slot, at],
    )?;
    Ok(())
}

/// Drops the assignment for `key`, returning the slot that held it.
pub(crate) fn release_on(conn: &Connection, key: &str) -> Result<Option<SlotId>> {
    let slot = conn
        .query_row(
            "SELECT slot FROM assignments WHERE key = ?1",
            rusqlite::params![key],
            |r| r.get(0),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            e => Err(e),
        })?;
    conn.execute(
        "DELETE FROM assignments WHERE key = ?1",
        rusqlite::params![key],
    )?;
    Ok(slot)
}

/// Appends an entry to the assignment history.
pub(crate) fn log_on(
    conn: &Connection,
    at: i64,
    key: &str,
    slot: Option<SlotId>,
    event: &str,
    detail: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO history (at, key, slot, event, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![at, key, slot, event, detail],
    )?;
    Ok(())
}

/// Leaves a notice for a slot.
pub(crate) fn add_notice_on(conn: &Connection, slot: SlotId, at: i64, text: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO notices (slot, at, text) VALUES (?1, ?2, ?3)",
        rusqlite::params![slot, at, text],
    )?;
    Ok(())
}

/// All notices, oldest first.
pub(crate) fn notices_on(conn: &Connection) -> Result<Vec<Notice>> {
    let mut stmt = conn.prepare("SELECT slot, at, text FROM notices ORDER BY id")?;
    let rows = stmt.query_map([], |r| {
        Ok(Notice {
            slot: r.get(0)?,
            at: r.get::<_, Option<i64>>(1)?.unwrap_or_default(),
            text: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

impl Ledger {
    /// Opens (creating if needed) the ledger file, making its parent
    /// directories first. A fresh database is created at the current schema
    /// version; an older one is migrated in place.
    pub fn open(path: &Path) -> Result<Ledger> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        migrate(&conn)?;
        Ok(Ledger {
            conn: Mutex::new(conn),
        })
    }

    /// A throwaway ledger for tests and for running without persistence.
    /// Panics if SQLite cannot open an in-memory database.
    pub fn in_memory() -> Ledger {
        let conn = Connection::open_in_memory().expect("open in-memory sqlite");
        migrate(&conn).expect("create ledger schema");
        Ledger {
            conn: Mutex::new(conn),
        }
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        // A panic while holding the lock cannot leave a half-applied
        // statement, so a poisoned mutex is safe to keep using.
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Runs `f` inside one transaction: it commits if `f` returns `Ok` and
    /// rolls back otherwise, so an assignment's checks and writes land
    /// together. The ledger's lock is held throughout.
    pub fn transaction<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.conn();
        let tx = conn.unchecked_transaction()?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    /// Every work item that has a slot, as `(key, slot)`.
    pub fn assignments(&self) -> Result<Vec<(String, SlotId)>> {
        assignments_on(&self.conn())
    }

    /// Gives `key` to `slot` at `at`, replacing any earlier owner.
    pub fn assign(&self, key: &str, slot: SlotId, at: i64) -> Result<()> {
        assign_on(&self.conn(), key, slot, at)
    }

    /// Drops the assignment for `key`, returning the slot that held it.
    pub fn release(&self, key: &str) -> Result<Option<SlotId>> {
        release_on(&self.conn(), key)
    }

    /// Appends an entry to the assignment history.
    pub fn log(
        &self,
        at: i64,
        key: &str,
        slot: Option<SlotId>,
        event: &str,
        detail: &str,
    ) -> Result<()> {
        log_on(&self.conn(), at, key, slot, event, detail)
    }

    /// Leaves a notice for a slot.
    pub fn add_notice(&self, slot: SlotId, at: i64, text: &str) -> Result<()> {
        add_notice_on(&self.conn(), slot, at, text)
    }

    /// All notices, oldest first.
    pub fn notices(&self) -> Result<Vec<Notice>> {
        notices_on(&self.conn())
    }

    /// Removes every notice for a slot.
    pub fn clear_notices(&self, slot: SlotId) -> Result<()> {
        self.conn().execute(
            "DELETE FROM notices WHERE slot = ?1",
            rusqlite::params![slot],
        )?;
        Ok(())
    }

    /// Marks relays delivered at `now`.
    pub fn mark_relays_delivered(&self, ids: &[i64], now: i64) -> Result<()> {
        self.transaction(|c| table::mark_delivered_on(c, ids, now))
    }

    /// Every claim and wait, every pending relay and the `recent` most
    /// recently delivered or dropped ones, read in one transaction.
    pub fn lock_view(&self, recent: usize, flag_after_secs: i64) -> Result<LockView> {
        self.transaction(|c| {
            Ok(LockView {
                claims: table::claims_on(c)?,
                waits: table::waits_on(c)?,
                relays: table::relays_on(c, recent)?,
                flag_after_secs,
            })
        })
    }

    /// Adds a commit to the 2ndBrain queue, returning its id.
    pub fn queue_push(
        &self,
        at: i64,
        from: &Sender,
        paths: &[String],
        message: &str,
    ) -> Result<i64> {
        queue_push_on(&self.conn(), at, from, paths, message)
    }

    /// The oldest entry still queued.
    pub fn queue_next(&self) -> Result<Option<QueueEntry>> {
        queue_next_on(&self.conn())
    }

    /// Records where a queue entry got to.
    pub fn queue_set(
        &self,
        id: i64,
        state: QueueState,
        sha: Option<&str>,
        pushed: Option<bool>,
        error: Option<&str>,
    ) -> Result<()> {
        queue_set_on(&self.conn(), id, state, sha, pushed, error)
    }

    /// Committed entries still waiting for a push, oldest first.
    pub fn queue_unpushed(&self) -> Result<Vec<QueueEntry>> {
        queue_unpushed_on(&self.conn())
    }

    /// Every queued entry plus the `n` most recent others, newest first.
    pub fn queue_recent(&self, n: usize) -> Result<Vec<QueueEntry>> {
        queue_recent_on(&self.conn(), n)
    }

    /// Records the last `backup` or `sync` run as `(at, ok, detail)`.
    pub fn set_brain_run(&self, kind: &str, at: i64, ok: bool, detail: &str) -> Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO brain_runs (kind, at, ok, detail) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![kind, at, ok, detail],
        )?;
        Ok(())
    }

    pub fn brain_run(&self, kind: &str) -> Result<Option<(i64, bool, String)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT at, ok, detail FROM brain_runs WHERE kind = ?1")?;
        let mut rows = stmt.query_map(rusqlite::params![kind], |r| {
            Ok((
                r.get(0)?,
                r.get::<_, i64>(1)? != 0,
                r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            ))
        })?;
        Ok(rows.next().transpose()?)
    }

    /// Fixes the slot's session note path (relative to the vault).
    pub fn set_session_note(&self, slot: SlotId, rel: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE sessions SET session_note = ?2 WHERE slot = ?1",
            rusqlite::params![slot, rel],
        )?;
        Ok(())
    }

    pub fn session_note(&self, slot: SlotId) -> Result<Option<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT session_note FROM sessions WHERE slot = ?1")?;
        let mut rows =
            stmt.query_map(rusqlite::params![slot], |r| r.get::<_, Option<String>>(0))?;
        Ok(rows.next().transpose()?.flatten())
    }

    /// Records a session starting in a slot, replacing whatever row the slot
    /// had (so `exited` and `exit_code` are cleared).
    pub fn record_start(&self, s: &SessionRow) -> Result<()> {
        let command = serde_json::to_string(&s.command)?;
        self.conn().execute(
            "INSERT OR REPLACE INTO sessions
                 (slot, session_id, label, cwd, command, started, exited, exit_code)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL)",
            rusqlite::params![
                s.slot,
                s.session_id,
                s.label,
                s.cwd.to_string_lossy(),
                command,
                s.started
            ],
        )?;
        Ok(())
    }

    /// Replaces the session id for a slot, for when Claude reports the real
    /// id after the session has started.
    pub fn set_session_id(&self, slot: SlotId, id: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE sessions SET session_id = ?2 WHERE slot = ?1",
            rusqlite::params![slot, id],
        )?;
        Ok(())
    }

    /// Marks the slot's session as exited at `at` (unix seconds).
    pub fn record_exit(&self, slot: SlotId, code: Option<i32>, at: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE sessions SET exited = ?2, exit_code = ?3 WHERE slot = ?1",
            rusqlite::params![slot, at, code],
        )?;
        Ok(())
    }

    /// Sessions that started and never recorded an exit, by slot. After a
    /// daemon restart these are the ones that were running when it went down.
    pub fn live_at_shutdown(&self) -> Result<Vec<SessionRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT slot, session_id, label, cwd, command, started
             FROM sessions WHERE exited IS NULL ORDER BY slot",
        )?;
        let rows = stmt.query_map([], |r| {
            let command: Option<String> = r.get(4)?;
            let command = command
                .and_then(|c| serde_json::from_str(&c).ok())
                .unwrap_or_default();
            Ok(SessionRow {
                slot: r.get(0)?,
                session_id: r.get(1)?,
                label: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                cwd: PathBuf::from(r.get::<_, Option<String>>(3)?.unwrap_or_default()),
                command,
                started: r.get::<_, Option<i64>>(5)?.unwrap_or_default(),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Makes the slot's row live again after a resume: clears the exit, keeps
    /// the session id and the rest.
    pub fn mark_resumed(&self, slot: SlotId) -> Result<()> {
        self.conn().execute(
            "UPDATE sessions SET exited = NULL, exit_code = NULL WHERE slot = ?1",
            rusqlite::params![slot],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(slot: SlotId, id: &str) -> SessionRow {
        SessionRow {
            slot,
            session_id: id.into(),
            label: format!("label {slot}"),
            cwd: PathBuf::from("/tmp/work"),
            command: vec!["claude".into(), "--model".into(), "opus".into()],
            started: 1_700_000_000,
        }
    }

    fn temp_db(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("overseer-ledger-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("nested").join("ledger.db")
    }

    #[test]
    fn start_then_exit_is_not_live() {
        let l = Ledger::in_memory();
        l.record_start(&row(0, "a")).unwrap();
        l.record_exit(0, Some(0), 1_700_000_100).unwrap();
        assert!(l.live_at_shutdown().unwrap().is_empty());
    }

    #[test]
    fn start_without_exit_is_live_after_reopen() {
        let path = temp_db("reopen");
        let r = row(2, "abc");
        {
            let l = Ledger::open(&path).unwrap();
            l.record_start(&r).unwrap();
        }
        let l = Ledger::open(&path).unwrap();
        assert_eq!(l.live_at_shutdown().unwrap(), vec![r]);
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn session_id_update_replaces_the_id() {
        let l = Ledger::in_memory();
        l.record_start(&row(1, "old")).unwrap();
        l.set_session_id(1, "new").unwrap();
        assert_eq!(l.live_at_shutdown().unwrap()[0].session_id, "new");
    }

    #[test]
    fn restart_replaces_the_row() {
        let l = Ledger::in_memory();
        l.record_start(&row(3, "first")).unwrap();
        l.record_exit(3, None, 1_700_000_050).unwrap();
        l.record_start(&row(3, "second")).unwrap();
        let live = l.live_at_shutdown().unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].session_id, "second");
    }

    #[test]
    fn mark_resumed_makes_the_row_live_again() {
        let l = Ledger::in_memory();
        l.record_start(&row(4, "keep")).unwrap();
        l.record_exit(4, Some(1), 1_700_000_100).unwrap();
        assert!(l.live_at_shutdown().unwrap().is_empty());
        l.mark_resumed(4).unwrap();
        let live = l.live_at_shutdown().unwrap();
        assert_eq!(live, vec![row(4, "keep")]);
    }

    #[test]
    fn live_rows_are_ordered_by_slot() {
        let l = Ledger::in_memory();
        l.record_start(&row(5, "e")).unwrap();
        l.record_start(&row(1, "b")).unwrap();
        let slots: Vec<_> = l
            .live_at_shutdown()
            .unwrap()
            .iter()
            .map(|r| r.slot)
            .collect();
        assert_eq!(slots, vec![1, 5]);
    }

    fn user_version(l: &Ledger) -> i64 {
        l.conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }

    fn lock_tables_are_empty(l: &Ledger) {
        let v = l.lock_view(50, 3600).unwrap();
        assert!(v.claims.is_empty() && v.waits.is_empty() && v.relays.is_empty());
    }

    #[test]
    fn version_1_file_migrates_to_4() {
        let path = temp_db("migrate");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SESSIONS_SCHEMA).unwrap();
            conn.execute_batch("PRAGMA user_version = 1").unwrap();
            conn.execute(
                "INSERT INTO sessions (slot, session_id, label, cwd, command, started)
                 VALUES (2, 'old-id', 'old label', '/tmp/work', '[\"claude\"]', 1700000000)",
                [],
            )
            .unwrap();
        }
        let l = Ledger::open(&path).unwrap();
        let live = l.live_at_shutdown().unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].session_id, "old-id");
        assert!(l.assignments().unwrap().is_empty());
        assert!(l.notices().unwrap().is_empty());
        l.log(1, "k", None, "e", "d").unwrap();
        lock_tables_are_empty(&l);
        assert_eq!(user_version(&l), 4);
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn version_2_file_migrates_to_4_keeping_assignments() {
        let path = temp_db("migrate-v2");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SESSIONS_SCHEMA).unwrap();
            conn.execute_batch(V2_SCHEMA).unwrap();
            conn.execute_batch("PRAGMA user_version = 2").unwrap();
            conn.execute(
                "INSERT INTO assignments (key, slot, at) VALUES ('DEMO-12', 3, 100)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO notices (slot, at, text) VALUES (3, 101, 'moved')",
                [],
            )
            .unwrap();
        }
        let l = Ledger::open(&path).unwrap();
        assert_eq!(user_version(&l), 4);
        assert_eq!(l.assignments().unwrap(), vec![("DEMO-12".to_string(), 3)]);
        assert_eq!(l.notices().unwrap()[0].text, "moved");
        lock_tables_are_empty(&l);
        drop(l);
        // Opening a v4 file again changes nothing.
        let l = Ledger::open(&path).unwrap();
        assert_eq!(user_version(&l), 4);
        assert_eq!(l.assignments().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn fresh_file_is_created_at_4() {
        let path = temp_db("fresh");
        let l = Ledger::open(&path).unwrap();
        assert_eq!(user_version(&l), 4);
        lock_tables_are_empty(&l);
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn version_3_file_migrates_to_4() {
        let path = temp_db("migrate-v3");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SESSIONS_SCHEMA).unwrap();
            conn.execute_batch(V2_SCHEMA).unwrap();
            conn.execute_batch(V3_SCHEMA).unwrap();
            conn.execute_batch("PRAGMA user_version = 3").unwrap();
            conn.execute(
                "INSERT INTO sessions (slot, session_id, label, cwd, command, started)
                 VALUES (2, 'old-id', 'lane', '/tmp/work', '[\"claude\"]', 1700000000)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO claims (target, slot, kind, since, last_used)
                 VALUES ('uat/root', 2, 'claim', 10, 11)",
                [],
            )
            .unwrap();
        }
        let l = Ledger::open(&path).unwrap();
        assert_eq!(user_version(&l), 4);
        let live = l.live_at_shutdown().unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].session_id, "old-id");
        assert_eq!(l.lock_view(50, 3600).unwrap().claims.len(), 1);
        assert!(l.queue_next().unwrap().is_none());
        assert!(l.queue_recent(10).unwrap().is_empty());
        assert_eq!(l.session_note(2).unwrap(), None);
        drop(l);
        // Opening a v4 file again changes nothing.
        let l = Ledger::open(&path).unwrap();
        assert_eq!(user_version(&l), 4);
        assert_eq!(l.live_at_shutdown().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn session_note_is_per_slot_and_kept() {
        let l = Ledger::in_memory();
        l.record_start(&row(3, "id3")).unwrap();
        assert_eq!(l.session_note(3).unwrap(), None);
        l.set_session_note(3, "Sessions/2026/11-November/2026-11-13-s3-lane.md")
            .unwrap();
        assert_eq!(
            l.session_note(3).unwrap().as_deref(),
            Some("Sessions/2026/11-November/2026-11-13-s3-lane.md")
        );
        assert_eq!(l.session_note(4).unwrap(), None);
        // Marking the slot resumed keeps its note.
        l.mark_resumed(3).unwrap();
        assert!(l.session_note(3).unwrap().is_some());
    }

    #[test]
    fn queue_is_fifo_and_survives_reopen() {
        use overseer_proto::msg::{QueueState, Sender};
        let path = temp_db("queue");
        let (a, b);
        {
            let l = Ledger::open(&path).unwrap();
            a = l
                .queue_push(
                    10,
                    &Sender::Slot { slot: 3 },
                    &["a.md".to_string()],
                    "first",
                )
                .unwrap();
            b = l
                .queue_push(
                    11,
                    &Sender::You,
                    &["b.md".to_string(), "c.md".to_string()],
                    "second",
                )
                .unwrap();
            assert!(b > a);
            let next = l.queue_next().unwrap().unwrap();
            assert_eq!(next.id, a);
            assert_eq!(next.from, Sender::Slot { slot: 3 });
            assert_eq!(next.state, QueueState::Queued);
            l.queue_set(a, QueueState::Committed, Some("abc1234"), Some(true), None)
                .unwrap();
        }
        let l = Ledger::open(&path).unwrap();
        let next = l.queue_next().unwrap().unwrap();
        assert_eq!(next.id, b);
        assert_eq!(next.paths, vec!["b.md", "c.md"]);
        assert_eq!(next.message, "second");
        l.queue_set(b, QueueState::Failed, None, None, Some("boom"))
            .unwrap();
        assert!(l.queue_next().unwrap().is_none());
        let recent = l.queue_recent(10).unwrap();
        assert_eq!(recent.iter().map(|e| e.id).collect::<Vec<_>>(), vec![b, a]);
        assert_eq!(recent[0].error.as_deref(), Some("boom"));
        assert_eq!(recent[1].commit.as_deref(), Some("abc1234"));
        assert_eq!(recent[1].pushed, Some(true));
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn unpushed_lists_committed_entries_not_pushed() {
        use overseer_proto::msg::{QueueState, Sender};
        let l = Ledger::in_memory();
        let a = l.queue_push(1, &Sender::You, &[], "a").unwrap();
        let b = l.queue_push(2, &Sender::You, &[], "b").unwrap();
        let c = l.queue_push(3, &Sender::You, &[], "c").unwrap();
        l.queue_set(a, QueueState::Committed, Some("1"), Some(true), None)
            .unwrap();
        l.queue_set(
            b,
            QueueState::Committed,
            Some("2"),
            Some(false),
            Some("conflict"),
        )
        .unwrap();
        l.queue_set(c, QueueState::NothingToCommit, None, None, None)
            .unwrap();
        let un = l.queue_unpushed().unwrap();
        assert_eq!(un.iter().map(|e| e.id).collect::<Vec<_>>(), vec![b]);
    }

    #[test]
    fn recent_view_is_every_queued_plus_20_others() {
        use overseer_proto::msg::{QueueState, Sender};
        let l = Ledger::in_memory();
        let first = l.queue_push(1, &Sender::You, &[], "old queued").unwrap();
        for i in 0..25 {
            let id = l.queue_push(10 + i, &Sender::You, &[], "done").unwrap();
            l.queue_set(id, QueueState::NothingToCommit, None, None, None)
                .unwrap();
        }
        let v = l.queue_recent(20).unwrap();
        assert_eq!(v.len(), 21);
        assert_eq!(v.last().unwrap().id, first);
        assert!(v.windows(2).all(|w| w[0].id > w[1].id));
    }

    #[test]
    fn brain_runs_record_backup_and_sync() {
        let l = Ledger::in_memory();
        assert_eq!(l.brain_run("backup").unwrap(), None);
        l.set_brain_run("backup", 100, true, "").unwrap();
        l.set_brain_run("backup", 200, false, "no remote").unwrap();
        assert_eq!(
            l.brain_run("backup").unwrap(),
            Some((200, false, "no remote".to_string()))
        );
        assert_eq!(l.brain_run("sync").unwrap(), None);
    }

    #[test]
    fn locks_waits_and_relays_survive_reopen() {
        use crate::locks::table::{
            claim_on, mark_delivered_on, queue_relay_on, ClaimOutcome, NewClaim, Via,
        };
        use overseer_proto::msg::{ClaimKind, RelayState, Sender};

        let path = temp_db("locks-reopen");
        let ask = |slot, via| NewClaim {
            target: "argo:uat/root",
            slot,
            kind: ClaimKind::Command,
            reason: Some("sync"),
            item: Some("DEMO-12"),
            tool_use_id: None,
            pin_revision: None,
            via,
        };
        {
            let l = Ledger::open(&path).unwrap();
            l.transaction(|c| {
                assert_eq!(claim_on(c, &ask(1, Via::Tool), 10)?, ClaimOutcome::Granted);
                claim_on(c, &ask(3, Via::Hook), 11)?;
                claim_on(c, &ask(2, Via::Tool), 12)?;
                let done = queue_relay_on(c, &Sender::You, 1, "seen", None, 13)?;
                mark_delivered_on(c, &[done], 14)?;
                queue_relay_on(c, &Sender::Slot { slot: 0 }, 2, "waiting", None, 15)?;
                Ok(())
            })
            .unwrap();
        }
        let l = Ledger::open(&path).unwrap();
        let v = l.lock_view(50, 600).unwrap();
        assert_eq!(v.flag_after_secs, 600);
        assert_eq!(v.claims.len(), 1);
        assert_eq!(v.claims[0].slot, 1);
        assert_eq!(v.claims[0].item.as_deref(), Some("DEMO-12"));
        let waits: Vec<_> = v.waits.iter().map(|w| (w.slot, w.at)).collect();
        assert_eq!(waits, vec![(3, 11), (2, 12)]);
        let relays: Vec<_> = v
            .relays
            .iter()
            .map(|r| (r.text.as_str(), r.state, r.from))
            .collect();
        assert_eq!(
            relays,
            vec![
                ("waiting", RelayState::Pending, Sender::Slot { slot: 0 }),
                ("seen", RelayState::Delivered, Sender::You),
            ]
        );
        assert_eq!(v.pending_for(2), 1);
        assert_eq!(v.waiting_on(3), vec!["argo:uat/root"]);
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn assign_release_round_trip() {
        let l = Ledger::in_memory();
        l.assign("DEMO-1", 2, 10).unwrap();
        l.assign("DEMO-2", 3, 11).unwrap();
        l.assign("DEMO-1", 4, 12).unwrap();
        let mut got = l.assignments().unwrap();
        got.sort();
        assert_eq!(
            got,
            vec![("DEMO-1".to_string(), 4), ("DEMO-2".to_string(), 3)]
        );
        assert_eq!(l.release("DEMO-1").unwrap(), Some(4));
        assert_eq!(l.release("DEMO-1").unwrap(), None);
        l.log(13, "DEMO-2", Some(3), "assigned", "by hand").unwrap();
    }

    #[test]
    fn assignment_survives_reopen() {
        let path = temp_db("assign-reopen");
        {
            let l = Ledger::open(&path).unwrap();
            l.assign("DEMO-9", 5, 20).unwrap();
        }
        let l = Ledger::open(&path).unwrap();
        assert_eq!(l.assignments().unwrap(), vec![("DEMO-9".to_string(), 5)]);
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn notices_per_slot_and_clear() {
        let l = Ledger::in_memory();
        l.add_notice(1, 10, "one").unwrap();
        l.add_notice(2, 11, "two").unwrap();
        l.add_notice(1, 12, "three").unwrap();
        let all = l.notices().unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].text, "one");
        l.clear_notices(1).unwrap();
        let left = l.notices().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].slot, 2);
    }

    #[test]
    fn transaction_rolls_back_on_error() {
        let l = Ledger::in_memory();
        let r: Result<()> = l.transaction(|c| {
            c.execute(
                "INSERT INTO assignments (key, slot, at) VALUES ('k', 1, 1)",
                [],
            )?;
            anyhow::bail!("nope")
        });
        assert!(r.is_err());
        assert!(l.assignments().unwrap().is_empty());
        l.transaction(|c| {
            c.execute(
                "INSERT INTO assignments (key, slot, at) VALUES ('k', 1, 1)",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        assert_eq!(l.assignments().unwrap().len(), 1);
    }

    #[test]
    fn work_writes_inside_a_transaction_do_not_deadlock() {
        let l = Ledger::in_memory();
        l.transaction(|c| {
            assign_on(c, "DEMO-1", 2, 1)?;
            log_on(c, 1, "DEMO-1", Some(2), "assigned", "")?;
            add_notice_on(c, 3, 1, "heads up")?;
            assert_eq!(assignments_on(c)?, vec![("DEMO-1".to_string(), 2)]);
            assert_eq!(release_on(c, "DEMO-1")?, Some(2));
            assert_eq!(notices_on(c)?.len(), 1);
            Ok(())
        })
        .unwrap();
        assert!(l.assignments().unwrap().is_empty());
        assert_eq!(l.notices().unwrap()[0].text, "heads up");
    }

    #[test]
    fn ledger_is_send_and_sync() {
        fn check<T: Send + Sync>() {}
        check::<Ledger>();
    }
}
