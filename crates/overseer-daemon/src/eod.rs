//! The end-of-day draft, built from the notes folder's split script output
//! and the session log checks. Pure: nothing here runs a command.

use crate::session_logs::{self, hhmm, local_minutes, ymd, Run};
use overseer_proto::{slot_name, DayDraft, DayProblem, DayView, EpicHours, TicketMinutes};
use serde_json::Value;
use std::path::Path;

/// What the split script worked out for the day.
#[derive(Debug, Clone)]
pub struct Split {
    /// The whole JSON object as the script wrote it.
    pub raw: Value,
    pub target_quarters: u32,
    pub epics: Vec<EpicHours>,
    pub tickets: Vec<TicketMinutes>,
    pub fit: Option<String>,
    pub problems: Vec<DayProblem>,
}

/// Hours as whole quarter hours.
fn quarters(hours: f64) -> u32 {
    (hours * 4.0).round().max(0.0) as u32
}

/// A problem about a session slug, linked to its log when the file exists.
fn slug_problem(text: String, slug: &str, notes: &Path) -> DayProblem {
    let rel = format!("Sessions/{slug}.md");
    let path = (!slug.is_empty() && notes.join(&rel).is_file()).then_some(rel);
    DayProblem {
        text,
        slot: None,
        path,
    }
}

fn strings<'a>(v: &'a Value, key: &str) -> Vec<&'a str> {
    v.get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

