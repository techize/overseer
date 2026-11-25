//! What the board knows of each slot's conversation: the entries and open
//! prompt the daemon sent, the reply being typed, and the answers being
//! picked. The model is pure; the app keeps one `Conv` per slot and sends
//! the requests. The pane that draws it sits to the right of the board
//! (`OverseerApp::render_conversation`), outside the board's key context.

use overseer_proto::msg::{
    Answer, ConvEntry, ConvKind, ConversationView, PromptInfo, PromptShown, Question,
    QuestionAnswer,
};
use overseer_proto::{ActivityKind, AttentionKind, AttentionView, Request, SlotId, SlotInfo};

use gpui::{
    div, prelude::*, px, rgb, AnyElement, Context, Div, ElementId, FontWeight, KeyDownEvent,
    SharedString, Stateful, Window,
};

use crate::app::{colors, OverseerApp};
use crate::attention_view::oldest_first;
use crate::start_dialog::{edit_key, DialogKey, TextField};

/// How many slots the board has, S0 included.
pub const SLOT_COUNT: usize = 7;

/// One slot's conversation as the app holds it.
#[derive(Debug, Clone)]
pub struct Conv {
    pub view: ConversationView,
    /// The reply being typed.
    pub draft: TextField,
    /// The answers picked so far for the open prompt's questions, one per
    /// question. Empty for a permission prompt or no prompt.
    pub picks: Vec<QuestionAnswer>,
    /// The last thing that went wrong with a send, shown until the next one.
    pub status: Option<String>,
    /// Counts the events pushed for this slot, so a full view asked for
    /// before the latest one can be told apart; see `answered`.
    pub events: u64,
    /// A Reply or Answer is on its way: another is not sent until it is
    /// answered.
    pub sending: bool,
}

impl Conv {
    pub fn new(slot: SlotId) -> Self {
        Conv {
            view: ConversationView {
                slot,
                session: String::new(),
                entries_from: 0,
                entries: Vec::new(),
                prompt: None,
            },
            draft: TextField::default(),
            picks: Vec::new(),
            status: None,
            events: 0,
            sending: false,
        }
    }

    /// Takes in an event the daemon pushed. As `apply`, and counts it.
    pub fn pushed(&mut self, ev: ConversationView) -> bool {
        self.events += 1;
        self.apply(ev)
    }

    /// What `answered` needs to know when a full view is asked for.
    pub fn asked(&self) -> u64 {
        self.events
    }

    /// Takes in the full view asked for when the count was `asked`. A view
    /// asked for before a later push may be older than what we hold, so it
    /// is dropped: `None`, and the caller asks again. Otherwise as `apply`.
    pub fn answered(&mut self, asked: u64, ev: ConversationView) -> Option<bool> {
        (self.events == asked).then(|| self.apply(ev))
    }

    /// Takes in what the daemon sent. True when it fitted; false when the
    /// full view should be asked for again (see `apply`). The prompt is
    /// always the daemon's latest, and a different prompt starts with fresh
    /// picks and no status.
    pub fn apply(&mut self, ev: ConversationView) -> bool {
        let before = self.view.prompt.as_ref().map(|p| p.id.clone());
        let fitted = apply(&mut self.view, ev);
        let after = self.view.prompt.as_ref().map(|p| &p.id);
        if before.as_ref() != after {
            self.picks = blank_picks(&self.view);
            self.status = None;
        }
        fitted
    }

    /// Forgets what the daemon said, as when the connection is lost. The
    /// draft stays: it is the user's own typing.
    /// A send on its way stays on its way, and the count goes up so a view
    /// asked for before is dropped.
    pub fn clear(&mut self) {
        let draft = std::mem::take(&mut self.draft);
        let (events, sending) = (self.events + 1, self.sending);
        *self = Conv::new(self.view.slot);
        self.draft = draft;
        self.events = events;
        self.sending = sending;
    }

    /// The request that sends the draft to the session this view shows, tied
    /// to the open prompt if there is one. Nothing for a blank draft; a slot
    /// with no session yet says so in `status` instead. Nothing while another
    /// send is on its way; a request returned counts as on its way.
    pub fn reply_request(&mut self) -> Option<Request> {
        let text = self.draft.text.trim();
        if text.is_empty() || self.sending {
            return None;
        }
        if self.view.session.is_empty() {
            self.status = Some(format!("{} has no session yet", slot_label(self.view.slot)));
            return None;
        }
        self.status = None;
        self.sending = true;
        Some(Request::Reply {
            slot: self.view.slot,
            session: self.view.session.clone(),
            prompt: self.view.prompt.as_ref().map(|p| p.id.clone()),
            text: self.draft.text.clone(),
        })
    }

    /// The request that answers the open prompt, if there is one and no
    /// other send is on its way. A request returned counts as on its way.
    pub fn answer_request(&mut self, answer: Answer) -> Option<Request> {
        if self.sending {
            return None;
        }
        let prompt = self.view.prompt.as_ref()?;
        self.sending = true;
        Some(Request::Answer {
            slot: self.view.slot,
            prompt: prompt.id.clone(),
            answer,
        })
    }
}

