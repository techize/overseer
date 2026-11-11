//! The work-item picker: assigns an item from the work view to one slot.
//!
//! Type to filter, Up and Down to move, Enter to assign, Esc to close. A
//! warning about overlapping work (`NeedsConfirm`: a bead whose ticket another
//! slot owns, or a ticket with beads another slot owns) asks again with
//! "Assign anyway", and an item owned by another slot offers "Move to S<n>";
//! Enter accepts either.
//!
//! The rows, the filter, the key handling and the reply handling are pure
//! (`rows`, `PickerState`) and unit tested.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    div, prelude::*, px, rgb, rgba, Context, EventEmitter, FocusHandle, Focusable, FontWeight,
    KeyDownEvent, Keystroke, MouseButton, ScrollHandle, SharedString, Window,
};
use overseer_proto::{slot_name, Client, Request, Response, SlotId, WorkItem, WorkView};

use crate::app::colors;

use crate::start_dialog::{edit_key, DialogKey, TextField};

// ---------------------------------------------------------------------------
// Pure logic
// ---------------------------------------------------------------------------

/// One line of the picker list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerRow {
    pub key: String,
    /// 0 for a ticket or a bead on its own, 1 for a bead under its ticket.
    pub depth: u8,
    pub title: String,
    pub status: String,
    pub priority: Option<String>,
    pub owner: Option<SlotId>,
    pub folder: Option<PathBuf>,
}

/// The view's items in its grouped order, filtered by `filter`.
/// The filter matches an item's key or title, ignoring case. A matching bead
/// keeps its ticket's row, and a matching ticket keeps all its beads.
pub fn rows(view: &WorkView, filter: &str) -> Vec<PickerRow> {
    let needle = filter.trim().to_lowercase();
    let hit = |i: &WorkItem| {
        needle.is_empty()
            || i.key.to_lowercase().contains(&needle)
            || i.title.to_lowercase().contains(&needle)
    };
    let hits: HashSet<&str> = view
        .items
        .iter()
        .filter(|i| hit(i))
        .map(|i| i.key.as_str())
        .collect();
    let parents_of_hits: HashSet<&str> = view
        .items
        .iter()
        .filter(|i| hits.contains(i.key.as_str()))
        .filter_map(|i| i.parent.as_deref())
        .collect();
    view.items
        .iter()
        .filter(|i| {
            hits.contains(i.key.as_str())
                || parents_of_hits.contains(i.key.as_str())
                || i.parent.as_deref().is_some_and(|p| hits.contains(p))
        })
        .map(|i| PickerRow {
            key: i.key.clone(),
            depth: u8::from(i.parent.is_some()),
            title: i.title.clone(),
            status: i.status.clone(),
            priority: i.priority.clone(),
            owner: i.owner,
            folder: i.folder.clone(),
        })
        .collect()
}

/// What a key or click in the picker asks the view to do.
#[derive(Debug, Clone, PartialEq)]
pub enum PickerAction {
    /// Nothing for the picker; let the key go on.
    Ignored,
    /// State changed; redraw.
    Redraw,
    /// Send this request; its reply goes to `PickerState::reply`.
    Send(Request),
    /// Read the clipboard into the filter.
    Paste,
    Close,
}

/// What the picker is asking about after a reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prompt {
    /// The daemon warned; Enter sends `confirm: true`.
    Confirm,
    /// Owned by this slot; Enter sends `move_owner: true`.
    Move(SlotId),
}

/// The question on screen and the request that answers it.
#[derive(Debug, Clone, PartialEq)]
pub struct Question {
    pub prompt: Prompt,
    pub message: String,
    pub next: Request,
}

/// What a reply means to the picker's owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyOutcome {
    /// Assigned: close the picker.
    Assigned { slot: SlotId, key: String },
    /// Still open, showing a question or an error.
    Stay,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PickerState {
    pub slot: SlotId,
    pub filter: TextField,
    pub selected: usize,
    pub question: Option<Question>,
    pub error: Option<String>,
    /// The request on its way; Enter does nothing until it is answered.
    pub in_flight: Option<Request>,
}

