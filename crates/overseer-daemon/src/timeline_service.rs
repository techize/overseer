//! Keeps today's timeline (#84) current: rebuilds it from the ledger and
//! the session logs when stretches change and on a tick, and announces it
//! when it changes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use overseer_proto::{DayRow, DayView, Event, SlotId};
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

/// Stands in for `now` where an open stretch ends, so views that differ
/// only in `now` compare equal.
const NOW: i64 = i64::MAX;

/// Holds today's timeline and announces it when it changes.
pub struct TimelineService {
    view: Mutex<DayView>,
}

impl TimelineService {
    /// The latest view of today, built at most a minute ago.
    pub fn current(&self) -> DayView {
        self.view.lock().unwrap().clone()
    }

    /// Stores the view; true only when its day or rows differ from the
    /// last one other than where `now` moved, so a rebuild that only moves
    /// `now` is not announced.
    fn update(&self, next: DayView) -> bool {
        let mut view = self.view.lock().unwrap();
        let changed = view.day != next.day || settled(&view) != settled(&next);
        *view = next;
        changed
    }

    /// Starts the task that keeps the day current. It rebuilds and reads
    /// the logs again when `nudge` is poked (a stretch or wait row was
    /// written) and every minute, which also catches a wait passing thirty
    /// minutes. `logs` is `None` when the log check is off; `min_secs` is
    /// the shortest stretch that can show a gap.
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
        let logs = logs.map(Arc::new);
        let task = svc.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(TICK);
            loop {
                // The first tick fires at once and builds the first view.
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = nudge.notified() => {}
                }
                let (ledger, logs) = (ledger.clone(), logs.clone());
                let built = tokio::task::spawn_blocking(move || {
                    build(&ledger, logs.as_deref(), min_secs, unix_now()).0
                })
                .await;
                let view = match built {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("timeline: build failed: {e}");
                        continue;
                    }
                };
                if task.update(view) {
                    // Nobody listening is fine.
                    let _ = events.send(Event::TimelineChanged(task.current()));
                }
            }
        });
        svc
    }
}

/// The view's rows with each open stretch's ends at `now` replaced by
/// [`NOW`]. A stretch is open when it is drawn to now: running, or waiting
/// over thirty minutes with no end stored.
fn settled(view: &DayView) -> Vec<DayRow> {
    let mut rows = view.rows.clone();
    for s in rows.iter_mut().flat_map(|r| r.stretches.iter_mut()) {
        if s.running || s.last == view.now {
            s.last = NOW;
            if let Some(span) = s.spans.last_mut() {
                span.to = NOW;
            }
        }
    }
    rows
}

/// Today's view at `now`, and the logs it read: those of each slot with a
/// stretch today.
fn build(
    ledger: &Ledger,
    logs: Option<&LogSource>,
    min_secs: i64,
    now: i64,
) -> (DayView, HashMap<SlotId, Vec<DayLog>>) {
    let day = session_logs::ymd(&crate::brain::paths::local_date(now));
    let day_start = session_logs::local_day_start(now);
    let stretches = today_stretches(ledger, day_start);
    let waits = ledger.waits_since(day_start).unwrap_or_else(|e| {
        tracing::warn!("timeline: reading waits failed: {e:#}");
        Vec::new()
    });
    let mut read = HashMap::new();
    if let Some(src) = logs {
        for s in &stretches {
            read.entry(s.slot).or_insert_with(|| {
                session_logs::today_logs(&src.brain_dir, &src.home, &src.patterns, s.slot, now)
            });
        }
    }
    let view = build_day(&Inputs {
        now,
        day_start,
        day,
        stretches: &stretches,
        waits: &waits,
        logs: &read,
        check_logs: logs.is_some(),
        min_secs,
        minute: session_logs::local_minute,
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

    /// One in the morning today: far enough from midnight on both sides.
    fn morning() -> i64 {
        session_logs::local_day_start(unix_now()) + 3600
    }

    fn empty_notes() -> (tempfile::TempDir, LogSource) {
        let notes = tempfile::tempdir().unwrap();
        let src = LogSource {
            brain_dir: notes.path().to_path_buf(),
            home: notes.path().to_path_buf(),
            patterns: vec!["Sessions/{yyyy}-{mm}-{dd}-{slot}-*.md".into()],
        };
        (notes, src)
    }

    #[test]
    fn logs_are_read_only_for_slots_with_a_stretch_and_only_when_on() {
        let ledger = Ledger::in_memory();
        let now = morning();
        ledger
            .put_stretch(2, "demo-a", now - 120, now - 60)
            .unwrap();
        let (_notes, src) = empty_notes();
        let (v, read) = build(&ledger, Some(&src), 0, now);
        assert_eq!(read.keys().copied().collect::<Vec<_>>(), vec![2]);
        assert_eq!(v.rows.len(), 1);
        let (_, read) = build(&ledger, None, 0, now);
        assert!(read.is_empty());
    }

    #[test]
    fn only_now_moving_on_a_running_stretch_is_no_change() {
        let ledger = Ledger::in_memory();
        let t0 = morning();
        ledger.put_stretch(2, "demo-a", t0 - 120, t0 - 120).unwrap();
        let (_notes, src) = empty_notes();
        let svc = TimelineService {
            view: Mutex::new(DayView::default()),
        };
        let at = |now| build(&ledger, Some(&src), 0, now).0;
        assert!(svc.update(at(t0)));
        assert!(svc.current().rows[0].stretches[0].running);
        assert!(!svc.update(at(t0 + 60)));
        assert_eq!(svc.current().now, t0 + 60);
        // Uncovered for over ten minutes but still the running end: not
        // logged yet, so no change.
        assert!(!svc.update(at(t0 + 700)));
        // A second stretch is a change.
        ledger.put_stretch(3, "demo-b", t0, t0 + 700).unwrap();
        assert!(svc.update(at(t0 + 760)));
    }

    #[test]
    fn a_stretch_waiting_over_thirty_minutes_does_not_resend_as_now_moves() {
        let ledger = Ledger::in_memory();
        let t0 = morning();
        ledger
            .put_stretch(2, "demo-a", t0 - 3600, t0 - 2400)
            .unwrap();
        ledger.open_wait(2, "demo-a", t0 - 2400).unwrap();
        let (_notes, src) = empty_notes();
        let svc = TimelineService {
            view: Mutex::new(DayView::default()),
        };
        let at = |now| build(&ledger, Some(&src), 0, now).0;
        assert!(svc.update(at(t0)));
        let s = &svc.current().rows[0].stretches[0];
        assert!(!s.running, "waiting over thirty minutes counts as ended");
        assert!(!svc.update(at(t0 + 5)));
        assert!(!svc.update(at(t0 + 65)));
    }
}