impl Conv {
    /// The open prompt's questions, when it asks some.
    pub fn questions(&self) -> Option<&[Question]> {
        match self.view.prompt.as_ref().map(|p| &p.info) {
            Some(PromptShown::Known(PromptInfo::Question { questions })) => Some(questions),
            _ => None,
        }
    }

    /// Picks option `option` of question `question`: a multi-select
    /// question toggles it, any other takes it in place of its pick.
    pub fn toggle(&mut self, question: usize, option: usize) {
        let Some(multi) = self
            .questions()
            .and_then(|q| q.get(question))
            .map(|q| q.multi)
        else {
            return;
        };
        let Some(QuestionAnswer::Options { picked }) = self.picks.get_mut(question) else {
            return;
        };
        if !multi {
            *picked = vec![option];
        } else if let Some(at) = picked.iter().position(|&p| p == option) {
            picked.remove(at);
        } else {
            picked.push(option);
            picked.sort_unstable();
        }
    }

    /// The whole prompt's answer, once every question has a pick.
    pub fn submit_answer(&self) -> Option<Answer> {
        let questions = self.questions()?;
        let answered = |a: &QuestionAnswer| match a {
            QuestionAnswer::Options { picked } => !picked.is_empty(),
            _ => false,
        };
        let complete = !questions.is_empty()
            && self.picks.len() == questions.len()
            && self.picks.iter().all(answered);
        complete.then(|| Answer::Questions {
            answers: self.picks.clone(),
        })
    }
}

/// What the empty reply box says: at a permission prompt a reply answers
/// No and then tells the session what to do instead.
pub fn reply_placeholder(conv: &Conv) -> String {
    match conv.view.prompt.as_ref().map(|p| &p.info) {
        Some(PromptShown::Known(PromptInfo::Permission { .. })) => "No, and tell it instead".into(),
        _ => format!("Reply to {}", slot_label(conv.view.slot)),
    }
}

/// Whether the reply box takes text: not at a prompt the pane cannot show,
/// which the daemon refuses anyway.
pub fn reply_enabled(conv: &Conv) -> bool {
    !matches!(
        conv.view.prompt.as_ref().map(|p| &p.info),
        Some(PromptShown::Unknown | PromptShown::Known(PromptInfo::Other))
    )
}

/// What a key in the reply box means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyKey {
    Edited,
    Send,
    Escape,
    Paste,
    /// Not for the box.
    Ignored,
}

/// Applies `ks` to the reply being typed: Enter sends, Shift+Enter starts
/// a new line, the rest edits as the start dialog's fields do.
pub fn reply_key(field: &mut TextField, ks: &gpui::Keystroke) -> ReplyKey {
    let m = &ks.modifiers;
    let plain = !m.platform && !m.control && !m.alt && !m.function;
    match ks.key.as_str() {
        "enter" if plain && m.shift => {
            insert_text(field, "\n");
            return ReplyKey::Edited;
        }
        "enter" if plain => return ReplyKey::Send,
        "escape" => return ReplyKey::Escape,
        _ => {}
    }
    match edit_key(field, ks) {
        DialogKey::Edited => ReplyKey::Edited,
        DialogKey::Paste => ReplyKey::Paste,
        _ => ReplyKey::Ignored,
    }
}

/// Puts `text` in at the cursor, new lines and all, in place of a
/// selection.
pub fn insert_text(field: &mut TextField, text: &str) {
    if std::mem::take(&mut field.selected) {
        field.text.clear();
        field.cursor = 0;
    }
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    field.text.insert_str(field.cursor, &text);
    field.cursor += text.len();
}

/// A piece of an entry's text as drawn: plain wrapped text, or a fenced
/// code block in the terminal's font.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Text(String),
    Code(String),
}

/// Splits `text` at its ``` fences; the fence lines themselves are left
/// out, and an unclosed fence runs to the end.
pub fn text_blocks(text: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut lines: Vec<&str> = Vec::new();
    let mut code = false;
    let mut flush = |lines: &mut Vec<&str>, code: bool| {
        let body = lines.join("\n");
        lines.clear();
        if code {
            blocks.push(Block::Code(body));
        } else if !body.trim().is_empty() {
            blocks.push(Block::Text(body.trim_matches('\n').to_string()));
        }
    };
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            flush(&mut lines, code);
            code = !code;
        } else {
            lines.push(line);
        }
    }
    flush(&mut lines, code);
    blocks
}

fn slot_label(slot: SlotId) -> String {
    overseer_proto::slot_name(slot)
}

/// One empty pick per question of the open prompt.
fn blank_picks(view: &ConversationView) -> Vec<QuestionAnswer> {
    match view.prompt.as_ref().map(|p| &p.info) {
        Some(PromptShown::Known(PromptInfo::Question { questions })) => questions
            .iter()
            .map(|_| QuestionAnswer::Options { picked: Vec::new() })
            .collect(),
        _ => Vec::new(),
    }
}

