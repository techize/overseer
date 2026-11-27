//! The end-of-day draft, built from the notes folder's split script output
//! and the session log checks. The functions at the top are pure; the
//! [`EndOfDay`] service runs the scripts and saves the approved hours.

use crate::brain::paths::{fill_template, local_date};
use crate::brain::run_program;
use crate::env::BaseEnv;
use crate::session_logs::{self, hhmm, local_minutes, ymd, Run};
use crate::timeline_service::TimelineService;
use overseer_proto::{
    slot_name, DayDraft, DayProblem, DayView, EpicHours, Response, TicketMinutes,
};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long each end-of-day command may run before it is killed.
const COMMAND_LIMIT: Duration = Duration::from_secs(180);

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

/// A problem with no slot or file.
fn problem(text: String) -> DayProblem {
    DayProblem {
        text,
        slot: None,
        path: None,
    }
}

/// Quarter hours as hours, `7.50`.
fn hours_text(q: u32) -> String {
    format!("{:.2}", f64::from(q) / 4.0)
}

/// `path` with the home folder shown as `~`.
fn tilde(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if !home.as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => path.display().to_string(),
    }
}

/// `path` with a leading `~/` (or a bare `~`) as the home folder.
fn expand_home(path: &str, home: &Path) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if path == "~" => home.to_path_buf(),
        None => PathBuf::from(path),
    }
}

/// A table cell: pipes escaped, line breaks flattened.
fn cell(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}

/// The quarters chosen for `key`, 0 when it is not listed.
fn chosen(hours: &[(String, u32)], key: &str) -> u32 {
    hours
        .iter()
        .filter(|(k, _)| k == key)
        .map(|(_, q)| *q)
        .sum()
}

/// The approved day as a markdown note.
pub fn time_log_text(d: &DayDraft, hours: &[(String, u32)], approved_at: &str) -> String {
    let mut t = format!("# Time log {}\n\n## Hours\n\n", d.day);
    t.push_str("| Epic | Summary | Hours | Tickets |\n|------|---------|-------|---------|\n");
    let mut total = 0;
    for e in &d.epics {
        let q = chosen(hours, &e.key);
        if q == 0 {
            continue;
        }
        total += q;
        t.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            cell(&e.key),
            cell(&e.summary),
            hours_text(q),
            cell(&e.tickets.join(", "))
        ));
    }
    t.push_str(&format!(
        "\nTotal: {} of {} h\n",
        hours_text(total),
        hours_text(d.target_quarters)
    ));
    if let Some(fit) = &d.fit {
        t.push_str(&format!("\nFit: {fit}\n"));
    }
    t.push_str(
        "\n## Minutes per ticket\n\n| Ticket | Epic | Minutes |\n|--------|------|---------|\n",
    );
    for m in &d.tickets {
        t.push_str(&format!(
            "| {} | {} | {:.0} |\n",
            cell(&m.ticket),
            cell(m.epic.as_deref().unwrap_or("")),
            m.minutes
        ));
    }
    t.push_str("\n## Open problems\n\n");
    if d.problems.is_empty() {
        t.push_str("None.\n");
    }
    for p in &d.problems {
        t.push_str(&format!("- {}\n", p.text.replace('\n', " ")));
    }
    t.push_str(&format!("\nApproved at {approved_at}\n"));
    t
}

/// The allocation file: the script's own JSON with each epic's hours set
/// to the approved quarters, and epics at 0 left out. Everything else,
/// `derivation` included, is kept as the script wrote it.
pub fn allocation_json(raw: &Value, d: &DayDraft, hours: &[(String, u32)]) -> Value {
    let mut out = raw.clone();
    let epics: Vec<Value> = raw
        .get("epics")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let key = e.get("key").and_then(Value::as_str)?;
            d.epics.iter().any(|de| de.key == key).then_some(())?;
            let q = chosen(hours, key);
            (q > 0).then(|| {
                let mut e = e.clone();
                e["hours"] = Value::from(f64::from(q) / 4.0);
                e
            })
        })
        .collect();
    if let Some(o) = out.as_object_mut() {
        o.insert("epics".into(), Value::Array(epics));
    }
    out
}