/// The slot named by the daemon's refusal `<KEY> is owned by S<n>`.
pub fn owner_in(message: &str, key: &str) -> Option<SlotId> {
    let n = message.strip_prefix(key)?.strip_prefix(" is owned by S")?;
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    n.parse().ok()
}

impl PickerState {
    pub fn new(slot: SlotId) -> Self {
        PickerState {
            slot,
            filter: TextField::default(),
            selected: 0,
            question: None,
            error: None,
            in_flight: None,
        }
    }

    /// The label of the button that answers the question.
    pub fn answer_label(&self) -> Option<String> {
        self.question.as_ref().map(|q| match q.prompt {
            Prompt::Confirm => "Assign anyway".to_string(),
            Prompt::Move(owner) => format!("Move to {}", slot_name(owner)),
        })
    }

    /// Back to choosing: the question and the error were about the row that
    /// was selected.
    fn reset(&mut self) {
        self.question = None;
        self.error = None;
    }

    /// Applies `ks`, with `rows` the list currently shown.
    pub fn key(&mut self, ks: &Keystroke, rows: &[PickerRow]) -> PickerAction {
        let m = &ks.modifiers;
        let plain = !(m.platform || m.control || m.alt || m.shift || m.function);
        if plain {
            match ks.key.as_str() {
                "up" => {
                    self.selected = self.selected.saturating_sub(1);
                    self.reset();
                    return PickerAction::Redraw;
                }
                "down" => {
                    self.selected = (self.selected + 1).min(rows.len().saturating_sub(1));
                    self.reset();
                    return PickerAction::Redraw;
                }
                _ => {}
            }
        }
        let before = self.filter.text.clone();
        match edit_key(&mut self.filter, ks) {
            DialogKey::Edited => {
                // Moving the cursor is not a new filter.
                if self.filter.text != before {
                    self.selected = 0;
                    self.reset();
                }
                PickerAction::Redraw
            }
            DialogKey::Submit => self.accept(rows),
            DialogKey::Cancel => PickerAction::Close,
            DialogKey::Paste => PickerAction::Paste,
            DialogKey::NextField | DialogKey::PrevField | DialogKey::Ignored => {
                PickerAction::Ignored
            }
        }
    }

    /// Adds pasted text to the filter.
    pub fn paste(&mut self, text: &str) {
        let before = self.filter.text.clone();
        self.filter.insert(text);
        if self.filter.text != before {
            self.selected = 0;
            self.reset();
        }
    }

    /// Selects row `n` (a click on it).
    pub fn select(&mut self, n: usize) {
        if n != self.selected {
            self.selected = n;
            self.reset();
        }
    }

    /// A click on row `n` assigns it. A click never answers a question:
    /// on the row being asked about it does nothing, and on another row it
    /// drops the question and assigns that row afresh.
    pub fn click(&mut self, n: usize, rows: &[PickerRow]) -> PickerAction {
        if self.in_flight.is_some() || (self.question.is_some() && n == self.selected) {
            return PickerAction::Ignored;
        }
        self.select(n);
        self.question = None;
        self.accept(rows)
    }

    /// The status line for a reply that came after the picker closed.
    pub fn late_reply(&mut self, reply: Result<Response, String>) -> String {
        let Some(Request::Assign { key, slot, .. }) = self.in_flight.take() else {
            return String::new();
        };
        let why = match reply {
            Ok(Response::Ok) => return format!("{key} assigned to {}", slot_name(slot)),
            Ok(Response::NeedsConfirm { message }) | Ok(Response::Error { message }) => message,
            Ok(other) => format!("Unexpected reply: {other:?}"),
            Err(e) => e,
        };
        format!("{key} not assigned to {}: {why}", slot_name(slot))
    }

    /// Whether a request is on its way.
    pub fn busy(&self) -> bool {
        self.in_flight.is_some()
    }

    /// Keeps the selection in range after the list changed.
    pub fn clamp(&mut self, rows: &[PickerRow]) {
        self.selected = self.selected.min(rows.len().saturating_sub(1));
    }

