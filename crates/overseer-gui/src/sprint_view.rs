//! The sprint kanban and open-PR panel models: pure functions from a
//! `WorkView` (and the clock, passed in as `now`) to what the board draws.

use std::path::PathBuf;

use gpui::{
    canvas, div, prelude::*, px, relative, rgb, rgba, ClickEvent, Context, CursorStyle, ElementId,
    FontWeight, MouseButton, MouseDownEvent, Rgba, SharedString, Window,
};
use overseer_proto::{
    slot_name, ItemKind, PullRequest, SlotId, SlotInfo, SlotState, Stage, WorkItem, WorkView,
    PRS_OFF,
};

use crate::app::{colors, open_external, ticket_click, OverseerApp, TicketClick};
use crate::board::{activity_state, format_elapsed};

const DAY: i64 = 86_400;
/// Height of the kanban block. Fixed so the board never grows with the
/// ticket count: the header, the six session cards and this fit a 1440 by
/// 900 window, and each column scrolls inside it.
/// Rows the PR panel shows before `+N more`.
const MAX_PR_ROWS: usize = 8;

/// The tickets in the sprint list: tickets still in the list, without the
/// placeholders made from bead titles.
pub fn sprint_tickets(view: &WorkView) -> Vec<&WorkItem> {
    view.items
        .iter()
        .filter(|i| i.kind == ItemKind::Ticket && i.in_list && !i.is_placeholder())
        .collect()
}

/// Ticket priority as the board shows it; the order is the sort order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Prio {
    Breaker,
    High,
    Med,
    Low,
}

pub fn prio_of(priority: Option<&str>) -> Prio {
    match priority {
        Some("Sprint Breaker") => Prio::Breaker,
        Some("Highest") | Some("High") => Prio::High,
        Some("Low") | Some("Lowest") => Prio::Low,
        _ => Prio::Med,
    }
}

