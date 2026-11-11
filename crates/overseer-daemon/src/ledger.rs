//! The session ledger: which Claude Code session ran in each slot.
//!
//! The ledger is what lets the daemon offer `claude --resume <id>` for the
//! sessions that were live when it last went down. It holds one row per slot,
//! replaced on every start, in a small SQLite file.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Result;
use overseer_proto::msg::{Notice, SlotId};
use rusqlite::Connection;

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

const VERSION: i64 = 2;

/// Creates any missing tables and brings the file up to [`VERSION`]. A
/// version 1 file keeps its session rows; the new tables are added beside
/// them. The tables and the version bump commit together.
fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version >= VERSION {
        return Ok(());
    }
    conn.execute_batch(&format!(
        "BEGIN;\n{SESSIONS_SCHEMA};\n{V2_SCHEMA};\nPRAGMA user_version = {VERSION};\nCOMMIT;"
    ))?;
    Ok(())
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

    #[test]
    fn version_1_file_migrates_to_2_keeping_sessions() {
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
        let v: i64 = l
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 2);
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn assign_release_round_trip() {
        let l = Ledger::in_memory();
        l.assign("PLAT-1", 2, 10).unwrap();
        l.assign("PLAT-2", 3, 11).unwrap();
        l.assign("PLAT-1", 4, 12).unwrap();
        let mut got = l.assignments().unwrap();
        got.sort();
        assert_eq!(
            got,
            vec![("PLAT-1".to_string(), 4), ("PLAT-2".to_string(), 3)]
        );
        assert_eq!(l.release("PLAT-1").unwrap(), Some(4));
        assert_eq!(l.release("PLAT-1").unwrap(), None);
        l.log(13, "PLAT-2", Some(3), "assigned", "by hand").unwrap();
    }

    #[test]
    fn assignment_survives_reopen() {
        let path = temp_db("assign-reopen");
        {
            let l = Ledger::open(&path).unwrap();
            l.assign("PLAT-9", 5, 20).unwrap();
        }
        let l = Ledger::open(&path).unwrap();
        assert_eq!(l.assignments().unwrap(), vec![("PLAT-9".to_string(), 5)]);
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
