//! The end-of-day overlay (#88): the board's End of day button and the
//! palette command open it. It asks the daemon for today's draft, shows the
//! problems the checks found, the minutes per ticket and the hours per epic,
//! lets the user move the hours a quarter at a time, and saves them on
//! Approve. Nothing leaves the machine: the user runs `jc push` themself.
//!
//! The hours editor (`Hours`) and `quarters_text` are pure and unit tested.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{
    div, prelude::*, px, rgb, rgba, ClipboardItem, Context, EventEmitter, FocusHandle, Focusable,
    FontWeight, KeyDownEvent, MouseButton, SharedString, Window,
};
use overseer_proto::{slot_name, Client, DayDraft, DayProblem, Request, Response, SlotId};

use crate::app::colors;

// ---------------------------------------------------------------------------
// Pure logic
// ---------------------------------------------------------------------------

/// The hours per epic being edited, in quarter hours, against the day's
/// target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hours {
    pub target: u32,
    /// Epic key and its quarters, in the draft's order.
    pub rows: Vec<(String, u32)>,
    edited: bool,
}

impl Hours {
    /// The script's hours, unedited.
    pub fn from_draft(d: &DayDraft) -> Hours {
        Hours {
            target: d.target_quarters,
            rows: d
                .epics
                .iter()
                .map(|e| (e.key.clone(), e.quarters))
                .collect(),
            edited: false,
        }
    }

    /// Moves row `i` a quarter up or down, never below zero.
    pub fn step(&mut self, i: usize, up: bool) {
        let Some((_, q)) = self.rows.get_mut(i) else {
            return;
        };
        let new = if up { *q + 1 } else { q.saturating_sub(1) };
        if new != *q {
            *q = new;
            self.edited = true;
        }
    }

    pub fn total(&self) -> u32 {
        self.rows.iter().map(|(_, q)| q).sum()
    }

    /// The target minus the total: what is left to place, negative when
    /// over.
    pub fn left(&self) -> i64 {
        i64::from(self.target) - i64::from(self.total())
    }

    /// Approve is on only when the hours make the target and the split
    /// worked.
    pub fn can_approve(&self, split_ok: bool) -> bool {
        split_ok && self.left() == 0
    }

    /// Whether the user changed any hours since the draft came.
    pub fn edited(&self) -> bool {
        self.edited
    }
}

/// Quarter hours as hours with two decimals: 29 is `7.25`.
pub fn quarters_text(q: u32) -> String {
    format!("{}.{:02}", q / 4, (q % 4) * 25)
}

/// The line over the hours: `7.50 of 7.50 h`, `0.25 h left to place`, or
/// how far over the target the hours are.
fn hours_header(h: &Hours) -> String {
    let left = h.left();
    let off = quarters_text(u32::try_from(left.unsigned_abs()).unwrap_or(u32::MAX));
    match left {
        0 => format!(
            "{} of {} h",
            quarters_text(h.total()),
            quarters_text(h.target)
        ),
        l if l > 0 => format!("{off} h left to place"),
        _ => format!("{off} h over the target"),
    }
}

/// Where a click on a problem goes: its file when it names one (that is
/// where it gets fixed), else its slot.
fn problem_link(p: &DayProblem) -> Option<EodEvent> {
    if let Some(path) = &p.path {
        return Some(EodEvent::OpenPath(PathBuf::from(path)));
    }
    p.slot.map(EodEvent::OpenSlot)
}

