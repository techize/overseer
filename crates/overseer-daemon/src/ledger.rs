//! The session ledger: which Claude Code session ran in each slot.
//!
//! The ledger is what lets the daemon offer `claude --resume <id>` for the
//! sessions that were live when it last went down. It holds one row per slot,
//! replaced on every start, in a small SQLite file.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Result;
use overseer_proto::msg::SlotId;
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
/// Callers must do the writes on a blocking thread
/// (`tokio::task::spawn_blocking`), never on the async runtime: every call
/// takes a mutex and does disk I/O.
pub struct Ledger {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS sessions (
    slot INTEGER PRIMARY KEY,
    session_id TEXT NOT NULL,
    label TEXT,
    cwd TEXT,
    command TEXT,
    started INTEGER,
    exited INTEGER,
    exit_code INTEGER
)";

impl Ledger {
    /// Opens (creating if needed) the ledger file, making its parent
    /// directories first. A fresh database gets `user_version = 1` so later
    /// schema changes have something to migrate from.
    pub fn open(path: &Path) -> Result<Ledger> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version == 0 {
            conn.execute_batch("PRAGMA user_version = 1")?;
        }
        conn.execute_batch(SCHEMA)?;
        Ok(Ledger {
            conn: Mutex::new(conn),
        })
    }

    /// A throwaway ledger for tests and for running without persistence.
    /// Panics if SQLite cannot open an in-memory database.
    pub fn in_memory() -> Ledger {
        let conn = Connection::open_in_memory().expect("open in-memory sqlite");
        conn.execute_batch(SCHEMA).expect("create ledger schema");
        Ledger {
            conn: Mutex::new(conn),
        }
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        // A panic while holding the lock cannot leave a half-applied
        // statement, so a poisoned mutex is safe to keep using.
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
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
    fn ledger_is_send_and_sync() {
        fn check<T: Send + Sync>() {}
        check::<Ledger>();
    }
}
