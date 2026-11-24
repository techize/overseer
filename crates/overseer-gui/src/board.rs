//! The board: header, coordinator panel with the S0 card, and a 3 by 2 grid
//! of S1 to S6 cards (layout A from the design).
//!
//! The card contents are worked out by `card_model`, a pure function, so the
//! badge text, elapsed time and empty, running and exited cards are tested
//! without a window.
//!
//! A running claude slot also carries its hook-reported activity: the badge
//! then says Working, Needs you or Your turn with the time in that state, and a
//! line under the cwd says what it is doing or why it needs you. A running
//! slot without activity (any other command) keeps the plain Running badge,
//! with its command (`htop`) on the line under the cwd.
//!
//! Each card also shows the work its slot owns as pills under the title,
//! and the header ends with the state of the work sources (`source_line`).

use std::path::Path;

use gpui::{div, prelude::*, px, rgb, Context, FontWeight, Window};
use overseer_proto::{
    slot_name, Activity, ActivityKind, BrainView, ClaimKind, CoordView, ItemKind, LockView,
    PlanState, QueueEntry, QueueState, RelayState, Sender, SlotId, SlotInfo, SlotState,
    SourceStatus, WorkItem, WorkView, PRS_OFF,
};

use crate::app::{colors, OverseerApp};

// ---------------------------------------------------------------------------
// Pure logic
// ---------------------------------------------------------------------------

/// Colour family of a card's state badge and left edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Running with no activity reported (not a claude slot).
    Running,
    Working,
    NeedsYou,
    /// Running and waiting for the next prompt.
    YourTurn,
    Idle,
    /// Queued behind another slot's lock and not working.
    Blocked,
    Exited,
    /// Was running when the daemon stopped; can be resumed.
    Resumable,
    Empty,
}

/// What one card shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardModel {
    /// `S1`, or `S1 · label`.
    pub title: String,
    pub badge: String,
    pub tone: Tone,
    pub cwd: Option<String>,
    /// `pid 1234` while running.
    pub pid: Option<String>,
    /// The tool while working, or the reason while it needs you.
    pub now_doing: Option<String>,
    /// Shows a Resume button.
    pub can_resume: bool,
    /// Keys of the work the slot owns; see `pills`.
    pub pills: Vec<String>,
    /// The targets the slot holds, see `lock_pills`.
    pub locks: Vec<String>,
    /// `waiting on repo:demo-api:main and 1 more`.
    pub waiting_on: Option<String>,
    /// `2 relays pending`.
    pub pending: Option<String>,
}

/// The items `slot` owns: tickets first, then beads, each in view order.
pub fn owned_by(view: &WorkView, slot: SlotId) -> Vec<&WorkItem> {
    let mine = |kind: ItemKind| {
        view.items
            .iter()
            .filter(move |i| i.owner == Some(slot) && i.kind == kind)
    };
    mine(ItemKind::Ticket).chain(mine(ItemKind::Bead)).collect()
}

/// Most pills a card shows before the rest become `+N`.
const MAX_PILLS: usize = 3;

/// The pill text for `items` (from `owned_by`): each ticket and each bead
/// whose ticket is not among them. More than three show as the first three
/// and `+N`.
pub fn pills(items: &[&WorkItem]) -> Vec<String> {
    let under_owned = |i: &WorkItem| {
        i.kind == ItemKind::Bead
            && i.parent.as_ref().is_some_and(|p| {
                items
                    .iter()
                    .any(|t| t.kind == ItemKind::Ticket && &t.key == p)
            })
    };
    let mut keys: Vec<String> = items
        .iter()
        .filter(|i| !under_owned(i))
        .map(|i| i.key.clone())
        .collect();
    if keys.len() > MAX_PILLS {
        let rest = keys.len() - MAX_PILLS;
        keys.truncate(MAX_PILLS);
        keys.push(format!("+{rest}"));
    }
    keys
}

/// The sprint text shown muted after `Overseer`: name and day, or `None`.
pub fn header_title(work: &WorkView) -> Option<String> {
    work.sprint.clone().filter(|s| !s.is_empty())
}

/// The sources the header shows, Jira first. The PR source is off by
/// default; the header does not mention it then.
fn shown_sources(view: &WorkView) -> Vec<&SourceStatus> {
    let mut sources: Vec<_> = view
        .sources
        .iter()
        .filter(|s| s.ok || s.message.as_deref() != Some(PRS_OFF))
        .collect();
    sources.sort_by_key(|s| s.name != "jira");
    sources
}