/// The last draft whose split worked, kept for `save`.
struct Stored {
    id: u64,
    /// When the run started: the day the saved files are named for.
    ran_at: i64,
    raw: Value,
    draft: DayDraft,
}

/// Runs the end-of-day scripts in the notes folder and saves the approved
/// hours. It needs no notes writer: the note is written because the user
/// asked for it by approving.
pub struct EndOfDay {
    notes: PathBuf,
    home: PathBuf,
    patterns: Vec<String>,
    refused: Option<String>,
    index_command: Vec<String>,
    split_command: Vec<String>,
    target_arg: Vec<String>,
    time_log: String,
    allocation_file: String,
    env: BaseEnv,
    /// `None` only in tests, which build drafts without a day view.
    timeline: Option<Arc<TimelineService>>,
    /// Where the split writes its `{out}` file.
    tmp: PathBuf,
    next_id: AtomicU64,
    /// Held for a whole run, so runs queue instead of overlapping: the index
    /// script rewrites files the next run would read.
    running: tokio::sync::Mutex<()>,
    last: Mutex<Option<Stored>>,
}

impl EndOfDay {
    pub fn new(
        cfg: &crate::Config,
        home: PathBuf,
        env: BaseEnv,
        timeline: Arc<TimelineService>,
        tmp: PathBuf,
    ) -> Arc<Self> {
        Arc::new(Self::build(cfg, home, env, Some(timeline), tmp))
    }

    fn build(
        cfg: &crate::Config,
        home: PathBuf,
        env: BaseEnv,
        timeline: Option<Arc<TimelineService>>,
        tmp: PathBuf,
    ) -> Self {
        EndOfDay {
            notes: cfg.brain_dir.clone(),
            home,
            patterns: cfg.session_logs.clone(),
            refused: cfg.notes_refused.clone(),
            index_command: cfg.eod_index_command.clone(),
            split_command: cfg.eod_split_command.clone(),
            target_arg: cfg.eod_target_arg.clone(),
            time_log: cfg.eod_time_log.clone(),
            allocation_file: cfg.eod_allocation_file.clone(),
            env,
            timeline,
            tmp,
            // Seeded from the clock, so ids from before a restart never match.
            next_id: AtomicU64::new(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(1, |d| d.as_millis() as u64),
            ),
            running: tokio::sync::Mutex::new(()),
            last: Mutex::new(None),
        }
    }

    /// A service already holding `draft` with the split `raw` (tests).
    #[cfg(test)]
    fn with_draft(
        cfg: &crate::Config,
        home: PathBuf,
        ran_at: i64,
        raw: Value,
        draft: DayDraft,
    ) -> Self {
        let tmp = home.join("tmp");
        let eod = Self::build(cfg, home, BaseEnv::ready(Vec::new()), None, tmp);
        *eod.last.lock().unwrap() = Some(Stored {
            id: draft.id,
            ran_at,
            raw,
            draft,
        });
        eod
    }

    /// Runs the checks and the split for the local day of `now` and returns
    /// the draft. `target_quarters` is passed to the split only when
    /// `eod_target_arg` is set.
    pub async fn run(&self, now: i64, target_quarters: Option<u32>) -> DayDraft {
        let _queued = self.running.lock().await;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let day = ymd(&local_date(now));
        let (mut d, raw) = match self.off_reason() {
            Some(why) => (draft(id, &day, Err(vec![problem(why)]), vec![]), None),
            None => self.checks(id, &day, target_quarters).await,
        };
        d.target_fixed = self.target_arg.is_empty();
        for p in &mut d.problems {
            if let Some(path) = &p.path {
                if Path::new(path).is_relative() {
                    p.path = Some(self.notes.join(path).display().to_string());
                }
            }
        }
        // A failed run leaves nothing to save, so an older draft is stale too.
        *self.last.lock().unwrap() = raw.map(|raw| Stored {
            id,
            ran_at: now,
            raw,
            draft: d.clone(),
        });
        d
    }

