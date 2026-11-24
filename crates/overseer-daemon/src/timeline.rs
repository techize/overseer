//! The day timeline (#84): each slot's stretches of work today, split into
//! the time its session logs cover, gaps, and long waits cut out.
//!
//! `build_day` is pure: the caller reads the ledger, the logs and the clock.

use crate::ledger::{StoredStretch, StoredWait};
use crate::session_logs::DayLog;
use overseer_proto::{DayRow, DayStretch, DayView, SlotId, Span, SpanKind};
use std::collections::{BTreeMap, HashMap};

/// An uncovered span longer than this, in seconds, is a gap.
pub const GAP_SECS: i64 = 600;
/// A wait longer than this, in seconds, is cut out of the stretch.
pub const WAIT_CUT_SECS: i64 = 600;
/// A slot waiting longer than this, in seconds, counts as ended.
pub const WAIT_ENDS_SECS: i64 = 1800;

/// What a day is built from.
pub struct Inputs<'a> {
    pub now: i64,
    /// Unix seconds of today's local midnight.
    pub day_start: i64,
    /// Local date, `YYYY-MM-DD`.
    pub day: String,
    pub stretches: &'a [StoredStretch],
    pub waits: &'a [StoredWait],
    /// Each slot's logs for today.
    pub logs: &'a HashMap<SlotId, Vec<DayLog>>,
    /// Whether logs are checked at all (`session_logs` is set).
    pub check_logs: bool,
    /// A stretch with less work than this, in seconds, is never red.
    pub min_secs: i64,
}

/// Today's rows: one per slot with a stretch today, in slot order.
pub fn build_day(i: &Inputs) -> DayView {
    let mut sorted: Vec<&StoredStretch> = i.stretches.iter().collect();
    sorted.sort_by_key(|s| (s.slot, s.first));
    let mut rows: BTreeMap<SlotId, Vec<DayStretch>> = BTreeMap::new();
    for s in sorted {
        if let Some(d) = build_stretch(i, s) {
            rows.entry(s.slot).or_default().push(d);
        }
    }
    DayView {
        day: i.day.clone(),
        start: i.day_start,
        now: i.now,
        rows: rows
            .into_iter()
            .map(|(slot, stretches)| DayRow { slot, stretches })
            .collect(),
    }
}

/// A log range as unix seconds: a lone start runs to now, and an end
/// before the start runs to midnight.
fn range_secs(i: &Inputs, (start, end): (u32, Option<u32>)) -> (i64, i64) {
    let at = |m: u32| i.day_start + i64::from(m) * 60;
    let to = match end {
        None => i.now,
        Some(e) if e < start => i.day_start + 86_400,
        Some(e) => at(e),
    };
    (at(start), to)
}