// ---------------------------------------------------------------------------
// The view
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum EodEvent {
    Closed,
    /// A problem's slot: go to it.
    OpenSlot(SlotId),
    /// A problem's log file: open it.
    OpenPath(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Busy {
    Idle,
    /// An EndOfDay request is on its way; no other is sent until it answers.
    Running,
    /// A SaveDay is on its way; the overlay stays open until it answers.
    Saving,
}

/// A question the overlay waits on before it acts.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ask {
    /// The hours were edited: run the checks again with this target?
    Discard(Option<u32>),
    /// Files for the day exist: replace them?
    Replace(Vec<String>),
}

const RUNNING: &str = "Running the checks...";
const DISCARD: &str = "Discard your changes to the hours?";
const REPLACE: &str =
    "The allocation file for today already exists (it may have been pushed). Replace it?";

/// What a save wrote.
struct Saved {
    time_log: String,
    allocation: Option<String>,
    push: Option<String>,
}

pub struct EodView {
    client: Arc<Client>,
    home: Option<PathBuf>,
    draft: Option<DayDraft>,
    hours: Option<Hours>,
    /// The target the user set, None until they change it.
    target: Option<u32>,
    busy: Busy,
    ask: Option<Ask>,
    error: Option<String>,
    saved: Option<Saved>,
    copied: bool,
    focus_handle: FocusHandle,
}

impl EventEmitter<EodEvent> for EodView {}

impl Focusable for EodView {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

fn lost(e: impl std::fmt::Display) -> String {
    format!("{} ({e:#})", crate::terminal_view::LOST_DAEMON)
}

impl EodView {
    /// Opens with `Running the checks...` and asks for today's draft.
    pub fn new(client: Arc<Client>, home: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
        let mut view = EodView {
            client,
            home,
            draft: None,
            hours: None,
            target: None,
            busy: Busy::Idle,
            ask: None,
            error: None,
            saved: None,
            copied: false,
            focus_handle: cx.focus_handle(),
        };
        view.run(cx);
        view
    }

    /// The client to use from now on, after a reconnect.
    pub fn set_client(&mut self, client: Arc<Client>) {
        self.client = client;
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        if self.busy == Busy::Saving {
            return;
        }
        cx.emit(EodEvent::Closed);
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let m = &k.modifiers;
        if k.key != "escape" || m.platform || m.control || m.alt || m.shift {
            return;
        }
        // Esc answers an open question with no; otherwise it closes.
        if self.ask.take().is_none() {
            self.close(cx);
        }
        cx.notify();
        cx.stop_propagation();
    }

    /// Sends EndOfDay with the target the user set. Ignored while a request
    /// is out, so only one run is ever in flight.
    fn run(&mut self, cx: &mut Context<Self>) {
        if self.busy != Busy::Idle {
            return;
        }
        self.busy = Busy::Running;
        self.error = None;
        self.ask = None;
        let req = Request::EndOfDay {
            target_quarters: self.target,
        };
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let reply = client.request(req).await;
            let _ = this.update(cx, |this, cx| {
                this.busy = Busy::Idle;
                match reply {
                    Ok(Response::EndOfDay(d)) => {
                        this.hours = Some(Hours::from_draft(&d));
                        this.draft = Some(d);
                        this.saved = None;
                    }
                    Ok(Response::Error { message }) => this.error = Some(message),
                    Ok(other) => this.error = Some(format!("unexpected reply: {other:?}")),
                    Err(e) => this.error = Some(lost(e)),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Run again, or a new target: asks first when the hours were edited.
    fn rerun(&mut self, target: Option<u32>, cx: &mut Context<Self>) {
        if self.busy != Busy::Idle {
            return;
        }
        if self.hours.as_ref().is_some_and(Hours::edited) {
            self.ask = Some(Ask::Discard(target));
            cx.notify();
            return;
        }
        self.target = target;
        self.run(cx);
    }

    /// The target a quarter up or down, never below a quarter.
    fn step_target(&mut self, up: bool, cx: &mut Context<Self>) {
        let Some(d) = &self.draft else {
            return;
        };
        if d.target_fixed {
            return;
        }
        let now = self.target.unwrap_or(d.target_quarters);
        let to = if up {
            now + 1
        } else {
            now.saturating_sub(1).max(1)
        };
        if to != now {
            self.rerun(Some(to), cx);
        }
    }

    fn step_hours(&mut self, i: usize, up: bool, cx: &mut Context<Self>) {
        if self.busy != Busy::Idle {
            return;
        }
        if let Some(h) = &mut self.hours {
            h.step(i, up);
            cx.notify();
        }
    }

    fn can_approve(&self) -> bool {
        self.busy == Busy::Idle
            && match (&self.draft, &self.hours) {
                (Some(d), Some(h)) => h.can_approve(d.split_ok),
                _ => false,
            }
    }

    /// Sends the hours; `replace` overwrites the day's files.
    fn save(&mut self, replace: bool, cx: &mut Context<Self>) {
        if !self.can_approve() {
            return;
        }
        let (Some(d), Some(h)) = (&self.draft, &self.hours) else {
            return;
        };
        let req = Request::SaveDay {
            id: d.id,
            hours: h.rows.clone(),
            replace,
        };
        self.busy = Busy::Saving;
        self.error = None;
        self.ask = None;
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let reply = client.request(req).await;
            let _ = this.update(cx, |this, cx| {
                this.busy = Busy::Idle;
                match reply {
                    Ok(Response::DaySaved {
                        time_log,
                        allocation,
                        push,
                    }) => {
                        this.copied = false;
                        this.saved = Some(Saved {
                            time_log,
                            allocation,
                            push,
                        });
                    }
                    Ok(Response::DayExists { paths }) => this.ask = Some(Ask::Replace(paths)),
                    Ok(Response::Error { message }) => this.error = Some(message),
                    Ok(other) => this.error = Some(format!("unexpected reply: {other:?}")),
                    Err(e) => this.error = Some(lost(e)),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Yes to the open question.
    fn answer_yes(&mut self, cx: &mut Context<Self>) {
        match self.ask.take() {
            Some(Ask::Discard(target)) => {
                self.target = target;
                self.run(cx);
            }
            Some(Ask::Replace(_)) => self.save(true, cx),
            None => {}
        }
        cx.notify();
    }

    fn copy_push(&mut self, cx: &mut Context<Self>) {
        if let Some(push) = self.saved.as_ref().and_then(|s| s.push.clone()) {
            cx.write_to_clipboard(ClipboardItem::new_string(push));
            self.copied = true;
            cx.notify();
        }
    }

    fn show_path(&self, path: &str) -> String {
        crate::board::tilde(Path::new(path), self.home.as_deref())
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn button(
    id: impl Into<gpui::ElementId>,
    text: impl Into<SharedString>,
    primary: bool,
    on: bool,
) -> gpui::Stateful<gpui::Div> {
    let b = div()
        .id(id)
        .flex_none()
        .px_3()
        .py_1()
        .rounded(px(5.))
        .border_1()
        .border_color(rgb(colors::BUTTON_BORDER))
        .bg(rgb(if primary && on {
            colors::ACCENT_BG
        } else {
            colors::BUTTON
        }))
        .text_color(rgb(match (on, primary) {
            (false, _) => colors::MUTED,
            (true, true) => colors::ACCENT,
            (true, false) => colors::TEXT,
        }))
        .child(text.into());
    if on {
        b.cursor_pointer().hover(|s| s.bg(rgb(colors::CARD_HOVER)))
    } else {
        b
    }
}

/// A small − or + for a quarter step.
fn stepper(
    id: impl Into<gpui::ElementId>,
    glyph: &'static str,
    on: bool,
) -> gpui::Stateful<gpui::Div> {
    let b = div()
        .id(id)
        .flex_none()
        .w(px(20.))
        .flex()
        .justify_center()
        .rounded(px(4.))
        .border_1()
        .border_color(rgb(colors::BUTTON_BORDER))
        .bg(rgb(colors::BUTTON))
        .text_color(rgb(if on { colors::TEXT } else { colors::MUTED }))
        .child(glyph);
    if on {
        b.cursor_pointer().hover(|s| s.bg(rgb(colors::CARD_HOVER)))
    } else {
        b
    }
}

fn heading(text: &'static str) -> gpui::Div {
    div()
        .pt_1()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(colors::MUTED))
        .child(text)
}

fn muted(text: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_xs()
        .text_color(rgb(colors::MUTED))
        .child(text.into())
}

impl EodView {
    fn render_problems(&self, d: &DayDraft, cx: &mut Context<Self>) -> gpui::Div {
        let mut out = div().flex().flex_col().gap_1().child(heading("Problems"));
        if d.problems.is_empty() {
            return out.child(muted("None found."));
        }
        for (i, p) in d.problems.iter().enumerate() {
            let link = problem_link(p);
            let place = match (&p.path, p.slot) {
                (Some(path), _) => Some(self.show_path(path)),
                (None, Some(s)) => Some(format!("go to {}", slot_name(s))),
                (None, None) => None,
            };
            let mut row = div()
                .id(("eod-problem", i))
                .flex()
                .flex_col()
                .px_2()
                .py_1()
                .rounded(px(4.))
                .bg(rgb(colors::FIELD))
                .border_1()
                .border_color(rgb(colors::BORDER))
                .child(div().text_color(rgb(colors::ORANGE)).child(p.text.clone()))
                .children(place.map(|t| muted(t).truncate()));
            if let Some(ev) = link {
                row = row
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(colors::CARD_HOVER)))
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(ev.clone())));
            }
            out = out.child(row);
        }
        out
    }

    fn render_tickets(d: &DayDraft) -> Option<gpui::Div> {
        if d.tickets.is_empty() {
            return None;
        }
        Some(
            div()
                .flex()
                .flex_col()
                .child(heading("Minutes per ticket"))
                .children(d.tickets.iter().map(|t| {
                    div()
                        .flex()
                        .gap_2()
                        .text_xs()
                        .child(div().w(px(110.)).flex_none().child(t.ticket.clone()))
                        .child(
                            div()
                                .w(px(70.))
                                .flex_none()
                                .text_color(rgb(colors::TEXT_DIM))
                                .child(format!("{:.0} min", t.minutes)),
                        )
                        .children(
                            t.epic
                                .clone()
                                .map(|e| div().text_color(rgb(colors::MUTED)).child(e)),
                        )
                })),
        )
    }

    fn render_hours(&self, d: &DayDraft, h: &Hours, cx: &mut Context<Self>) -> gpui::Div {
        let idle = self.busy == Busy::Idle;
        let target_on = idle && !d.target_fixed;
        let header_color = if h.left() == 0 {
            colors::GREEN
        } else {
            colors::ORANGE
        };
        let target = div()
            .flex()
            .items_center()
            .gap_1()
            .text_xs()
            .child(muted("Target"))
            .child(
                stepper("eod-target-down", "\u{2212}", target_on && h.target > 1)
                    .when(target_on, |b| {
                        b.on_click(cx.listener(|this, _, _, cx| this.step_target(false, cx)))
                    }),
            )
            .child(
                div()
                    .w(px(36.))
                    .flex()
                    .justify_center()
                    .child(quarters_text(h.target)),
            )
            .child(
                stepper("eod-target-up", "+", target_on).when(target_on, |b| {
                    b.on_click(cx.listener(|this, _, _, cx| this.step_target(true, cx)))
                }),
            )
            .child(muted("h"));
        let mut out = div().flex().flex_col().gap_1().child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(heading("Hours per epic"))
                .child(
                    div()
                        .pt_1()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(header_color))
                        .child(hours_header(h)),
                )
                .child(div().ml_auto().child(target)),
        );
        if d.epics.is_empty() {
            out = out.child(muted("No hours could be worked out."));
        }
        for (i, e) in d.epics.iter().enumerate() {
            let q = h.rows.get(i).map_or(0, |r| r.1);
            out = out.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_xs()
                    .child(div().w(px(110.)).flex_none().child(e.key.clone()))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_color(rgb(colors::TEXT_DIM))
                            .child(e.summary.clone()),
                    )
                    .child(
                        stepper(("eod-down", i), "\u{2212}", idle && q > 0).when(idle, |b| {
                            b.on_click(
                                cx.listener(move |this, _, _, cx| this.step_hours(i, false, cx)),
                            )
                        }),
                    )
                    .child(
                        div()
                            .w(px(36.))
                            .flex()
                            .justify_center()
                            .child(quarters_text(q)),
                    )
                    .child(stepper(("eod-up", i), "+", idle).when(idle, |b| {
                        b.on_click(cx.listener(move |this, _, _, cx| this.step_hours(i, true, cx)))
                    }))
                    .child(
                        div()
                            .w(px(120.))
                            .flex_none()
                            .text_color(rgb(colors::MUTED))
                            .child(format!("{:.2} h from sessions", e.derived_hours)),
                    ),
            );
        }
        out.children(d.fit.clone().map(muted))
    }

    fn render_ask(&self, ask: &Ask, cx: &mut Context<Self>) -> gpui::Div {
        let (question, paths, yes): (&str, &[String], &str) = match ask {
            Ask::Discard(_) => (DISCARD, &[], "Discard"),
            Ask::Replace(paths) => (REPLACE, paths, "Replace"),
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .w_full()
                    .text_color(rgb(colors::ORANGE))
                    .child(question),
            )
            .children(paths.iter().map(|p| muted(self.show_path(p))))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        button("eod-ask-no", "Cancel", false, true).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.ask = None;
                                cx.notify();
                            },
                        )),
                    )
                    .child(
                        button("eod-ask-yes", yes, true, true)
                            .on_click(cx.listener(|this, _, _, cx| this.answer_yes(cx))),
                    ),
            )
    }

    fn render_saved(&self, s: &Saved, cx: &mut Context<Self>) -> gpui::Div {
        let line = |label: &'static str, value: String| {
            div()
                .flex()
                .gap_2()
                .text_xs()
                .child(
                    div()
                        .w(px(80.))
                        .flex_none()
                        .text_color(rgb(colors::MUTED))
                        .child(label),
                )
                .child(div().min_w(px(0.)).truncate().child(value))
        };
        div()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .rounded(px(4.))
            .border_1()
            .border_color(rgb(colors::GREEN))
            .child(div().text_color(rgb(colors::GREEN)).child("Saved"))
            .child(line("Time log", self.show_path(&s.time_log)))
            .children(
                s.allocation
                    .as_ref()
                    .map(|a| line("Allocation", self.show_path(a))),
            )
            .children(s.push.clone().map(|push| {
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .px_2()
                            .py_1()
                            .rounded(px(4.))
                            .bg(rgb(colors::FIELD))
                            .text_xs()
                            .child(push),
                    )
                    .child(
                        button(
                            "eod-copy",
                            if self.copied { "Copied" } else { "Copy" },
                            false,
                            true,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.copy_push(cx))),
                    )
            }))
    }
}