    /// Enter, or a click on the answer button: answers the question when
    /// one is shown, else assigns the selected row.
    pub fn accept(&mut self, rows: &[PickerRow]) -> PickerAction {
        if self.in_flight.is_some() {
            return PickerAction::Ignored;
        }
        let req = match self.question.take() {
            Some(q) => q.next,
            None => match rows.get(self.selected) {
                Some(row) => Request::Assign {
                    key: row.key.clone(),
                    slot: self.slot,
                    confirm: false,
                    move_owner: false,
                },
                None => return PickerAction::Ignored,
            },
        };
        self.error = None;
        self.in_flight = Some(req.clone());
        PickerAction::Send(req)
    }

    /// The reply to the request `key` or `accept` asked for. `Err` is a
    /// lost connection.
    pub fn reply(&mut self, reply: Result<Response, String>) -> ReplyOutcome {
        let Some(Request::Assign {
            key,
            slot,
            confirm,
            move_owner,
        }) = self.in_flight.take()
        else {
            return ReplyOutcome::Stay;
        };
        let again = |confirm, move_owner| Request::Assign {
            key: key.clone(),
            slot,
            confirm,
            move_owner,
        };
        match reply {
            Ok(Response::Ok) => return ReplyOutcome::Assigned { slot, key },
            Ok(Response::NeedsConfirm { message }) => {
                self.question = Some(Question {
                    prompt: Prompt::Confirm,
                    message,
                    next: again(true, move_owner),
                });
            }
            Ok(Response::Error { message }) => match owner_in(&message, &key) {
                Some(owner) if !move_owner => {
                    self.question = Some(Question {
                        prompt: Prompt::Move(owner),
                        message,
                        next: again(confirm, true),
                    });
                }
                _ => self.error = Some(message),
            },
            Ok(other) => self.error = Some(format!("Unexpected reply: {other:?}")),
            Err(e) => self.error = Some(e),
        }
        ReplyOutcome::Stay
    }
}

// ---------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------

/// What the picker tells the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerEvent {
    Assigned {
        slot: SlotId,
        key: String,
    },
    Closed,
    /// The reply to a request sent before the picker closed, as a status
    /// line.
    Late(String),
}

pub struct Picker {
    state: PickerState,
    /// Closed with a request on its way; its reply becomes `Late`.
    closed: bool,
    view: WorkView,
    client: Arc<Client>,
    scroll: ScrollHandle,
    focus_handle: FocusHandle,
}

impl EventEmitter<PickerEvent> for Picker {}

