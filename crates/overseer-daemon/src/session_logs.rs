//! The session-log check run when a slot's session ends (#73, #74).
//!
//! A session writes its own log in the notes folder, and the day's figures
//! are built from the log's `**Time**` header and its `outcome` and
//! `highlights` frontmatter. Overseer knows when the session really started
//! and ended, so at the end it reads the slot's logs for that run and says
//! what is missing or does not cover the run: a notice on the slot's card,
//! and a relay to S0 so the coordinator can follow up. With the notes
//! writer on it also appends the run's real times to `slot_runs` (a file a
//! day), a JSON line per run, for the index to fill missing or short times
//! from.

use crate::brain::bash::glob;
use crate::brain::paths::{local_date, LocalDate};
use overseer_proto::SlotId;
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// How far the logs' times may fall inside the run before they are said
/// not to cover it.
pub const SLACK_MINS: u32 = 15;

/// What the check reads from one log.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LogFacts {
    /// The `**Time**` header's start and end, as minutes after midnight.
    pub start: Option<u32>,
    pub end: Option<u32>,
    pub outcome: bool,
    pub highlights: bool,
}

/// One log the run wrote: its file stem and what it says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Log {
    pub name: String,
    pub facts: LogFacts,
}

/// A session's run: unix seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub slot: SlotId,
    pub started: i64,
    pub ended: i64,
}

/// The `**Time**` line: an optional list marker, then the rest of the line.
fn time_line_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?m)^\s*(?:[-*]\s+)?\*\*Time\*\*:(.*)$").expect("time line regex")
    })
}

/// One time, or a range after a hyphen, en dash or em dash, each time with an optional `~`.
fn range_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"~?\s*(\d{1,2}):(\d{2})(?:\s*[-\u{2013}\u{2014}]\s*~?\s*(\d{1,2}):(\d{2}))?")
            .expect("range regex")
    })
}

/// Words that mark a range as time away, not work.
const AWAY_WORDS: [&str; 6] = ["idle", "break", "lunch", "away", "paused", "gap"];

/// The work ranges on a log's `**Time**` line, the same rule the session
/// index uses: text in brackets is a note and skipped; the rest splits at
/// `,` and `;`, and a part naming time away (idle, a break) is skipped.
/// Every `start - end` range counts, and so does a lone start when it is
/// the line's first time (a log written before its end). Ranges are
/// minutes after midnight.
pub fn time_ranges(text: &str) -> Vec<(u32, Option<u32>)> {
    let Some(line) = time_line_re().captures(text).map(|c| c[1].to_string()) else {
        return Vec::new();
    };
    let mut plain = String::new();
    let mut depth = 0u32;
    for ch in line.chars() {
        match ch {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            _ if depth == 0 => plain.push(ch),
            _ => {}
        }
    }
    let mut out = Vec::new();
    let mut first = true;
    for part in plain.split([',', ';']) {
        let lower = part.to_lowercase();
        let away = lower
            .split(|c: char| !c.is_alphabetic())
            .any(|w| AWAY_WORDS.contains(&w));
        for c in range_re().captures_iter(part) {
            let start = minutes(&c[1], &c[2]);
            let end = c
                .get(3)
                .zip(c.get(4))
                .and_then(|(h, m)| minutes(h.as_str(), m.as_str()));
            let lone_ok = first;
            first = false;
            match (start, end) {
                _ if away => {}
                (Some(s), Some(e)) => out.push((s, Some(e))),
                (Some(s), None) if lone_ok => out.push((s, None)),
                _ => {}
            }
        }
    }
    out
}