    /// Why the end of day cannot run, if it cannot.
    fn off_reason(&self) -> Option<String> {
        if let Some(why) = &self.refused {
            return Some(format!(
                "The end of day is off because the notes folder was refused: {why}"
            ));
        }
        self.split_command
            .is_empty()
            .then(|| "The end of day is not set up: set eod_split_command".to_string())
    }

    /// The index, the split and the log checks, in that order. Also returns
    /// the split's JSON when it worked.
    async fn checks(
        &self,
        id: u64,
        day: &str,
        target_quarters: Option<u32>,
    ) -> (DayDraft, Option<Value>) {
        let mut extra = Vec::new();
        if let Some(p) = self.index().await {
            extra.push(p);
        }
        let split = self.split(id, day, target_quarters).await;
        if let Some(t) = &self.timeline {
            extra.extend(log_problems(
                &t.current(),
                &self.notes,
                &self.home,
                &self.patterns,
            ));
        }
        let raw = split.as_ref().ok().map(|s| s.raw.clone());
        (draft(id, day, split, extra), raw)
    }

    /// Runs `cmd` in the notes folder with the login-shell environment.
    async fn command(&self, cmd: &[String]) -> Result<crate::brain::RunOut, String> {
        let (program, args) = cmd.split_first().ok_or("the command is empty")?;
        let mut env = self.env.clone();
        env.wait().await;
        let vars = env.get();
        run_program(
            Path::new(program),
            args,
            &self.notes,
            COMMAND_LIMIT,
            None,
            Some(vars.as_slice()),
        )
        .await
    }

    /// Runs the index command; a problem when its file is missing or it fails.
    async fn index(&self) -> Option<DayProblem> {
        if self.index_command.is_empty() {
            return None;
        }
        let key = "eod_index_command";
        if let Some(p) = missing_script(&self.index_command, &self.notes, key) {
            return Some(p);
        }
        let why = match self.command(&self.index_command).await {
            Ok(o) if o.code == 0 => return None,
            Ok(o) => failure_text(&o.stderr).unwrap_or_else(|| format!("exit code {}", o.code)),
            Err(e) => e,
        };
        Some(problem(format!("{key} failed: {why}")))
    }

    /// Runs the split into a fresh file under `tmp` and reads it.
    async fn split(
        &self,
        id: u64,
        day: &str,
        target_quarters: Option<u32>,
    ) -> Result<Split, Vec<DayProblem>> {
        let cmd = &self.split_command;
        if let Some(p) = missing_script(cmd, &self.notes, "eod_split_command") {
            return Err(vec![p]);
        }
        std::fs::create_dir_all(&self.tmp).map_err(|e| {
            vec![problem(format!(
                "cannot make {}: {e}",
                tilde(&self.tmp, &self.home)
            ))]
        })?;
        let out = self
            .tmp
            .join(format!("eod-{}-{id}.json", std::process::id()));
        let target = target_quarters
            .filter(|_| !self.target_arg.is_empty())
            .map(|q| (self.target_arg.as_slice(), q));
        let filled = fill_command(cmd, day, &out, target);
        let ran = self.command(&filled).await;
        let read = std::fs::read_to_string(&out);
        let _ = std::fs::remove_file(&out);
        match ran {
            Err(e) => Err(vec![problem(format!("eod_split_command failed: {e}"))]),
            Ok(o) if o.code != 0 => Err(failed_split(o.code, &o.stdout, &o.stderr, &self.notes)),
            Ok(_) => {
                let json = read
                    .map_err(|e| vec![problem(format!("the split wrote no output file: {e}"))])?;
                let split = read_split(&json, &self.notes).map_err(|e| vec![problem(e)])?;
                match split.raw.get("date").and_then(Value::as_str) {
                    Some(date) if date != day => Err(vec![problem(format!(
                        "The split was for {date}, not {day}."
                    ))]),
                    _ => Ok(split),
                }
            }
        }
    }