/// Puts `ev` into `conv`. From index 0 it replaces everything. Otherwise it
/// is the daemon resending from `entries_from`, so anything we hold from
/// there on is dropped and replaced. A different session, or a start past
/// what we hold, cannot be joined: it replaces with what came and returns
/// false, so the caller asks for the whole view again.
pub fn apply(conv: &mut ConversationView, ev: ConversationView) -> bool {
    let from = ev.entries_from;
    let joins = from == 0 || (conv.session == ev.session && from <= conv.entries.len());
    if joins && from > 0 {
        conv.entries.truncate(from);
        conv.entries.extend(ev.entries);
    } else {
        conv.entries = ev.entries;
    }
    conv.session = ev.session;
    conv.entries_from = 0;
    conv.prompt = ev.prompt;
    joins
}

/// The slot to jump to next from `current`: one that is `NeedsYou` or has an
/// open prompt. Those the attention list shows come first, oldest first as
/// it orders them; any other waiting slot follows by id. Goes to the one
/// after `current` in that order, wrapping, or to the first when `current`
/// is not waiting. `None` when no slot other than `current` is waiting.
pub fn next_waiting(
    attention: &AttentionView,
    slots: &[SlotInfo],
    convs: &[Conv],
    current: SlotId,
) -> Option<SlotId> {
    let order = waiting_order(attention, slots, convs);
    match order.iter().position(|&s| s == current) {
        Some(at) => order[at + 1..].iter().chain(&order[..at]).copied().next(),
        None => order.first().copied(),
    }
}

/// Where Tab goes from `current`, or what the status says when it stays.
pub fn tab_target(
    attention: &AttentionView,
    slots: &[SlotInfo],
    convs: &[Conv],
    current: SlotId,
) -> Result<SlotId, &'static str> {
    match next_waiting(attention, slots, convs, current) {
        Some(s) => Ok(s),
        None if waiting_order(attention, slots, convs).is_empty() => Err("nothing waiting"),
        None => Err("no other slot waiting"),
    }
}

/// Every waiting slot in the order `next_waiting` goes through them.
fn waiting_order(attention: &AttentionView, slots: &[SlotInfo], convs: &[Conv]) -> Vec<SlotId> {
    let waiting = |s: SlotId| {
        let needs = slots.get(s as usize).is_some_and(|i| {
            i.activity
                .as_ref()
                .is_some_and(|a| a.kind == ActivityKind::NeedsYou && !a.idle_wait)
        });
        let prompt = convs
            .get(s as usize)
            .is_some_and(|c| c.view.prompt.is_some());
        needs || prompt
    };
    let mut order: Vec<SlotId> = Vec::new();
    for item in oldest_first(attention) {
        if let (AttentionKind::NeedsYou, Some(s)) = (item.kind, item.slot) {
            if waiting(s) && !order.contains(&s) {
                order.push(s);
            }
        }
    }
    for s in 0..slots.len().max(convs.len()) {
        let s = s as SlotId;
        if waiting(s) && !order.contains(&s) {
            order.push(s);
        }
    }
    order
}

/// The line an entry shows: what was said, or a tool as `Bash: cargo test`.
/// `None` for an entry kind this build does not know.
pub fn entry_text(e: &ConvEntry) -> Option<String> {
    match &e.kind {
        ConvKind::User { text } | ConvKind::Assistant { text } => Some(text.clone()),
        ConvKind::Tool { name, summary } if summary.is_empty() => Some(name.clone()),
        ConvKind::Tool { name, summary } => Some(format!("{name}: {summary}")),
        ConvKind::Other => None,
    }
}

// ---------------------------------------------------------------------------
// The pane
// ---------------------------------------------------------------------------

/// The pane's width beside the board.
pub const PANE_WIDTH: f32 = 420.;

/// The choices of a permission prompt, by the index `Answer::Permission`
/// sends.
pub const PERMISSION_CHOICES: [&str; 3] = ["Yes", "Yes, and don't ask again", "No"];

fn is_working(info: Option<&SlotInfo>) -> bool {
    info.and_then(|i| i.activity.as_ref())
        .is_some_and(|a| a.kind == ActivityKind::Working)
}

/// A button; `on` marks a picked option.
fn pane_button(
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    enabled: bool,
    on: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded(px(5.))
        .text_sm()
        .bg(rgb(if on {
            colors::ACCENT_BG
        } else {
            colors::BUTTON
        }))
        .border_1()
        .border_color(rgb(if on {
            colors::ACCENT
        } else {
            colors::BUTTON_BORDER
        }))
        .text_color(rgb(if !enabled {
            colors::MUTED
        } else if on {
            colors::ACCENT
        } else {
            colors::TEXT
        }))
        .when(enabled, |b| {
            b.cursor_pointer().hover(|s| s.bg(rgb(colors::CARD_HOVER)))
        })
        .child(text.into())
}

/// `text` as plain wrapped paragraphs and fenced code in `font`.
fn text_element(text: &str, font: &str) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .children(text_blocks(text).into_iter().map(|b| {
            match b {
                Block::Text(t) => div().child(t),
                Block::Code(t) => div()
                    .px_1()
                    .rounded(px(3.))
                    .bg(rgb(colors::FIELD))
                    .font_family(SharedString::from(font.to_string()))
                    .text_xs()
                    .child(if t.is_empty() { " ".to_string() } else { t }),
            }
        }))
}