/// The header's source health, each source with the time of its last good
/// read through `clock`: `jira 12 14:02 · beads 133 14:05`. A source that
/// failed shows why and, when it read before, since when its items are
/// from: `jira: acli not found (last 13:40)`. A good read with a warning
/// (such as Jira returning no tickets, so the last list is kept) shows it
/// after the time. Each source has its own time, so a stale one cannot
/// hide behind a fresh one.
pub fn source_line(view: &WorkView, clock: impl Fn(i64) -> String) -> String {
    shown_sources(view)
        .iter()
        .map(|s| {
            if s.ok {
                let mut part = format!("{} {}", s.name, s.count);
                if let Some(at) = s.at {
                    part.push_str(&format!(" {}", clock(at)));
                }
                if let Some(msg) = s.message.as_deref().filter(|m| !m.is_empty()) {
                    part.push_str(&format!(": {}", short_message(msg)));
                }
                part
            } else {
                let msg = s.message.as_deref().unwrap_or("failed");
                let mut part = format!("{}: {}", s.name, short_message(msg));
                if let Some(at) = s.at {
                    part.push_str(&format!(" (last {})", clock(at)));
                }
                part
            }
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Whether a source the header shows failed its last read, so the header
/// marks the line and a stale column is visible rather than silent.
pub fn sources_failing(view: &WorkView) -> bool {
    shown_sources(view).iter().any(|s| !s.ok)
}

/// The header's Refresh: reads every work source now. It reads
/// `Refreshing...` and ignores clicks until the daemon answers.
fn refresh_button(busy: bool, cx: &mut Context<OverseerApp>) -> impl IntoElement {
    div()
        .id("work-refresh")
        .px_2()
        .rounded(px(4.))
        .border_1()
        .border_color(rgb(colors::BUTTON_BORDER))
        .bg(rgb(colors::BUTTON))
        .text_xs()
        .text_color(rgb(if busy { colors::MUTED } else { colors::TEXT }))
        .when(!busy, |b| {
            b.cursor_pointer()
                .hover(|s| s.bg(rgb(colors::CARD_HOVER)))
                .on_click(cx.listener(|this, _, _, cx| this.refresh_work(cx)))
        })
        .child(if busy { "Refreshing..." } else { "Refresh" })
}

/// Longest source error the header shows before cutting it.
const MAX_SOURCE_MESSAGE: usize = 40;

/// `msg` cut to 40 characters with an ellipsis, so a long source error
/// does not crowd the header.
pub fn short_message(msg: &str) -> String {
    match msg.char_indices().nth(MAX_SOURCE_MESSAGE) {
        Some((cut, _)) => format!("{}…", &msg[..cut]),
        None => msg.to_string(),
    }
}

pub use overseer_proto::local_hhmm;

/// `45s`, `8m 05s`, `2h 03m`. Negative input (clock skew) shows `0s`.
pub fn format_elapsed(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    }
}

/// `Working 3m 05s`: the activity and how long it has been in it.
pub fn activity_state(a: &Activity, now: i64) -> String {
    let word = match a.kind {
        ActivityKind::Working => "Working",
        ActivityKind::NeedsYou => "Needs you",
        ActivityKind::Idle => "Your turn",
    };
    format!("{word} {}", format_elapsed(now - a.since))
}

/// The line that says what the session is up to: the tool while working,
/// the reason while it needs you, nothing while idle (the last tool is
/// stale by then).
pub fn activity_detail(a: &Activity) -> Option<String> {
    let text = match a.kind {
        ActivityKind::Working => a.now_doing.as_deref(),
        ActivityKind::NeedsYou => a.reason.as_deref(),
        ActivityKind::Idle => None,
    };
    text.map(str::trim)
        .filter(|t| !t.is_empty())
        .map(String::from)
}

/// `path` with the home directory shown as `~`.
pub fn tilde(path: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home {
        if let Ok(rest) = path.strip_prefix(home) {
            return if rest.as_os_str().is_empty() {
                "~".into()
            } else {
                format!("~/{}", rest.display())
            };
        }
    }
    path.display().to_string()
}

/// `S1` or `S1 · label`.
pub fn slot_title(info: &SlotInfo) -> String {
    let name = slot_name(info.id);
    if info.label.trim().is_empty() || info.state == SlotState::Empty {
        name
    } else {
        format!("{name} · {}", info.label.trim())
    }
}

/// The detail line for a running slot without activity, such as `htop`: the
/// program's file name and its arguments, cut to 80 characters.
fn command_line(command: &[String]) -> Option<String> {
    let (program, args) = command.split_first()?;
    let program = Path::new(program)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(program);
    let line = std::iter::once(program)
        .chain(args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    Some(match line.char_indices().nth(80) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_string(),
    })
}

/// The card for `info` at unix time `now`, with the pills of the work it
/// owns in `work`.
pub fn card_model(
    info: &SlotInfo,
    now: i64,
    home: Option<&Path>,
    work: &WorkView,
    locks: &LockView,
) -> CardModel {
    let cwd = info.cwd.as_deref().map(|p| tilde(p, home));
    let title = slot_title(info);
    let pills = pills(&owned_by(work, info.id));
    let waits = locks.waiting_on(info.id);
    let waiting_on = waiting_label(&waits);
    let lock_pills = lock_pills(locks, info.id, now);
    let pending = pending_label(locks.pending_for(info.id));
    match &info.state {
        SlotState::Empty => CardModel {
            title,
            badge: "+ start".into(),
            tone: Tone::Empty,
            cwd: None,
            pid: None,
            now_doing: None,
            can_resume: false,
            pills,
            locks: lock_pills,
            waiting_on,
            pending,
        },
        SlotState::Running { pid, since } => {
            let (mut badge, mut tone, now_doing) = match &info.activity {
                None => (
                    format!("● Running {}", format_elapsed(now - since)),
                    Tone::Running,
                    info.command.as_deref().and_then(command_line),
                ),
                Some(a) => (
                    format!(
                        "{} {}",
                        if a.kind == ActivityKind::Idle {
                            "○"
                        } else {
                            "●"
                        },
                        activity_state(a, now)
                    ),
                    match a.kind {
                        ActivityKind::Working => Tone::Working,
                        ActivityKind::NeedsYou => Tone::NeedsYou,
                        ActivityKind::Idle => Tone::YourTurn,
                    },
                    activity_detail(a),
                ),
            };
            let working = info
                .activity
                .as_ref()
                .is_some_and(|a| a.kind == ActivityKind::Working);
            if !waits.is_empty() && !working {
                badge = "blocked by lock".into();
                tone = Tone::Blocked;
            }
            CardModel {
                title,
                badge,
                tone,
                cwd,
                pid: Some(format!("pid {pid}")),
                now_doing,
                can_resume: false,
                pills,
                locks: lock_pills,
                waiting_on,
                pending,
            }
        }
        SlotState::Exited { code, .. } => CardModel {
            title,
            badge: match code {
                Some(c) => format!("Exited (code {c})"),
                None => "Exited".into(),
            },
            tone: Tone::Exited,
            cwd,
            pid: None,
            now_doing: None,
            can_resume: false,
            pills,
            locks: lock_pills,
            waiting_on,
            pending,
        },
        SlotState::Resumable { .. } => CardModel {
            title,
            badge: "Resumable".into(),
            tone: Tone::Resumable,
            cwd,
            pid: None,
            now_doing: None,
            can_resume: true,
            pills,
            locks: lock_pills,
            waiting_on,
            pending,
        },
    }
}

/// The targets `slot` holds, pins first; a long-held one ends with
/// ` · held long`. More than three show as three plus `+N`.
pub fn lock_pills(view: &LockView, slot: SlotId, now: i64) -> Vec<String> {
    let mut mine: Vec<_> = view.claims.iter().filter(|c| c.slot == slot).collect();
    mine.sort_by_key(|c| c.kind != ClaimKind::Pin);
    let mut pills: Vec<String> = mine
        .iter()
        .map(|c| {
            if view.flagged(c, now) {
                format!("{} · held long", c.target)
            } else {
                c.target.clone()
            }
        })
        .collect();
    if pills.len() > 3 {
        let more = pills.len() - 3;
        pills.truncate(3);
        pills.push(format!("+{more}"));
    }
    pills
}

/// `waiting on <first>`, then `and N more` for the rest.
fn waiting_label(waits: &[&str]) -> Option<String> {
    let (first, rest) = waits.split_first()?;
    Some(match rest.len() {
        0 => format!("waiting on {first}"),
        n => format!("waiting on {first} and {n} more"),
    })
}

/// `1 relay pending`, `2 relays pending`, nothing for none.
pub fn pending_label(n: usize) -> Option<String> {
    match n {
        0 => None,
        1 => Some("1 relay pending".into()),
        n => Some(format!("{n} relays pending")),
    }
}

/// The relay strip: the newest `max` relays, newest first, by delivery (or
/// drop) time when retired and creation time otherwise. `clock` renders a
/// time; a pending relay shows its age at `now`.
pub fn strip_lines(
    view: &LockView,
    now: i64,
    clock: impl Fn(i64) -> String,
    max: usize,
) -> Vec<crate::session::RelayRow> {
    let when = |r: &overseer_proto::Relay| r.delivered_at.or(r.dropped_at).unwrap_or(r.at);
    let mut relays: Vec<_> = view.relays.iter().collect();
    relays.sort_by_key(|r| std::cmp::Reverse((when(r), r.id)));
    relays
        .into_iter()
        .take(max)
        .map(|r| {
            let text = one_line(&r.text, 80);
            let (time, to) = (clock(when(r)), slot_name(r.to));
            let line = match r.state {
                RelayState::Delivered => format!("{time} told {to}: {text}"),
                RelayState::Pending => {
                    let pending = crate::session::pending_age(r, now);
                    format!("{time} for {to}, {pending}: {text}")
                }
                RelayState::Dropped => {
                    let dropped = r.drop_text().unwrap_or_default();
                    format!("{time} for {to}, {dropped}: {text}")
                }
            };
            crate::session::RelayRow {
                line,
                pending: (r.state == RelayState::Pending).then_some(r.id),
            }
        })
        .collect()
}

/// `text` on one line, cut to `max` characters.
pub(crate) fn one_line(text: &str, max: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line,
    }
}

/// The Notes strip's text and colour. Writer off beats a push problem,
/// which beats a failure, which beats a queue, which beats the last
/// commit time. Problems use the warning tone (orange).
pub fn strip_text(v: &BrainView) -> (String, Tone) {
    if !v.on {
        return ("Notes: writer off".into(), Tone::Empty);
    }
    if v.push_problem.is_some() {
        return ("Notes: push needs you".into(), Tone::NeedsYou);
    }
    if let Some(why) = brain_failure(v) {
        return (format!("Notes: {why}"), Tone::NeedsYou);
    }
    // Commits on both sides: no push lands until the folder is merged (#39).
    if let Some(r) = v.remote.as_ref().filter(|r| r.ahead > 0 && r.behind > 0) {
        return (
            format!("Notes: {} ahead, {} behind its remote", r.ahead, r.behind),
            Tone::NeedsYou,
        );
    }
    let queued = v
        .entries
        .iter()
        .filter(|e| e.state == QueueState::Queued)
        .count();
    if queued > 0 {
        return (format!("Notes: {queued} queued"), Tone::Idle);
    }
    match v
        .entries
        .iter()
        .filter(|e| e.state == QueueState::Committed)
        .map(QueueEntry::when)
        .max()
    {
        Some(at) => (format!("Notes: committed {}", local_hhmm(at)), Tone::Idle),
        None => ("Notes: nothing yet".into(), Tone::Idle),
    }
}

/// What failed last, with a short reason: a queue entry that failed after
/// the newest commit, else the last backup, else the last sync.
fn brain_failure(v: &BrainView) -> Option<String> {
    let short = |what: &str, why: &str| match one_line(why, 40) {
        r if r.is_empty() => format!("{what} failed"),
        r => format!("{what} failed: {r}"),
    };
    let newest_commit = v
        .entries
        .iter()
        .filter(|e| e.state == QueueState::Committed)
        .map(|e| e.id)
        .max();
    let failed = v
        .entries
        .iter()
        .filter(|e| e.state == QueueState::Failed && Some(e.id) > newest_commit)
        .max_by_key(|e| e.id);
    if let Some(e) = failed {
        return Some(short("commit", e.error.as_deref().unwrap_or("")));
    }
    if let Some(r) = v.last_backup.as_ref().filter(|r| !r.ok) {
        return Some(short("backup", &r.detail));
    }
    v.last_sync
        .as_ref()
        .filter(|r| !r.ok)
        .map(|r| short("sync", &r.detail))
}

/// The notes folder against its remote, first in the strip's list:
/// `Remote: 3 ahead, 0 behind at 09:41; last push 09:12`.
pub fn remote_line(v: &BrainView) -> Option<String> {
    let r = v.remote.as_ref()?;
    let mut line = format!(
        "Remote: {} ahead, {} behind at {}",
        r.ahead,
        r.behind,
        local_hhmm(r.checked_at)
    );
    match &v.last_push {
        Some(p) if p.ok => line.push_str(&format!("; last push {}", local_hhmm(p.at))),
        Some(p) => line.push_str(&format!("; last push failed {}", local_hhmm(p.at))),
        None => line.push_str("; no push yet"),
    }
    if let Some(e) = &r.fetch_error {
        line.push_str(&format!("; fetch failed: {}", one_line(e, 60)));
    }
    Some(line)
}

/// One line of the strip's list: `09:41 S1  Sessions/.../s1-demo.md  committed, pushed`.
pub fn entry_line(e: &QueueEntry) -> String {
    let who = match e.from {
        Sender::Overseer => "Overseer".to_string(),
        Sender::You => "you".to_string(),
        Sender::Slot { slot } => slot_name(slot),
    };
    let what = match e.paths.as_slice() {
        [] => one_line(&e.message, 60),
        [only] => only.clone(),
        [first, rest @ ..] => format!("{first} (+{} more)", rest.len()),
    };
    let state = match (e.state, e.pushed) {
        (QueueState::Queued, _) => "queued".to_string(),
        (QueueState::Committed, Some(true)) => "committed, pushed".to_string(),
        (QueueState::Committed, Some(false)) => "committed, not pushed".to_string(),
        (QueueState::Committed, None) => "committed".to_string(),
        (QueueState::NothingToCommit, _) => "nothing to commit".to_string(),
        (QueueState::Failed, _) => match e.error.as_deref().map(str::trim) {
            Some(err) if !err.is_empty() => format!("failed: {}", one_line(err, 80)),
            _ => "failed".to_string(),
        },
    };
    format!("{} {who}  {what}  {state}", local_hhmm(e.when()))
}

pub fn running_count(slots: &[SlotInfo]) -> usize {
    slots
        .iter()
        .filter(|s| matches!(s.state, SlotState::Running { .. }))
        .count()
}

/// The header's right-hand text.
pub fn header_count(running: usize) -> String {
    match running {
        0 => "none running".into(),
        n => format!("{n} running"),
    }
}

/// Slots waiting on Jonathan.
pub fn needs_you_count(slots: &[SlotInfo]) -> usize {
    slots
        .iter()
        .filter(|s| {
            s.activity
                .as_ref()
                .is_some_and(|a| a.kind == ActivityKind::NeedsYou)
        })
        .count()
}

/// Running slots that wait for the next prompt.
pub fn your_turn_count(slots: &[SlotInfo]) -> usize {
    slots
        .iter()
        .filter(|s| {
            matches!(s.state, SlotState::Running { .. })
                && s.activity
                    .as_ref()
                    .is_some_and(|a| a.kind == ActivityKind::Idle)
        })
        .count()
}

/// `2 need you · 1 your turn`, leaving out a part whose count is zero.
pub fn attention_text(need: usize, turn: usize) -> Option<String> {
    let mut parts = Vec::new();
    if need > 0 {
        parts.push(format!("{need} need you"));
    }
    if turn > 0 {
        parts.push(format!("{turn} your turn"));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// Done and total tickets in the list, from `sprint_view::progress` so the
/// header and the kanban agree; see `sprint_view::board_stage` for what
/// counts as done.
pub fn done_count(work: &WorkView) -> (usize, usize) {
    crate::sprint_view::progress(work).map_or((0, 0), |p| (p.done, p.total))
}

/// The header's right-hand text with the sessions that need Jonathan and
/// the ticket count: `3 running, 1 need you, done 2/5`. No tickets, no
/// count.
pub fn header_text(
    running: usize,
    need_you: usize,
    your_turn: usize,
    done: (usize, usize),
) -> String {
    let mut text = match attention_text(need_you, your_turn) {
        None => header_count(running),
        Some(a) => format!("{}, {a}", header_count(running)),
    };
    if done.1 > 0 {
        text.push_str(&format!(", done {}/{}", done.0, done.1));
    }
    text
}

/// The coordinator panel's summary: a title (`Updated 14:05`, or `No
/// summary yet`) and the text.
pub fn summary_lines(view: &CoordView, clock: impl Fn(i64) -> String) -> (String, String) {
    match &view.summary {
        Some(s) => (format!("Updated {}", clock(s.at)), s.text.clone()),
        None => (
            "No summary yet".into(),
            "Ask S0 \"what's going on?\" to get one.".into(),
        ),
    }
}

/// The coordinator panel's line for the plan waiting for Jonathan. A click
/// on it opens the plan view like `p`, so it names the plan `open_plan`
/// opens.
pub fn plan_line(view: &CoordView) -> Option<String> {
    let plan = view.pending_plan()?;
    Some(format!("Plan {} waiting for you (p)", plan.id))
}

/// The coordinator panel's lines for the newest approved plan's outcome, so
/// an approval made through ctl shows too: a header, then each failed step.
pub fn outcome_lines(view: &CoordView) -> Vec<String> {
    let Some(plan) = view.plans.iter().find(|p| p.state == PlanState::Approved) else {
        return Vec::new();
    };
    let failed: Vec<&String> = plan
        .outcome
        .iter()
        .filter(|l| l.contains(" failed: "))
        .collect();
    let steps = plan.outcome.len();
    let mut out = vec![if failed.is_empty() {
        format!("Plan {} approved: {steps} steps, none failed", plan.id)
    } else {
        format!(
            "Plan {} approved: {steps} steps, {} failed",
            plan.id,
            failed.len()
        )
    }];
    out.extend(failed.into_iter().cloned());
    out
}

/// Arrow-key moves between cards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

/// The card selected after moving `dir` from `from`. S0 sits above the grid
/// (S1 S2 S3 / S4 S5 S6); moves off an edge stay put.
pub fn move_selection(from: SlotId, dir: Dir) -> SlotId {
    if from == 0 {
        return if dir == Dir::Down { 1 } else { 0 };
    }
    let (row, col) = ((from - 1) / 3, (from - 1) % 3);
    match dir {
        Dir::Left if col > 0 => from - 1,
        Dir::Right if col < 2 => from + 1,
        Dir::Up if row == 0 => 0,
        Dir::Up => from - 3,
        Dir::Down if row == 0 => from + 3,
        _ => from,
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Badge colour. Needs you is the loudest, so it is the only orange.
fn tone_color(tone: Tone) -> u32 {
    match tone {
        Tone::Running | Tone::Working => colors::GREEN,
        Tone::NeedsYou => colors::ORANGE,
        Tone::YourTurn => colors::ACCENT,
        Tone::Blocked => colors::RED,
        Tone::Idle | Tone::Exited | Tone::Resumable | Tone::Empty => colors::GREY,
    }
}

/// The card's left edge: the badge colour while the slot runs.
fn edge_color(tone: Tone) -> u32 {
    match tone {
        Tone::Running
        | Tone::Working
        | Tone::NeedsYou
        | Tone::YourTurn
        | Tone::Idle
        | Tone::Blocked => tone_color(tone),
        Tone::Exited | Tone::Resumable | Tone::Empty => colors::EDGE_IDLE,
    }
}

/// One small rounded tag with a work-item key.
fn pill(key: String) -> gpui::Div {
    div()
        .px_1()
        .rounded(px(4.))
        .text_xs()
        .bg(rgb(colors::ACCENT_BG))
        .text_color(rgb(colors::ACCENT))
        .child(key)
}

/// A held lock target, outlined to tell it from a work-item pill.
fn lock_pill(target: String) -> gpui::Div {
    div()
        .px_1()
        .rounded(px(4.))
        .text_xs()
        .border_1()
        .border_color(rgb(colors::BORDER_COORD))
        .text_color(rgb(colors::TEXT))
        .child(target)
}

impl OverseerApp {
    fn render_card(&self, id: SlotId, now: i64, cx: &mut Context<Self>) -> impl IntoElement {
        let info = &self.slots[id as usize];
        let m = card_model(info, now, self.home.as_deref(), &self.work, &self.locks);
        let empty = m.tone == Tone::Empty;
        let selected = self.selected == id;
        let edge = edge_color(m.tone);
        div()
            .id(("card", id as usize))
            .flex_1()
            .min_w(px(0.))
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .rounded(px(6.))
            .bg(rgb(colors::CARD))
            .border_1()
            .border_color(rgb(if selected {
                colors::ACCENT
            } else {
                colors::BORDER
            }))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_w(px(0.))
                            .gap_2()
                            .child(
                                div()
                                    .flex_none()
                                    .w(px(4.))
                                    .h(px(16.))
                                    .rounded(px(2.))
                                    .bg(rgb(edge)),
                            )
                            .child(
                                div()
                                    .min_w(px(0.))
                                    .truncate()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(m.title),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .gap_2()
                            .children(empty.then(|| {
                                div()
                                    .id(("card-assign", id as usize))
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(colors::ACCENT))
                                    .cursor_pointer()
                                    .child("+ assign")
                                    // Not also the start dialog behind it.
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.open_picker(id, window, cx)
                                    }))
                            }))
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(tone_color(m.tone)))
                                    .child(m.badge),
                            ),
                    ),
            )
            .children((!m.pills.is_empty()).then(|| {
                div()
                    .flex()
                    .flex_wrap()
                    .gap_1()
                    .children(m.pills.into_iter().map(pill))
            }))
            .children((!m.locks.is_empty()).then(|| {
                div()
                    .flex()
                    .flex_wrap()
                    .gap_1()
                    .children(m.locks.into_iter().map(lock_pill))
            }))
            .children(m.waiting_on.map(|w| {
                div()
                    .text_xs()
                    .text_color(rgb(colors::RED))
                    .truncate()
                    .child(w)
            }))
            .children(
                m.pending
                    .map(|p| div().text_xs().text_color(rgb(colors::ORANGE)).child(p)),
            )
            .children(
                m.cwd
                    .map(|c| div().text_xs().text_color(rgb(colors::TEXT_DIM)).child(c)),
            )
            .children(m.now_doing.map(|d| {
                div()
                    .text_xs()
                    .text_color(rgb(if m.tone == Tone::NeedsYou {
                        colors::ORANGE
                    } else {
                        colors::TEXT
                    }))
                    .truncate()
                    .child(d)
            }))
            .children(
                m.pid
                    .map(|p| div().text_xs().text_color(rgb(colors::MUTED)).child(p)),
            )
            .children(m.can_resume.then(|| {
                div().flex().pt_1().child(
                    div()
                        .id(("card-resume", id as usize))
                        .px_2()
                        .py_1()
                        .rounded(px(5.))
                        .text_xs()
                        .bg(rgb(colors::BUTTON))
                        .border_1()
                        .border_color(rgb(colors::BUTTON_BORDER))
                        .text_color(rgb(colors::ACCENT))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(colors::CARD_HOVER)))
                        .child("Resume")
                        // The click must not also open the card behind it.
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.resume_slot(id, window, cx)
                        })),
                )
            }))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(colors::CARD_HOVER)))
            .on_click(cx.listener(move |this, _, window, cx| this.open_card(id, window, cx)))
    }

    pub(crate) fn render_board(
        &mut self,
        now: i64,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let running = running_count(&self.slots);
        let need_you = needs_you_count(&self.slots);
        let your_turn = your_turn_count(&self.slots);
        let strip = strip_lines(&self.locks, now, local_hhmm, 5);
        let (summary_title, summary_body) = summary_lines(&self.coord, local_hhmm);
        let plan_line = plan_line(&self.coord);
        let outcome_lines = outcome_lines(&self.coord);
        let (strip_label, strip_tone) = strip_text(&self.brain);
        let show_list = self.brain_hover || self.brain_pinned;
        let s0 = self.render_card(0, now, cx);
        let row = |ids: [SlotId; 3], this: &Self, cx: &mut Context<Self>| {
            let cards: Vec<_> = ids
                .into_iter()
                .map(|id| this.render_card(id, now, cx))
                .collect();
            div().flex().gap_2().children(cards)
        };
        let top = row([1, 2, 3], self, cx);
        let bottom = row([4, 5, 6], self, cx);
        let pr_panel = self.render_pr_panel(now, cx);
        let kanban = self.render_kanban(now, cx);
        div()
            .key_context("Board")
            .track_focus(&self.board_focus)
            .on_action(cx.listener(|this, _: &crate::app::SelectLeft, _, cx| {
                this.move_selection(Dir::Left, cx)
            }))
            .on_action(cx.listener(|this, _: &crate::app::SelectRight, _, cx| {
                this.move_selection(Dir::Right, cx)
            }))
            .on_action(cx.listener(|this, _: &crate::app::SelectUp, _, cx| {
                this.move_selection(Dir::Up, cx)
            }))
            .on_action(cx.listener(|this, _: &crate::app::SelectDown, _, cx| {
                this.move_selection(Dir::Down, cx)
            }))
            .on_action(
                cx.listener(|this, _: &crate::app::ResumeSelected, window, cx| {
                    if let Some(id) = crate::app::resume_target(this.selected, &this.slots) {
                        this.resume_slot(id, window, cx);
                    }
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::app::AssignSelected, window, cx| {
                    this.open_picker(this.selected, window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &crate::app::OpenPlan, window, cx| {
                this.open_plan(window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &crate::app::OpenSelected, window, cx| {
                    this.open_card(this.selected, window, cx)
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &gpui::MouseMoveEvent, window, cx| {
                this.drag_kanban(ev, window, cx)
            }))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseUpEvent, _, cx| this.end_kanban_drag(cx)),
            )
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_3()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap_3()
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(FontWeight::BOLD)
                                    .child("Overseer"),
                            )
                            .children(header_title(&self.work).map(|t| {
                                div()
                                    .text_sm()
                                    .text_color(rgb(colors::MUTED))
                                    .child(t)
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_3()
                            .text_sm()
                            .child(
                                div()
                                    .text_color(rgb(if need_you > 0 {
                                        colors::ORANGE
                                    } else if your_turn > 0 {
                                        colors::ACCENT
                                    } else {
                                        colors::MUTED
                                    }))
                                    .child(header_text(
                                        running,
                                        need_you,
                                        your_turn,
                                        done_count(&self.work),
                                    )),
                            )
                            .child(
                                div()
                                    .text_color(rgb(if sources_failing(&self.work) {
                                        colors::ORANGE
                                    } else {
                                        colors::MUTED
                                    }))
                                    .child(source_line(&self.work, local_hhmm)),
                            )
                            .child(refresh_button(self.refreshing_work, cx)),
                    ),
            )
            // A notes folder Overseer would not take: notes stay off until
            // the user picks another folder, so say so above everything.
            .children(self.work.notes_off.clone().map(|text| {
                div()
                    .p_2()
                    .rounded(px(6.))
                    .border_1()
                    .border_color(rgb(colors::ORANGE))
                    .text_sm()
                    .text_color(rgb(colors::ORANGE))
                    .child(text)
            }))
            // The coordinator panel with the open PRs to its right; the
            // PR panel wraps below in a window too narrow for both.
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                div()
                    .flex_grow()
                    .flex_basis(px(560.))
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_2()
                    .rounded(px(6.))
                    .bg(rgb(colors::CARD))
                    .border_1()
                    .border_color(rgb(colors::BORDER_COORD))
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(colors::ACCENT))
                            .child("COORDINATOR (S0)"),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .text_sm()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(colors::MUTED))
                                    .child(summary_title),
                            )
                            .child(
                                div()
                                    .text_color(rgb(colors::TEXT_DIM))
                                    .child(summary_body),
                            ),
                    )
                    .children(plan_line.map(|line| {
                        div()
                            .id("plan-line")
                            .cursor_pointer()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(colors::ORANGE))
                            .hover(|s| s.underline())
                            .child(line)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.open_plan(window, cx)),
                            )
                    }))
                    .children((!outcome_lines.is_empty()).then(|| {
                        div()
                            .flex()
                            .flex_col()
                            .text_xs()
                            .text_color(rgb(colors::TEXT_DIM))
                            .children(outcome_lines.into_iter().map(|l| div().child(l)))
                    }))
                    .child(div().flex().child(s0)),
                    )
                    .children(pr_panel),
            )
            .child(div().flex_none().flex().flex_col().gap_2().child(top).child(bottom))
            .children(kanban)
            .children(self.status.clone().map(|s| {
                div()
                    .text_sm()
                    .text_color(rgb(colors::ORANGE))
                    .child(s)
            }))
            .children((!strip.is_empty()).then(|| {
                div()
                    .mt_auto()
                    .flex()
                    .flex_col()
                    .text_xs()
                    .text_color(rgb(colors::TEXT_DIM))
                    .children(strip.into_iter().map(|row| {
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().flex_1().min_w(px(0.)).truncate().child(row.line))
                            .children(row.pending.map(|relay| {
                                div()
                                    .id(("strip-drop-relay", relay as u64))
                                    .flex_none()
                                    .px_1()
                                    .rounded(px(4.))
                                    .border_1()
                                    .border_color(rgb(colors::BUTTON_BORDER))
                                    .text_color(rgb(colors::ACCENT))
                                    .cursor_pointer()
                                    .hover(|s| s.bg(rgb(colors::CARD_HOVER)))
                                    .child("Drop")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.drop_relay(relay, cx)
                                    }))
                            }))
                    }))
            }))
            .children(show_list.then(|| {
                div()
                    .mt_auto()
                    .flex()
                    .flex_col()
                    .text_xs()
                    .text_color(rgb(colors::TEXT_DIM))
                    .children(
                        self.brain
                            .push_problem
                            .clone()
                            .map(|p| div().text_color(rgb(colors::ORANGE)).child(p)),
                    )
                    .children(remote_line(&self.brain).map(|l| div().truncate().child(l)))
                    .children(
                        self.brain
                            .entries
                            .iter()
                            .take(5)
                            .map(|e| div().truncate().child(entry_line(e))),
                    )
                    .children(self.brain.entries.is_empty().then(|| div().child("Nothing queued")))
            }))
            .child(
                div()
                    .when(self.locks.relays.is_empty() && !show_list, |d| d.mt_auto())
                    .flex()
                    .gap_2()
                    .text_xs()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_color(rgb(colors::MUTED))
                            .child("Click a card or press Enter to open it · A assigns work · Cmd+0 to Cmd+6 open a session · Cmd+N starts the selected slot · L shows the locks"),
                    )
                    .child(
                        div()
                            .id("locks-strip")
                            .flex_none()
                            .cursor_pointer()
                            .text_color(rgb(colors::MUTED))
                            .hover(|s| s.text_color(rgb(colors::TEXT)))
                            .child(crate::locks_view::locks_strip(&self.locks))
                            .on_click(cx.listener(|this, _, window, cx| this.go_locks(window, cx))),
                    )
                    .child(
                        div()
                            .id("brain-strip")
                            .flex_none()
                            .cursor_pointer()
                            .text_color(rgb(tone_color(strip_tone)))
                            .child(strip_label)
                            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                                this.brain_hover = *hovered;
                                cx.notify();
                            }))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.brain_pinned = !this.brain_pinned;
                                cx.notify();
                            })),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use overseer_proto::{
        Activity, ActivityKind, BrainRun, Claim, ItemKind, Relay, Sender, SourceStatus, Wait,
        WorkItem,
    };
    use std::path::PathBuf;

    fn info(id: SlotId, label: &str, state: SlotState) -> SlotInfo {
        SlotInfo {
            id,
            label: label.into(),
            cwd: (state != SlotState::Empty).then(|| PathBuf::from("/Users/me/Repos/eks")),
            command: None,
            state,
            cols: 80,
            rows: 24,
            activity: None,
            session_note: None,
            held: false,
        }
    }

    const HOME: &str = "/Users/me";

    fn no_work() -> WorkView {
        WorkView {
            notes_off: None,
            items: vec![],
            sources: vec![],
            notices: vec![],
            sprint: None,
            prs: Vec::new(),
            sprint_day: None,
        }
    }

    #[test]
    fn header_title_is_the_sprint_text() {
        let mut w = no_work();
        assert_eq!(header_title(&w), None);
        w.sprint = Some("sprint-26-01, day 4 of 10".into());
        assert_eq!(
            header_title(&w).as_deref(),
            Some("sprint-26-01, day 4 of 10")
        );
        w.sprint = Some(String::new());
        assert_eq!(header_title(&w), None);
    }

    fn no_locks() -> LockView {
        crate::app::empty_locks()
    }

    fn claim(slot: SlotId, target: &str, kind: ClaimKind, last_used: i64) -> Claim {
        Claim {
            target: target.into(),
            slot,
            kind,
            reason: None,
            item: None,
            since: 0,
            last_used,
            pin_revision: None,
        }
    }

    fn wait(slot: SlotId, target: &str) -> Wait {
        Wait {
            target: target.into(),
            slot,
            at: 0,
            reason: None,
        }
    }

    fn relay(id: i64, to: SlotId, at: i64, delivered_at: Option<i64>, text: &str) -> Relay {
        Relay {
            id,
            at,
            from: Sender::Overseer,
            to,
            text: text.into(),
            state: if delivered_at.is_some() {
                RelayState::Delivered
            } else {
                RelayState::Pending
            },
            delivered_at,
            dropped_at: None,
            kind: overseer_proto::RelayKind::Note,
            drop_reason: None,
            held: None,
            held_since: None,
        }
    }

    fn running(id: SlotId, kind: Option<ActivityKind>) -> SlotInfo {
        let mut i = info(id, "", SlotState::Running { pid: 7, since: 0 });
        i.activity = kind.map(|kind| Activity {
            kind,
            since: 0,
            now_doing: None,
            reason: None,
            idle_wait: false,
        });
        i
    }

    #[test]
    fn lock_pills_pins_first_and_flagged() {
        let mut v = no_locks();
        v.flag_after_secs = 100;
        v.claims = vec![
            claim(2, "repo:demo-api:main", ClaimKind::Command, 950),
            claim(2, "argocd:demo-app", ClaimKind::Pin, 100),
            claim(3, "tf:demo", ClaimKind::Claim, 0),
        ];
        assert_eq!(
            lock_pills(&v, 2, 1000),
            ["argocd:demo-app · held long", "repo:demo-api:main"]
        );
        assert!(lock_pills(&v, 5, 1000).is_empty());
    }

    #[test]
    fn lock_pills_collapse_after_three() {
        let mut v = no_locks();
        v.flag_after_secs = 100;
        v.claims = (1..=5)
            .map(|n| claim(2, &format!("repo:demo-api:b{n}"), ClaimKind::Claim, 99))
            .collect();
        assert_eq!(
            lock_pills(&v, 2, 100),
            [
                "repo:demo-api:b1",
                "repo:demo-api:b2",
                "repo:demo-api:b3",
                "+2"
            ]
        );
        v.claims.truncate(3);
        assert_eq!(lock_pills(&v, 2, 100).len(), 3);
    }

    #[test]
    fn waiting_slot_is_blocked_when_not_working() {
        let mut v = no_locks();
        v.waits = vec![wait(2, "repo:demo-api:main"), wait(2, "tf:demo")];
        for kind in [Some(ActivityKind::Idle), Some(ActivityKind::NeedsYou), None] {
            let m = card_model(&running(2, kind), 10, None, &no_work(), &v);
            assert_eq!(m.tone, Tone::Blocked);
            assert_eq!(m.badge, "blocked by lock");
            assert_eq!(
                m.waiting_on.as_deref(),
                Some("waiting on repo:demo-api:main and 1 more")
            );
        }
        assert_eq!(tone_color(Tone::Blocked), colors::RED);
        assert_eq!(edge_color(Tone::Blocked), colors::RED);
        // Only the waiting slot is blocked.
        let m = card_model(
            &running(3, Some(ActivityKind::Idle)),
            10,
            None,
            &no_work(),
            &v,
        );
        assert_eq!(m.tone, Tone::YourTurn);
        assert_eq!(m.waiting_on, None);
    }

    #[test]
    fn working_slot_keeps_its_tone_but_shows_the_wait() {
        let mut v = no_locks();
        v.waits = vec![wait(2, "repo:demo-api:main")];
        let m = card_model(
            &running(2, Some(ActivityKind::Working)),
            10,
            None,
            &no_work(),
            &v,
        );
        assert_eq!(m.tone, Tone::Working);
        assert!(m.badge.starts_with("● Working"), "{}", m.badge);
        assert_eq!(
            m.waiting_on.as_deref(),
            Some("waiting on repo:demo-api:main")
        );
    }

    #[test]
    fn pending_label_counts() {
        assert_eq!(pending_label(0), None);
        assert_eq!(pending_label(1).as_deref(), Some("1 relay pending"));
        assert_eq!(pending_label(2).as_deref(), Some("2 relays pending"));
        let mut v = no_locks();
        v.relays = vec![relay(1, 2, 5, None, "a"), relay(2, 2, 6, Some(7), "b")];
        let m = card_model(&running(2, None), 10, None, &no_work(), &v);
        assert_eq!(m.pending.as_deref(), Some("1 relay pending"));
    }

    fn q(id: i64, at: i64, state: QueueState) -> QueueEntry {
        QueueEntry {
            id,
            at,
            from: Sender::Slot { slot: 1 },
            paths: vec!["Sessions/2026/11-November/2026-11-13-s1-demo.md".into()],
            message: "S1: note".into(),
            state,
            commit: None,
            pushed: None,
            error: None,
            committed_at: None,
        }
    }

    fn brain(entries: Vec<QueueEntry>) -> BrainView {
        BrainView {
            remote: None,
            last_push: None,
            on: true,
            entries,
            last_backup: None,
            last_sync: None,
            push_problem: None,
            brain_dir: None,
            notes_dir: None,
        }
    }

    #[test]
    fn strip_counts_the_queue() {
        let v = brain(vec![
            q(3, 30, QueueState::Queued),
            q(2, 20, QueueState::Queued),
            q(1, 10, QueueState::Committed),
        ]);
        let (text, tone) = strip_text(&v);
        assert_eq!(text, "Notes: 2 queued");
        assert_eq!(tone, Tone::Idle);
    }

    #[test]
    fn strip_shows_the_newest_commit_time() {
        // A failure that a later commit has passed is history.
        let v = brain(vec![
            q(3, 2000, QueueState::Committed),
            q(2, 1000, QueueState::Committed),
            q(1, 5, QueueState::Failed),
        ]);
        let (text, tone) = strip_text(&v);
        assert_eq!(text, format!("Notes: committed {}", local_hhmm(2000)));
        assert_eq!(tone, Tone::Idle);
    }

    #[test]
    fn strip_and_lines_show_when_it_committed_not_when_it_was_queued() {
        // Queued at 1000, held, committed at 4600: the commit time shows.
        let mut e = q(1, 1000, QueueState::Committed);
        e.committed_at = Some(4600);
        let (text, _) = strip_text(&brain(vec![e.clone()]));
        assert_eq!(text, format!("Notes: committed {}", local_hhmm(4600)));
        assert!(entry_line(&e).starts_with(&format!("{} ", local_hhmm(4600))));
        // Still queued: the queue time.
        let e = q(2, 1000, QueueState::Queued);
        assert!(entry_line(&e).starts_with(&format!("{} ", local_hhmm(1000))));
    }

    #[test]
    fn strip_says_push_needs_you_in_orange() {
        let mut v = brain(vec![q(1, 10, QueueState::Queued)]);
        v.push_problem = Some("push rejected".into());
        let (text, tone) = strip_text(&v);
        assert_eq!(text, "Notes: push needs you");
        assert_eq!(tone, Tone::NeedsYou);
        assert_eq!(tone_color(tone), colors::ORANGE);
    }

    #[test]
    fn strip_warns_of_a_failed_entry_with_its_reason() {
        let mut failed = q(2, 20, QueueState::Failed);
        failed.error = Some("index.lock still held after 4 attempts".into());
        let v = brain(vec![
            q(3, 30, QueueState::Queued),
            failed,
            q(1, 10, QueueState::Committed),
        ]);
        let (text, tone) = strip_text(&v);
        assert_eq!(
            text,
            "Notes: commit failed: index.lock still held after 4 attempts"
        );
        assert_eq!(tone, Tone::NeedsYou);
        assert_eq!(tone_color(tone), colors::ORANGE);
        // A long reason is cut short.
        let mut long = q(2, 20, QueueState::Failed);
        long.error = Some(format!("pathspec {} did not match", "x".repeat(80)));
        let (text, _) = strip_text(&brain(vec![long]));
        assert!(
            text.starts_with("Notes: commit failed: pathspec xxx"),
            "{text}"
        );
        assert!(text.ends_with('…'), "{text}");
    }

    #[test]
    fn strip_warns_of_a_failed_backup() {
        let mut v = brain(vec![q(1, 10, QueueState::Committed)]);
        v.last_backup = Some(BrainRun {
            at: 20,
            ok: false,
            detail: "git status failed".into(),
        });
        v.last_sync = Some(BrainRun {
            at: 30,
            ok: false,
            detail: "sync-run.sh exited 1".into(),
        });
        let (text, tone) = strip_text(&v);
        assert_eq!(text, "Notes: backup failed: git status failed");
        assert_eq!(tone, Tone::NeedsYou);
        // A good backup shows the commit time as before.
        v.last_backup.as_mut().unwrap().ok = true;
        v.last_sync.as_mut().unwrap().ok = true;
        assert_eq!(strip_text(&v).1, Tone::Idle);
    }

    #[test]
    fn strip_warns_of_a_failed_sync() {
        let mut v = brain(vec![q(1, 10, QueueState::Committed)]);
        v.last_sync = Some(BrainRun {
            at: 30,
            ok: false,
            detail: "held: git status failed".into(),
        });
        let (text, tone) = strip_text(&v);
        assert_eq!(text, "Notes: sync failed: held: git status failed");
        assert_eq!(tone, Tone::NeedsYou);
        // A push problem still comes first.
        v.push_problem = Some("push rejected".into());
        assert_eq!(strip_text(&v).0, "Notes: push needs you");
    }

    #[test]
    fn strip_writer_off_is_grey_and_beats_everything() {
        let mut v = brain(vec![q(1, 10, QueueState::Queued)]);
        v.on = false;
        v.push_problem = Some("push rejected".into());
        let (text, tone) = strip_text(&v);
        assert_eq!(text, "Notes: writer off");
        assert_eq!(tone, Tone::Empty);
        assert_eq!(tone_color(tone), colors::GREY);
    }

    #[test]
    fn strip_warns_of_a_diverged_folder_and_the_list_says_where_it_stands() {
        let remote = |ahead, behind, fetch_error: Option<&str>| {
            Some(overseer_proto::NotesRemote {
                ahead,
                behind,
                checked_at: 100,
                fetch_error: fetch_error.map(Into::into),
            })
        };
        let mut v = brain(vec![q(1, 10, QueueState::Committed)]);
        v.remote = remote(65, 4, None);
        let (text, tone) = strip_text(&v);
        assert_eq!(text, "Notes: 65 ahead, 4 behind its remote");
        assert_eq!(tone, Tone::NeedsYou);
        assert_eq!(
            remote_line(&v).unwrap(),
            format!(
                "Remote: 65 ahead, 4 behind at {}; no push yet",
                local_hhmm(100)
            )
        );
        // Only ahead, or only behind, is not a problem by itself.
        v.remote = remote(3, 0, None);
        assert_eq!(strip_text(&v).1, Tone::Idle);
        v.remote = remote(0, 2, None);
        assert_eq!(strip_text(&v).1, Tone::Idle);
        v.last_push = Some(BrainRun {
            at: 90,
            ok: true,
            detail: String::new(),
        });
        v.remote = remote(0, 0, Some("could not resolve host"));
        assert_eq!(
            remote_line(&v).unwrap(),
            format!(
                "Remote: 0 ahead, 0 behind at {}; last push {}; fetch failed: could not resolve host",
                local_hhmm(100),
                local_hhmm(90)
            )
        );
        v.remote = None;
        assert_eq!(remote_line(&v), None);
    }

    #[test]
    fn strip_with_nothing_yet() {
        let (text, tone) = strip_text(&brain(vec![q(1, 10, QueueState::NothingToCommit)]));
        assert_eq!(text, "Notes: nothing yet");
        assert_eq!(tone, Tone::Idle);
    }

    #[test]
    fn entry_lines_say_where_each_commit_stands() {
        let mut e = q(1, 3000, QueueState::Committed);
        e.pushed = Some(true);
        assert_eq!(
            entry_line(&e),
            format!(
                "{} S1  Sessions/2026/11-November/2026-11-13-s1-demo.md  committed, pushed",
                local_hhmm(3000)
            )
        );
        e.pushed = Some(false);
        assert!(entry_line(&e).ends_with("committed, not pushed"));
        e.pushed = None;
        assert!(entry_line(&e).ends_with("  committed"));
        e.state = QueueState::Queued;
        assert!(entry_line(&e).ends_with("  queued"));
        e.state = QueueState::NothingToCommit;
        assert!(entry_line(&e).ends_with("  nothing to commit"));
        e.state = QueueState::Failed;
        e.error = Some("index.lock held".into());
        assert!(entry_line(&e).ends_with("  failed: index.lock held"));
        e.from = Sender::You;
        e.paths = vec!["a.md".into(), "b.md".into()];
        assert!(entry_line(&e).contains(" you  a.md (+1 more)  "));
        e.paths = vec![];
        assert!(entry_line(&e).contains(" you  S1: note  "));
    }

    #[test]
    fn strip_lines_newest_first_and_cut() {
        let long = format!("line one\n{}", "x".repeat(100));
        let mut v = no_locks();
        v.relays = vec![
            // Created first, delivered last: sorts by delivery time.
            relay(1, 3, 100, Some(500), "done it"),
            relay(2, 4, 300, None, "wait for it"),
            relay(3, 5, 200, None, &long),
            relay(4, 6, 50, Some(60), "old"),
        ];
        let clock = |t: i64| format!("t{t}");
        let rows = strip_lines(&v, 420, clock, 3);
        let lines: Vec<_> = rows.iter().map(|r| r.line.clone()).collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "t500 told S3: done it");
        assert_eq!(lines[1], "t300 for S4, pending 2m: wait for it");
        let cut = lines[2].strip_prefix("t200 for S5, pending 3m: ").unwrap();
        assert!(cut.starts_with("line one xxx"), "{cut}");
        assert_eq!(cut.chars().count(), 81);
        assert!(cut.ends_with('…'));
        // Pending rows carry their id, for the Drop button.
        let drops: Vec<_> = rows.iter().map(|r| r.pending).collect();
        assert_eq!(drops, vec![None, Some(2), Some(3)]);
        assert_eq!(strip_lines(&v, 420, clock, 10).len(), 4);
    }

    #[test]
    fn strip_shows_a_dropped_relay_at_its_drop_time() {
        let mut v = no_locks();
        v.relays = vec![
            Relay {
                state: RelayState::Dropped,
                dropped_at: Some(400),
                drop_reason: Some("session ended".into()),
                ..relay(1, 3, 100, None, "hello")
            },
            relay(2, 4, 300, None, "wait for it"),
            Relay {
                state: RelayState::Dropped,
                dropped_at: Some(250),
                drop_reason: Some("no longer holds deploy:demo/api".into()),
                ..relay(
                    3,
                    4,
                    200,
                    None,
                    "S1 has released deploy:demo/api. You now hold it.",
                )
            },
        ];
        let lines: Vec<_> = strip_lines(&v, 420, |t: i64| format!("t{t}"), 5)
            .into_iter()
            .map(|r| r.line)
            .collect();
        assert_eq!(
            lines,
            [
                "t400 for S3, dropped: session ended: hello",
                "t300 for S4, pending 2m: wait for it",
                "t250 for S4, dropped: no longer holds deploy:demo/api: S1 has released deploy:demo/api. You now hold it.",
            ]
        );
        // A dropped relay is not pending.
        assert_eq!(v.pending_for(3), 0);
    }

    fn cm(i: &SlotInfo, now: i64, home: Option<&Path>) -> CardModel {
        card_model(i, now, home, &no_work(), &no_locks())
    }

    fn item(key: &str, kind: ItemKind, parent: Option<&str>, owner: Option<SlotId>) -> WorkItem {
        WorkItem {
            key: key.into(),
            kind,
            title: format!("title of {key}"),
            status: "open".into(),
            priority: None,
            parent: parent.map(String::from),
            owner,
            in_list: true,
            folder: None,
            stage: None,
            stage_since: None,
        }
    }

    fn work(items: Vec<WorkItem>) -> WorkView {
        WorkView { items, ..no_work() }
    }

    #[test]
    fn owned_by_orders_tickets_first() {
        let v = work(vec![
            item("ov-3", ItemKind::Bead, None, Some(2)),
            item("DEMO-12", ItemKind::Ticket, None, Some(2)),
            item("DEMO-7", ItemKind::Ticket, None, Some(4)),
            item("ov-5", ItemKind::Bead, Some("DEMO-12"), Some(2)),
            item("DEMO-14", ItemKind::Ticket, None, Some(2)),
            item("ov-9", ItemKind::Bead, None, None),
        ]);
        let keys: Vec<_> = owned_by(&v, 2).iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, ["DEMO-12", "DEMO-14", "ov-3", "ov-5"]);
        assert!(owned_by(&v, 1).is_empty());
    }

    #[test]
    fn pills_skip_beads_under_owned_tickets() {
        let v = work(vec![
            item("DEMO-12", ItemKind::Ticket, None, Some(2)),
            item("ov-5", ItemKind::Bead, Some("DEMO-12"), Some(2)),
            // Its ticket belongs to someone else, so it stands alone here.
            item("ov-6", ItemKind::Bead, Some("DEMO-7"), Some(2)),
            item("ov-3", ItemKind::Bead, None, Some(2)),
        ]);
        assert_eq!(pills(&owned_by(&v, 2)), ["DEMO-12", "ov-6", "ov-3"]);
        assert!(pills(&[]).is_empty());
    }

    #[test]
    fn pills_collapse_after_three() {
        let v = work(
            ["DEMO-1", "DEMO-2", "DEMO-3", "DEMO-4", "DEMO-5"]
                .into_iter()
                .map(|k| item(k, ItemKind::Ticket, None, Some(1)))
                .collect(),
        );
        let owned = owned_by(&v, 1);
        assert_eq!(pills(&owned), ["DEMO-1", "DEMO-2", "DEMO-3", "+2"]);
        assert_eq!(pills(&owned[..4]), ["DEMO-1", "DEMO-2", "DEMO-3", "+1"]);
        assert_eq!(pills(&owned[..3]), ["DEMO-1", "DEMO-2", "DEMO-3"]);
    }

    fn source(
        name: &str,
        ok: bool,
        count: usize,
        at: Option<i64>,
        msg: Option<&str>,
    ) -> SourceStatus {
        SourceStatus {
            name: name.into(),
            ok,
            count,
            at,
            message: msg.map(String::from),
        }
    }

    fn hhmm(t: i64) -> String {
        format!("{:02}:{:02}", t / 60, t % 60)
    }

    #[test]
    fn source_line_shows_missing_jira() {
        // The daemon lists beads first; the line leads with jira.
        let mut v = no_work();
        v.sources = vec![
            source("beads", true, 133, Some(14 * 60 + 5), None),
            source("jira", true, 12, Some(14 * 60 + 2), None),
        ];
        assert_eq!(source_line(&v, hhmm), "jira 12 14:02 · beads 133 14:05");
        assert!(!sources_failing(&v));
        v.sources[1] = source("jira", false, 0, None, Some("acli not found"));
        assert_eq!(
            source_line(&v, hhmm),
            "jira: acli not found · beads 133 14:05"
        );
        assert!(sources_failing(&v));
        // Nothing read yet: no time.
        v.sources[0] = source("beads", false, 0, None, Some("not read yet"));
        assert_eq!(
            source_line(&v, hhmm),
            "jira: acli not found · beads: not read yet"
        );
        assert_eq!(source_line(&no_work(), hhmm), "");
        assert!(!sources_failing(&no_work()));
    }

    #[test]
    fn a_failed_source_says_since_when_its_items_are() {
        let mut v = no_work();
        v.sources = vec![
            source("jira", false, 12, Some(13 * 60 + 40), Some("HTTP 401")),
            source("beads", true, 133, Some(14 * 60 + 5), None),
        ];
        assert_eq!(
            source_line(&v, hhmm),
            "jira: HTTP 401 (last 13:40) · beads 133 14:05"
        );
        assert!(sources_failing(&v));
    }

    #[test]
    fn a_kept_jira_list_shows_its_old_time_and_why() {
        let mut v = no_work();
        v.sources = vec![
            source(
                "jira",
                true,
                12,
                Some(13 * 60 + 40),
                Some("Jira returned no tickets; keeping the last list"),
            ),
            source("beads", true, 133, Some(14 * 60 + 5), None),
        ];
        assert_eq!(
            source_line(&v, hhmm),
            "jira 12 13:40: Jira returned no tickets; keeping the la… · beads 133 14:05"
        );
    }

    #[test]
    fn source_line_leaves_out_the_prs_source_when_off() {
        let mut v = no_work();
        v.sources = vec![
            source("jira", true, 12, Some(14 * 60 + 2), None),
            source("beads", true, 133, Some(14 * 60 + 5), None),
        ];
        let without = source_line(&v, hhmm);
        v.sources.push(source("prs", false, 0, None, Some(PRS_OFF)));
        assert_eq!(source_line(&v, hhmm), without);
        assert!(!sources_failing(&v));
        // A failing PR source still shows.
        v.sources[2] = source("prs", false, 0, None, Some("bb failed"));
        assert_eq!(
            source_line(&v, hhmm),
            "jira 12 14:02 · beads 133 14:05 · prs: bb failed"
        );
        assert!(sources_failing(&v));
    }

    #[test]
    fn source_line_cuts_long_errors() {
        let long = format!("acli: {}", "x".repeat(80));
        let mut v = no_work();
        v.sources = vec![source("jira", false, 0, None, Some(&long))];
        let line = source_line(&v, hhmm);
        let msg = line.strip_prefix("jira: ").unwrap();
        assert_eq!(msg.chars().count(), 41);
        assert!(msg.ends_with('…'));
        assert!(long.starts_with(msg.trim_end_matches('…')));
        assert_eq!(short_message("acli not found"), "acli not found");
        assert_eq!(short_message(&"y".repeat(40)), "y".repeat(40));
    }

    #[test]
    fn card_model_has_pills() {
        let v = work(vec![
            item("DEMO-12", ItemKind::Ticket, None, Some(3)),
            item("ov-5", ItemKind::Bead, Some("DEMO-12"), Some(3)),
            item("ov-3", ItemKind::Bead, None, Some(3)),
            item("DEMO-7", ItemKind::Ticket, None, Some(4)),
        ]);
        let st = SlotState::Running { pid: 1, since: 0 };
        let m = card_model(&info(3, "eks", st), 10, None, &v, &no_locks());
        assert_eq!(m.pills, ["DEMO-12", "ov-3"]);
        // An empty slot can already own work.
        let m = card_model(&info(4, "", SlotState::Empty), 10, None, &v, &no_locks());
        assert_eq!(m.pills, ["DEMO-7"]);
        assert!(
            card_model(&info(5, "", SlotState::Empty), 10, None, &v, &no_locks())
                .pills
                .is_empty()
        );
    }

    #[test]
    fn elapsed() {
        assert_eq!(format_elapsed(-5), "0s");
        assert_eq!(format_elapsed(0), "0s");
        assert_eq!(format_elapsed(59), "59s");
        assert_eq!(format_elapsed(60), "1m 00s");
        assert_eq!(format_elapsed(8 * 60 + 5), "8m 05s");
        assert_eq!(format_elapsed(3599), "59m 59s");
        assert_eq!(format_elapsed(3600), "1h 00m");
        assert_eq!(format_elapsed(2 * 3600 + 3 * 60 + 59), "2h 03m");
    }

    #[test]
    fn empty_card() {
        let m = cm(&info(5, "", SlotState::Empty), 100, None);
        assert_eq!(
            m,
            CardModel {
                title: "S5".into(),
                badge: "+ start".into(),
                tone: Tone::Empty,
                cwd: None,
                pid: None,
                now_doing: None,
                can_resume: false,
                pills: vec![],
                locks: vec![],
                waiting_on: None,
                pending: None,
            }
        );
    }

    #[test]
    fn running_card() {
        let st = SlotState::Running {
            pid: 4321,
            since: 1000,
        };
        let m = cm(
            &info(1, "health report", st),
            1000 + 485,
            Some(Path::new(HOME)),
        );
        assert_eq!(m.title, "S1 · health report");
        assert_eq!(m.badge, "● Running 8m 05s");
        assert_eq!(m.tone, Tone::Running);
        assert_eq!(m.cwd.as_deref(), Some("~/Repos/eks"));
        assert_eq!(m.pid.as_deref(), Some("pid 4321"));
    }

    fn with_activity(
        kind: ActivityKind,
        now_doing: Option<&str>,
        reason: Option<&str>,
    ) -> SlotInfo {
        let st = SlotState::Running {
            pid: 4321,
            since: 1000,
        };
        let mut i = info(2, "eks", st);
        i.activity = Some(Activity {
            kind,
            since: 2000,
            now_doing: now_doing.map(String::from),
            reason: reason.map(String::from),
            idle_wait: false,
        });
        i
    }

    #[test]
    fn card_model_for_each_activity() {
        let now = 2000 + 185;
        let w = cm(
            &with_activity(ActivityKind::Working, Some("Bash: cargo test"), None),
            now,
            None,
        );
        assert_eq!(w.badge, "● Working 3m 05s");
        assert_eq!(w.tone, Tone::Working);
        assert_eq!(tone_color(w.tone), colors::GREEN);
        assert_eq!(w.now_doing.as_deref(), Some("Bash: cargo test"));
        assert_eq!(w.pid.as_deref(), Some("pid 4321"));

        let n = cm(
            &with_activity(
                ActivityKind::NeedsYou,
                Some("Bash: rm -rf target"),
                Some("Permission to run Bash"),
            ),
            2000 + 60,
            None,
        );
        assert_eq!(n.badge, "● Needs you 1m 00s");
        assert_eq!(n.tone, Tone::NeedsYou);
        assert_eq!(tone_color(n.tone), colors::ORANGE);
        // The reason, not the tool, says what to do.
        assert_eq!(n.now_doing.as_deref(), Some("Permission to run Bash"));

        let i = cm(
            &with_activity(ActivityKind::Idle, Some("Edit"), None),
            2000 + 720,
            None,
        );
        assert_eq!(i.badge, "○ Your turn 12m 00s");
        assert_eq!(i.tone, Tone::YourTurn);
        assert_eq!(tone_color(i.tone), colors::ACCENT);
        assert_eq!(edge_color(i.tone), colors::ACCENT);
        assert_eq!(i.now_doing, None);

        // Working before the first tool, or needs you without a reason.
        let bare = cm(&with_activity(ActivityKind::Working, None, None), now, None);
        assert_eq!(bare.now_doing, None);
        let bare = cm(
            &with_activity(ActivityKind::NeedsYou, None, None),
            now,
            None,
        );
        assert_eq!(bare.now_doing, None);
    }

    #[test]
    fn running_without_activity_looks_as_before() {
        let st = SlotState::Running {
            pid: 4321,
            since: 1000,
        };
        let m = cm(&info(1, "htop", st), 1000 + 485, None);
        assert_eq!(m.badge, "● Running 8m 05s");
        assert_eq!(m.tone, Tone::Running);
        assert_eq!(tone_color(m.tone), colors::GREEN);
        assert_eq!(m.now_doing, None);
    }

    #[test]
    fn running_without_activity_shows_the_command() {
        let st = SlotState::Running {
            pid: 4321,
            since: 1000,
        };
        let mut i = info(2, "", st);
        i.command = Some(vec![
            "/opt/homebrew/bin/htop".into(),
            "-d".into(),
            "10".into(),
        ]);
        assert_eq!(cm(&i, 1000, None).now_doing.as_deref(), Some("htop -d 10"));
        i.command = Some(vec!["x".repeat(100)]);
        let long = cm(&i, 1000, None).now_doing.unwrap();
        assert_eq!(long.chars().count(), 81);
        assert!(long.ends_with('…'));
        i.command = Some(vec![]);
        assert_eq!(cm(&i, 1000, None).now_doing, None);
    }

    #[test]
    fn header_counts_sessions_that_need_you() {
        let mut slots = vec![
            with_activity(ActivityKind::NeedsYou, None, None),
            with_activity(ActivityKind::Working, None, None),
            with_activity(ActivityKind::NeedsYou, None, Some("x")),
            info(4, "", SlotState::Empty),
        ];
        assert_eq!(needs_you_count(&slots), 2);
        assert_eq!(header_text(3, 2, 0, (0, 0)), "3 running, 2 need you");
        assert_eq!(header_text(3, 1, 0, (0, 0)), "3 running, 1 need you");
        assert_eq!(header_text(3, 0, 0, (0, 0)), "3 running");
        assert_eq!(header_text(0, 0, 0, (0, 0)), "none running");
        slots.truncate(2);
        assert_eq!(needs_you_count(&slots), 1);
    }

    #[test]
    fn your_turn_activity_and_counts() {
        let a = Activity {
            kind: ActivityKind::Idle,
            since: 0,
            now_doing: None,
            reason: None,
            idle_wait: false,
        };
        assert_eq!(activity_state(&a, 185), "Your turn 3m 05s");
        let slots = vec![
            with_activity(ActivityKind::Idle, None, None),
            with_activity(ActivityKind::Idle, None, None),
            with_activity(ActivityKind::Working, None, None),
            with_activity(ActivityKind::NeedsYou, None, None),
            info(5, "", SlotState::Empty),
        ];
        assert_eq!(your_turn_count(&slots), 2);
        let mut exited = with_activity(ActivityKind::Idle, None, None);
        exited.state = SlotState::Exited {
            code: Some(0),
            at: 1,
        };
        assert_eq!(your_turn_count(&[exited]), 0);
        assert_eq!(
            header_text(3, 1, 2, (0, 0)),
            "3 running, 1 need you · 2 your turn"
        );
    }

    #[test]
    fn attention_text_leaves_out_zero_parts() {
        assert_eq!(
            attention_text(2, 1).as_deref(),
            Some("2 need you · 1 your turn")
        );
        assert_eq!(attention_text(0, 1).as_deref(), Some("1 your turn"));
        assert_eq!(attention_text(1, 0).as_deref(), Some("1 need you"));
        assert_eq!(attention_text(0, 0), None);
    }

    #[test]
    fn exited_card() {
        let st = SlotState::Exited {
            code: Some(3),
            at: 5,
        };
        let m = cm(&info(3, "", st), 9, None);
        assert_eq!(m.title, "S3");
        assert_eq!(m.badge, "Exited (code 3)");
        assert_eq!(m.tone, Tone::Exited);
        assert_eq!(m.cwd.as_deref(), Some("/Users/me/Repos/eks"));
        assert_eq!(m.pid, None);
        let killed = SlotState::Exited { code: None, at: 5 };
        assert_eq!(cm(&info(3, "", killed), 9, None).badge, "Exited");
    }

    #[test]
    fn resumable_cm() {
        let st = SlotState::Resumable {
            session_id: "abc".into(),
            at: 5,
        };
        let m = cm(&info(3, "eks", st), 9, Some(Path::new(HOME)));
        assert_eq!(m.title, "S3 · eks");
        assert_eq!(m.badge, "Resumable");
        assert_eq!(m.tone, Tone::Resumable);
        assert_eq!(tone_color(m.tone), colors::GREY);
        assert_eq!(m.cwd.as_deref(), Some("~/Repos/eks"));
        assert_eq!(m.pid, None);
        assert_eq!(m.now_doing, None);
        assert!(m.can_resume);
        // No other card offers it.
        let ex = SlotState::Exited { code: None, at: 5 };
        assert!(!cm(&info(3, "eks", ex), 9, None).can_resume);
    }

    #[test]
    fn tilde_paths() {
        let home = Some(Path::new(HOME));
        assert_eq!(tilde(Path::new("/Users/me"), home), "~");
        assert_eq!(tilde(Path::new("/Users/me/x"), home), "~/x");
        assert_eq!(tilde(Path::new("/Users/meow"), home), "/Users/meow");
        assert_eq!(tilde(Path::new("/tmp"), None), "/tmp");
    }

    #[test]
    fn header() {
        let r = SlotState::Running { pid: 1, since: 0 };
        let slots = vec![
            info(0, "", SlotState::Empty),
            info(1, "", r.clone()),
            info(3, "", r),
        ];
        assert_eq!(running_count(&slots), 2);
        assert_eq!(header_count(2), "2 running");
        assert_eq!(header_count(0), "none running");
    }

    #[test]
    fn selection_moves() {
        use Dir::*;
        assert_eq!(move_selection(0, Down), 1);
        assert_eq!(move_selection(0, Left), 0);
        assert_eq!(move_selection(0, Up), 0);
        assert_eq!(move_selection(1, Up), 0);
        assert_eq!(move_selection(3, Up), 0);
        assert_eq!(move_selection(1, Left), 1);
        assert_eq!(move_selection(1, Right), 2);
        assert_eq!(move_selection(3, Right), 3);
        assert_eq!(move_selection(2, Down), 5);
        assert_eq!(move_selection(5, Down), 5);
        assert_eq!(move_selection(5, Up), 2);
        assert_eq!(move_selection(4, Left), 4);
    }
    fn ticket(key: &str, status: &str, kind: ItemKind) -> WorkItem {
        WorkItem {
            key: key.into(),
            kind,
            title: "t".into(),
            status: status.into(),
            priority: None,
            parent: None,
            owner: None,
            in_list: true,
            folder: None,
            stage: None,
            stage_since: None,
        }
    }

    #[test]
    fn summary_lines_with_and_without_summary() {
        let mut view = crate::app::empty_coord();
        assert_eq!(
            summary_lines(&view, hhmm),
            (
                "No summary yet".to_string(),
                "Ask S0 \"what's going on?\" to get one.".to_string()
            )
        );
        view.summary = Some(overseer_proto::Summary {
            at: 14 * 60 + 5,
            text: "DEMO-12 is in review.".into(),
        });
        assert_eq!(
            summary_lines(&view, hhmm),
            (
                "Updated 14:05".to_string(),
                "DEMO-12 is in review.".to_string()
            )
        );
    }

    #[test]
    fn done_count_counts_done_closed_resolved_tickets_only() {
        let mut work = crate::app::empty_work();
        assert_eq!(done_count(&work), (0, 0));
        work.items = vec![
            ticket("DEMO-1", "Done", ItemKind::Ticket),
            ticket("DEMO-2", "closed", ItemKind::Ticket),
            ticket("DEMO-3", "RESOLVED", ItemKind::Ticket),
            ticket("DEMO-4", "In Progress", ItemKind::Ticket),
            ticket("demo-b1", "done", ItemKind::Bead),
        ];
        assert_eq!(done_count(&work), (3, 4));
    }

    #[test]
    fn done_count_leaves_out_tickets_known_only_from_beads() {
        let mut work = crate::app::empty_work();
        // Jira returned nothing: the ticket rows are built from bead titles.
        let placeholder = |key: &str| WorkItem {
            title: key.into(),
            ..ticket(key, overseer_proto::UNKNOWN_STATUS, ItemKind::Ticket)
        };
        work.items = vec![placeholder("DEMO-12"), placeholder("DEMO-13")];
        assert_eq!(done_count(&work), (0, 0));
        assert_eq!(header_text(1, 0, 0, done_count(&work)), "1 running");
        work.items.push(ticket("DEMO-14", "Done", ItemKind::Ticket));
        assert_eq!(done_count(&work), (1, 1));
    }

    #[test]
    fn header_text_has_done_count() {
        assert_eq!(
            header_text(3, 1, 0, (2, 5)),
            "3 running, 1 need you, done 2/5"
        );
        assert_eq!(header_text(0, 0, 0, (0, 4)), "none running, done 0/4");
        assert_eq!(header_text(3, 0, 0, (0, 0)), "3 running");
    }

    #[test]
    fn plan_line_names_the_pending_plan() {
        let mut view = crate::app::empty_coord();
        assert_eq!(plan_line(&view), None);
        view.plans
            .push(crate::app::test_plan(7, overseer_proto::PlanState::Pending));
        assert_eq!(
            plan_line(&view).as_deref(),
            Some("Plan 7 waiting for you (p)")
        );
    }

    #[test]
    fn plan_line_names_the_plan_a_click_opens() {
        use overseer_proto::PlanState;
        let mut view = crate::app::empty_coord();
        view.plans
            .push(crate::app::test_plan(5, PlanState::Approved));
        // Only an approved plan: no line, so nothing to click that would
        // find no plan waiting.
        assert_eq!(plan_line(&view), None);
        assert!(view.pending_plan().is_none());
        view.plans
            .push(crate::app::test_plan(7, PlanState::Pending));
        let opened = view.pending_plan().map(|p| p.id);
        assert_eq!(opened, Some(7));
        assert_eq!(
            plan_line(&view),
            Some(format!("Plan {} waiting for you (p)", opened.unwrap()))
        );
    }

    #[test]
    fn outcome_lines_show_the_newest_approved_plan() {
        let mut view = crate::app::empty_coord();
        assert!(outcome_lines(&view).is_empty());
        let mut ok = crate::app::test_plan(8, overseer_proto::PlanState::Approved);
        ok.outcome = vec!["S3: assigned DEMO-12".into(), "S3: started in /x".into()];
        view.plans.push(ok);
        assert_eq!(
            outcome_lines(&view),
            vec!["Plan 8 approved: 2 steps, none failed"]
        );
        let mut bad = crate::app::test_plan(9, overseer_proto::PlanState::Approved);
        bad.outcome = vec![
            "S3: assigned DEMO-12".into(),
            "S4: worktree failed: no repo; the rest of S4's plan was skipped".into(),
        ];
        view.plans.insert(0, bad);
        assert_eq!(
            outcome_lines(&view),
            vec![
                "Plan 9 approved: 2 steps, 1 failed".to_string(),
                "S4: worktree failed: no repo; the rest of S4's plan was skipped".to_string()
            ]
        );
    }
}