/// One stretch clipped to today, or `None` when it ended before today.
fn build_stretch(i: &Inputs, s: &StoredStretch) -> Option<DayStretch> {
    let from = s.first.max(i.day_start);
    let to = if s.ended.is_none() {
        i.now
    } else {
        s.last.min(i.now)
    };
    if to < i.day_start || to < from {
        return None;
    }
    let overlaps = |(a, b): (i64, i64)| a < to && b > from;

    let slot_waits = i.waits.iter().filter(|w| w.slot == s.slot);
    let long_wait = slot_waits
        .clone()
        .any(|w| w.end.is_none() && i.now - w.start > WAIT_ENDS_SECS);
    let running = s.ended.is_none() && !long_wait;
    let cut: Vec<(i64, i64)> = slot_waits
        .map(|w| (w.start, w.end.unwrap_or(i.now)))
        .filter(|&(a, b)| b - a > WAIT_CUT_SECS && overlaps((a, b)))
        .map(|(a, b)| (a.max(from), b.min(to)))
        .collect();

    let mut covered: Vec<(i64, i64)> = Vec::new();
    let mut logs: Vec<String> = Vec::new();
    if i.check_logs {
        for l in i.logs.get(&s.slot).into_iter().flatten() {
            let ranges: Vec<(i64, i64)> = l
                .ranges
                .iter()
                .map(|&r| range_secs(i, r))
                .filter(|&r| overlaps(r))
                .collect();
            if !ranges.is_empty() && !logs.contains(&l.name) {
                logs.push(l.name.clone());
            }
            covered.extend(ranges.into_iter().map(|(a, b)| (a.max(from), b.min(to))));
        }
    }

    // Cut the stretch at every edge, and say what each piece is.
    let mut edges = vec![from, to];
    edges.extend(cut.iter().chain(&covered).flat_map(|&(a, b)| [a, b]));
    edges.sort_unstable();
    edges.dedup();
    let inside = |set: &[(i64, i64)], a: i64, b: i64| set.iter().any(|&(x, y)| x <= a && b <= y);
    // `None` is uncovered work, classified once its whole run is known.
    let mut pieces: Vec<(i64, i64, Option<SpanKind>)> = Vec::new();
    for w in edges.windows(2) {
        let (a, b) = (w[0], w[1]);
        let kind = if inside(&cut, a, b) {
            Some(SpanKind::Wait)
        } else if inside(&covered, a, b) {
            Some(SpanKind::Logged)
        } else {
            None
        };
        match pieces.last_mut() {
            Some(last) if last.2 == kind => last.1 = b,
            _ => pieces.push((a, b, kind)),
        }
    }

    let waited: i64 = pieces
        .iter()
        .filter(|p| p.2 == Some(SpanKind::Wait))
        .map(|p| p.1 - p.0)
        .sum();
    let work = (to - from) - waited;
    let uncovered = |len: i64| {
        if !i.check_logs {
            SpanKind::Unlogged
        } else if len <= GAP_SECS {
            SpanKind::Short
        } else if running || work < i.min_secs {
            SpanKind::Unlogged
        } else {
            SpanKind::Gap
        }
    };
    let spans = pieces
        .into_iter()
        .map(|(a, b, kind)| Span {
            from: a,
            to: b,
            kind: kind.unwrap_or_else(|| uncovered(b - a)),
        })
        .collect();

    Some(DayStretch {
        session: s.session.clone(),
        first: from,
        last: to,
        running,
        spans,
        logs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_logs::time_ranges;
    use overseer_proto::SpanKind::{Gap, Logged, Short, Unlogged, Wait};

    const NAME: &str = "2026-11-24-s1-demo";

    /// Seconds after a `day_start` of 0.
    fn at(h: i64, m: i64) -> i64 {
        h * 3600 + m * 60
    }

    fn stretch(first: i64, last: i64, ended: bool) -> StoredStretch {
        StoredStretch {
            slot: 1,
            session: "demo-session".into(),
            first,
            last,
            ended: ended.then_some(last),
        }
    }

    fn wait(start: i64, end: Option<i64>) -> StoredWait {
        StoredWait {
            slot: 1,
            session: "demo-session".into(),
            start,
            end,
        }
    }

    /// Slot 1's logs: one log per `**Time**` line given.
    fn logs(lines: &[&str]) -> HashMap<SlotId, Vec<DayLog>> {
        let logs = lines
            .iter()
            .map(|l| DayLog {
                name: NAME.into(),
                ranges: time_ranges(&format!("**Time**: {l}\n")),
            })
            .collect();
        HashMap::from([(1, logs)])
    }

    struct Day {
        now: i64,
        stretches: Vec<StoredStretch>,
        waits: Vec<StoredWait>,
        logs: HashMap<SlotId, Vec<DayLog>>,
        check_logs: bool,
    }

    impl Day {
        fn new(now: i64, s: StoredStretch, lines: &[&str]) -> Day {
            Day {
                now,
                stretches: vec![s],
                waits: Vec::new(),
                logs: logs(lines),
                check_logs: true,
            }
        }

        fn build(&self) -> DayView {
            build_day(&Inputs {
                now: self.now,
                day_start: 0,
                day: "2026-11-24".into(),
                stretches: &self.stretches,
                waits: &self.waits,
                logs: &self.logs,
                check_logs: self.check_logs,
                min_secs: 900,
            })
        }

        /// The only stretch built.
        fn one(&self) -> DayStretch {
            let v = self.build();
            assert_eq!(v.rows.len(), 1);
            assert_eq!(v.rows[0].stretches.len(), 1);
            v.rows[0].stretches[0].clone()
        }
    }

    fn spans(s: &DayStretch) -> Vec<(i64, i64, SpanKind)> {
        s.spans.iter().map(|sp| (sp.from, sp.to, sp.kind)).collect()
    }

    #[test]
    fn logged_stretch_is_all_logged() {
        let d = Day::new(
            at(12, 0),
            stretch(at(9, 0), at(10, 0), true),
            &["09:00 - 10:00"],
        );
        let s = d.one();
        assert_eq!(spans(&s), [(at(9, 0), at(10, 0), Logged)]);
        assert_eq!(s.logs, [NAME]);
        assert!(!s.running);
        let v = d.build();
        assert_eq!(
            (v.start, v.now, v.day.as_str()),
            (0, at(12, 0), "2026-11-24")
        );
        assert_eq!(v.rows[0].slot, 1);
    }

    #[test]
    fn ended_stretch_gap_over_ten_minutes_is_red() {
        let d = Day::new(
            at(12, 0),
            stretch(at(9, 0), at(10, 0), true),
            &["09:00 - 09:40"],
        );
        assert_eq!(
            spans(&d.one()),
            [(at(9, 0), at(9, 40), Logged), (at(9, 40), at(10, 0), Gap)]
        );
    }

    #[test]
    fn gap_of_ten_minutes_or_less_is_short() {
        let d = Day::new(
            at(12, 0),
            stretch(at(9, 0), at(10, 0), true),
            &["09:00 - 09:50"],
        );
        assert_eq!(
            spans(&d.one()),
            [(at(9, 0), at(9, 50), Logged), (at(9, 50), at(10, 0), Short)]
        );
    }

    #[test]
    fn running_stretch_is_never_red() {
        let d = Day::new(
            at(10, 0),
            stretch(at(9, 0), at(10, 0), false),
            &["09:00 - 09:40"],
        );
        let s = d.one();
        assert!(s.running);
        assert_eq!(
            spans(&s),
            [
                (at(9, 0), at(9, 40), Logged),
                (at(9, 40), at(10, 0), Unlogged)
            ]
        );
    }

    #[test]
    fn edge_gap_counts() {
        let d = Day::new(
            at(12, 0),
            stretch(at(9, 0), at(10, 0), true),
            &["09:20 - 10:00"],
        );
        assert_eq!(
            spans(&d.one()),
            [(at(9, 0), at(9, 20), Gap), (at(9, 20), at(10, 0), Logged)]
        );
    }

    #[test]
    fn long_wait_is_cut_out() {
        let mut d = Day::new(
            at(13, 0),
            stretch(at(9, 0), at(12, 0), true),
            &["09:00 - 10:00, 11:30 - 12:00"],
        );
        d.waits = vec![wait(at(10, 0), Some(at(11, 30)))];
        assert_eq!(
            spans(&d.one()),
            [
                (at(9, 0), at(10, 0), Logged),
                (at(10, 0), at(11, 30), Wait),
                (at(11, 30), at(12, 0), Logged)
            ]
        );
    }

    #[test]
    fn short_wait_is_work() {
        let mut d = Day::new(at(13, 0), stretch(at(9, 0), at(11, 0), true), &[]);
        d.waits = vec![wait(at(10, 0), Some(at(10, 5)))];
        assert_eq!(spans(&d.one()), [(at(9, 0), at(11, 0), Gap)]);
    }

    #[test]
    fn waiting_over_thirty_minutes_counts_as_ended() {
        let mut d = Day::new(
            at(10, 31),
            stretch(at(9, 0), at(10, 0), false),
            &["09:00 - 09:30"],
        );
        d.waits = vec![wait(at(10, 0), None)];
        let s = d.one();
        assert!(!s.running);
        assert_eq!(
            spans(&s),
            [
                (at(9, 0), at(9, 30), Logged),
                (at(9, 30), at(10, 0), Gap),
                (at(10, 0), at(10, 31), Wait)
            ]
        );
        // Under thirty minutes it is still running and never red.
        d.now = at(10, 29);
        let s = d.one();
        assert!(s.running);
        assert_eq!(s.spans[1].kind, Unlogged);
    }

    #[test]
    fn lone_start_covers_to_now() {
        let d = Day::new(at(9, 40), stretch(at(9, 0), at(9, 40), false), &["~9:05"]);
        let s = d.one();
        assert_eq!(
            spans(&s),
            [(at(9, 0), at(9, 5), Short), (at(9, 5), at(9, 40), Logged)]
        );
        assert_eq!(s.logs, [NAME]);
    }

    #[test]
    fn short_stretch_is_never_red() {
        let d = Day::new(at(12, 0), stretch(at(9, 0), at(9, 12), true), &[]);
        let s = d.one();
        assert_eq!(spans(&s), [(at(9, 0), at(9, 12), Unlogged)]);
        assert!(s.logs.is_empty());
    }

    #[test]
    fn logs_off_shows_stretches_only() {
        let mut d = Day::new(at(12, 0), stretch(at(9, 0), at(10, 0), true), &[]);
        d.check_logs = false;
        assert_eq!(spans(&d.one()), [(at(9, 0), at(10, 0), Unlogged)]);
    }

    #[test]
    fn stretch_from_yesterday_is_clipped_to_midnight() {
        let d = Day::new(at(12, 0), stretch(-3600, 3600, true), &["00:00 - 01:00"]);
        let s = d.one();
        assert_eq!(s.first, 0);
        assert_eq!(s.last, 3600);
        assert_eq!(spans(&s), [(0, 3600, Logged)]);
    }

    #[test]
    fn stretch_ended_yesterday_is_dropped() {
        let d = Day::new(at(12, 0), stretch(-7200, -3600, true), &[]);
        assert!(d.build().rows.is_empty());
    }

    #[test]
    fn index_rule_cases() {
        let d = Day::new(
            at(17, 0),
            stretch(at(8, 2), at(12, 15), true),
            &["08:02 - 10:35, 10:46 - 12:15, idle 13:15 - 16:00"],
        );
        assert_eq!(
            spans(&d.one()),
            [
                (at(8, 2), at(10, 35), Logged),
                (at(10, 35), at(10, 46), Gap),
                (at(10, 46), at(12, 15), Logged)
            ]
        );
    }

    #[test]
    fn rows_follow_slot_order_and_stretches_follow_first() {
        let mut d = Day::new(at(12, 0), stretch(at(10, 0), at(11, 0), true), &[]);
        d.stretches.push(stretch(at(8, 0), at(9, 0), true));
        d.stretches.push(StoredStretch {
            slot: 0,
            ..stretch(at(9, 0), at(9, 30), true)
        });
        let v = d.build();
        assert_eq!(v.rows.iter().map(|r| r.slot).collect::<Vec<_>>(), [0, 1]);
        let firsts: Vec<i64> = v.rows[1].stretches.iter().map(|s| s.first).collect();
        assert_eq!(firsts, [at(8, 0), at(10, 0)]);
    }
}