fn entry_element(e: &ConvEntry, font: &str) -> Option<AnyElement> {
    Some(match &e.kind {
        ConvKind::User { text } => div()
            .p_2()
            .rounded(px(6.))
            .bg(rgb(colors::ACCENT_BG))
            .child(text_element(text, font))
            .into_any_element(),
        ConvKind::Assistant { text } => text_element(text, font).into_any_element(),
        ConvKind::Tool { .. } => div()
            .text_xs()
            .text_color(rgb(colors::MUTED))
            .truncate()
            .child(entry_text(e)?)
            .into_any_element(),
        ConvKind::Other => return None,
    })
}

impl OverseerApp {
    /// Hides or shows the pane and remembers it. A failed save only costs
    /// the choice on the next start, so it goes to the status line.
    pub(crate) fn toggle_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pane_hidden = !self.pane_hidden;
        if self.pane_hidden && self.reply_focus.is_focused(window) {
            window.focus(&self.board_focus);
        }
        let path = crate::kanban_size::path();
        if let Err(e) = crate::kanban_size::save_pane_hidden(&path, self.pane_hidden) {
            self.status = Some(format!("cannot save whether the pane shows: {e}"));
        }
        cx.notify();
    }

    /// Tab: selects the next waiting slot, or says why not.
    pub(crate) fn next_waiting_slot(&mut self, cx: &mut Context<Self>) {
        match tab_target(&self.attention, &self.slots, &self.convs, self.selected) {
            Ok(slot) => {
                self.status = None;
                self.select(slot, cx);
            }
            Err(why) => self.status = Some(why.into()),
        }
        cx.notify();
    }

    fn on_reply_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let slot = self.selected;
        let working = is_working(self.slots.get(slot as usize));
        let Some(conv) = self.convs.get_mut(slot as usize) else {
            return;
        };
        let ks = &ev.keystroke;
        let key = if reply_enabled(conv) {
            reply_key(&mut conv.draft, ks)
        } else if ks.key == "escape" {
            ReplyKey::Escape
        } else {
            ReplyKey::Ignored
        };
        match key {
            ReplyKey::Ignored => return,
            ReplyKey::Edited => {}
            ReplyKey::Paste => {
                if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                    insert_text(&mut conv.draft, &text);
                }
            }
            ReplyKey::Send => self.send_reply(slot, cx),
            ReplyKey::Escape if working => self.interrupt(slot, cx),
            ReplyKey::Escape => window.focus(&self.board_focus),
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// The open prompt's buttons, above the reply box.
    fn render_prompt(&self, slot: SlotId, conv: &Conv, cx: &mut Context<Self>) -> Option<Div> {
        let prompt = conv.view.prompt.as_ref()?;
        let font = self.theme.font_family.as_str();
        let sending = conv.sending;
        let area = div()
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .rounded(px(6.))
            .bg(rgb(colors::CARD))
            .border_1()
            .border_color(rgb(colors::ORANGE))
            .when(prompt.subagent, |d| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(rgb(colors::PURPLE))
                        .child("subagent"),
                )
            });
        Some(match &prompt.info {
            PromptShown::Known(PromptInfo::Question { questions }) => {
                let rows = questions.iter().enumerate().map(|(qi, q)| {
                    let picked: &[usize] = match conv.picks.get(qi) {
                        Some(QuestionAnswer::Options { picked }) => picked,
                        _ => &[],
                    };
                    let options = q.options.iter().enumerate().map(|(oi, text)| {
                        pane_button(
                            ("conv-option", qi * 1000 + oi),
                            text.clone(),
                            !sending,
                            picked.contains(&oi),
                        )
                        .when(!sending, |b| {
                            b.on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(c) = this.convs.get_mut(slot as usize) {
                                    c.toggle(qi, oi);
                                }
                                cx.notify();
                            }))
                        })
                    });
                    let header = if q.multi {
                        format!("{} (pick any)", q.header)
                    } else {
                        q.header.clone()
                    };
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(div().text_xs().text_color(rgb(colors::MUTED)).child(header))
                        .child(div().text_sm().child(q.question.clone()))
                        .child(div().flex().flex_wrap().gap_1().children(options))
                        .into_any_element()
                });
                let ready = conv.submit_answer().filter(|_| !sending);
                let submit = pane_button("conv-submit", "Submit", ready.is_some(), false)
                    .when_some(ready, |b, answer| {
                        b.on_click(cx.listener(move |this, _, _, cx| {
                            this.send_answer(slot, answer.clone(), cx)
                        }))
                    });
                area.children(rows).child(div().flex().child(submit))
            }
            PromptShown::Known(PromptInfo::Permission {
                tool,
                summary,
                plan,
            }) => {
                let line = if summary.is_empty() {
                    tool.clone()
                } else {
                    format!("{tool}: {summary}")
                };
                let choices = PERMISSION_CHOICES.iter().enumerate().map(|(i, text)| {
                    pane_button(("conv-choice", i), *text, !sending, false).when(!sending, |b| {
                        b.on_click(cx.listener(move |this, _, _, cx| {
                            this.send_answer(slot, Answer::Permission { choice: i }, cx)
                        }))
                    })
                });
                area.children(plan.as_ref().map(|plan| {
                    div()
                        .id("conv-plan")
                        .max_h(px(220.))
                        .overflow_y_scroll()
                        .text_sm()
                        .child(text_element(plan, font))
                }))
                .child(
                    div()
                        .text_sm()
                        .font_family(SharedString::from(font.to_string()))
                        .child(line),
                )
                .child(div().flex().flex_wrap().gap_1().children(choices))
            }
            PromptShown::Known(PromptInfo::Other) | PromptShown::Unknown => area
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(colors::ORANGE))
                        .child("waiting on a prompt Overseer can't show"),
                )
                .child(div().flex().child(
                    pane_button("conv-prompt-open", "Open session", true, false).on_click(
                        cx.listener(move |this, _, window, cx| this.open_card(slot, window, cx)),
                    ),
                )),
        })
    }

    /// The selected slot's conversation: header, entries, the open prompt,
    /// the reply box and its status.
    pub(crate) fn render_conversation(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let slot = self.selected;
        let Some(conv) = self.convs.get(slot as usize) else {
            return div().into_any_element();
        };
        // Follow new entries, unless the user has scrolled up from the end.
        let seen = (slot, conv.view.session.clone(), conv.view.entries.len());
        if self.pane_seen != seen {
            let moved = self.pane_seen.0 != slot || self.pane_seen.1 != seen.1;
            let h = &self.pane_scroll;
            let from_end = f32::from(h.max_offset().height) + f32::from(h.offset().y);
            if moved || from_end < 24. {
                h.scroll_to_bottom();
            }
            self.pane_seen = seen;
        }
        let conv = &self.convs[slot as usize];
        let info = self.slots.get(slot as usize);
        let working = is_working(info);
        let font = self.theme.font_family.as_str();
        let focused = self.reply_focus.is_focused(window);
        let title = info.map_or_else(|| slot_label(slot), crate::board::slot_title);
        let header = div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(rgb(colors::MUTED))
                    .child("Tab next waiting"),
            )
            .child(
                pane_button("conv-open", "Open session", true, false).on_click(
                    cx.listener(move |this, _, window, cx| this.open_card(slot, window, cx)),
                ),
            )
            .child(
                pane_button("conv-hide", "Hide", true, false)
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_pane(window, cx))),
            );
        let entries: Vec<_> = conv
            .view
            .entries
            .iter()
            .filter_map(|e| entry_element(e, font))
            .collect();
        let empty = entries.is_empty().then(|| {
            div()
                .text_sm()
                .text_color(rgb(colors::MUTED))
                .child(if conv.view.session.is_empty() {
                    format!("{} has no conversation to show", slot_label(slot))
                } else {
                    "Nothing said yet".to_string()
                })
        });
        let body = div()
            .id("conv-body")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.pane_scroll)
            .flex()
            .flex_col()
            .gap_2()
            .text_sm()
            .children(empty)
            .children(entries);
        let prompt = self.render_prompt(slot, conv, cx);
        let enabled = reply_enabled(conv);
        let draft = &conv.draft;
        let shown: AnyElement = if draft.text.is_empty() {
            let placeholder = reply_placeholder(conv);
            div()
                .text_color(rgb(colors::MUTED))
                .child(if focused && enabled {
                    format!("|{placeholder}")
                } else {
                    placeholder
                })
                .into_any_element()
        } else {
            div().child(draft.display(focused)).into_any_element()
        };
        let reply = div()
            .key_context("ConvReply")
            .track_focus(&self.reply_focus)
            .on_key_down(cx.listener(Self::on_reply_key))
            .min_h(px(56.))
            .p_2()
            .rounded(px(6.))
            .bg(rgb(colors::FIELD))
            .border_1()
            .border_color(rgb(if focused && enabled {
                colors::ACCENT
            } else {
                colors::BORDER
            }))
            .text_sm()
            .when(!enabled, |d| d.text_color(rgb(colors::MUTED)))
            .cursor_text()
            .child(shown);
        let can_send = enabled && !conv.sending && !draft.text.trim().is_empty();
        let actions = div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .text_xs()
                    .text_color(rgb(colors::MUTED))
                    .truncate()
                    .child("Enter sends · Shift+Enter new line · Cmd+\\ hides"),
            )
            .children(working.then(|| {
                pane_button("conv-stop", "Stop", true, false)
                    .on_click(cx.listener(move |this, _, _, cx| this.interrupt(slot, cx)))
            }))
            .child(
                pane_button(
                    "conv-send",
                    if conv.sending {
                        "Sending\u{2026}"
                    } else {
                        "Send"
                    },
                    can_send,
                    false,
                )
                .when(can_send, |b| {
                    b.on_click(cx.listener(move |this, _, _, cx| this.send_reply(slot, cx)))
                }),
            );
        let status = conv
            .status
            .clone()
            .map(|s| div().text_xs().text_color(rgb(colors::ORANGE)).child(s));
        div()
            .key_context("Conversation")
            .flex_none()
            .w(px(PANE_WIDTH))
            .h_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .border_l_1()
            .border_color(rgb(colors::BORDER))
            .bg(rgb(colors::BG))
            .child(header)
            .child(body)
            .children(prompt)
            .child(reply)
            .child(actions)
            .children(status)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use overseer_proto::msg::Prompt;
    use overseer_proto::{Activity, AttentionItem, SlotState};

    fn entry(text: &str) -> ConvEntry {
        ConvEntry {
            at: 1,
            kind: ConvKind::User { text: text.into() },
        }
    }

    fn view(session: &str, from: usize, texts: &[&str]) -> ConversationView {
        ConversationView {
            slot: 1,
            session: session.into(),
            entries_from: from,
            entries: texts.iter().map(|t| entry(t)).collect(),
            prompt: None,
        }
    }

    fn texts(v: &ConversationView) -> Vec<String> {
        v.entries.iter().filter_map(entry_text).collect()
    }

    fn question_prompt(id: &str, n: usize) -> Prompt {
        Prompt {
            id: id.into(),
            subagent: false,
            info: PromptShown::Known(PromptInfo::Question {
                questions: (0..n)
                    .map(|i| Question {
                        header: format!("Q{i}"),
                        question: "Which?".into(),
                        options: vec!["a".into(), "b".into()],
                        multi: false,
                    })
                    .collect(),
            }),
        }
    }

    #[test]
    fn appends_from_entries_from() {
        let mut v = view("s1", 0, &["a", "b"]);
        assert!(apply(&mut v, view("s1", 2, &["c"])));
        assert_eq!(texts(&v), ["a", "b", "c"]);
        // A resend from earlier replaces the tail.
        assert!(apply(&mut v, view("s1", 1, &["B", "c"])));
        assert_eq!(texts(&v), ["a", "B", "c"]);
    }

    #[test]
    fn a_new_session_replaces() {
        let mut v = view("s1", 0, &["a", "b"]);
        assert!(apply(&mut v, view("s2", 0, &["x"])));
        assert_eq!((v.session.as_str(), texts(&v)), ("s2", vec!["x".into()]));
        // A tail of another session cannot be joined.
        assert!(!apply(&mut v, view("s3", 1, &["y"])));
        assert_eq!((v.session.as_str(), texts(&v)), ("s3", vec!["y".into()]));
    }

    #[test]
    fn a_gap_asks_again() {
        let mut v = view("s1", 0, &["a"]);
        assert!(!apply(&mut v, view("s1", 5, &["f"])));
        assert_eq!(texts(&v), ["f"]);
    }

    #[test]
    fn a_new_prompt_resets_picks_and_status() {
        let mut c = Conv::new(1);
        let mut ev = view("s1", 0, &[]);
        ev.prompt = Some(question_prompt("p1", 2));
        assert!(c.apply(ev.clone()));
        assert_eq!(c.picks.len(), 2);
        c.picks[0] = QuestionAnswer::Options { picked: vec![1] };
        c.status = Some("S1 moved on, not sent".into());
        // The same prompt again keeps what was picked.
        assert!(c.apply(ev.clone()));
        assert_eq!(c.picks[0], QuestionAnswer::Options { picked: vec![1] });
        assert!(c.status.is_some());
        ev.prompt = Some(question_prompt("p2", 1));
        c.apply(ev.clone());
        assert_eq!(c.picks, [QuestionAnswer::Options { picked: vec![] }]);
        assert_eq!(c.status, None);
        ev.prompt = None;
        c.apply(ev);
        assert!(c.picks.is_empty());
    }

    #[test]
    fn replies_carry_the_shown_session_and_prompt() {
        let mut c = Conv::new(2);
        c.draft.set("  go ahead ");
        assert_eq!(c.reply_request(), None);
        assert_eq!(c.status.as_deref(), Some("S2 has no session yet"));
        let mut ev = view("s9", 0, &[]);
        ev.slot = 2;
        ev.prompt = Some(question_prompt("p1", 1));
        c.apply(ev);
        assert_eq!(
            c.reply_request(),
            Some(Request::Reply {
                slot: 2,
                session: "s9".into(),
                prompt: Some("p1".into()),
                text: "  go ahead ".into(),
            })
        );
        assert_eq!(c.status, None);
        c.draft.set("   ");
        assert_eq!(c.reply_request(), None);
    }

    #[test]
    fn answers_need_an_open_prompt() {
        let mut c = Conv::new(1);
        let a = Answer::Permission { choice: 0 };
        assert_eq!(c.answer_request(a.clone()), None);
        let mut ev = view("s1", 0, &[]);
        ev.prompt = Some(question_prompt("p1", 1));
        c.apply(ev);
        assert_eq!(
            c.answer_request(a.clone()),
            Some(Request::Answer {
                slot: 1,
                prompt: "p1".into(),
                answer: a
            })
        );
    }

    #[test]
    fn clearing_keeps_the_draft() {
        let mut c = Conv::new(1);
        c.apply(view("s1", 0, &["a"]));
        c.draft.set("typing");
        c.clear();
        assert!(c.view.entries.is_empty() && c.view.session.is_empty());
        assert_eq!(c.draft.text, "typing");
        assert_eq!(c.view.slot, 1);
    }

    fn slot(id: SlotId, needs: bool) -> SlotInfo {
        SlotInfo {
            id,
            label: String::new(),
            cwd: None,
            command: None,
            state: SlotState::Empty,
            cols: 80,
            rows: 24,
            activity: needs.then_some(Activity {
                kind: ActivityKind::NeedsYou,
                since: 0,
                now_doing: None,
                reason: None,
                idle_wait: false,
            }),
            session_note: None,
            held: false,
        }
    }

    fn needs_item(slot: SlotId, since: i64) -> AttentionItem {
        AttentionItem {
            key: format!("needs_you:{slot}"),
            kind: AttentionKind::NeedsYou,
            slot: Some(slot),
            pr: None,
            url: None,
            since,
            text: "needs you".into(),
        }
    }

    fn convs() -> Vec<Conv> {
        (0..SLOT_COUNT as SlotId).map(Conv::new).collect()
    }

    #[test]
    fn next_waiting_follows_the_attention_order_and_wraps() {
        // S5 has waited longest, then S2; S4 only has a prompt (no item).
        let slots: Vec<_> = (0..7).map(|i| slot(i, i == 2 || i == 5)).collect();
        let attention = AttentionView {
            items: vec![needs_item(2, 200), needs_item(5, 100)],
        };
        let mut cs = convs();
        let mut ev = view("s", 0, &[]);
        ev.slot = 4;
        ev.prompt = Some(question_prompt("p", 1));
        cs[4].apply(ev);
        // Order is S5, S2, S4.
        assert_eq!(next_waiting(&attention, &slots, &cs, 5), Some(2));
        assert_eq!(next_waiting(&attention, &slots, &cs, 2), Some(4));
        assert_eq!(next_waiting(&attention, &slots, &cs, 4), Some(5));
        // From a slot that is not waiting, the first.
        assert_eq!(next_waiting(&attention, &slots, &cs, 1), Some(5));
    }

    #[test]
    fn next_waiting_with_none_is_none() {
        let slots: Vec<_> = (0..7).map(|i| slot(i, false)).collect();
        let cs = convs();
        assert_eq!(
            next_waiting(&AttentionView::default(), &slots, &cs, 1),
            None
        );
        // Only the current slot waiting is not somewhere to go.
        let slots: Vec<_> = (0..7).map(|i| slot(i, i == 3)).collect();
        assert_eq!(
            next_waiting(&AttentionView::default(), &slots, &cs, 3),
            None
        );
    }

    #[test]
    fn drafts_are_kept_per_slot() {
        let mut cs = crate::app::empty_convs();
        assert_eq!(cs.len(), crate::app::empty_slots().len());
        cs[1].draft.set("half a thought");
        // Selecting S2 and back only asks for views; S1's draft is its own.
        let mut ev = view("s2", 0, &["hello"]);
        ev.slot = 2;
        cs[2].apply(ev);
        assert_eq!(cs[2].draft.text, "");
        cs[1].apply(view("s1", 0, &["hi"]));
        assert_eq!(cs[1].draft.text, "half a thought");
        assert_eq!(cs[2].draft.text, "");
    }

    #[test]
    fn tool_entries_read_as_one_line() {
        let tool = |name: &str, summary: &str| ConvEntry {
            at: 0,
            kind: ConvKind::Tool {
                name: name.into(),
                summary: summary.into(),
            },
        };
        assert_eq!(
            entry_text(&tool("Bash", "cargo test")).as_deref(),
            Some("Bash: cargo test")
        );
        assert_eq!(entry_text(&tool("Task", "")).as_deref(), Some("Task"));
        let other = ConvEntry {
            at: 0,
            kind: ConvKind::Other,
        };
        assert_eq!(entry_text(&other), None);
    }

    fn permission_prompt(id: &str) -> Prompt {
        Prompt {
            id: id.into(),
            subagent: false,
            info: PromptShown::Known(PromptInfo::Permission {
                tool: "Bash".into(),
                summary: "rm -rf target".into(),
                plan: None,
            }),
        }
    }

    fn with_prompt(slot: SlotId, prompt: Option<Prompt>) -> Conv {
        let mut c = Conv::new(slot);
        let mut ev = view("s1", 0, &[]);
        ev.slot = slot;
        ev.prompt = prompt;
        c.apply(ev);
        c
    }

    #[test]
    fn submit_waits_for_every_question() {
        let mut c = with_prompt(1, Some(question_prompt("p1", 2)));
        assert_eq!(c.submit_answer(), None);
        c.toggle(0, 1);
        assert_eq!(c.submit_answer(), None, "one question still open");
        c.toggle(1, 0);
        // A single-select question takes the latest pick only.
        c.toggle(1, 1);
        assert_eq!(
            c.submit_answer(),
            Some(Answer::Questions {
                answers: vec![
                    QuestionAnswer::Options { picked: vec![1] },
                    QuestionAnswer::Options { picked: vec![1] },
                ]
            })
        );
        // A multi-select question toggles, and none picked is unanswered.
        let mut p = question_prompt("p2", 1);
        if let PromptShown::Known(PromptInfo::Question { questions }) = &mut p.info {
            questions[0].multi = true;
        }
        let mut c = with_prompt(1, Some(p));
        c.toggle(0, 1);
        c.toggle(0, 0);
        assert_eq!(
            c.submit_answer(),
            Some(Answer::Questions {
                answers: vec![QuestionAnswer::Options { picked: vec![0, 1] }]
            })
        );
        c.toggle(0, 0);
        c.toggle(0, 1);
        assert_eq!(c.submit_answer(), None);
        // No question prompt, nothing to submit; a stray pick is ignored.
        let mut c = with_prompt(1, Some(permission_prompt("p3")));
        c.toggle(0, 0);
        assert_eq!(c.submit_answer(), None);
    }

    #[test]
    fn reply_placeholder_follows_the_prompt() {
        assert_eq!(reply_placeholder(&with_prompt(3, None)), "Reply to S3");
        assert_eq!(
            reply_placeholder(&with_prompt(3, Some(question_prompt("p", 1)))),
            "Reply to S3"
        );
        assert_eq!(
            reply_placeholder(&with_prompt(3, Some(permission_prompt("p")))),
            "No, and tell it instead"
        );
        // A prompt the pane cannot show takes no reply.
        let unknown = Prompt {
            id: "p".into(),
            subagent: false,
            info: PromptShown::Unknown,
        };
        assert!(reply_enabled(&with_prompt(3, None)));
        assert!(reply_enabled(&with_prompt(3, Some(permission_prompt("p")))));
        assert!(!reply_enabled(&with_prompt(3, Some(unknown))));
        let other = Prompt {
            id: "p".into(),
            subagent: false,
            info: PromptShown::Known(PromptInfo::Other),
        };
        assert!(!reply_enabled(&with_prompt(3, Some(other))));
    }

    #[test]
    fn a_send_on_its_way_is_not_sent_again() {
        let mut c = with_prompt(2, Some(permission_prompt("p1")));
        c.draft.set("try again");
        assert!(c.reply_request().is_some());
        assert_eq!(c.reply_request(), None, "a second Enter");
        let a = Answer::Permission { choice: 0 };
        assert_eq!(c.answer_request(a.clone()), None, "a click meanwhile");
        c.sending = false;
        assert!(c.answer_request(a.clone()).is_some());
        assert_eq!(c.answer_request(a), None);
        // Losing the connection does not forget it.
        c.clear();
        assert!(c.sending);
    }

    #[test]
    fn a_view_asked_for_before_a_push_is_dropped() {
        let mut c = Conv::new(1);
        let asked = c.asked();
        assert!(c.pushed(view("s1", 0, &["a", "b", "c"])));
        // The full view left before the push and may be older.
        assert_eq!(c.answered(asked, view("s1", 0, &["a"])), None);
        assert_eq!(texts(&c.view), ["a", "b", "c"]);
        // Asked again with nothing pushed since: taken.
        let asked = c.asked();
        assert_eq!(
            c.answered(asked, view("s1", 0, &["a", "b", "c", "d"])),
            Some(true)
        );
        assert_eq!(texts(&c.view), ["a", "b", "c", "d"]);
        // A view asked for before the connection was lost is dropped too.
        let asked = c.asked();
        c.clear();
        assert_eq!(c.answered(asked, view("s1", 0, &["x"])), None);
    }

    #[test]
    fn tab_says_why_it_stays() {
        let none: Vec<_> = (0..7).map(|i| slot(i, false)).collect();
        let cs = convs();
        let attention = AttentionView::default();
        assert_eq!(
            tab_target(&attention, &none, &cs, 1),
            Err("nothing waiting")
        );
        let only3: Vec<_> = (0..7).map(|i| slot(i, i == 3)).collect();
        assert_eq!(
            tab_target(&attention, &only3, &cs, 3),
            Err("no other slot waiting")
        );
        assert_eq!(tab_target(&attention, &only3, &cs, 1), Ok(3));
    }

    #[test]
    fn enter_sends_and_shift_enter_breaks_the_line() {
        let ks = |s: &str| gpui::Keystroke::parse(s).unwrap();
        let mut f = TextField::default();
        for k in ["h", "i"] {
            let mut k = ks(k);
            k.key_char = Some(k.key.clone());
            assert_eq!(reply_key(&mut f, &k), ReplyKey::Edited);
        }
        assert_eq!(reply_key(&mut f, &ks("shift-enter")), ReplyKey::Edited);
        assert_eq!(f.text, "hi\n");
        assert_eq!(reply_key(&mut f, &ks("enter")), ReplyKey::Send);
        assert_eq!(reply_key(&mut f, &ks("escape")), ReplyKey::Escape);
        assert_eq!(reply_key(&mut f, &ks("cmd-v")), ReplyKey::Paste);
        assert_eq!(reply_key(&mut f, &ks("shift-tab")), ReplyKey::Ignored);
        // A paste keeps its lines and replaces a selection.
        f.select_all();
        insert_text(&mut f, "one\ntwo");
        assert_eq!((f.text.as_str(), f.cursor), ("one\ntwo", 7));
    }

    #[test]
    fn fenced_code_is_its_own_block() {
        let text = "Run this:\n```sh\ncargo test\n  -p demo-api\n```\nThen look.";
        assert_eq!(
            text_blocks(text),
            [
                Block::Text("Run this:".into()),
                Block::Code("cargo test\n  -p demo-api".into()),
                Block::Text("Then look.".into()),
            ]
        );
        // An unclosed fence runs to the end; plain text is one block.
        assert_eq!(
            text_blocks("```\nfn main() {}"),
            [Block::Code("fn main() {}".into())]
        );
        assert_eq!(text_blocks("a\nb"), [Block::Text("a\nb".into())]);
    }
}