impl Render for EodView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let idle = self.busy == Busy::Idle;
        let mut body = div()
            .id("eod-body")
            .max_h(px(520.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_2();
        if self.busy == Busy::Running {
            body = body.child(div().text_color(rgb(colors::TEXT_DIM)).child(RUNNING));
        }
        if let (Some(d), Some(h)) = (self.draft.clone(), self.hours.clone()) {
            body = body
                .child(self.render_problems(&d, cx))
                .children(Self::render_tickets(&d))
                .child(self.render_hours(&d, &h, cx));
        }
        let title = match &self.draft {
            Some(d) => format!("End of day {}", d.day),
            None => "End of day".to_string(),
        };
        let ask = self.ask.clone().map(|a| self.render_ask(&a, cx));
        let saved = self.saved.as_ref().map(|s| self.render_saved(s, cx));
        let approve_on = self.can_approve() && self.ask.is_none();
        let rerun_on = idle && self.ask.is_none();
        let hint: SharedString = match self.busy {
            Busy::Saving => "Saving\u{2026}".into(),
            _ => "Esc close".into(),
        };
        let card = div()
            .w(px(720.))
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .rounded(px(8.))
            .bg(rgb(colors::CARD))
            .border_1()
            .border_color(rgb(colors::BORDER_COORD))
            .text_sm()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
            .child(body)
            .children(
                self.error
                    .clone()
                    .map(|e| div().text_color(rgb(colors::RED)).child(e)),
            )
            .children(saved)
            .children(ask)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .pt_1()
                    .child(muted(hint).mr_auto())
                    .child(
                        button("eod-close", "Close", false, self.busy != Busy::Saving)
                            .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
                    )
                    .child(
                        button("eod-rerun", "Run again", false, rerun_on).when(rerun_on, |b| {
                            b.on_click(cx.listener(|this, _, _, cx| {
                                let target = this.target;
                                this.rerun(target, cx)
                            }))
                        }),
                    )
                    .child(
                        button("eod-approve", "Approve", true, approve_on).when(approve_on, |b| {
                            b.on_click(cx.listener(|this, _, _, cx| this.save(false, cx)))
                        }),
                    ),
            );
        div()
            .key_context("EodView")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x00000099))
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.close(cx)),
            )
            .child(card)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use overseer_proto::EpicHours;

    fn epic(key: &str, quarters: u32) -> EpicHours {
        EpicHours {
            key: key.into(),
            summary: format!("{key} work"),
            quarters,
            derived_hours: 1.0,
            tickets: vec![],
        }
    }

    fn draft() -> DayDraft {
        DayDraft {
            id: 3,
            day: "2026-11-26".into(),
            target_quarters: 30,
            split_ok: true,
            epics: vec![epic("DEMO-1", 0), epic("DEMO-2", 30)],
            ..DayDraft::default()
        }
    }

    #[test]
    fn the_editor_steps_in_quarters_and_gates_approve() {
        let mut h = Hours::from_draft(&draft());
        assert_eq!(h.target, 30);
        assert_eq!(
            h.rows,
            vec![("DEMO-1".to_string(), 0), ("DEMO-2".to_string(), 30)]
        );
        assert_eq!((h.total(), h.left()), (30, 0));
        assert!(!h.edited());
        assert!(h.can_approve(true));
        // At the target, a failed split still keeps Approve off.
        assert!(!h.can_approve(false));

        // Never below zero, and a step that changes nothing is no edit.
        h.step(0, false);
        assert_eq!(h.rows[0].1, 0);
        assert!(!h.edited());

        // A quarter at a time; off the target Approve is off.
        h.step(1, false);
        assert_eq!((h.rows[1].1, h.total(), h.left()), (29, 29, 1));
        assert!(!h.can_approve(true));
        h.step(0, true);
        h.step(0, true);
        assert_eq!((h.total(), h.left()), (31, -1));
        assert!(!h.can_approve(true));
        h.step(0, false);
        assert_eq!((h.total(), h.left()), (30, 0));
        // Back at the target, but the hours are no longer the script's.
        assert!(h.edited());
        assert!(h.can_approve(true));
        assert!(!h.can_approve(false));

        // A row that is not there is left alone.
        h.step(9, true);
        assert_eq!(h.total(), 30);
    }

    #[test]
    fn the_eod_header_says_what_is_left() {
        let mut h = Hours::from_draft(&draft());
        assert_eq!(hours_header(&h), "7.50 of 7.50 h");
        h.step(1, false);
        assert_eq!(hours_header(&h), "0.25 h left to place");
        h.step(1, true);
        h.step(1, true);
        assert_eq!(hours_header(&h), "0.25 h over the target");
    }

    #[test]
    fn an_eod_problem_links_to_its_file_then_its_slot() {
        let p = |slot: Option<SlotId>, path: Option<&str>| DayProblem {
            text: "S2 has no log for 10:00-10:30".into(),
            slot,
            path: path.map(String::from),
        };
        assert_eq!(
            problem_link(&p(Some(2), Some("/notes/Sessions/demo-slug.md"))),
            Some(EodEvent::OpenPath("/notes/Sessions/demo-slug.md".into()))
        );
        assert_eq!(problem_link(&p(Some(2), None)), Some(EodEvent::OpenSlot(2)));
        assert_eq!(problem_link(&p(None, None)), None);
    }

    #[test]
    fn quarters_show_as_hours() {
        assert_eq!(quarters_text(0), "0.00");
        assert_eq!(quarters_text(1), "0.25");
        assert_eq!(quarters_text(29), "7.25");
        assert_eq!(quarters_text(30), "7.50");
        assert_eq!(quarters_text(36), "9.00");
    }
}