/// `12m`, `5h`, `3d`: the largest whole unit. Negative counts as 0.
pub fn age_text(secs: i64) -> String {
    let s = secs.max(0);
    if s < 3600 {
        format!("{}m", s / 60)
    } else if s < DAY {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / DAY)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgeTone {
    Fresh,
    Ageing,
    Old,
    /// The PR carries no creation date.
    Unknown,
}

/// A PR under 2 days old is fresh, from 2 days ageing, from 5 days old.
pub fn pr_age_tone(secs: i64) -> AgeTone {
    if secs >= 5 * DAY {
        AgeTone::Old
    } else if secs >= 2 * DAY {
        AgeTone::Ageing
    } else {
        AgeTone::Fresh
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TicketCard {
    pub key: String,
    pub title: String,
    pub prio: Prio,
    /// How long it has been in its stage, when the daemon saw it move.
    pub age: Option<String>,
    pub slot: Option<SlotId>,
    /// `#<id> <age>` of the oldest linked PR (no age when undated) and its
    /// tone.
    pub pr: Option<(String, AgeTone)>,
    /// Beads grouped under the ticket.
    pub beads: usize,
    /// The Jira status, when the ticket is In progress only because no
    /// stage list named that status.
    pub raw_status: Option<String>,
    pub folder: Option<PathBuf>,
    /// The Jira status as Jira names it, always.
    pub status: String,
    /// The Jira priority, when set.
    pub priority: Option<String>,
    /// The column the card sits in.
    pub stage: Stage,
    /// The oldest linked PR, the one the chip shows.
    pub linked_pr: Option<PullRequest>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub stage: Stage,
    pub total: usize,
    pub cards: Vec<TicketCard>,
}

fn card(view: &WorkView, t: &WorkItem, stage: Stage, now: i64) -> TicketCard {
    let mut linked: Vec<_> = view
        .prs
        .iter()
        .filter(|p| p.ticket.as_deref() == Some(t.key.as_str()))
        .collect();
    // Oldest first; a PR with no date last.
    linked.sort_by_key(|p| (p.created.is_none(), p.created));
    let pr = linked.first().map(|p| {
        let tone = p.created.map_or(AgeTone::Unknown, |c| pr_age_tone(now - c));
        let chip = match p.created {
            Some(c) => format!("#{} {}", p.id, age_text(now - c)),
            None => format!("#{}", p.id),
        };
        (chip, tone)
    });
    let raw = t.status.trim();
    let raw_status = (t.stage == Some(Stage::InProgress)
        && !raw.is_empty()
        && raw.to_lowercase() != "in progress")
        .then(|| t.status.clone());
    TicketCard {
        key: t.key.clone(),
        title: t.title.clone(),
        prio: prio_of(t.priority.as_deref()),
        age: t.stage_since.map(|s| age_text(now - s)),
        slot: t.owner,
        pr,
        beads: view
            .items
            .iter()
            .filter(|b| b.kind == ItemKind::Bead && b.parent.as_deref() == Some(t.key.as_str()))
            .count(),
        raw_status,
        folder: t.folder.clone(),
        status: t.status.clone(),
        priority: t.priority.clone(),
        stage,
        linked_pr: linked.first().map(|p| (*p).clone()),
    }
}

/// What the hover card on a ticket shows: label and value rows, the key,
/// the full title and the click hint.
#[derive(Debug, Clone, PartialEq)]
pub struct HoverModel {
    pub lines: Vec<(String, String)>,
    pub title: String,
    pub key: String,
    pub hint: String,
}

/// The hover card's last line: what a click and a Cmd+click do.
pub fn hover_hint(click: &TicketClick, has_folder: bool) -> String {
    let mut hint = match click {
        TicketClick::Open(s) => format!("Click: open {}", slot_name(*s)),
        TicketClick::Start(s) => format!("Click: start in {}", slot_name(*s)),
        TicketClick::NoSlot => "Click: no free slot".to_string(),
    };
    if has_folder {
        hint.push_str(" · Cmd+click: folder");
    }
    hint
}

/// `S2 · Your turn 1m 00s`: the owner and what its session is doing, or
/// `Not started` with no owner.
fn session_text(owner: Option<SlotId>, slots: &[SlotInfo], now: i64) -> String {
    let Some(o) = owner else {
        return "Not started".into();
    };
    let state = match slots.get(o as usize) {
        Some(info) => match (&info.state, &info.activity) {
            (SlotState::Running { .. }, Some(a)) => activity_state(a, now),
            (SlotState::Running { since, .. }, None) => {
                format!("Running {}", format_elapsed(now - since))
            }
            (SlotState::Resumable { .. }, _) => "Resumable".into(),
            (SlotState::Exited { .. }, _) => "Exited".into(),
            (SlotState::Empty, _) => "Not running".into(),
        },
        None => "Not running".into(),
    };
    format!("{} · {state}", slot_name(o))
}

/// The hover card for a ticket card, given the slots (for the session and
/// what a click would do).
pub fn hover_model(c: &TicketCard, slots: &[SlotInfo], work: &WorkView, now: i64) -> HoverModel {
    let mut lines = vec![
        ("Status".to_string(), c.status.clone()),
        (
            "Priority".to_string(),
            c.priority.clone().unwrap_or_else(|| "Not set".into()),
        ),
        (
            "Stage".to_string(),
            match &c.age {
                Some(a) => format!("{} · {a}", c.stage.label()),
                None => c.stage.label().to_string(),
            },
        ),
        ("Session".to_string(), session_text(c.slot, slots, now)),
    ];
    if let Some(p) = &c.linked_pr {
        let mut text = format!("{} #{} {}", p.repo, p.id, p.title);
        if let Some(created) = p.created {
            text.push(' ');
            text.push_str(&age_text(now - created));
        }
        if let Some(a) = p.approvals {
            let state = if a.ready() {
                "ready to merge"
            } else if a.changes_requested {
                "changes requested"
            } else {
                ""
            };
            for part in [a.badge().as_str(), state] {
                if !part.is_empty() {
                    text.push_str(" · ");
                    text.push_str(part);
                }
            }
        }
        lines.push(("PR".to_string(), text));
    }
    if c.beads > 0 {
        lines.push(("Beads".to_string(), c.beads.to_string()));
    }
    HoverModel {
        lines,
        title: c.title.clone(),
        key: c.key.clone(),
        hint: hover_hint(&ticket_click(c.slot, slots, work), c.folder.is_some()),
    }
}

/// Six columns in `Stage::ALL` order, every ticket in its column. Cards
/// sort by priority, then by the time in stage (oldest first, unknown
/// last). The Done column lists the latest finished first, unknown last.
pub fn columns(view: &WorkView, now: i64) -> Vec<Column> {
    let tickets = sprint_tickets(view);
    Stage::ALL
        .iter()
        .map(|&stage| {
            let mut mine: Vec<&WorkItem> = tickets
                .iter()
                .copied()
                .filter(|t| board_stage(t) == Some(stage))
                .collect();
            let total = mine.len();
            if stage == Stage::Done {
                mine.sort_by_key(|t| (t.stage_since.is_none(), std::cmp::Reverse(t.stage_since)));
            } else {
                mine.sort_by_key(|t| {
                    (
                        prio_of(t.priority.as_deref()),
                        t.stage_since.is_none(),
                        t.stage_since,
                    )
                });
            }
            Column {
                stage,
                total,
                cards: mine
                    .into_iter()
                    .map(|t| card(view, t, stage, now))
                    .collect(),
            }
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
    /// Tickets per stage, in `Stage::ALL` order.
    pub per_stage: [usize; 6],
    /// Tickets that should be done by today at an even pace.
    pub expected: Option<usize>,
    /// Sprint Breaker tickets not yet done.
    pub breakers_open: usize,
}

/// The stage the board counts a ticket in: its stage, or for a ticket with
/// none (an old daemon) Done when its status is `done`, `closed` or
/// `resolved`, any case, and no stage otherwise.
pub fn board_stage(t: &WorkItem) -> Option<Stage> {
    t.stage.or_else(|| {
        matches!(
            t.status.trim().to_lowercase().as_str(),
            "done" | "closed" | "resolved"
        )
        .then_some(Stage::Done)
    })
}

/// `None` when the sprint has no tickets. `done` is also the header's done
/// count (`board::done_count`) and the Done column's count.
pub fn progress(view: &WorkView) -> Option<Progress> {
    let tickets = sprint_tickets(view);
    let total = tickets.len();
    if total == 0 {
        return None;
    }
    let mut per_stage = [0; 6];
    for t in &tickets {
        if let Some(i) = board_stage(t).and_then(|s| Stage::ALL.iter().position(|&x| x == s)) {
            per_stage[i] += 1;
        }
    }
    let done = per_stage[Stage::ALL.len() - 1];
    // Past the last day (a long sprint) the pace still tops out at all.
    let expected = view
        .sprint_day
        .filter(|d| d.days > 0)
        .map(|d| (total * d.day as usize / d.days as usize).min(total));
    let breakers_open = tickets
        .iter()
        .filter(|t| {
            board_stage(t) != Some(Stage::Done) && prio_of(t.priority.as_deref()) == Prio::Breaker
        })
        .count();
    Some(Progress {
        done,
        total,
        per_stage,
        expected,
        breakers_open,
    })
}

/// The warning when fewer tickets are done than an even pace expects.
pub fn behind_text(p: &Progress) -> Option<String> {
    let expected = p.expected?;
    (p.done < expected).then(|| format!("Behind pace: {expected} expected done by today"))
}

#[derive(Debug, Clone, PartialEq)]
pub struct PrRow {
    pub repo: String,
    pub id: u64,
    pub ticket: Option<String>,
    pub title: String,
    pub age: Option<String>,
    pub tone: AgeTone,
    pub url: Option<String>,
    /// The Running or Resumable slot that owns the PR's ticket: a click
    /// opens it.
    pub slot: Option<SlotId>,
    /// `1H 1AI`, or empty with no approvals or no participants read.
    pub approvals: String,
    /// Approved enough to merge.
    pub ready: bool,
    /// Someone asked for changes.
    pub changes: bool,
}

/// The Running or Resumable slot owning `ticket`, if any: the same owners
/// a click on the ticket card opens.
fn live_owner(view: &WorkView, slots: &[SlotInfo], ticket: &str) -> Option<SlotId> {
    let owner = view.items.iter().find(|i| i.key == ticket)?.owner?;
    slots
        .get(owner as usize)
        .filter(|i| {
            matches!(
                i.state,
                SlotState::Running { .. } | SlotState::Resumable { .. }
            )
        })
        .map(|i| i.id)
}

/// Open PRs ready to merge first, then oldest first (no date last), at
/// most 8, and how many are left. A row is linked to its ticket's owner
/// while that slot is Running or Resumable.
pub fn pr_rows(view: &WorkView, slots: &[SlotInfo], now: i64) -> (Vec<PrRow>, usize) {
    let mut prs: Vec<_> = view.prs.iter().collect();
    prs.sort_by_key(|p| (!pr_ready(p), p.created.is_none(), p.created));
    let more = prs.len().saturating_sub(MAX_PR_ROWS);
    prs.truncate(MAX_PR_ROWS);
    let rows = prs
        .into_iter()
        .map(|p| PrRow {
            repo: p.repo.clone(),
            id: p.id,
            ticket: p.ticket.clone(),
            title: p.title.clone(),
            age: p.created.map(|c| age_text(now - c)),
            tone: p.created.map_or(AgeTone::Unknown, |c| pr_age_tone(now - c)),
            url: p.url.clone(),
            slot: p.ticket.as_deref().and_then(|t| live_owner(view, slots, t)),
            approvals: p.approvals.map(|a| a.badge()).unwrap_or_default(),
            ready: pr_ready(p),
            changes: p.approvals.is_some_and(|a| a.changes_requested),
        })
        .collect();
    (rows, more)
}

/// Bitbucket reports enough approvals to merge `p`.
fn pr_ready(p: &PullRequest) -> bool {
    p.approvals.is_some_and(|a| a.ready())
}

/// `OPEN PRS · 3`, or `OPEN PRS · 3 · oldest 6d` when any PR has a date.
pub fn pr_header(view: &WorkView, now: i64) -> String {
    let mut text = format!("OPEN PRS · {}", view.prs.len());
    if let Some(oldest) = view.prs.iter().filter_map(|p| p.created).min() {
        text.push_str(&format!(" · oldest {}", age_text(now - oldest)));
    }
    text
}

/// The PR panel shows while the daemon reports a `prs` source that is not
/// off. An old daemon sends none, so it stays hidden there too.
pub fn pr_panel_shown(view: &WorkView) -> bool {
    view.sources
        .iter()
        .any(|s| s.name == "prs" && s.message.as_deref() != Some(PRS_OFF))
}

/// The PR source's state for the panel: a word and the line to show, or
/// `None` when the last read was clean. A source that never answered is
/// `not read yet` or `failed`; one that answered before and fails now is
/// `stale`, since the rows shown are from the earlier read.
pub fn pr_source_note(view: &WorkView) -> Option<(&'static str, String)> {
    let s = view.sources.iter().find(|s| s.name == "prs")?;
    if s.ok {
        return None;
    }
    let msg = s.message.as_deref().unwrap_or("").trim();
    let short: String = if msg.chars().count() > 100 {
        msg.chars().take(100).chain("...".chars()).collect()
    } else {
        msg.to_owned()
    };
    Some(match (s.at.is_some(), msg) {
        (false, "not read yet") => ("not read yet", "not read yet".to_owned()),
        (false, _) => ("failed", format!("failed: {short}")),
        (true, _) => ("stale", format!("stale: {short}")),
    })
}

/// Whether the PR source has read successfully at least once, so an empty
/// list means no open PRs and not no answer.
pub fn pr_answered(view: &WorkView) -> bool {
    view.sources
        .iter()
        .any(|s| s.name == "prs" && s.at.is_some())
}

/// Only web links go to `open`: not a `file:` URL, an app path or an option.
pub fn is_http_url(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

/// `1 sprint breaker open`, `2 sprint breakers open`.
pub fn breakers_text(n: usize) -> String {
    match n {
        1 => "1 sprint breaker open".into(),
        n => format!("{n} sprint breakers open"),
    }
}

/// The chip text, background and foreground for a priority.
pub fn prio_chip(prio: Prio) -> (&'static str, u32, u32) {
    match prio {
        Prio::Breaker => ("BREAKER", 0x4a1d1d, 0xff9a94),
        Prio::High => ("HIGH", 0x45290f, 0xf0a868),
        Prio::Med => ("MED", 0x232a38, 0xaab3c2),
        Prio::Low => ("LOW", 0x1b2029, 0x8b949e),
    }
}

/// The column heading's icon. Blocked is `‖`, not the pause emoji, which
/// would draw in colour.
pub fn stage_icon(stage: Stage) -> &'static str {
    match stage {
        Stage::ToDo => "○",
        Stage::Blocked => "‖",
        Stage::Next => "→",
        Stage::InProgress => "▶",
        Stage::ToBeTested => "⚗",
        Stage::Done => "✓",
    }
}

/// The stage's colour for headings and counts.
pub fn stage_color(stage: Stage) -> u32 {
    match stage {
        Stage::ToDo => colors::GREY,
        Stage::InProgress | Stage::Done => colors::GREEN,
        Stage::Next => colors::ACCENT,
        Stage::ToBeTested => colors::PURPLE,
        Stage::Blocked => colors::ORANGE,
    }
}

/// The stage's fill in the progress bar: In progress is a dimmed green so
/// Done stands out at full strength.
fn stage_fill(stage: Stage) -> Rgba {
    match stage {
        Stage::InProgress => rgba((colors::GREEN << 8) | 0x8c),
        s => rgb(stage_color(s)),
    }
}

pub fn tone_color(tone: AgeTone) -> u32 {
    match tone {
        AgeTone::Fresh => colors::GREEN,
        AgeTone::Ageing => colors::ORANGE,
        AgeTone::Old => colors::RED,
        AgeTone::Unknown => colors::MUTED,
    }
}

/// The tooltip on a PR's age dot.
pub fn tone_word(tone: AgeTone) -> &'static str {
    match tone {
        AgeTone::Fresh => "fresh",
        AgeTone::Ageing => "ageing",
        AgeTone::Old => "old",
        AgeTone::Unknown => "age unknown",
    }
}

pub fn beads_text(n: usize) -> String {
    match n {
        1 => "1 bead".into(),
        n => format!("{n} beads"),
    }
}

/// Each non-empty stage and its share of the bar: Done first, then the
/// other five in `Stage::ALL` order.
pub fn bar_segments(p: &Progress) -> Vec<(Stage, f32)> {
    let done = Stage::Done;
    std::iter::once(done)
        .chain(Stage::ALL.iter().copied().filter(|&s| s != done))
        .filter_map(|s| {
            let i = Stage::ALL.iter().position(|&x| x == s)?;
            let n = p.per_stage[i];
            (n > 0).then(|| (s, n as f32 / p.total as f32))
        })
        .collect()
}

/// Left margin for the 2px pace tick so it stays inside the bar at the
/// far end: nothing at 0, the full tick width at 1.
pub fn tick_margin(at: f32) -> f32 {
    -2.0 * at.clamp(0.0, 1.0)
}

/// Where the pace tick sits along the bar, 0 to 1.
pub fn tick_at(p: &Progress) -> Option<f32> {
    p.expected.map(|e| e as f32 / p.total as f32)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Plain text in a small dark box, for `.tooltip`.
struct TextTooltip(SharedString);

impl Render for TextTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded(px(4.))
            .text_xs()
            .bg(rgb(colors::CARD_HOVER))
            .border_1()
            .border_color(rgb(colors::BORDER_COORD))
            .text_color(rgb(colors::TEXT))
            .child(self.0.clone())
    }
}

/// The hover card on a ticket: the key in the accent colour, the title
/// wrapped, label and value rows, and the click hint in grey.
struct HoverCard(HoverModel);

impl Render for HoverCard {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let m = &self.0;
        div()
            .max_w(px(360.))
            .flex()
            .flex_col()
            .gap_1()
            .px_2()
            .py_1()
            .rounded(px(4.))
            .text_xs()
            .bg(rgb(colors::CARD_HOVER))
            .border_1()
            .border_color(rgb(colors::BORDER_COORD))
            .text_color(rgb(colors::TEXT))
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(colors::ACCENT))
                    .child(m.key.clone()),
            )
            .child(div().child(m.title.clone()))
            .children(m.lines.iter().map(|(label, value)| {
                div()
                    .flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_none()
                            .w(px(56.))
                            .text_color(rgb(colors::MUTED))
                            .child(label.clone()),
                    )
                    .child(div().flex_1().min_w(px(0.)).child(value.clone()))
            }))
            .child(div().text_color(rgb(colors::MUTED)).child(m.hint.clone()))
    }
}

