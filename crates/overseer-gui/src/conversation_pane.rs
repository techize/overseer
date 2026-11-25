//! The pane beside the board that draws a slot's conversation: entries, the
//! open prompt's buttons and the reply box (`OverseerApp::render_conversation`).
//! The model it draws lives in `conversation_view`.

use overseer_proto::msg::{Answer, ConvEntry, ConvKind, PromptInfo, PromptShown, QuestionAnswer};
use overseer_proto::SlotId;

use gpui::{
    div, prelude::*, px, rgb, AnyElement, Context, Div, ElementId, FontWeight, KeyDownEvent,
    SharedString, Stateful, Window,
};

use crate::app::{colors, OverseerApp};
use crate::conversation_view::{
    activity_of, choice_buttons, entry_text, insert_text, interrupt_note, is_working,
    reply_enabled, reply_key, reply_placeholder, slot_label, tab_target, text_blocks,
    unseen_prompt, Block, Conv, ReplyKey, TAB_MESSAGES,
};

/// The pane's width beside the board.
pub const PANE_WIDTH: f32 = 420.;

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
        ConvKind::User { text } if interrupt_note(text) => div()
            .text_xs()
            .text_color(rgb(colors::MUTED))
            .child("interrupted")
            .into_any_element(),
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
                if self
                    .status
                    .as_deref()
                    .is_some_and(|s| TAB_MESSAGES.contains(&s))
                {
                    self.status = None;
                }
                self.select(slot, cx);
            }
            Err(why) => self.status = Some(why.into()),
        }
        cx.notify();
    }

    fn on_reply_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let slot = self.selected;
        let info = self.slots.get(slot as usize);
        let working = is_working(info);
        let enabled = self
            .convs
            .get(slot as usize)
            .is_some_and(|c| reply_enabled(c, activity_of(info)));
        let Some(conv) = self.convs.get_mut(slot as usize) else {
            return;
        };
        let ks = &ev.keystroke;
        let key = if enabled {
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
            ReplyKey::Escape if conv.can_stop(working) => self.interrupt(slot, cx),
            ReplyKey::Escape => window.focus(&self.board_focus),
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// The open prompt's buttons, above the reply box.
    fn render_prompt(
        &self,
        slot: SlotId,
        conv: &Conv,
        cx: &mut Context<Self>,
    ) -> Option<Stateful<Div>> {
        let activity = activity_of(self.slots.get(slot as usize));
        let Some(prompt) = conv.view.prompt.as_ref() else {
            return unseen_prompt(conv, activity).then(|| self.cant_show(slot, cx));
        };
        let font = self.theme.font_family.as_str();
        let sending = conv.sending;
        // Kept to a height, so many questions cannot push the reply box away.
        let area = div()
            .id("conv-prompt")
            .flex_none()
            .max_h(px(320.))
            .overflow_y_scroll()
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
                let choices = choice_buttons(plan.is_some())
                    .iter()
                    .enumerate()
                    .map(|(i, text)| {
                        pane_button(("conv-choice", i), *text, !sending, false).when(
                            !sending,
                            |b| {
                                b.on_click(cx.listener(move |this, _, _, cx| {
                                    this.send_answer(slot, Answer::Permission { choice: i }, cx)
                                }))
                            },
                        )
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
                .when(plan.is_some(), |d| {
                    d.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .text_sm()
                                    .text_color(rgb(colors::ORANGE))
                                    .child("Approve the plan in the session"),
                            )
                            .child(
                                pane_button("conv-plan-open", "Open session", true, false)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.open_card(slot, window, cx)
                                    })),
                            ),
                    )
                })
            }
            PromptShown::Known(PromptInfo::Other) | PromptShown::Unknown => {
                return Some(self.cant_show(slot, cx));
            }
        })
    }

    /// Says the slot waits on a prompt the pane cannot show, with a button
    /// to open its session.
    fn cant_show(&self, slot: SlotId, cx: &mut Context<Self>) -> Stateful<Div> {
        div()
            .id("conv-prompt")
            .flex_none()
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .rounded(px(6.))
            .bg(rgb(colors::CARD))
            .border_1()
            .border_color(rgb(colors::ORANGE))
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
            ))
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
        let unseen = unseen_prompt(conv, activity_of(info));
        let empty = (entries.is_empty() && !unseen).then(|| {
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
        let enabled = !unseen;
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
            .children(conv.can_stop(working).then(|| {
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
