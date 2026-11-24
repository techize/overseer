//! Keeps today's timeline (#84) current: rebuilds it from the ledger and
//! the session logs when stretches change and on a tick, and announces it
//! when it changes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use overseer_proto::{DayView, Event, SlotId};
use tokio::sync::{broadcast, Notify};

use crate::ledger::{Ledger, StoredStretch};
use crate::session_logs::{self, DayLog};
use crate::timeline::{build_day, Inputs};
use crate::work::store::unix_now;

/// How often the day is rebuilt with no change, to pick up new or edited
/// logs and move "now".
const TICK: Duration = Duration::from_secs(60);

/// Where a slot's session logs are found: `session_logs` patterns in the
/// notes folder.
pub struct LogSource {
    pub brain_dir: PathBuf,
    pub home: PathBuf,
    pub patterns: Vec<String>,
}

/// Holds today's timeline and announces it when it changes.
pub struct TimelineService {
    view: Mutex<DayView>,
}

impl TimelineService {
    pub fn current(&self) -> DayView {
        self.view.lock().unwrap().clone()
    }

    /// Stores the view; true only when its day or rows differ from the
    /// last one, so a tick that only moves `now` is not announced.
    fn update(&self, next: DayView) -> bool {
        let mut view = self.view.lock().unwrap();
        let changed = view.day != next.day || view.rows != next.rows;
        *view = next;
        changed
    }

    /// Starts the task that keeps the day current. It rebuilds when
    /// `nudge` is poked (a stretch or wait row was written), when a slot
    /// changes, and every minute. Logs are read again on a nudge or the
    /// tick, never per slot change. `logs` is `None` when the log check
    /// is off; `min_secs` is the shortest stretch that can show a gap.
    pub fn spawn(
        ledger: Arc<Ledger>,
        logs: Option<LogSource>,
        min_secs: i64,
        nudge: Arc<Notify>,
        events: broadcast::Sender<Event>,
    ) -> Arc<TimelineService> {
        let svc = Arc::new(TimelineService {
            view: Mutex::new(DayView::default()),
        });
        // Subscribe before the first build so no change falls between the two.
        let mut rx = events.subscribe();
        let logs = logs.map(Arc::new);
        let task = svc.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(TICK);
            let mut read: HashMap<SlotId, Vec<DayLog>> = HashMap::new();
            loop {
                // The first tick fires at once and builds the first view.
                let reread_logs = tokio::select! {
                    _ = tick.tick() => true,
                    _ = nudge.notified() => true,
                    ev = rx.recv() => match ev {
                        Ok(Event::SlotChanged(_)) => false,
                        Ok(_) => continue,
                        Err(broadcast::error::RecvError::Lagged(_)) => false,
                        Err(broadcast::error::RecvError::Closed) => return,
                    },
                };
                let (ledger, logs) = (ledger.clone(), logs.clone());
                let cached = (!reread_logs).then(|| std::mem::take(&mut read));
                let built = tokio::task::spawn_blocking(move || {
                    build(&ledger, logs.as_deref(), cached, min_secs, unix_now())
                })
                .await;
                let (view, logs_read) = match built {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!("timeline: build failed: {e}");
                        continue;
                    }
                };
                read = logs_read;
                if task.update(view) {
                    // Nobody listening is fine.
                    let _ = events.send(Event::TimelineChanged(task.current()));
                }
            }
        });
        svc
    }
}

/// Today's view at `now`, and the logs it used. `cached` is reused when
/// given; otherwise each slot with a stretch today has its logs read.
fn build(
    ledger: &Ledger,
    logs: Option<&LogSource>,
    cached: Option<HashMap<SlotId, Vec<DayLog>>>,
    min_secs: i64,
    now: i64,
) -> (DayView, HashMap<SlotId, Vec<DayLog>>) {
    let day_start = session_logs::local_day_start(now);
    let stretches = today_stretches(ledger, day_start);
    let waits = ledger.waits_since(day_start).unwrap_or_else(|e| {
        tracing::warn!("timeline: reading waits failed: {e:#}");
        Vec::new()
    });
    let read = match (logs, cached) {
        (None, _) => HashMap::new(),
        (Some(_), Some(c)) => c,
        (Some(src), None) => {
            let mut read = HashMap::new();
            for s in &stretches {
                read.entry(s.slot).or_insert_with(|| {
                    session_logs::today_logs(&src.brain_dir, &src.home, &src.patterns, s.slot, now)
                });
            }
            read
        }
    };
    let view = build_day(&Inputs {
        now,
        day_start,
        day: session_logs::ymd(&crate::brain::paths::local_date(now)),
        stretches: &stretches,
        waits: &waits,
        logs: &read,
        check_logs: logs.is_some(),
        min_secs,
    });
    (view, read)
}

/// Stretches with work today, and any still running from before it.
fn today_stretches(ledger: &Ledger, day_start: i64) -> Vec<StoredStretch> {
    let warn = |e: anyhow::Error| {
        tracing::warn!("timeline: reading stretches failed: {e:#}");
        Vec::new()
    };
    let mut stretches = ledger.stretches_since(day_start).unwrap_or_else(warn);
    for open in ledger.open_stretches().unwrap_or_else(warn) {
        if !stretches.contains(&open) {
            stretches.push(open);
        }
    }
    stretches
}

#[cfg(test)]
mod tests {
    use super::*;
    use overseer_proto::DayRow;

    fn view(now: i64, rows: Vec<DayRow>) -> DayView {
        DayView {
            day: "2026-11-24".into(),
            start: 0,
            now,
            rows,
        }
    }

    #[test]
    fn update_ignores_a_new_now_alone() {
        let svc = TimelineService {
            view: Mutex::new(DayView::default()),
        };
        assert!(svc.update(view(100, vec![])));
        assert!(!svc.update(view(160, vec![])));
        // The stored view still moves on.
        assert_eq!(svc.current().now, 160);
        let row = DayRow {
            slot: 1,
            stretches: vec![],
        };
        assert!(svc.update(view(220, vec![row])));
    }

    #[test]
    fn logs_are_read_only_for_slots_with_a_stretch_and_only_when_on() {
        let ledger = Ledger::in_memory();
        let now = unix_now();
        ledger
            .put_stretch(2, "demo-a", now - 120, now - 60)
            .unwrap();
        let notes = tempfile::tempdir().unwrap();
        let src = LogSource {
            brain_dir: notes.path().to_path_buf(),
            home: notes.path().to_path_buf(),
            patterns: vec!["Sessions/{yyyy}-{mm}-{dd}-{slot}-*.md".into()],
        };
        let (v, read) = build(&ledger, Some(&src), None, 0, now);
        assert_eq!(read.keys().copied().collect::<Vec<_>>(), vec![2]);
        assert_eq!(v.rows.len(), 1);
        let (_, read) = build(&ledger, None, None, 0, now);
        assert!(read.is_empty());
    }
}