impl Focusable for Picker {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Picker {
    pub fn new(slot: SlotId, client: Arc<Client>, view: WorkView, cx: &mut Context<Self>) -> Self {
        Picker {
            state: PickerState::new(slot),
            closed: false,
            view,
            client,
            scroll: ScrollHandle::new(),
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn slot(&self) -> SlotId {
        self.state.slot
    }

    /// A request is on its way; the app keeps a closed picker until it is
    /// answered.
    pub fn busy(&self) -> bool {
        self.state.busy()
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.closed = true;
        cx.emit(PickerEvent::Closed);
    }

    /// The daemon's new work view. The selection stays on the same key when
    /// it is still listed.
    pub fn set_view(&mut self, view: WorkView, cx: &mut Context<Self>) {
        let was = self.rows().get(self.state.selected).map(|r| r.key.clone());
        self.view = view;
        let rows = self.rows();
        if let Some(n) = was.and_then(|k| rows.iter().position(|r| r.key == k)) {
            self.state.selected = n;
        }
        self.state.clamp(&rows);
        self.scroll.scroll_to_item(self.state.selected);
        cx.notify();
    }

    fn rows(&self) -> Vec<PickerRow> {
        rows(&self.view, &self.state.filter.text)
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let rows = self.rows();
        let action = self.state.key(&ev.keystroke, &rows);
        if action == PickerAction::Ignored {
            return;
        }
        cx.stop_propagation();
        self.run(action, cx);
    }

    fn run(&mut self, action: PickerAction, cx: &mut Context<Self>) {
        match action {
            PickerAction::Ignored => return,
            PickerAction::Redraw => {}
            PickerAction::Close => self.close(cx),
            PickerAction::Paste => {
                if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                    self.state.paste(&text);
                }
            }
            PickerAction::Send(req) => {
                let client = self.client.clone();
                cx.spawn(async move |this, cx| {
                    let reply = client
                        .request(req)
                        .await
                        .map_err(|e| format!("{} ({e:#})", crate::terminal_view::LOST_DAEMON));
                    let _ = this.update(cx, |this, cx| {
                        if this.closed {
                            let line = this.state.late_reply(reply);
                            cx.emit(PickerEvent::Late(line));
                        } else if let ReplyOutcome::Assigned { slot, key } = this.state.reply(reply)
                        {
                            cx.emit(PickerEvent::Assigned { slot, key });
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
        }
        self.scroll.scroll_to_item(self.state.selected);
        cx.notify();
    }

    fn click_row(&mut self, n: usize, cx: &mut Context<Self>) {
        let rows = self.rows();
        let action = self.state.click(n, &rows);
        self.run(action, cx);
    }
}

fn button(id: &'static str, text: String, primary: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded(px(5.))
        .border_1()
        .border_color(rgb(colors::BUTTON_BORDER))
        .bg(rgb(if primary {
            colors::ACCENT_BG
        } else {
            colors::BUTTON
        }))
        .text_color(rgb(if primary {
            colors::ACCENT
        } else {
            colors::TEXT
        }))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(colors::CARD_HOVER)))
        .child(text)
}

impl Render for Picker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);
        let slot = self.state.slot;
        let rows = self.rows();
        let busy = self.state.in_flight.is_some();
        let filter = if focused {
            self.state.filter.with_cursor()
        } else {
            self.state.filter.text.clone()
        };
        let list: Vec<_> = rows
            .into_iter()
            .enumerate()
            .map(|(n, row)| {
                let on = n == self.state.selected;
                let owner = row.owner.map(|o| {
                    div()
                        .flex_none()
                        .px_1()
                        .rounded(px(4.))
                        .text_xs()
                        .bg(rgb(colors::ACCENT_BG))
                        .text_color(rgb(if o == slot {
                            colors::ACCENT
                        } else {
                            colors::ORANGE
                        }))
                        .child(slot_name(o))
                });
                div()
                    .id(("picker-row", n))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py(px(3.))
                    .pl(px(8. + 18. * f32::from(row.depth)))
                    .rounded(px(4.))
                    .when(on, |d| d.bg(rgb(colors::ACCENT_BG)))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(colors::CARD_HOVER)))
                    .child(
                        div()
                            .flex_none()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(if on { colors::ACCENT } else { colors::TEXT }))
                            .child(row.key),
                    )
                    .child(div().flex_1().min_w(px(0.)).truncate().child(row.title))
                    .children(row.priority.map(|p| {
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(rgb(colors::MUTED))
                            .child(p)
                    }))
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(rgb(colors::MUTED))
                            .child(row.status),
                    )
                    .children(owner)
                    .on_click(cx.listener(move |this, _, _, cx| this.click_row(n, cx)))
            })
            .collect();
        let empty = list.is_empty();
        let question = self.state.question.as_ref().map(|q| q.message.clone());
        let answer = self.state.answer_label();
        let card = div()
            .w(px(640.))
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .rounded(px(8.))
            .bg(rgb(colors::CARD))
            .border_1()
            .border_color(rgb(colors::BORDER_COORD))
            .text_sm()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!("Assign work to {}", slot_name(slot))),
            )
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded(px(4.))
                    .bg(rgb(colors::FIELD))
                    .border_1()
                    .border_color(rgb(colors::ACCENT))
                    .child(SharedString::from(filter)),
            )
            .child(
                div()
                    .id("picker-rows")
                    .flex()
                    .flex_col()
                    .max_h(px(360.))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .children(list)
                    .children(empty.then(|| {
                        div().px_2().text_color(rgb(colors::TEXT_DIM)).child(
                            if self.view.items.is_empty() {
                                "No work items"
                            } else {
                                "Nothing matches"
                            },
                        )
                    })),
            )
            .children(question.map(|m| div().text_color(rgb(colors::ORANGE)).child(m)))
            .children(
                self.state
                    .error
                    .clone()
                    .map(|e| div().text_color(rgb(colors::RED)).child(e)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .child(
                        div()
                            .mr_auto()
                            .text_xs()
                            .text_color(rgb(colors::MUTED))
                            .child(if busy {
                                "Assigning\u{2026}"
                            } else {
                                "Type to filter · Up/Down move · Enter assign · Esc close"
                            }),
                    )
                    .child(
                        button("picker-close", "Close".into(), false)
                            .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
                    )
                    .children(answer.map(|label| {
                        button("picker-answer", label, true).on_click(cx.listener(
                            |this, _, _, cx| {
                                let rows = this.rows();
                                let action = this.state.accept(&rows);
                                this.run(action, cx);
                            },
                        ))
                    })),
            );
        div()
            .key_context("Picker")
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
    use overseer_proto::{ItemKind, WorkItem};

    fn item(key: &str, title: &str, parent: Option<&str>, owner: Option<SlotId>) -> WorkItem {
        WorkItem {
            key: key.into(),
            kind: if parent.is_some() || key.starts_with("ov-") {
                ItemKind::Bead
            } else {
                ItemKind::Ticket
            },
            title: title.into(),
            status: "open".into(),
            priority: Some("P2".into()),
            parent: parent.map(Into::into),
            owner,
            in_list: true,
            folder: None,
        }
    }

    /// DEMO-12 with two beads, DEMO-40 with none, then a bead on its own.
    fn view() -> WorkView {
        WorkView {
            items: vec![
                item("DEMO-12", "Login page", None, None),
                item("ov-3", "Fix the redirect", Some("DEMO-12"), Some(2)),
                item("ov-4", "Add a test", Some("DEMO-12"), None),
                item("DEMO-40", "Billing export", None, None),
                item("ov-9", "Tidy the docs", None, None),
            ],
            sources: vec![],
            notices: vec![],
        }
    }

    fn keys(rows: &[PickerRow]) -> Vec<&str> {
        rows.iter().map(|r| r.key.as_str()).collect()
    }

    #[test]
    fn rows_group_and_indent() {
        let r = rows(&view(), "");
        assert_eq!(keys(&r), ["DEMO-12", "ov-3", "ov-4", "DEMO-40", "ov-9"]);
        let depths: Vec<u8> = r.iter().map(|r| r.depth).collect();
        assert_eq!(depths, [0, 1, 1, 0, 0]);
        assert_eq!(
            r[1],
            PickerRow {
                key: "ov-3".into(),
                depth: 1,
                title: "Fix the redirect".into(),
                status: "open".into(),
                priority: Some("P2".into()),
                owner: Some(2),
                folder: None,
            }
        );
        // Blank filter is no filter.
        assert_eq!(rows(&view(), "  ").len(), 5);
    }

    #[test]
    fn filter_keeps_parent_of_matching_bead() {
        assert_eq!(keys(&rows(&view(), "redirect")), ["DEMO-12", "ov-3"]);
        assert_eq!(keys(&rows(&view(), "ov-4")), ["DEMO-12", "ov-4"]);
        // A bead on its own has no parent to keep.
        assert_eq!(keys(&rows(&view(), "docs")), ["ov-9"]);
    }

    #[test]
    fn filter_keeps_children_of_matching_ticket() {
        assert_eq!(keys(&rows(&view(), "login")), ["DEMO-12", "ov-3", "ov-4"]);
        assert_eq!(keys(&rows(&view(), "DEMO-40")), ["DEMO-40"]);
        assert!(rows(&view(), "nothing like this").is_empty());
    }

    #[test]
    fn filter_is_case_insensitive() {
        assert_eq!(keys(&rows(&view(), "demo-4")), ["DEMO-40"]);
        assert_eq!(keys(&rows(&view(), "BILLING")), ["DEMO-40"]);
        assert_eq!(keys(&rows(&view(), "OV-9")), ["ov-9"]);
    }

    fn key(k: &str, ch: Option<&str>) -> Keystroke {
        let mut ks = Keystroke::parse(k).unwrap();
        ks.key_char = ch.map(String::from);
        ks
    }

    fn assign(key: &str, confirm: bool, move_owner: bool) -> Request {
        Request::Assign {
            key: key.into(),
            slot: 4,
            confirm,
            move_owner,
        }
    }

    #[test]
    fn picker_keys() {
        let all = rows(&view(), "");
        let mut p = PickerState::new(4);
        // Up at the top and Down at the bottom stay put.
        assert_eq!(p.key(&key("up", None), &all), PickerAction::Redraw);
        assert_eq!(p.selected, 0);
        for _ in 0..10 {
            p.key(&key("down", None), &all);
        }
        assert_eq!(p.selected, 4);
        p.key(&key("up", None), &all);
        assert_eq!(p.selected, 3);
        assert_eq!(
            p.key(&key("enter", None), &all),
            PickerAction::Send(assign("DEMO-40", false, false))
        );
        // Enter again while that is on its way does nothing.
        assert_eq!(p.key(&key("enter", None), &all), PickerAction::Ignored);
        assert_eq!(p.key(&key("escape", None), &all), PickerAction::Close);

        // Typing filters and goes back to the top row.
        let mut p = PickerState::new(4);
        p.selected = 2;
        assert_eq!(p.key(&key("b", Some("b")), &all), PickerAction::Redraw);
        assert_eq!(p.filter.text, "b");
        assert_eq!(p.selected, 0);
        assert_eq!(p.key(&key("cmd-v", None), &all), PickerAction::Paste);
        assert_eq!(p.key(&key("tab", None), &all), PickerAction::Ignored);

        // Nothing listed: Enter and Down do nothing.
        let mut p = PickerState::new(4);
        assert_eq!(p.key(&key("enter", None), &[]), PickerAction::Ignored);
        p.key(&key("down", None), &[]);
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn ok_closes_the_picker() {
        let all = rows(&view(), "");
        let mut p = PickerState::new(4);
        p.accept(&all);
        assert_eq!(
            p.reply(Ok(Response::Ok)),
            ReplyOutcome::Assigned {
                slot: 4,
                key: "DEMO-12".into()
            }
        );
    }

    #[test]
    fn warning_asks_and_enter_confirms() {
        let all = rows(&view(), "");
        let mut p = PickerState::new(4);
        p.accept(&all);
        let msg = "S4 is running; assign anyway?".to_string();
        assert_eq!(
            p.reply(Ok(Response::NeedsConfirm {
                message: msg.clone()
            })),
            ReplyOutcome::Stay
        );
        assert_eq!(p.question.as_ref().unwrap().message, msg);
        assert_eq!(p.answer_label().as_deref(), Some("Assign anyway"));
        assert_eq!(
            p.key(&key("enter", None), &all),
            PickerAction::Send(assign("DEMO-12", true, false))
        );
    }

    #[test]
    fn owned_elsewhere_offers_a_move() {
        let all = rows(&view(), "");
        let mut p = PickerState::new(4);
        p.selected = 1;
        p.accept(&all);
        p.reply(Ok(Response::Error {
            message: "ov-3 is owned by S2".into(),
        }));
        let q = p.question.clone().unwrap();
        assert_eq!(q.prompt, Prompt::Move(2));
        assert_eq!(q.message, "ov-3 is owned by S2");
        assert_eq!(p.answer_label().as_deref(), Some("Move to S2"));
        assert_eq!(
            p.accept(&all),
            PickerAction::Send(assign("ov-3", false, true))
        );
        // The move can still be warned about; confirming keeps the move.
        p.reply(Ok(Response::NeedsConfirm {
            message: "overlaps S1".into(),
        }));
        assert_eq!(
            p.accept(&all),
            PickerAction::Send(assign("ov-3", true, true))
        );
    }

    #[test]
    fn other_errors_show_inline() {
        let all = rows(&view(), "");
        let mut p = PickerState::new(4);
        p.accept(&all);
        p.reply(Ok(Response::Error {
            message: "no such item DEMO-12".into(),
        }));
        assert_eq!(p.error.as_deref(), Some("no such item DEMO-12"));
        assert_eq!(p.question, None);
        // Moving clears it; Enter tries again.
        p.key(&key("down", None), &all);
        assert_eq!(p.error, None);
        p.reply(Err("lost the daemon".into()));
        assert_eq!(p.error, None, "no request was in flight");
        p.accept(&all);
        p.reply(Err("lost the daemon".into()));
        assert_eq!(p.error.as_deref(), Some("lost the daemon"));
    }

    #[test]
    fn typing_drops_the_question() {
        let all = rows(&view(), "");
        let mut p = PickerState::new(4);
        p.accept(&all);
        p.reply(Ok(Response::NeedsConfirm {
            message: "busy".into(),
        }));
        p.key(&key("x", Some("x")), &all);
        assert_eq!(p.question, None);
        assert_eq!(p.answer_label(), None);
    }

    /// The picker with a Move question up on row 1 (ov-3, owned by S2).
    fn moving_ov3() -> (Vec<PickerRow>, PickerState) {
        let all = rows(&view(), "");
        let mut p = PickerState::new(4);
        p.selected = 1;
        p.accept(&all);
        p.reply(Ok(Response::Error {
            message: "ov-3 is owned by S2".into(),
        }));
        assert!(p.question.is_some());
        (all, p)
    }

    #[test]
    fn cursor_keys_keep_the_selection_and_question() {
        let (all, mut p) = moving_ov3();
        p.filter.set("ov");
        for k in [
            "left",
            "right",
            "home",
            "end",
            "cmd-left",
            "cmd-right",
            "left",
        ] {
            p.key(&key(k, None), &all);
            assert_eq!(p.selected, 1, "{k}");
            assert!(p.question.is_some(), "{k}");
        }
        assert_eq!(p.filter.text, "ov");
        // Backspace at the start changes nothing either.
        p.key(&key("home", None), &all);
        p.key(&key("backspace", None), &all);
        assert_eq!(p.selected, 1);
        assert!(p.question.is_some());
        // A real edit does reset.
        p.key(&key("end", None), &all);
        p.key(&key("backspace", None), &all);
        assert_eq!(p.filter.text, "o");
        assert_eq!(p.selected, 0);
        assert!(p.question.is_none());
        // So does a paste that adds text, but not an empty one.
        let (_, mut p) = moving_ov3();
        p.paste("\n");
        assert!(p.question.is_some());
        p.paste("x");
        assert!(p.question.is_none());
    }

    #[test]
    fn clicking_the_question_row_does_not_answer() {
        let (all, mut p) = moving_ov3();
        assert_eq!(p.click(1, &all), PickerAction::Ignored);
        assert_eq!(p.question.as_ref().unwrap().prompt, Prompt::Move(2));
        assert_eq!(p.in_flight, None);
        // Another row is a fresh plain assign of that row.
        assert_eq!(
            p.click(3, &all),
            PickerAction::Send(assign("DEMO-40", false, false))
        );
        assert_eq!(p.question, None);
        // Only Enter or the answer button answers.
        let (all, mut p) = moving_ov3();
        assert_eq!(
            p.key(&key("enter", None), &all),
            PickerAction::Send(assign("ov-3", false, true))
        );
    }

    #[test]
    fn late_replies_become_a_status_line() {
        let all = rows(&view(), "");
        let sent = |p: &mut PickerState| {
            *p = PickerState::new(3);
            p.accept(&all);
        };
        let mut p = PickerState::new(3);
        sent(&mut p);
        assert_eq!(p.late_reply(Ok(Response::Ok)), "DEMO-12 assigned to S3");
        sent(&mut p);
        assert_eq!(
            p.late_reply(Ok(Response::NeedsConfirm {
                message: "S3 is busy".into()
            })),
            "DEMO-12 not assigned to S3: S3 is busy"
        );
        sent(&mut p);
        assert_eq!(
            p.late_reply(Ok(Response::Error {
                message: "DEMO-12 is owned by S2".into()
            })),
            "DEMO-12 not assigned to S3: DEMO-12 is owned by S2"
        );
        sent(&mut p);
        assert_eq!(
            p.late_reply(Err("lost the daemon".into())),
            "DEMO-12 not assigned to S3: lost the daemon"
        );
        assert_eq!(p.in_flight, None);
    }

    #[test]
    fn owner_parsing() {
        assert_eq!(owner_in("DEMO-12 is owned by S3", "DEMO-12"), Some(3));
        assert_eq!(owner_in("DEMO-12 is owned by S0", "DEMO-12"), Some(0));
        assert_eq!(owner_in("ov-3 is owned by S3", "DEMO-12"), None);
        assert_eq!(owner_in("DEMO-12 is owned by Sx", "DEMO-12"), None);
        assert_eq!(owner_in("DEMO-12 is owned by S3 now", "DEMO-12"), None);
        assert_eq!(owner_in("invalid slot id 9", "DEMO-12"), None);
    }
}