/// Small caps-style section heading in the accent colour.
fn heading(text: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(colors::ACCENT))
        .child(text.into())
}

fn chip(text: impl Into<SharedString>, bg: u32, fg: u32) -> gpui::Div {
    div()
        .flex_none()
        .px_1()
        .rounded(px(4.))
        .text_xs()
        .font_weight(FontWeight::BOLD)
        .bg(rgb(bg))
        .text_color(rgb(fg))
        .child(text.into())
}

/// One kanban card: priority chip and key, the title on up to two lines,
/// then slot, PR, beads, raw status and the age.
fn ticket_card(
    c: TicketCard,
    hover: HoverModel,
    cx: &mut Context<OverseerApp>,
) -> impl IntoElement {
    let (prio, bg, fg) = prio_chip(c.prio);
    let pr_key = c.key.clone();
    let click_key = c.key.clone();
    let owner = c.slot;
    let folder = c.folder.clone();
    div()
        .id(ElementId::Name(format!("kanban-{}", c.key).into()))
        .flex_none()
        .flex()
        .flex_col()
        .gap_1()
        .px_2()
        .py_1()
        .rounded(px(6.))
        .bg(rgb(colors::CARD))
        .border_1()
        .border_color(rgb(colors::BORDER))
        .child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(chip(prio, bg, fg))
                .child(
                    div()
                        .min_w(px(0.))
                        .truncate()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(colors::ACCENT))
                        .child(c.key),
                ),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(colors::TEXT))
                .line_clamp(2)
                .child(c.title),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .text_xs()
                .overflow_hidden()
                .children(c.slot.map(|s| {
                    div()
                        .flex_none()
                        .px_1()
                        .rounded(px(4.))
                        .bg(rgb(colors::ACCENT_BG))
                        .text_color(rgb(colors::ACCENT))
                        .child(slot_name(s))
                }))
                .children(c.pr.map(|(text, tone)| {
                    let word = tone_word(tone);
                    div()
                        .id(ElementId::Name(format!("kanban-pr-{pr_key}").into()))
                        .flex_none()
                        .text_color(rgb(tone_color(tone)))
                        .tooltip(move |_, cx| cx.new(|_| TextTooltip(word.into())).into())
                        .child(text)
                }))
                .children((c.beads > 0).then(|| {
                    div()
                        .flex_none()
                        .text_color(rgb(colors::MUTED))
                        .child(beads_text(c.beads))
                }))
                .children(c.raw_status.map(|s| {
                    div()
                        .min_w(px(0.))
                        .truncate()
                        .text_color(rgb(colors::MUTED))
                        .child(s)
                }))
                .children(c.age.map(|a| {
                    div()
                        .flex_none()
                        .ml_auto()
                        .text_color(rgb(colors::MUTED))
                        .child(a)
                })),
        )
        .cursor_pointer()
        .hover(|s| s.bg(rgb(colors::CARD_HOVER)))
        .tooltip(move |_, cx| cx.new(|_| HoverCard(hover.clone())).into())
        .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
            // Cmd+click keeps the ticket-folder shortcut.
            if ev.modifiers().platform {
                if let Some(folder) = &folder {
                    open_external(folder.clone(), "the ticket folder");
                }
                return;
            }
            this.click_ticket(click_key.clone(), owner, window, cx);
        }))
}