    /// Writes draft `id` with `hours` (quarters per epic key) for the day of
    /// `now`. Nothing is written when a file exists and `replace` is false.
    pub fn save(&self, id: u64, hours: &[(String, u32)], replace: bool, now: i64) -> Response {
        let err = |message: String| Response::Error { message };
        // Held through the writes, so two saves never interleave.
        let last = self.last.lock().unwrap();
        let Some(s) = last.as_ref().filter(|s| s.id == id) else {
            return err("Run the checks again".into());
        };
        let d = &s.draft;
        for (key, _) in hours {
            if !d.epics.iter().any(|e| &e.key == key) {
                return err(format!("{key} is not an epic in the draft"));
            }
        }
        let total: u64 = hours.iter().map(|(_, q)| u64::from(*q)).sum();
        if total != u64::from(d.target_quarters) {
            return err(format!(
                "the hours add up to {} h, not the day's {} h",
                hours_text(u32::try_from(total).unwrap_or(u32::MAX)),
                hours_text(d.target_quarters)
            ));
        }
        // Named for the run's day, which the content and `--date-str` use,
        // even when the save comes after midnight.
        let note = self
            .notes
            .join(fill_template(&self.time_log, s.ran_at, 0, ""));
        let alloc = (!self.allocation_file.is_empty()).then(|| {
            expand_home(
                &fill_template(&self.allocation_file, s.ran_at, 0, ""),
                &self.home,
            )
        });
        let existing: Vec<String> = std::iter::once(&note)
            .chain(alloc.as_ref())
            .filter(|p| p.exists())
            .map(|p| tilde(p, &self.home))
            .collect();
        if !existing.is_empty() && !replace {
            return Response::DayExists { paths: existing };
        }
        let approved_at = hhmm(local_minutes(now).1);
        if let Err(e) = write_file(&note, &time_log_text(d, hours, &approved_at)) {
            return err(format!("cannot write {}: {e}", tilde(&note, &self.home)));
        }
        let allocation = match &alloc {
            Some(path) => {
                let json = allocation_json(&s.raw, d, hours);
                let text = serde_json::to_string_pretty(&json).unwrap_or_default() + "\n";
                if let Err(e) = write_file(path, &text) {
                    return err(format!("cannot write {}: {e}", tilde(path, &self.home)));
                }
                Some(tilde(path, &self.home))
            }
            None => None,
        };
        Response::DaySaved {
            time_log: tilde(&note, &self.home),
            push: allocation
                .as_ref()
                .map(|a| format!("jc push {a} --date-str {} --non-interactive", d.day)),
            allocation,
        }
    }
}

