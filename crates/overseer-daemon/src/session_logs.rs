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

/// The same `**Time**` shapes the session index reads: an optional list
/// marker, an optional `~`, and an optional end after `-`, `–` or `—`.
fn time_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?m)^\s*(?:[-*]\s+)?\*\*Time\*\*:\s*~?\s*(\d{1,2}):(\d{2})(?:\s*[-–—]\s*~?\s*(\d{1,2}):(\d{2}))?",
        )
        .expect("time regex")
    })
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
    let (start, end) = match time_re().captures(body) {
        Some(c) => (
            minutes(&c[1], &c[2]),
            c.get(3)
                .zip(c.get(4))
                .and_then(|(h, m)| minutes(h.as_str(), m.as_str())),
        ),
        None => (None, None),
    };
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

fn ymd(d: &LocalDate) -> String {
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
                    "the logs start at {} but the session started at {}",
                    hhmm(first),
                    hhmm(start)
                ));
            }
        }
        if let Some(last) = last {
            if last + SLACK_MINS < end {
                out.push(format!(
                    "the logs end at {} but the session ran until {}",
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
        for p in patterns {
            for path in glob(&fill(p, d, run.slot), brain, home) {
                let fresh = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .is_ok_and(|t| t >= since);
                if fresh && !found.contains(&path) {
                    found.push(path);
                }
            }
        }
    }
    found.sort();
    found
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
                "the logs end at 08:40 but the session ran until 15:30",
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
            ["the logs start at 10:00 but the session started at 08:00"]
        );
        // Past midnight the minutes are not compared.
        assert!(problems(&late, 23 * 60, 30, false).is_empty());
    }
}