fn minutes(h: &str, m: &str) -> Option<u32> {
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// The leading `---` block and the rest, or no block and the whole text.
fn split_frontmatter(text: &str) -> (Option<&str>, &str) {
    let Some(rest) = text.strip_prefix("---\n") else {
        return (None, text);
    };
    match rest.find("\n---") {
        Some(i) => {
            let after = &rest[i + 4..];
            let body = after.split_once('\n').map_or("", |(_, b)| b);
            (Some(&rest[..i]), body)
        }
        None => (None, text),
    }
}

/// A scalar that says nothing: empty, `null`, `~` or empty quotes.
fn blank(v: &str) -> bool {
    matches!(v.trim(), "" | "null" | "~" | "\"\"" | "''")
}

/// Whether the top-level `key` in the frontmatter has a value: a scalar
/// that is not blank, a flow list with an item, or a block list with an
/// item that is not blank.
fn has_value(fm: &str, key: &str) -> bool {
    let mut lines = fm.lines();
    while let Some(line) = lines.next() {
        let Some(v) = line.strip_prefix(key).and_then(|r| r.strip_prefix(':')) else {
            continue;
        };
        let v = v.trim();
        if let Some(inner) = v.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            return inner.split(',').any(|i| !blank(i));
        }
        if !blank(v) {
            return true;
        }
        // A block list: the indented `- item` lines that follow.
        for next in lines.by_ref() {
            let t = next.trim_start();
            if next.len() == t.len() && !t.is_empty() {
                break;
            }
            if let Some(item) = t.strip_prefix('-') {
                if !blank(item) {
                    return true;
                }
            }
        }
        return false;
    }
    false
}

/// What a log's text says about its times and authored fields.
pub fn parse(text: &str) -> LogFacts {
    let (fm, body) = split_frontmatter(text);
    let fm = fm.unwrap_or("");
    // The first range's start and the last range's end.
    let ranges = time_ranges(body);
    let start = ranges.first().map(|r| r.0);
    let end = ranges.last().and_then(|r| r.1);
    LogFacts {
        start,
        end,
        outcome: has_value(fm, "outcome"),
        highlights: has_value(fm, "highlights"),
    }
}

/// `HH:MM` for minutes after midnight.
pub fn hhmm(mins: u32) -> String {
    format!("{:02}:{:02}", mins / 60, mins % 60)
}

/// The local date and minutes after midnight of unix seconds.
pub fn local_minutes(t: i64) -> (LocalDate, u32) {
    // SAFETY: localtime_r only writes the `tm` we pass it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let time = t as libc::time_t;
    let ok = !unsafe { libc::localtime_r(&time, &mut tm) }.is_null();
    let mins = if ok {
        (tm.tm_hour.clamp(0, 23) * 60 + tm.tm_min.clamp(0, 59)) as u32
    } else {
        0
    };
    (local_date(t), mins)
}

/// A local date as `YYYY-MM-DD`.
pub fn ymd(d: &LocalDate) -> String {
    format!("{:04}-{:02}-{:02}", d.year, d.month, d.day)
}

/// What is wrong with a run's logs, one phrase each; empty when nothing
/// is. `start` and `end` are the run's local minutes; `same_day` is false
/// when it ran past midnight, where minutes do not compare and only the
/// logs' own fields are checked.
pub fn problems(logs: &[Log], start: u32, end: u32, same_day: bool) -> Vec<String> {
    if logs.is_empty() {
        return vec!["no session log was written".to_string()];
    }
    let mut out = Vec::new();
    for l in logs {
        let mut missing = Vec::new();
        if l.facts.start.is_none() {
            missing.push("**Time** header");
        } else if l.facts.end.is_none() {
            missing.push("end time");
        }
        if !l.facts.outcome {
            missing.push("outcome");
        }
        if !l.facts.highlights {
            missing.push("highlights");
        }
        if !missing.is_empty() {
            out.push(format!("{}.md has no {}", l.name, join_and(&missing)));
        }
    }
    if same_day {
        let first = logs.iter().filter_map(|l| l.facts.start).min();
        let last = logs.iter().filter_map(|l| l.facts.end).max();
        if let Some(first) = first {
            if first > start + SLACK_MINS {
                out.push(format!(
                    "the logs start at {} but work started at {}",
                    hhmm(first),
                    hhmm(start)
                ));
            }
        }
        if let Some(last) = last {
            if last + SLACK_MINS < end {
                out.push(format!(
                    "the logs end at {} but work went on until {}",
                    hhmm(last),
                    hhmm(end)
                ));
            }
        }
    }
    out
}