/// The tickets' minutes, each with the epic it rolls up to.
fn ticket_minutes(derivation: &Value) -> Vec<TicketMinutes> {
    let minutes = derivation.get("ticket_minutes").and_then(Value::as_object);
    let epic_of = derivation.get("ticket_epic").and_then(Value::as_object);
    minutes
        .into_iter()
        .flatten()
        .filter_map(|(t, m)| {
            Some(TicketMinutes {
                ticket: t.clone(),
                minutes: m.as_f64()?,
                epic: epic_of
                    .and_then(|e| e.get(t))
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect()
}

/// Reads the JSON the split script wrote.
pub fn read_split(json: &str, notes: &Path) -> Result<Split, String> {
    let raw: Value =
        serde_json::from_str(json).map_err(|e| format!("the split is not JSON: {e}"))?;
    let target = raw
        .get("target_hours")
        .and_then(Value::as_f64)
        .ok_or("the split has no target_hours")?;
    let derivation = raw.get("derivation").unwrap_or(&Value::Null);
    let tickets = ticket_minutes(derivation);
    let mut epics = Vec::new();
    for e in raw
        .get("epics")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let key = e
            .get("key")
            .and_then(Value::as_str)
            .ok_or("an epic in the split has no key")?;
        let hours = e.get("hours").and_then(Value::as_f64).unwrap_or(0.0);
        let mine: Vec<&TicketMinutes> = tickets
            .iter()
            .filter(|t| t.epic.as_deref() == Some(key))
            .collect();
        epics.push(EpicHours {
            key: key.to_string(),
            summary: e
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            quarters: quarters(hours),
            derived_hours: mine.iter().map(|t| t.minutes).sum::<f64>() / 60.0,
            tickets: mine.iter().map(|t| t.ticket.clone()).collect(),
        });
    }
    let mut problems = Vec::new();
    for slug in strings(derivation, "unknown_times") {
        let text = format!("{slug} has no start or end time");
        problems.push(slug_problem(text, slug, notes));
    }
    for entry in strings(derivation, "bad_times") {
        let slug = entry.split(" (").next().unwrap_or(entry);
        let text = format!("{entry} has times that do not work");
        problems.push(slug_problem(text, slug, notes));
    }
    Ok(Split {
        target_quarters: quarters(target),
        epics,
        tickets,
        fit: derivation
            .get("fit")
            .and_then(Value::as_str)
            .map(str::to_string),
        problems,
        raw,
    })
}

/// The line that says why a command failed: its `FATAL` line, else the last
/// 5 lines of stderr. `None` when stderr is empty.
pub fn failure_text(stderr: &str) -> Option<String> {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect();
    if let Some(fatal) = lines.iter().find(|l| l.starts_with("FATAL")) {
        return Some(fatal.to_string());
    }
    let from = lines.len().saturating_sub(5);
    (!lines.is_empty()).then(|| lines[from..].join("\n"))
}

/// The problems when the split script exited non-zero: why it failed, then
/// each `note:` line it printed.
pub fn failed_split(code: i32, stdout: &str, stderr: &str, notes: &Path) -> Vec<DayProblem> {
    let text = failure_text(stderr)
        .unwrap_or_else(|| format!("the split script failed with exit code {code}"));
    let mut out = vec![DayProblem {
        text,
        slot: None,
        path: None,
    }];
    for note in stdout.lines().filter_map(|l| l.strip_prefix("note: ")) {
        let slug = note.split_whitespace().next().unwrap_or_default();
        out.push(slug_problem(note.to_string(), slug, notes));
    }
    out
}

/// A problem when the command's script is a relative file that is not in
/// the notes folder. `key` is the config key to name. Arguments holding a
/// `{placeholder}` are values, not scripts, and are skipped.
pub fn missing_script(cmd: &[String], notes: &Path, key: &str) -> Option<DayProblem> {
    let file = cmd
        .iter()
        .skip(1)
        .find(|a| !a.starts_with('-') && !a.contains('{'))?;
    let path = Path::new(file);
    (path.is_relative() && !notes.join(path).exists()).then(|| DayProblem {
        text: format!("{file} is missing in the notes folder; set {key}"),
        slot: None,
        path: None,
    })
}

/// The command with `{date}` and `{out}` filled, then the target argument
/// (its `{hours}` filled) when the day's target was set.
pub fn fill_command(
    cmd: &[String],
    date: &str,
    out: &Path,
    target: Option<(&[String], u32)>,
) -> Vec<String> {
    let out = out.to_string_lossy();
    let mut filled: Vec<String> = cmd
        .iter()
        .map(|a| a.replace("{date}", date).replace("{out}", &out))
        .collect();
    if let Some((arg, q)) = target {
        let hours = (f64::from(q) / 4.0).to_string();
        filled.extend(arg.iter().map(|a| a.replace("{hours}", &hours)));
    }
    filled
}

/// The session log problems for every stretch of the day, and its gap spans.
pub fn log_problems(
    day: &DayView,
    notes: &Path,
    home: &Path,
    patterns: &[String],
) -> Vec<DayProblem> {
    let mut out = Vec::new();
    for row in &day.rows {
        let name = slot_name(row.slot);
        for s in &row.stretches {
            let run = Run {
                slot: row.slot,
                started: s.first,
                ended: s.last,
                worked_until: None,
            };
            let paths = session_logs::find_logs(notes, home, patterns, &run);
            let logs = session_logs::read_logs(&paths);
            let (sd, start) = local_minutes(run.started);
            let (ed, end) = local_minutes(run.ended);
            let path = paths.first().map(|p| p.display().to_string());
            for p in session_logs::problems(&logs, start, end, ymd(&sd) == ymd(&ed)) {
                out.push(DayProblem {
                    text: format!("{name}: {p}"),
                    slot: Some(row.slot),
                    path: path.clone(),
                });
            }
        }
        for g in day.gaps(row.slot) {
            out.push(DayProblem {
                text: format!(
                    "{name} has no log for {}-{}",
                    hhmm(local_minutes(g.from).1),
                    hhmm(local_minutes(g.to).1)
                ),
                slot: Some(row.slot),
                path: None,
            });
        }
    }
    out
}

/// The draft: `extra` problems (index and log checks) first, then the
/// split's own. A failed split gives no hours.
pub fn draft(
    id: u64,
    day: &str,
    split: Result<Split, Vec<DayProblem>>,
    mut extra: Vec<DayProblem>,
) -> DayDraft {
    let base = DayDraft {
        id,
        day: day.to_string(),
        ..DayDraft::default()
    };
    match split {
        Ok(s) => {
            extra.extend(s.problems);
            DayDraft {
                target_quarters: s.target_quarters,
                split_ok: true,
                problems: extra,
                tickets: s.tickets,
                epics: s.epics,
                fit: s.fit,
                ..base
            }
        }
        Err(p) => {
            extra.extend(p);
            DayDraft {
                problems: extra,
                ..base
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use overseer_proto::{DayRow, DayStretch, Span, SpanKind};

    const SPLIT: &str = r#"{
      "date": "2026-01-02", "target_hours": 7.5,
      "epics": [
        {"key": "DEMO-9", "summary": "Demo epic", "hours": 5.0},
        {"key": "DEMO-8", "summary": "Demo BAU", "hours": 2.5}
      ],
      "derivation": {
        "sessions": ["demo-a"], "unknown_times": [], "bad_times": [],
        "raw_minutes": {"DEMO-1": 100.0},
        "ticket_minutes": {"DEMO-1": 90.0, "DEMO-2": 30.0, "DEMO-3": 60.0},
        "ticket_epic": {"DEMO-1": "DEMO-9", "DEMO-2": "DEMO-9", "DEMO-3": "DEMO-8"},
        "fit": "gap 0.50h to BAU DEMO-8"
      }
    }"#;

    fn strs(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_clean_split_reads_epics_and_tickets() {
        let notes = tempfile::tempdir().unwrap();
        let s = read_split(SPLIT, notes.path()).unwrap();
        assert_eq!(s.target_quarters, 30);
        assert_eq!(s.epics.len(), 2);
        assert_eq!(
            (s.epics[0].key.as_str(), s.epics[0].quarters),
            ("DEMO-9", 20)
        );
        assert_eq!(s.epics[0].derived_hours, 2.0);
        assert_eq!(s.epics[0].tickets, vec!["DEMO-1", "DEMO-2"]);
        assert_eq!(s.epics[1].derived_hours, 1.0);
        assert_eq!(s.tickets.len(), 3);
        assert_eq!(s.fit.as_deref(), Some("gap 0.50h to BAU DEMO-8"));
        assert!(s.problems.is_empty());
        assert_eq!(s.raw["derivation"]["raw_minutes"]["DEMO-1"], 100.0);
    }

    #[test]
    fn unknown_and_bad_times_link_to_their_logs() {
        let notes = tempfile::tempdir().unwrap();
        std::fs::create_dir(notes.path().join("Sessions")).unwrap();
        std::fs::write(notes.path().join("Sessions/demo-slug.md"), "x").unwrap();
        let json = SPLIT.replace(
            r#""unknown_times": [], "bad_times": []"#,
            r#""unknown_times": ["demo-slug", "demo-none"], "bad_times": ["demo-slug (10:00-09:00)"]"#,
        );
        let s = read_split(&json, notes.path()).unwrap();
        assert_eq!(s.problems.len(), 3);
        assert_eq!(s.problems[0].path.as_deref(), Some("Sessions/demo-slug.md"));
        assert_eq!(s.problems[1].path, None);
        assert_eq!(s.problems[2].path.as_deref(), Some("Sessions/demo-slug.md"));
        assert!(s.problems[2].text.contains("demo-slug (10:00-09:00)"));
    }

    #[test]
    fn a_fatal_split_gives_problems_and_no_hours() {
        let notes = tempfile::tempdir().unwrap();
        let p = failed_split(
            1,
            "note: demo-a has no start/end, contributes nothing; cover it in the BAU gap\n",
            "FATAL DEMO-9 has no epic\n",
            notes.path(),
        );
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].text, "FATAL DEMO-9 has no epic");
        assert!(p[1].text.starts_with("demo-a has no start/end"));
        let d = draft(4, "2026-01-02", Err(p), vec![]);
        assert!(!d.split_ok && d.epics.is_empty() && d.tickets.is_empty());
        assert_eq!(d.problems.len(), 2);
    }

    #[test]
    fn stderr_without_fatal_gives_the_last_five_lines() {
        let err = "1\n2\n3\n4\n5\n6\n7\n";
        assert_eq!(failure_text(err).as_deref(), Some("3\n4\n5\n6\n7"));
        assert_eq!(failure_text("  \n"), None);
        let p = failed_split(2, "", "", Path::new("/nonexistent"));
        assert!(p[0].text.contains("exit code 2"));
    }

    #[test]
    fn a_missing_script_is_a_problem() {
        let notes = tempfile::tempdir().unwrap();
        let cmd = strs(&["python3", "Claude/tools/day-minutes.py", "--out", "{out}"]);
        let p = missing_script(&cmd, notes.path(), "eod_split_command").unwrap();
        assert_eq!(
            p.text,
            "Claude/tools/day-minutes.py is missing in the notes folder; set eod_split_command"
        );
        std::fs::create_dir_all(notes.path().join("Claude/tools")).unwrap();
        std::fs::write(notes.path().join("Claude/tools/day-minutes.py"), "").unwrap();
        assert!(missing_script(&cmd, notes.path(), "eod_split_command").is_none());
        let bare = strs(&["demo-split"]);
        assert!(missing_script(&bare, notes.path(), "eod_split_command").is_none());
    }

    #[test]
    fn commands_fill_date_and_out() {
        let cmd = strs(&["demo", "--date", "{date}", "--out", "{out}"]);
        let out = Path::new("/tmp/x.json");
        assert_eq!(
            fill_command(&cmd, "2026-01-02", out, None),
            strs(&["demo", "--date", "2026-01-02", "--out", "/tmp/x.json"])
        );
        let arg = strs(&["--target", "{hours}"]);
        let nine = fill_command(&cmd, "2026-01-02", out, Some((&arg, 36)));
        assert_eq!(&nine[nine.len() - 2..], &strs(&["--target", "9"])[..]);
        let q = fill_command(&cmd, "2026-01-02", out, Some((&arg, 35)));
        assert_eq!(q.last().unwrap(), "8.75");
        let h = fill_command(&cmd, "2026-01-02", out, Some((&arg, 30)));
        assert_eq!(h.last().unwrap(), "7.5");
    }

    #[test]
    fn log_problems_name_the_slot_and_the_gaps() {
        let notes = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let start = session_logs::local_day_start(1_767_355_200);
        let at = |m: u32| session_logs::local_minute(start, m);
        let day = DayView {
            day: "2026-01-02".into(),
            start,
            now: at(1000),
            rows: vec![DayRow {
                slot: 2,
                stretches: vec![DayStretch {
                    session: "demo".into(),
                    first: at(540),
                    last: at(660),
                    running: false,
                    spans: vec![Span {
                        from: at(600),
                        to: at(630),
                        kind: SpanKind::Gap,
                    }],
                    logs: vec![],
                }],
            }],
        };
        let p = log_problems(
            &day,
            notes.path(),
            home.path(),
            &strs(&["Sessions/*-s{slot}.md"]),
        );
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].text, "S2: no session log was written");
        assert_eq!(p[0].slot, Some(2));
        assert_eq!(p[1].text, "S2 has no log for 10:00-10:30");
        assert_eq!(p[1].slot, Some(2));
    }
}