/// Writes `text` to `path`, making its folders first.
fn write_file(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, text)
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

    /// A service holding `SPLIT` as draft 7, writing under `dir`.
    fn stored(dir: &Path) -> EndOfDay {
        let cfg = crate::Config {
            brain_dir: dir.join("notes"),
            eod_allocation_file: "~/alloc/{yyyy}-{mm}-{dd}.json".into(),
            ..crate::Config::default()
        };
        let raw: Value = serde_json::from_str(SPLIT).unwrap();
        let split = read_split(SPLIT, &cfg.brain_dir).unwrap();
        let d = draft(7, "2026-01-02", Ok(split), vec![]);
        EndOfDay::with_draft(&cfg, dir.join("home"), noon(), raw, d)
    }

    fn hours(h: &[(&str, u32)]) -> Vec<(String, u32)> {
        h.iter().map(|(k, q)| (k.to_string(), *q)).collect()
    }

    /// Noon local time on 2026-01-02.
    fn noon() -> i64 {
        let start = session_logs::local_day_start(1_767_355_200);
        session_logs::local_minute(start, 12 * 60)
    }

    #[test]
    fn the_allocation_keeps_the_scripts_shape() {
        let raw: Value = serde_json::from_str(SPLIT).unwrap();
        let notes = tempfile::tempdir().unwrap();
        let d = draft(
            1,
            "2026-01-02",
            Ok(read_split(SPLIT, notes.path()).unwrap()),
            vec![],
        );
        let a = allocation_json(&raw, &d, &hours(&[("DEMO-9", 30), ("DEMO-8", 0)]));
        assert_eq!(a["derivation"], raw["derivation"]);
        assert_eq!(a["target_hours"], 7.5);
        assert_eq!(a["date"], "2026-01-02");
        let epics = a["epics"].as_array().unwrap();
        assert_eq!(epics.len(), 1);
        assert_eq!(epics[0]["key"], "DEMO-9");
        assert_eq!(epics[0]["summary"], "Demo epic");
        assert_eq!(epics[0]["hours"], 7.5);
    }

    #[test]
    fn the_time_log_lists_hours_tickets_and_problems() {
        let notes = tempfile::tempdir().unwrap();
        let s = read_split(SPLIT, notes.path()).unwrap();
        let open = vec![DayProblem {
            text: "S2 has no log for 10:00-10:30".into(),
            slot: Some(2),
            path: None,
        }];
        let d = draft(1, "2026-01-02", Ok(s), open);
        let t = time_log_text(&d, &hours(&[("DEMO-9", 21), ("DEMO-8", 9)]), "17:45");
        assert!(t.starts_with("# Time log 2026-01-02\n"), "{t}");
        assert!(
            t.contains("| DEMO-9 | Demo epic | 5.25 | DEMO-1, DEMO-2 |"),
            "{t}"
        );
        assert!(t.contains("| DEMO-8 | Demo BAU | 2.25 | DEMO-3 |"), "{t}");
        assert!(t.contains("| DEMO-1 | DEMO-9 | 90 |"), "{t}");
        assert!(t.contains("- S2 has no log for 10:00-10:30"), "{t}");
        assert!(t.trim_end().ends_with("Approved at 17:45"), "{t}");
        let none = time_log_text(
            &DayDraft {
                problems: vec![],
                ..d
            },
            &hours(&[("DEMO-9", 30)]),
            "17:45",
        );
        assert!(none.contains("None."), "{none}");
    }

    #[test]
    fn a_stale_id_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let eod = stored(dir.path());
        let r = eod.save(6, &hours(&[("DEMO-9", 20), ("DEMO-8", 10)]), false, noon());
        assert_eq!(
            r,
            Response::Error {
                message: "Run the checks again".into()
            }
        );
        assert!(!dir.path().join("notes").exists());
    }

    #[test]
    fn a_total_off_the_target_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let eod = stored(dir.path());
        let r = eod.save(7, &hours(&[("DEMO-9", 20), ("DEMO-8", 9)]), false, noon());
        assert!(
            matches!(&r, Response::Error { message } if message.contains("7.50")),
            "{r:?}"
        );
        let r = eod.save(7, &hours(&[("DEMO-9", 20), ("DEMO-7", 10)]), false, noon());
        assert!(
            matches!(&r, Response::Error { message } if message.contains("DEMO-7")),
            "{r:?}"
        );
        assert!(!dir.path().join("notes").exists());
    }

    #[test]
    fn a_total_that_overflows_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let eod = stored(dir.path());
        let big = u32::MAX - 5;
        let r = eod.save(
            7,
            &hours(&[("DEMO-9", big), ("DEMO-8", big)]),
            false,
            noon(),
        );
        assert!(
            matches!(&r, Response::Error { message } if message.contains("not the day's 7.50 h")),
            "{r:?}"
        );
        assert!(!dir.path().join("notes").exists());
    }

    #[test]
    fn an_empty_allocation_file_saves_the_note_only() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = crate::Config {
            brain_dir: dir.path().join("notes"),
            eod_allocation_file: String::new(),
            ..crate::Config::default()
        };
        let raw: Value = serde_json::from_str(SPLIT).unwrap();
        let split = read_split(SPLIT, &cfg.brain_dir).unwrap();
        let d = draft(7, "2026-01-02", Ok(split), vec![]);
        let eod = EndOfDay::with_draft(&cfg, dir.path().join("home"), noon(), raw, d);
        let r = eod.save(7, &hours(&[("DEMO-9", 20), ("DEMO-8", 10)]), false, noon());
        let note = dir
            .path()
            .join("notes/06-Daily/2026/01-January/2026-01-02-time-log.md");
        assert_eq!(
            r,
            Response::DaySaved {
                time_log: note.display().to_string(),
                allocation: None,
                push: None,
            }
        );
        assert!(note.exists());
        assert!(!dir.path().join("home/alloc").exists());
    }

    #[test]
    fn an_existing_file_needs_replace() {
        let dir = tempfile::tempdir().unwrap();
        let eod = stored(dir.path());
        let alloc = dir.path().join("home/alloc/2026-01-02.json");
        std::fs::create_dir_all(alloc.parent().unwrap()).unwrap();
        std::fs::write(&alloc, "pushed").unwrap();
        let h = hours(&[("DEMO-9", 20), ("DEMO-8", 10)]);
        let r = eod.save(7, &h, false, noon());
        assert_eq!(
            r,
            Response::DayExists {
                paths: vec!["~/alloc/2026-01-02.json".into()]
            }
        );
        assert_eq!(std::fs::read_to_string(&alloc).unwrap(), "pushed");
        let note = dir
            .path()
            .join("notes/06-Daily/2026/01-January/2026-01-02-time-log.md");
        assert!(!note.exists());
        let r = eod.save(7, &h, true, noon());
        assert_eq!(
            r,
            Response::DaySaved {
                time_log: note.display().to_string(),
                allocation: Some("~/alloc/2026-01-02.json".into()),
                push: Some(
                    "jc push ~/alloc/2026-01-02.json --date-str 2026-01-02 --non-interactive"
                        .into()
                ),
            }
        );
        let a: Value = serde_json::from_str(&std::fs::read_to_string(&alloc).unwrap()).unwrap();
        assert_eq!(a["epics"][0]["hours"], 5.0);
        assert!(std::fs::read_to_string(&note)
            .unwrap()
            .contains("Approved at 12:00"));
    }

    /// A service running demo scripts from `notes/Claude/tools`.
    fn running(notes: &Path, target_arg: &[&str]) -> EndOfDay {
        let cfg = crate::Config {
            brain_dir: notes.to_path_buf(),
            eod_index_command: strs(&["sh", "Claude/tools/demo-index.sh"]),
            eod_split_command: strs(&["sh", "Claude/tools/demo-split.sh", "--out", "{out}"]),
            eod_target_arg: strs(target_arg),
            ..crate::Config::default()
        };
        let mut eod = EndOfDay::build(
            &cfg,
            notes.join("home"),
            BaseEnv::ready(crate::env::own_env()),
            None,
            notes.join("tmp"),
        );
        eod.patterns.clear();
        eod
    }

    #[tokio::test]
    async fn a_run_passes_the_target_and_reports_a_failed_index() {
        let notes = tempfile::tempdir().unwrap();
        let tools = notes.path().join("Claude/tools");
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::create_dir(notes.path().join("Sessions")).unwrap();
        std::fs::write(notes.path().join("Sessions/demo-slug.md"), "x").unwrap();
        std::fs::write(
            tools.join("demo-index.sh"),
            "echo FATAL demo index >&2\nexit 1\n",
        )
        .unwrap();
        // Writes the target it was given (or 7.5) as the split's target.
        std::fs::write(
            tools.join("demo-split.sh"),
            r#"out=""; target=7.5
while [ $# -gt 0 ]; do
  case "$1" in --out) out="$2";; --target) target="$2";; esac
  shift
done
printf '{"target_hours": %s, "epics": [], "derivation": {"unknown_times": ["demo-slug"]}}' "$target" > "$out"
"#,
        )
        .unwrap();
        let eod = running(notes.path(), &["--target", "{hours}"]);
        let d = eod.run(noon(), Some(36)).await;
        assert!(d.split_ok, "{:?}", d.problems);
        assert_eq!(d.target_quarters, 36);
        assert!(!d.target_fixed);
        assert_eq!(
            d.problems[0].text,
            "eod_index_command failed: FATAL demo index"
        );
        let log = notes.path().join("Sessions/demo-slug.md");
        assert_eq!(d.problems[1].path, Some(log.display().to_string()));
        assert!(std::fs::read_dir(notes.path().join("tmp"))
            .unwrap()
            .next()
            .is_none());
        // With no target argument the target is fixed and not passed.
        let fixed = running(notes.path(), &[]);
        let d = fixed.run(noon(), Some(36)).await;
        assert_eq!(d.target_quarters, 30);
        assert!(d.target_fixed);
    }

    #[tokio::test]
    async fn a_split_for_another_day_is_a_failed_split() {
        let notes = tempfile::tempdir().unwrap();
        let tools = notes.path().join("Claude/tools");
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::write(tools.join("demo-index.sh"), "exit 0\n").unwrap();
        std::fs::write(
            tools.join("demo-split.sh"),
            r#"out="$2"
printf '{"date": "2025-12-31", "target_hours": 7.5, "epics": []}' > "$out"
"#,
        )
        .unwrap();
        let eod = running(notes.path(), &[]);
        let d = eod.run(noon(), None).await;
        assert!(!d.split_ok);
        assert_eq!(d.problems.len(), 1);
        assert_eq!(
            d.problems[0].text,
            "The split was for 2025-12-31, not 2026-01-02."
        );
    }

    #[tokio::test]
    async fn runs_queue_instead_of_overlapping() {
        let notes = tempfile::tempdir().unwrap();
        let tools = notes.path().join("Claude/tools");
        std::fs::create_dir_all(&tools).unwrap();
        let marks = notes.path().join("marks.txt");
        // The index script marks its start, waits, then marks its end.
        std::fs::write(
            tools.join("demo-index.sh"),
            "echo start >> marks.txt\nsleep 0.3\necho end >> marks.txt\n",
        )
        .unwrap();
        std::fs::write(
            tools.join("demo-split.sh"),
            r#"out="$2"
printf '{"target_hours": 7.5, "epics": []}' > "$out"
"#,
        )
        .unwrap();
        let eod = running(notes.path(), &[]);
        let (a, b) = tokio::join!(eod.run(noon(), None), eod.run(noon(), None));
        assert!(a.split_ok && b.split_ok);
        assert_ne!(a.id, b.id);
        let text = std::fs::read_to_string(marks).unwrap();
        assert_eq!(text, "start\nend\nstart\nend\n");
    }

    #[tokio::test]
    async fn an_unset_split_command_says_so() {
        let notes = tempfile::tempdir().unwrap();
        let mut eod = running(notes.path(), &[]);
        eod.split_command.clear();
        let d = eod.run(noon(), None).await;
        assert!(!d.split_ok);
        assert_eq!(d.problems.len(), 1);
        assert!(d.problems[0].text.contains("eod_split_command"));
        let r = eod.save(d.id, &[], false, noon());
        assert!(matches!(r, Response::Error { .. }));
    }

    #[test]
    fn a_save_after_midnight_names_the_files_for_the_runs_day() {
        let dir = tempfile::tempdir().unwrap();
        let eod = stored(dir.path());
        let next_day = noon() + 13 * 3600;
        let h = hours(&[("DEMO-9", 20), ("DEMO-8", 10)]);
        let r = eod.save(7, &h, false, next_day);
        let note = dir
            .path()
            .join("notes/06-Daily/2026/01-January/2026-01-02-time-log.md");
        assert_eq!(
            r,
            Response::DaySaved {
                time_log: note.display().to_string(),
                allocation: Some("~/alloc/2026-01-02.json".into()),
                push: Some(
                    "jc push ~/alloc/2026-01-02.json --date-str 2026-01-02 --non-interactive"
                        .into()
                ),
            }
        );
        assert!(dir.path().join("home/alloc/2026-01-02.json").exists());
        assert!(!dir.path().join("home/alloc/2026-01-03.json").exists());
        assert!(std::fs::read_to_string(&note)
            .unwrap()
            .contains("Approved at 01:00"));
    }
}