fn join_and(parts: &[&str]) -> String {
    match parts {
        [] => String::new(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
    }
}

/// The notice for the slot's card, or `None` when the logs are fine.
pub fn notice(run: &Run, logs: &[Log], runs_file: Option<&str>) -> Option<String> {
    let (sd, start) = local_minutes(run.started);
    let (ed, end) = local_minutes(run.ended);
    let same_day = ymd(&sd) == ymd(&ed);
    let found = problems(logs, start, end, same_day);
    if found.is_empty() {
        return None;
    }
    let mut text = format!(
        "Session log check for {} ({}-{}): {}.",
        overseer_proto::slot_name(run.slot),
        hhmm(start),
        hhmm(end),
        found.join("; ")
    );
    if let Some(f) = runs_file {
        text.push_str(&format!(" The real times are in {f}."));
    }
    Some(text)
}

/// The JSON line `slot_runs` gets for a run.
pub fn run_line(run: &Run, logs: &[Log]) -> String {
    let (sd, start) = local_minutes(run.started);
    let (_, end) = local_minutes(run.ended);
    serde_json::json!({
        "kind": "run",
        "date": ymd(&sd),
        "slot": overseer_proto::slot_name(run.slot),
        "start": hhmm(start),
        "end": hhmm(end),
        "started": run.started,
        "ended": run.ended,
        "logs": logs.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(),
    })
    .to_string()
}

/// The JSON line `slot_runs` gets for one transcript's stretch of work: a
/// slot's real activity, which a `/clear` splits where the run does not.
pub fn stretch_line(s: &crate::stretches::Stretch) -> String {
    let (sd, start) = local_minutes(s.first);
    let (_, end) = local_minutes(s.last);
    serde_json::json!({
        "kind": "activity",
        "date": ymd(&sd),
        "slot": overseer_proto::slot_name(s.slot),
        "session": s.session,
        "start": hhmm(start),
        "end": hhmm(end),
        "started": s.first,
        "ended": s.last,
    })
    .to_string()
}

/// What a slot cleared after a stretch of work is told: the work from here
/// is a new stretch and needs a log of its own (#80).
pub fn cleared_text(s: &crate::stretches::Stretch) -> String {
    let (_, start) = local_minutes(s.first);
    let (_, end) = local_minutes(s.last);
    let name = overseer_proto::slot_name(s.slot);
    format!(
        "{name} was cleared after work from {} to {}. What you do from here is a new stretch: \
         when you wrap it, write a new session log for it (a part log, named like \
         Sessions/<date>-{}-<topic>-part2.md) with its own **Time** line. Leave the Time lines \
         of earlier logs as they are.",
        hhmm(start),
        hhmm(end),
        name.to_lowercase()
    )
}

/// The `slot_runs` file for a run: the template filled for its start day.
pub fn runs_file(template: &str, run: &Run) -> String {
    fill(template, &local_date(run.started), run.slot)
}

/// Fills `{yyyy}`, `{mm}`, `{dd}`, `{Month}` and `{slot}` (`s1`).
fn fill(pattern: &str, d: &LocalDate, slot: SlotId) -> String {
    pattern
        .replace("{yyyy}", &format!("{:04}", d.year))
        .replace("{mm}", &format!("{:02}", d.month))
        .replace("{dd}", &format!("{:02}", d.day))
        .replace("{Month}", d.month_name)
        .replace("{slot}", &format!("s{slot}"))
}

/// The files `patterns` (vault-relative globs) match for day `d` and
/// `slot`, each once, in the order found.
fn matching(
    brain: &Path,
    home: &Path,
    patterns: &[String],
    d: &LocalDate,
    slot: SlotId,
) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    for p in patterns {
        for path in glob(&fill(p, d, slot), brain, home) {
            if !found.contains(&path) {
                found.push(path);
            }
        }
    }
    found
}