/// A Done card: the key and the title on one line each.
fn done_card(c: TicketCard, cx: &mut Context<OverseerApp>) -> impl IntoElement {
    div()
        .id(ElementId::Name(format!("kanban-{}", c.key).into()))
        .flex_none()
        .flex()
        .flex_col()
        .px_2()
        .py_1()
        .rounded(px(6.))
        .bg(rgb(colors::CARD))
        .border_1()
        .border_color(rgb(colors::BORDER))
        .text_xs()
        .child(
            div()
                .truncate()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(colors::ACCENT))
                .child(c.key),
        )
        .child(
            div()
                .truncate()
                .text_color(rgb(colors::TEXT))
                .child(c.title),
        )
        .when_some(c.folder, |d, folder| {
            d.cursor_pointer()
                .hover(|s| s.bg(rgb(colors::CARD_HOVER)))
                .on_click(
                    cx.listener(move |_, _, _, _| {
                        open_external(folder.clone(), "the ticket folder")
                    }),
                )
        })
}

/// The column heading: icon, label and count in the stage colour.
fn column_heading(stage: Stage, count: usize) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(stage_color(stage)))
        .child(
            div()
                .flex_none()
                .w(px(16.))
                .text_center()
                .child(stage_icon(stage)),
        )
        .child(stage.label())
        .child(div().ml_auto().child(count.to_string()))
}

