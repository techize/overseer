//! The board: header, coordinator panel with the S0 card, and a 3 by 2 grid
//! of S1 to S6 cards (layout A from the design).
//!
//! The card contents are worked out by `card_model`, a pure function, so the
//! badge text, elapsed time and empty, running and exited cards are tested
//! without a window.
//!
//! A running claude slot also carries its hook-reported activity: the badge
//! then says Working, Needs you or Idle with the time in that state, and a
//! line under the cwd says what it is doing or why it needs you. A running
//! slot without activity (any other command) keeps the plain Running badge,
//! with its command (`htop`) on the line under the cwd.
//!
//! Each card also shows the work its slot owns as pills under the title,
//! and the header ends with the state of the work sources (`source_line`).

use std::path::Path;

use gpui::{div, prelude::*, px, rgb, Context, FontWeight, Window};
use overseer_proto::{
    slot_name, Activity, ActivityKind, BrainView, ClaimKind, ItemKind, LockView, QueueEntry,
    QueueState, RelayState, Sender, SlotId, SlotInfo, SlotState, WorkItem, WorkView,
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

/// The header's source health: `jira 12 · beads 133 · 14:05`, or
/// `jira: acli not found · beads 133 · 14:05` when a source failed. Jira
/// leads; the time is the newest successful read, through `clock`.
pub fn source_line(view: &WorkView, clock: impl Fn(i64) -> String) -> String {
    let mut sources: Vec<_> = view.sources.iter().collect();
    sources.sort_by_key(|s| s.name != "jira");
    let mut parts: Vec<String> = sources
        .iter()
        .map(|s| {
            if s.ok {
                format!("{} {}", s.name, s.count)
            } else {
                let msg = s.message.as_deref().unwrap_or("failed");
                format!("{}: {}", s.name, short_message(msg))
            }
        })
        .collect();
    if let Some(at) = sources.iter().filter(|s| s.ok).filter_map(|s| s.at).max() {
        parts.push(clock(at));
    }
    parts.join(" · ")
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

/// Local `HH:MM` for unix seconds.
pub fn local_hhmm(t: i64) -> String {
    let full = crate::session::local_clock(t);
    full.get(..5).unwrap_or(&full).to_string()
}

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
        ActivityKind::Idle => "Idle",
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
                    format!("● {}", activity_state(a, now)),
                    match a.kind {
                        ActivityKind::Working => Tone::Working,
                        ActivityKind::NeedsYou => Tone::NeedsYou,
                        ActivityKind::Idle => Tone::Idle,
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
/// time.
pub fn strip_lines(view: &LockView, clock: impl Fn(i64) -> String, max: usize) -> Vec<String> {
    let when = |r: &overseer_proto::Relay| r.delivered_at.or(r.dropped_at).unwrap_or(r.at);
    let mut relays: Vec<_> = view.relays.iter().collect();
    relays.sort_by_key(|r| std::cmp::Reverse((when(r), r.id)));
    relays
        .into_iter()
        .take(max)
        .map(|r| {
            let text = one_line(&r.text, 80);
            let (time, to) = (clock(when(r)), slot_name(r.to));
            match r.state {
                RelayState::Delivered => format!("{time} told {to}: {text}"),
                RelayState::Pending => format!("{time} for {to}, pending: {text}"),
                RelayState::Dropped => {
                    format!("{time} for {to}, dropped: session ended: {text}")
                }
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

/// The 2ndBrain strip's text and colour. Writer off beats a push problem,
/// which beats a failure, which beats a queue, which beats the last
/// commit time. Problems use the warning tone (orange).
pub fn strip_text(v: &BrainView) -> (String, Tone) {
    if !v.on {
        return ("2ndBrain: writer off".into(), Tone::Empty);
    }
    if v.push_problem.is_some() {
        return ("2ndBrain: push needs you".into(), Tone::NeedsYou);
    }
    if let Some(why) = brain_failure(v) {
        return (format!("2ndBrain: {why}"), Tone::NeedsYou);
    }
    let queued = v
        .entries
        .iter()
        .filter(|e| e.state == QueueState::Queued)
        .count();
    if queued > 0 {
        return (format!("2ndBrain: {queued} queued"), Tone::Idle);
    }
    match v
        .entries
        .iter()
        .filter(|e| e.state == QueueState::Committed)
        .map(|e| e.at)
        .max()
    {
        Some(at) => (
            format!("2ndBrain: committed {}", local_hhmm(at)),
            Tone::Idle,
        ),
        None => ("2ndBrain: nothing yet".into(), Tone::Idle),
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
    format!("{} {who}  {what}  {state}", local_hhmm(e.at))
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

/// The header's right-hand text with the sessions that need Jonathan:
/// `3 running, 1 need you`.
pub fn header_text(running: usize, need_you: usize) -> String {
    match need_you {
        0 => header_count(running),
        n => format!("{}, {n} need you", header_count(running)),
    }
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
        Tone::Blocked => colors::RED,
        Tone::Idle | Tone::Exited | Tone::Resumable | Tone::Empty => colors::GREY,
    }
}

/// The card's left edge: the badge colour while the slot runs.
fn edge_color(tone: Tone) -> u32 {
    match tone {
        Tone::Running | Tone::Working | Tone::NeedsYou | Tone::Idle | Tone::Blocked => {
            tone_color(tone)
        }
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
        let strip = strip_lines(&self.locks, local_hhmm, 5);
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
            .on_action(
                cx.listener(|this, _: &crate::app::OpenSelected, window, cx| {
                    this.open_card(this.selected, window, cx)
                }),
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
                            .text_lg()
                            .font_weight(FontWeight::BOLD)
                            .child("Overseer"),
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
                                    } else {
                                        colors::MUTED
                                    }))
                                    .child(header_text(running, need_you)),
                            )
                            .child(
                                div()
                                    .text_color(rgb(colors::MUTED))
                                    .child(source_line(&self.work, local_hhmm)),
                            ),
                    ),
            )
            .child(
                div()
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
                            .text_sm()
                            .text_color(rgb(colors::TEXT_DIM))
                            .child("Coordinator summary appears here in phase 6"),
                    )
                    .child(div().flex().child(s0)),
            )
            .child(div().flex().flex_col().gap_2().child(top).child(bottom))
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
                    .children(strip.into_iter().map(|l| div().truncate().child(l)))
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
                            .child("Click a card or press Enter to open it · A assigns work · Cmd+0 to Cmd+6 open a session · Cmd+N starts the selected slot"),
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
        }
    }

    const HOME: &str = "/Users/me";

    fn no_work() -> WorkView {
        WorkView {
            items: vec![],
            sources: vec![],
            notices: vec![],
        }
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
            held: None,
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
        assert_eq!(m.tone, Tone::Idle);
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
        }
    }

    fn brain(entries: Vec<QueueEntry>) -> BrainView {
        BrainView {
            on: true,
            entries,
            last_backup: None,
            last_sync: None,
            push_problem: None,
            brain_dir: None,
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
        assert_eq!(text, "2ndBrain: 2 queued");
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
        assert_eq!(text, format!("2ndBrain: committed {}", local_hhmm(2000)));
        assert_eq!(tone, Tone::Idle);
    }

    #[test]
    fn strip_says_push_needs_you_in_orange() {
        let mut v = brain(vec![q(1, 10, QueueState::Queued)]);
        v.push_problem = Some("push rejected".into());
        let (text, tone) = strip_text(&v);
        assert_eq!(text, "2ndBrain: push needs you");
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
            "2ndBrain: commit failed: index.lock still held after 4 attempts"
        );
        assert_eq!(tone, Tone::NeedsYou);
        assert_eq!(tone_color(tone), colors::ORANGE);
        // A long reason is cut short.
        let mut long = q(2, 20, QueueState::Failed);
        long.error = Some(format!("pathspec {} did not match", "x".repeat(80)));
        let (text, _) = strip_text(&brain(vec![long]));
        assert!(
            text.starts_with("2ndBrain: commit failed: pathspec xxx"),
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
        assert_eq!(text, "2ndBrain: backup failed: git status failed");
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
        assert_eq!(text, "2ndBrain: sync failed: held: git status failed");
        assert_eq!(tone, Tone::NeedsYou);
        // A push problem still comes first.
        v.push_problem = Some("push rejected".into());
        assert_eq!(strip_text(&v).0, "2ndBrain: push needs you");
    }

    #[test]
    fn strip_writer_off_is_grey_and_beats_everything() {
        let mut v = brain(vec![q(1, 10, QueueState::Queued)]);
        v.on = false;
        v.push_problem = Some("push rejected".into());
        let (text, tone) = strip_text(&v);
        assert_eq!(text, "2ndBrain: writer off");
        assert_eq!(tone, Tone::Empty);
        assert_eq!(tone_color(tone), colors::GREY);
    }

    #[test]
    fn strip_with_nothing_yet() {
        let (text, tone) = strip_text(&brain(vec![q(1, 10, QueueState::NothingToCommit)]));
        assert_eq!(text, "2ndBrain: nothing yet");
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
        let lines = strip_lines(&v, clock, 3);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "t500 told S3: done it");
        assert_eq!(lines[1], "t300 for S4, pending: wait for it");
        let cut = lines[2].strip_prefix("t200 for S5, pending: ").unwrap();
        assert!(cut.starts_with("line one xxx"), "{cut}");
        assert_eq!(cut.chars().count(), 81);
        assert!(cut.ends_with('…'));
        assert_eq!(strip_lines(&v, clock, 10).len(), 4);
    }

    #[test]
    fn strip_shows_a_dropped_relay_at_its_drop_time() {
        let mut v = no_locks();
        v.relays = vec![
            Relay {
                state: RelayState::Dropped,
                dropped_at: Some(400),
                ..relay(1, 3, 100, None, "hello")
            },
            relay(2, 4, 300, None, "wait for it"),
        ];
        let lines = strip_lines(&v, |t: i64| format!("t{t}"), 5);
        assert_eq!(
            lines,
            [
                "t400 for S3, dropped: session ended: hello",
                "t300 for S4, pending: wait for it"
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
        assert_eq!(source_line(&v, hhmm), "jira 12 · beads 133 · 14:05");
        v.sources[1] = source("jira", false, 0, None, Some("acli not found"));
        assert_eq!(
            source_line(&v, hhmm),
            "jira: acli not found · beads 133 · 14:05"
        );
        // Nothing read yet: no time.
        v.sources[0] = source("beads", false, 0, None, Some("not read yet"));
        assert_eq!(
            source_line(&v, hhmm),
            "jira: acli not found · beads: not read yet"
        );
        assert_eq!(source_line(&no_work(), hhmm), "");
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
        assert_eq!(i.badge, "● Idle 12m 00s");
        assert_eq!(i.tone, Tone::Idle);
        assert_eq!(tone_color(i.tone), colors::GREY);
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
        assert_eq!(header_text(3, 2), "3 running, 2 need you");
        assert_eq!(header_text(3, 1), "3 running, 1 need you");
        assert_eq!(header_text(3, 0), "3 running");
        assert_eq!(header_text(0, 0), "none running");
        slots.truncate(2);
        assert_eq!(needs_you_count(&slots), 1);
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
}