/// The logs the run wrote: files matching `patterns` (vault-relative globs)
/// for the days it ran on, changed since it started, in name order.
/// A minute's slack on the start allows for clock rounding.
pub fn find_logs(brain: &Path, home: &Path, patterns: &[String], run: &Run) -> Vec<PathBuf> {
    let mut days = vec![local_date(run.started)];
    let end = local_date(run.ended);
    if ymd(&end) != ymd(&days[0]) {
        days.push(end);
    }
    let since =
        std::time::UNIX_EPOCH + std::time::Duration::from_secs((run.started - 60).max(0) as u64);
    let mut found: Vec<PathBuf> = Vec::new();
    for d in &days {
        for path in matching(brain, home, patterns, d, run.slot) {
            let fresh = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .is_ok_and(|t| t >= since);
            if fresh && !found.contains(&path) {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// One of a slot's logs for today: its file stem and the work ranges on
/// its `**Time**` line, minutes after midnight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DayLog {
    pub name: String,
    pub ranges: Vec<(u32, Option<u32>)>,
}

/// Every log `patterns` match for `slot` on `now`'s local day, however
/// long ago it was changed, in name order.
pub fn today_logs(
    brain: &Path,
    home: &Path,
    patterns: &[String],
    slot: SlotId,
    now: i64,
) -> Vec<DayLog> {
    let mut paths = matching(brain, home, patterns, &local_date(now), slot);
    paths.sort();
    paths
        .iter()
        .filter_map(|p| {
            let text = std::fs::read_to_string(p).ok()?;
            Some(DayLog {
                name: p.file_stem()?.to_string_lossy().into_owned(),
                ranges: time_ranges(split_frontmatter(&text).1),
            })
        })
        .collect()
}

/// Unix seconds of local midnight at the start of `t`'s day.
pub fn local_day_start(t: i64) -> i64 {
    // SAFETY: localtime_r and mktime only touch the `tm` we pass them.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let time = t as libc::time_t;
    if unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
        return t - t.rem_euclid(86_400);
    }
    tm.tm_hour = 0;
    tm.tm_min = 0;
    tm.tm_sec = 0;
    tm.tm_isdst = -1;
    match unsafe { libc::mktime(&mut tm) } {
        -1 => t - t.rem_euclid(86_400),
        m => m,
    }
}

/// Unix seconds of local time `minutes` after the midnight `day_start`,
/// read off the clock so a day with a clock change is right; 1440 is the
/// next midnight.
pub fn local_minute(day_start: i64, minutes: u32) -> i64 {
    let naive = day_start + i64::from(minutes) * 60;
    // SAFETY: localtime_r and mktime only touch the `tm` we pass them.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let time = day_start as libc::time_t;
    if unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
        return naive;
    }
    tm.tm_mday += (minutes / 1440) as libc::c_int;
    tm.tm_hour = (minutes % 1440 / 60) as libc::c_int;
    tm.tm_min = (minutes % 60) as libc::c_int;
    tm.tm_sec = 0;
    tm.tm_isdst = -1;
    match unsafe { libc::mktime(&mut tm) } {
        -1 => naive,
        m => m,
    }
}

/// Reads each log found for the run.
pub fn read_logs(paths: &[PathBuf]) -> Vec<Log> {
    paths
        .iter()
        .filter_map(|p| {
            let text = std::fs::read_to_string(p).ok()?;
            Some(Log {
                name: p.file_stem()?.to_string_lossy().into_owned(),
                facts: parse(&text),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "---\noutcome: shipped\nhighlights:\n  - Added the demo endpoint\nlowlights: []\n---\n# DEMO-1 demo endpoint\n\n**Time**: 08:06 - 16:24 GMT\n";

    fn log(name: &str, text: &str) -> Log {
        Log {
            name: name.into(),
            facts: parse(text),
        }
    }

    #[test]
    fn reads_times_and_authored_fields() {
        let f = parse(GOOD);
        assert_eq!(f.start, Some(8 * 60 + 6));
        assert_eq!(f.end, Some(16 * 60 + 24));
        assert!(f.outcome && f.highlights);
        let f = parse("---\noutcome: null\nhighlights: []\n---\n- **Time**: ~9:05\n");
        assert_eq!((f.start, f.end), (Some(9 * 60 + 5), None));
        assert!(!f.outcome && !f.highlights);
        let f = parse("---\noutcome: ''\nhighlights:\n  -\n---\n**Time**: 10:00–11:30\n");
        assert_eq!(f.end, Some(11 * 60 + 30));
        assert!(!f.outcome && !f.highlights, "blank block-list item");
        let f = parse("---\noutcome: diagnosed\nhighlights: [one, two]\n---\nno header\n");
        assert_eq!((f.start, f.end), (None, None));
        assert!(f.outcome && f.highlights);
        // A key further down is not the block list's item.
        let f = parse("---\nhighlights:\nlowlights:\n  - slow\n---\n");
        assert!(!f.highlights);
        // No frontmatter at all.
        assert!(!parse("# t\n**Time**: 08:00 - 09:00\n").outcome);
    }

    #[test]
    fn every_work_range_on_the_time_line_counts() {
        let m = |h: u32, mm: u32| h * 60 + mm;
        assert_eq!(
            time_ranges("**Time**: 08:02 - 10:35, 10:46 - 12:15, 16:50 - 17:25 GMT\n"),
            [
                (m(8, 2), Some(m(10, 35))),
                (m(10, 46), Some(m(12, 15))),
                (m(16, 50), Some(m(17, 25)))
            ]
        );
        assert_eq!(
            time_ranges("- **Time**: 11:57 - 12:05 BST, plus a close-off check 15:25 - 15:35 BST"),
            [(m(11, 57), Some(m(12, 5))), (m(15, 25), Some(m(15, 35)))]
        );
        // Time away and bracketed notes are not work.
        assert_eq!(
            time_ranges("**Time**: 12:14 - 16:12 GMT, idle 13:15 - 16:00"),
            [(m(12, 14), Some(m(16, 12)))]
        );
        assert_eq!(
            time_ranges("**Time**: 13:08 - 16:45 GMT (started 28 Oct 17:30, with gaps)"),
            [(m(13, 8), Some(m(16, 45)))]
        );
        assert_eq!(time_ranges("**Time**: ~ (continued session) BST"), []);
        // A lone start counts only as the line's first time.
        assert_eq!(time_ranges("**Time**: ~9:05"), [(m(9, 5), None)]);
        assert_eq!(
            time_ranges("**Time**: 08:00 - 09:00, then 10:00"),
            [(m(8, 0), Some(m(9, 0)))]
        );
        let f = parse("**Time**: 08:02 - 10:35, 10:46 - 12:15\n");
        assert_eq!((f.start, f.end), (Some(m(8, 2)), Some(m(12, 15))));
    }

    #[test]
    fn a_good_log_covering_the_run_is_fine() {
        let logs = [log("2026-11-20-s1-demo", GOOD)];
        assert!(problems(&logs, 8 * 60 + 6, 16 * 60 + 24, true).is_empty());
        // Inside the slack either end.
        assert!(problems(&logs, 7 * 60 + 55, 16 * 60 + 35, true).is_empty());
    }

    #[test]
    fn names_what_is_missing_and_what_is_not_covered() {
        let logs = [log(
            "2026-11-20-s5-demo",
            "---\noutcome:\nhighlights: []\n---\n**Time**: 08:06 - 08:40\n",
        )];
        assert_eq!(
            problems(&logs, 8 * 60 + 6, 15 * 60 + 30, true),
            [
                "2026-11-20-s5-demo.md has no outcome or highlights",
                "the logs end at 08:40 but work went on until 15:30",
            ]
        );
        let logs = [log(
            "2026-11-20-s1-demo",
            "---\noutcome: shipped\nhighlights: [x]\n---\n",
        )];
        assert_eq!(
            problems(&logs, 8 * 60, 9 * 60, true),
            ["2026-11-20-s1-demo.md has no **Time** header"]
        );
        assert_eq!(problems(&[], 0, 60, true), ["no session log was written"]);
    }

    #[test]
    fn several_logs_cover_the_run_together() {
        let a = "---\noutcome: shipped\nhighlights: [a]\n---\n**Time**: 08:00 - 10:00\n";
        let b = "---\noutcome: shipped\nhighlights: [b]\n---\n**Time**: 10:00 - 12:00\n";
        let logs = [log("a", a), log("b", b)];
        assert!(problems(&logs, 8 * 60, 12 * 60, true).is_empty());
        let late = [log("b", b)];
        assert_eq!(
            problems(&late, 8 * 60, 12 * 60, true),
            ["the logs start at 10:00 but work started at 08:00"]
        );
        // Past midnight the minutes are not compared.
        assert!(problems(&late, 23 * 60, 30, false).is_empty());
    }

    #[test]
    fn local_day_start_is_local_midnight() {
        let t = 1_795_000_000; // a fixed moment in November 2026
        let start = local_day_start(t);
        assert!(start <= t && t - start < 25 * 3600);
        let (d, mins) = local_minutes(start);
        assert_eq!(mins, 0);
        assert_eq!(ymd(&d), ymd(&local_date(t)));
    }

    #[test]
    fn local_minute_follows_the_clock_on_every_day_of_the_year() {
        // Every day of 2026, so a clock change in the local zone is crossed.
        let mut start = local_day_start(1_767_225_600 + 12 * 3600);
        for _ in 0..366 {
            let noon = local_minute(start, 12 * 60);
            let (d, mins) = local_minutes(noon);
            assert_eq!(mins, 12 * 60, "noon on {}", ymd(&d));
            assert_eq!(ymd(&d), ymd(&local_date(start)));
            let next = local_minute(start, 24 * 60);
            assert_eq!(
                next,
                local_day_start(next),
                "next midnight after {}",
                ymd(&d)
            );
            assert!(next > start);
            start = next;
        }
    }

    #[test]
    fn today_logs_ignores_modified_time() {
        let brain = tempfile::tempdir().unwrap();
        let now = 1_795_000_000;
        let d = local_date(now);
        let dir = brain.path().join("Sessions");
        std::fs::create_dir_all(&dir).unwrap();
        let mine = dir.join(format!("{}-s1-demo.md", ymd(&d)));
        std::fs::write(&mine, "# DEMO-1\n\n**Time**: 09:00 - 10:00\n").unwrap();
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        std::fs::File::options()
            .write(true)
            .open(&mine)
            .unwrap()
            .set_modified(old)
            .unwrap();
        std::fs::write(
            dir.join(format!("{}-s2-demo.md", ymd(&d))),
            "**Time**: 11:00 - 12:00\n",
        )
        .unwrap();
        let patterns = ["Sessions/{yyyy}-{mm}-{dd}-{slot}-*.md".to_string()];
        let found = today_logs(brain.path(), brain.path(), &patterns, 1, now);
        assert_eq!(
            found,
            [DayLog {
                name: format!("{}-s1-demo", ymd(&d)),
                ranges: vec![(9 * 60, Some(10 * 60))],
            }]
        );
    }
}