impl OverseerApp {
    /// The open-PR panel beside the coordinator, or `None` while PRs are
    /// off.
    pub(crate) fn render_pr_panel(
        &self,
        now: i64,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        if !pr_panel_shown(&self.work) {
            return None;
        }
        let (rows, more) = pr_rows(&self.work, &self.slots, now);
        let empty = rows.is_empty();
        let note = pr_source_note(&self.work);
        let answered = pr_answered(&self.work);
        let rows: Vec<_> = rows
            .into_iter()
            .enumerate()
            .map(|(n, r)| {
                let tone = tone_color(r.tone);
                let word = tone_word(r.tone);
                let url = r.url.clone().filter(|u| is_http_url(u));
                let slot = r.slot;
                div()
                    .id(("pr-row", n))
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_xs()
                    .child(
                        div()
                            .id(("pr-dot", n))
                            .flex_none()
                            .size(px(8.))
                            .rounded(px(4.))
                            .bg(rgb(tone))
                            .tooltip(move |_, cx| cx.new(|_| TextTooltip(word.into())).into()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(format!("{} #{}", r.repo, r.id)),
                    )
                    .children(
                        r.ticket
                            .map(|t| div().flex_none().text_color(rgb(colors::ACCENT)).child(t)),
                    )
                    .children(r.slot.map(|s| {
                        div()
                            .flex_none()
                            .px_1()
                            .rounded(px(4.))
                            .bg(rgb(colors::ACCENT_BG))
                            .text_color(rgb(colors::ACCENT))
                            .child(slot_name(s))
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_color(rgb(colors::TEXT_DIM))
                            .child(r.title),
                    )
                    .children((!r.approvals.is_empty()).then(|| {
                        div()
                            .flex_none()
                            .text_color(rgb(colors::TEXT_DIM))
                            .child(r.approvals.clone())
                    }))
                    .children(r.ready.then(|| {
                        div()
                            .flex_none()
                            .px_1()
                            .rounded(px(4.))
                            .border_1()
                            .border_color(rgb(colors::GREEN))
                            .text_color(rgb(colors::GREEN))
                            .child("ready")
                    }))
                    .children(r.changes.then(|| {
                        div()
                            .flex_none()
                            .text_color(rgb(colors::ORANGE))
                            .child("changes")
                    }))
                    .children(r.age.map(|a| {
                        div()
                            .flex_none()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(tone))
                            .child(a)
                    }))
                    .when(url.is_some() || r.slot.is_some(), |d| {
                        d.cursor_pointer()
                            .hover(|s| s.bg(rgb(colors::CARD_HOVER)))
                            .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                                // A linked row opens its session; Cmd+click
                                // always opens the browser.
                                match (slot, &url) {
                                    (Some(id), _) if !ev.modifiers().platform => {
                                        if !this.modal_open() {
                                            this.go_session(id, window, cx);
                                        }
                                    }
                                    (_, Some(url)) => {
                                        open_external(url.clone(), "the pull request")
                                    }
                                    _ => {}
                                }
                            }))
                    })
            })
            .collect();
        Some(
            div()
                .flex_none()
                .w(px(380.))
                .flex()
                .flex_col()
                .gap_1()
                .p_2()
                .rounded(px(6.))
                .bg(rgb(colors::CARD))
                .border_1()
                .border_color(rgb(colors::BORDER_COORD))
                .child(heading(pr_header(&self.work, now)))
                .children(rows)
                .children(note.map(|(word, text)| {
                    let color = match word {
                        "stale" => colors::ORANGE,
                        "failed" => colors::RED,
                        _ => colors::MUTED,
                    };
                    div().text_xs().text_color(rgb(color)).child(text)
                }))
                .children((empty && answered).then(|| {
                    div()
                        .text_xs()
                        .text_color(rgb(colors::MUTED))
                        .child("No open PRs")
                }))
                .children((more > 0).then(|| {
                    div()
                        .text_xs()
                        .text_color(rgb(colors::MUTED))
                        .child(format!("+{more} more"))
                })),
        )
    }

    /// The sprint kanban: progress header and six columns, or `None` when
    /// the sprint has no tickets.
    pub(crate) fn render_kanban(
        &self,
        now: i64,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let p = progress(&self.work)?;
        let behind = behind_text(&p);
        let bar = div()
            .flex_1()
            .min_w(px(200.))
            .relative()
            .h(px(10.))
            .child(
                div()
                    .size_full()
                    .flex()
                    .rounded(px(5.))
                    .overflow_hidden()
                    .bg(rgb(colors::FIELD))
                    .children(
                        bar_segments(&p)
                            .into_iter()
                            .map(|(s, share)| div().h_full().w(relative(share)).bg(stage_fill(s))),
                    ),
            )
            .children(tick_at(&p).map(|at| {
                div()
                    .absolute()
                    .left(relative(at))
                    .ml(px(tick_margin(at)))
                    .top(px(-5.))
                    .w(px(2.))
                    .h(px(20.))
                    .bg(rgb(colors::TEXT))
            }));
        let header = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_3()
            .text_xs()
            .child(heading("SPRINT"))
            .child(bar)
            .child(
                div()
                    .flex_none()
                    .text_color(rgb(colors::TEXT_DIM))
                    .child(format!("{} of {} done", p.done, p.total)),
            )
            .children(behind.map(|b| div().flex_none().text_color(rgb(colors::ORANGE)).child(b)))
            .children((p.breakers_open > 0).then(|| {
                div()
                    .flex_none()
                    .text_color(rgb(colors::RED))
                    .child(breakers_text(p.breakers_open))
            }));
        let cols: Vec<_> = columns(&self.work, now)
            .into_iter()
            .enumerate()
            .map(|(n, col)| {
                let done = col.stage == Stage::Done;
                let count = col.total;
                let cards: Vec<_> = col
                    .cards
                    .into_iter()
                    .map(|c| {
                        if done {
                            done_card(c, cx).into_any_element()
                        } else {
                            let hover = hover_model(&c, &self.slots, &self.work, now);
                            ticket_card(c, hover, cx).into_any_element()
                        }
                    })
                    .collect();
                // Done stays the narrowest column.
                let col_div = if done {
                    div().flex_none().w(px(150.))
                } else {
                    div().flex_1()
                };
                col_div
                    .min_w(px(0.))
                    .min_h(px(0.))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_2()
                    .rounded(px(6.))
                    .bg(rgb(colors::BG))
                    .child(column_heading(col.stage, count))
                    .child(
                        div()
                            .id(("kanban-col", n))
                            .flex_1()
                            .min_h(px(0.))
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .children(cards),
                    )
            })
            .collect();
        // A column with more than fits scrolls in place. The kanban fills
        // the space left below the session cards, or keeps the height its
        // top edge was dragged to; either way it shrinks to fit the window
        // before the cards or the footer would move off screen.
        let measured = self.kanban_measured.clone();
        let panel = div()
            .flex_1()
            .min_h(px(0.))
            .relative()
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .rounded(px(6.))
            .bg(rgb(colors::CARD))
            .border_1()
            .border_color(rgb(colors::BORDER))
            .child(
                canvas(
                    move |b, _, _| measured.set(b.size.height.into()),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(header)
            .child(div().flex_1().min_h(px(0.)).flex().gap_2().children(cols));
        let handle = div()
            .id("kanban-resize")
            .flex_none()
            .h(px(6.))
            .mb(px(-3.))
            .mt(px(-3.))
            .rounded(px(3.))
            .cursor(CursorStyle::ResizeUpDown)
            .when(self.kanban_drag.is_some(), |d| d.bg(rgb(colors::ACCENT)))
            .hover(|s| s.bg(rgb(colors::BORDER)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                    let y: f32 = ev.position.y.into();
                    this.kanban_drag = Some((y, this.kanban_measured.get()));
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .on_click(cx.listener(|this, ev: &ClickEvent, _, cx| {
                if ev.click_count() == 2 {
                    this.kanban_drag = None;
                    this.set_kanban_height(None);
                    cx.notify();
                }
            }));
        let sized = match self.kanban_height {
            Some(h) => div().flex_shrink().h(px(h)),
            None => div().flex_1(),
        };
        Some(
            sized
                .min_h(px(crate::kanban_size::MIN))
                .flex()
                .flex_col()
                .child(handle)
                .child(panel),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::done_count;
    use overseer_proto::{PullRequest, SprintDay};

    fn ticket(key: &str, stage: Stage, prio: Option<&str>) -> WorkItem {
        WorkItem {
            key: key.into(),
            kind: ItemKind::Ticket,
            title: format!("{key} title"),
            status: "In Progress".into(),
            priority: prio.map(String::from),
            parent: None,
            owner: None,
            in_list: true,
            folder: None,
            stage: Some(stage),
            stage_since: None,
        }
    }

    fn pr(id: u64, created: Option<i64>) -> PullRequest {
        PullRequest {
            repo: "demo-api".into(),
            id,
            title: format!("PR {id}"),
            branch: "demo-branch".into(),
            url: None,
            author: None,
            created,
            ticket: None,
            approvals: None,
        }
    }

    fn view(items: Vec<WorkItem>) -> WorkView {
        WorkView {
            items,
            ..crate::app::empty_work()
        }
    }

    #[test]
    fn age_text_and_tones() {
        assert_eq!(age_text(59), "0m");
        assert_eq!(age_text(3600 * 5), "5h");
        assert_eq!(age_text(86_400 * 3), "3d");
        assert_eq!(age_text(-30), "0m");
        assert_eq!(pr_age_tone(86_400 * 2 - 1), AgeTone::Fresh);
        assert_eq!(pr_age_tone(86_400 * 2), AgeTone::Ageing);
        assert_eq!(pr_age_tone(86_400 * 5 - 1), AgeTone::Ageing);
        assert_eq!(pr_age_tone(86_400 * 5), AgeTone::Old);
    }

    #[test]
    fn prio_mapping() {
        assert_eq!(prio_of(Some("Sprint Breaker")), Prio::Breaker);
        assert_eq!(prio_of(Some("Highest")), Prio::High);
        assert_eq!(prio_of(Some("High")), Prio::High);
        assert_eq!(prio_of(Some("Medium")), Prio::Med);
        assert_eq!(prio_of(Some("Low")), Prio::Low);
        assert_eq!(prio_of(Some("Lowest")), Prio::Low);
        assert_eq!(prio_of(None), Prio::Med);
        assert_eq!(prio_of(Some("Whatever")), Prio::Med);
    }

    #[test]
    fn columns_sort_without_truncation() {
        let mut items = vec![
            ticket("DEMO-1", Stage::InProgress, Some("High")),
            ticket("DEMO-2", Stage::InProgress, Some("High")),
            ticket("DEMO-3", Stage::InProgress, Some("Sprint Breaker")),
            ticket("DEMO-4", Stage::InProgress, None),
            ticket("DEMO-5", Stage::InProgress, None),
            ticket("DEMO-6", Stage::InProgress, Some("Low")),
        ];
        items[0].stage_since = Some(100);
        items[1].stage_since = Some(50);
        let cols = columns(&view(items), 1000);
        assert_eq!(cols.len(), 6);
        assert_eq!(
            cols.iter().map(|c| c.stage).collect::<Vec<_>>(),
            Stage::ALL.to_vec()
        );
        let c = &cols[3];
        assert_eq!((c.total, c.cards.len()), (6, 6));
        assert_eq!(c.cards[0].key, "DEMO-3");
        assert_eq!(c.cards[1].key, "DEMO-2");
        assert_eq!(c.cards[1].age.as_deref(), Some("15m"));
        assert_eq!(c.cards[2].key, "DEMO-1");
        assert_eq!(c.cards[3].age, None);
        assert_eq!(c.cards[3].key, "DEMO-4");
        assert_eq!(c.cards[5].key, "DEMO-6");
        assert_eq!(cols[0].total, 0);
    }

    #[test]
    fn columns_show_every_ticket() {
        let items = (1..=30)
            .map(|n| ticket(&format!("DEMO-{n}"), Stage::ToDo, None))
            .collect();
        let todo = &columns(&view(items), 100)[0];
        assert_eq!((todo.total, todo.cards.len()), (30, 30));
    }

    #[test]
    fn done_column_lists_tickets_latest_first() {
        let mut items = vec![
            ticket("DEMO-1", Stage::Done, None),
            ticket("DEMO-2", Stage::Done, None),
            ticket("DEMO-3", Stage::Done, None),
        ];
        items[0].stage_since = Some(100);
        items[1].stage_since = Some(300);
        let done = &columns(&view(items), 400)[5];
        let keys: Vec<_> = done.cards.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, ["DEMO-2", "DEMO-1", "DEMO-3"]);
        assert_eq!(done.total, 3);
    }

    #[test]
    fn card_links_prs_and_beads() {
        let mut v = view(vec![ticket("DEMO-1", Stage::InProgress, None)]);
        let mut bead = ticket("demo-b1", Stage::InProgress, None);
        bead.kind = ItemKind::Bead;
        bead.parent = Some("DEMO-1".into());
        v.items.push(bead);
        let mut newer = pr(8, Some(1000));
        newer.ticket = Some("DEMO-1".into());
        let mut older = pr(7, Some(1000 - 3 * 86_400));
        older.ticket = Some("DEMO-1".into());
        v.prs = vec![newer, older, pr(9, Some(0))];
        let c = &columns(&v, 1000)[3].cards[0];
        assert_eq!(c.pr, Some(("#7 3d".to_string(), AgeTone::Ageing)));
        assert_eq!(c.beads, 1);
    }

    #[test]
    fn hover_hints() {
        assert_eq!(
            hover_hint(&TicketClick::Open(3), true),
            "Click: open S3 · Cmd+click: folder"
        );
        assert_eq!(
            hover_hint(&TicketClick::Start(4), false),
            "Click: start in S4"
        );
        assert_eq!(
            hover_hint(&TicketClick::NoSlot, false),
            "Click: no free slot"
        );
    }

    fn row<'a>(m: &'a HoverModel, label: &str) -> Option<&'a str> {
        m.lines
            .iter()
            .find(|(l, _)| l == label)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn hover_model_for_an_owned_ticket_with_a_pr() {
        let now = 10 * 86_400;
        let mut t = ticket("DEMO-1", Stage::InProgress, Some("High"));
        t.owner = Some(2);
        t.stage_since = Some(now - 5 * 3600);
        t.folder = Some(PathBuf::from("/demo/DEMO-1"));
        let mut v = view(vec![t]);
        let mut p = pr(12, Some(now - 3 * 86_400));
        p.title = "Add the demo endpoint".into();
        p.ticket = Some("DEMO-1".into());
        v.prs = vec![p];
        let mut slots = crate::app::empty_slots();
        slots[2].state = SlotState::Running { pid: 1, since: 0 };
        slots[2].activity = Some(overseer_proto::Activity {
            kind: overseer_proto::ActivityKind::Idle,
            since: now - 60,
            now_doing: None,
            reason: None,
            idle_wait: false,
        });
        let c = &columns(&v, now)[3].cards[0];
        let m = hover_model(c, &slots, &v, now);
        assert_eq!(m.key, "DEMO-1");
        assert_eq!(m.title, "DEMO-1 title");
        assert_eq!(row(&m, "Status"), Some("In Progress"));
        assert_eq!(row(&m, "Priority"), Some("High"));
        assert_eq!(row(&m, "Stage"), Some("IN PROGRESS · 5h"));
        assert_eq!(row(&m, "Session"), Some("S2 · Your turn 1m 00s"));
        assert_eq!(row(&m, "PR"), Some("demo-api #12 Add the demo endpoint 3d"));
        assert_eq!(row(&m, "Beads"), None);
        assert_eq!(m.hint, "Click: open S2 · Cmd+click: folder");
    }

    #[test]
    fn hover_model_for_an_unowned_ticket() {
        let mut v = view(vec![ticket("DEMO-2", Stage::ToDo, None)]);
        let mut bead = ticket("demo-b1", Stage::ToDo, None);
        bead.kind = ItemKind::Bead;
        bead.parent = Some("DEMO-2".into());
        v.items.push(bead);
        let slots = crate::app::empty_slots();
        let c = &columns(&v, 100)[0].cards[0];
        let m = hover_model(c, &slots, &v, 100);
        assert_eq!(row(&m, "Session"), Some("Not started"));
        assert_eq!(row(&m, "Stage"), Some("TODO"));
        assert_eq!(row(&m, "PR"), None);
        assert_eq!(row(&m, "Beads"), Some("1"));
        assert_eq!(m.hint, "Click: start in S1");
    }

    #[test]
    fn unmapped_status_shows_raw() {
        let mut odd = ticket("DEMO-1", Stage::InProgress, None);
        odd.status = "Ready for Release".into();
        let plain = ticket("DEMO-2", Stage::InProgress, None);
        let mut review = ticket("DEMO-3", Stage::Next, None);
        review.status = "Selected for Development".into();
        let cols = columns(&view(vec![odd, plain, review]), 0);
        assert_eq!(
            cols[3].cards[0].raw_status.as_deref(),
            Some("Ready for Release")
        );
        assert_eq!(cols[3].cards[1].raw_status, None);
        assert_eq!(cols[2].cards[0].raw_status, None);
    }

    #[test]
    fn progress_and_pace() {
        let mut items: Vec<_> = (0..10)
            .map(|i| {
                let stage = if i < 3 { Stage::Done } else { Stage::ToDo };
                ticket(&format!("DEMO-{i}"), stage, None)
            })
            .collect();
        items[5].priority = Some("Sprint Breaker".into());
        items[0].priority = Some("Sprint Breaker".into());
        let mut v = view(items);
        v.sprint_day = Some(SprintDay { day: 6, days: 10 });
        let p = progress(&v).unwrap();
        assert_eq!((p.done, p.total), (3, 10));
        assert_eq!(p.per_stage, [7, 0, 0, 0, 0, 3]);
        assert_eq!(p.expected, Some(6));
        assert_eq!(p.breakers_open, 1);
        assert_eq!(
            behind_text(&p).as_deref(),
            Some("Behind pace: 6 expected done by today")
        );
        // A sprint run past its last day expects all, not more.
        v.sprint_day = Some(SprintDay { day: 13, days: 10 });
        assert_eq!(progress(&v).unwrap().expected, Some(10));
        v.sprint_day = None;
        let p = progress(&v).unwrap();
        assert_eq!(p.expected, None);
        assert_eq!(behind_text(&p), None);
        assert!(progress(&view(vec![])).is_none());
    }

    #[test]
    fn ready_prs_sort_first_with_their_badge() {
        use overseer_proto::Approvals;
        let now = 10 * 86_400;
        let with = |id, human, ai, changes_requested| PullRequest {
            approvals: Some(Approvals {
                human,
                ai,
                changes_requested,
            }),
            ..pr(id, Some(now - id as i64 * 86_400))
        };
        let mut v = view(vec![]);
        // Oldest first would be 5, 4, 3, 2, 1.
        v.prs = vec![
            with(1, 1, 1, false),
            with(2, 2, 0, true),
            pr(3, Some(0)),
            with(4, 1, 0, false),
            with(5, 0, 0, false),
        ];
        let (rows, _) = pr_rows(&v, &[], now);
        let ids: Vec<_> = rows.iter().map(|r| r.id).collect();
        assert_eq!(ids, [1, 3, 5, 4, 2]);
        assert!(rows[0].ready && !rows[0].changes);
        assert_eq!(rows[0].approvals, "1H 1AI");
        let changed = rows.iter().find(|r| r.id == 2).unwrap();
        assert!(!changed.ready && changed.changes);
        assert_eq!(changed.approvals, "2H");
        let unknown = rows.iter().find(|r| r.id == 3).unwrap();
        assert_eq!((unknown.approvals.as_str(), unknown.ready), ("", false));
    }

    #[test]
    fn pr_rows_oldest_first_capped() {
        let now = 10 * 86_400;
        let mut v = view(vec![]);
        v.prs = (1..=9)
            .map(|i| pr(i, Some(now - i as i64 * 86_400)))
            .collect();
        v.prs.push(pr(10, None));
        let (rows, more) = pr_rows(&v, &[], now);
        assert_eq!((rows.len(), more), (8, 2));
        assert_eq!(rows[0].id, 9);
        assert_eq!(rows[0].tone, AgeTone::Old);
        assert_eq!(rows[7].id, 2);
        let (rows, _) = pr_rows(
            &WorkView {
                prs: vec![pr(1, None), pr(2, Some(now))],
                ..view(vec![])
            },
            &[],
            now,
        );
        assert_eq!((rows[0].id, rows[1].id), (2, 1));
        assert_eq!(
            (rows[1].age.clone(), rows[1].tone),
            (None, AgeTone::Unknown)
        );
        v.prs = (1..=10)
            .map(|i| pr(i, Some(now - if i == 3 { 6 * 86_400 } else { 3_600 })))
            .collect();
        assert_eq!(pr_header(&v, now), "OPEN PRS · 10 · oldest 6d");
        assert_eq!(pr_header(&view(vec![]), now), "OPEN PRS · 0");
    }

    #[test]
    fn pr_rows_link_a_running_or_resumable_owner() {
        let now = 86_400;
        let owner = |key: &str, slot| {
            let mut t = ticket(key, Stage::InProgress, None);
            t.owner = slot;
            t
        };
        let mut v = view(vec![
            owner("DEMO-1", Some(3)),
            owner("DEMO-2", Some(4)),
            owner("DEMO-3", Some(5)),
            owner("DEMO-4", None),
        ]);
        v.prs = (1..=5).map(|i| pr(i, Some(now - i as i64))).collect();
        for (p, t) in v
            .prs
            .iter_mut()
            .zip(["DEMO-1", "DEMO-2", "DEMO-3", "DEMO-4"])
        {
            p.ticket = Some(t.into());
        }
        let mut slots = crate::app::empty_slots();
        slots[3].state = SlotState::Running { pid: 1, since: 0 };
        slots[4].state = SlotState::Exited { code: None, at: 0 };
        slots[5].state = SlotState::Resumable {
            session_id: "demo-session".into(),
            at: 0,
        };
        let (rows, _) = pr_rows(&v, &slots, now);
        let linked: Vec<_> = rows.iter().map(|r| (r.id, r.slot)).collect();
        // Oldest first: PR 5 has no ticket, 4 is unowned, 3's owner is
        // Resumable, 2's owner exited, 1's owner is Running.
        assert_eq!(
            linked,
            [(5, None), (4, None), (3, Some(5)), (2, None), (1, Some(3))]
        );
    }

    #[test]
    fn header_and_kanban_done_agree() {
        // A stage-less Closed ticket (old daemon) counts as done in the
        // header, the kanban summary and its bar alike.
        let mut old = ticket("DEMO-1", Stage::Done, None);
        old.stage = None;
        old.status = "Closed".into();
        let mut odd = ticket("DEMO-2", Stage::Done, None);
        odd.stage = None;
        odd.status = "Ready for Release".into();
        let items = vec![old, odd, ticket("DEMO-3", Stage::Done, None)];
        let v = view(items);
        let p = progress(&v).unwrap();
        assert_eq!(done_count(&v), (p.done, p.total));
        assert_eq!((p.done, p.total), (2, 3));
        assert_eq!(p.per_stage[5], p.done);
        assert_eq!(done_count(&view(vec![])), (0, 0));
    }

    #[test]
    fn pr_panel_shows_unless_the_source_is_off() {
        let src = |message: Option<&str>| overseer_proto::SourceStatus {
            name: "prs".into(),
            ok: message.is_none(),
            count: 0,
            at: None,
            message: message.map(String::from),
        };
        let mut v = view(vec![]);
        assert!(!pr_panel_shown(&v), "an old daemon sends no prs source");
        v.sources = vec![src(Some(overseer_proto::PRS_OFF))];
        assert!(!pr_panel_shown(&v));
        v.sources = vec![src(Some("not read yet"))];
        assert!(pr_panel_shown(&v));
        v.sources = vec![src(None)];
        assert!(pr_panel_shown(&v));
    }

    #[test]
    fn chips_icons_and_bar_weights() {
        assert_eq!(prio_chip(Prio::Breaker), ("BREAKER", 0x4a1d1d, 0xff9a94));
        assert_eq!(prio_chip(Prio::High), ("HIGH", 0x45290f, 0xf0a868));
        assert_eq!(prio_chip(Prio::Med), ("MED", 0x232a38, 0xaab3c2));
        assert_eq!(prio_chip(Prio::Low), ("LOW", 0x1b2029, 0x8b949e));
        let icons: Vec<_> = Stage::ALL.iter().map(|&s| stage_icon(s)).collect();
        assert_eq!(icons, ["○", "‖", "→", "▶", "⚗", "✓"]);
        assert_eq!(tone_word(AgeTone::Fresh), "fresh");
        assert_eq!(tone_word(AgeTone::Ageing), "ageing");
        assert_eq!(tone_word(AgeTone::Old), "old");
        assert_eq!(beads_text(1), "1 bead");
        assert_eq!(beads_text(3), "3 beads");
        let p = Progress {
            done: 1,
            total: 4,
            per_stage: [2, 0, 0, 1, 0, 1],
            expected: Some(2),
            breakers_open: 0,
        };
        assert_eq!(
            bar_segments(&p),
            vec![
                (Stage::Done, 0.25),
                (Stage::ToDo, 0.5),
                (Stage::InProgress, 0.25),
            ]
        );
        assert_eq!(tone_word(AgeTone::Unknown), "age unknown");
        assert_eq!(tone_color(AgeTone::Unknown), colors::MUTED);
        assert_eq!(breakers_text(1), "1 sprint breaker open");
        assert_eq!(breakers_text(2), "2 sprint breakers open");
        assert_eq!(tick_margin(0.0), 0.0);
        assert_eq!(tick_margin(1.0), -2.0);
        assert_eq!(tick_margin(3.0), -2.0);
        assert_eq!(tick_at(&p), Some(0.5));
        assert_eq!(
            tick_at(&Progress {
                expected: None,
                ..p
            }),
            None
        );
    }

    #[test]
    fn undated_pr_chip_has_unknown_tone_and_no_age() {
        let mut v = view(vec![ticket("DEMO-1", Stage::InProgress, None)]);
        let mut p = pr(5, None);
        p.ticket = Some("DEMO-1".into());
        v.prs = vec![p];
        let c = &columns(&v, 1000)[3].cards[0];
        assert_eq!(c.pr, Some(("#5".to_string(), AgeTone::Unknown)));
    }

    #[test]
    fn pr_panel_says_when_the_source_failed_or_is_stale() {
        let src = |ok: bool, at: Option<i64>, message: Option<&str>| overseer_proto::SourceStatus {
            name: "prs".into(),
            ok,
            count: 0,
            at,
            message: message.map(String::from),
        };
        let mut v = view(vec![]);
        assert_eq!(pr_source_note(&v), None);
        assert!(!pr_answered(&v));
        v.sources = vec![src(false, None, Some("not read yet"))];
        assert_eq!(pr_source_note(&v).unwrap().0, "not read yet");
        assert!(!pr_answered(&v), "no answer yet, so no `No open PRs`");
        v.sources = vec![src(false, None, Some("demo-api: bb not found"))];
        let (word, text) = pr_source_note(&v).unwrap();
        assert_eq!(
            (word, text.as_str()),
            ("failed", "failed: demo-api: bb not found")
        );
        assert!(!pr_answered(&v));
        v.sources = vec![src(false, Some(50), Some("demo-web: timed out"))];
        let (word, text) = pr_source_note(&v).unwrap();
        assert_eq!(
            (word, text.as_str()),
            ("stale", "stale: demo-web: timed out")
        );
        assert!(pr_answered(&v));
        v.sources = vec![src(true, Some(50), None)];
        assert_eq!(pr_source_note(&v), None);
        assert!(pr_answered(&v));
        let long = "x".repeat(300);
        v.sources = vec![src(false, Some(50), Some(&long))];
        assert!(pr_source_note(&v).unwrap().1.chars().count() < 120);
    }

    #[test]
    fn only_web_urls_are_opened() {
        assert!(is_http_url("https://bitbucket.example.test/demo/pr/1"));
        assert!(is_http_url("http://bitbucket.example.test/demo/pr/1"));
        assert!(!is_http_url("file:///etc/hosts"));
        assert!(!is_http_url("-a Calculator"));
        assert!(!is_http_url("/Applications/Demo.app"));
    }

    #[test]
    fn done_count_uses_stage_when_present() {
        let mut shipped = ticket("DEMO-1", Stage::Done, None);
        shipped.status = "Shipped".into();
        let v = view(vec![shipped]);
        assert_eq!(done_count(&v), (1, 1));
        // A stage-less ticket (old daemon) still falls back to the status.
        let mut old = ticket("DEMO-2", Stage::Done, None);
        old.stage = None;
        old.status = "Closed".into();
        assert_eq!(done_count(&view(vec![old])), (1, 1));
    }
}
