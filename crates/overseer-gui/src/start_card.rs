//! The confirm card for a ticket dropped onto a free slot (#87). It shows
//! what the start will do (slot, repo, branch, worktree and the brief) and
//! does nothing until Start; Cancel or Esc leaves the board as it was.
//!
//! Left and Right (or the repo row's arrows) pick another repo, which asks
//! the daemon for a new draft, since the free branch name depends on the
//! repo. Enter starts.
//!
//! The key handling and the repo cycling are pure (`card_key`, `next_repo`)
//! and unit tested.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{
    div, prelude::*, px, rgb, rgba, Context, EventEmitter, FocusHandle, Focusable, FontWeight,
    KeyDownEvent, Keystroke, MouseButton, SharedString, Window,
};
use overseer_proto::{slot_name, Client, Request, Response, SlotId, StartDraft};

use crate::app::colors;

// ---------------------------------------------------------------------------
// Pure logic
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardKey {
    Close,
    Start,
    /// Pick the repo before (-1) or after (+1) the current one.
    Repo(i8),
    Ignored,
}

pub fn card_key(k: &Keystroke) -> CardKey {
    let m = &k.modifiers;
    if m.platform || m.control || m.alt {
        return CardKey::Ignored;
    }
    match k.key.as_str() {
        "escape" => CardKey::Close,
        "enter" => CardKey::Start,
        "left" | "up" => CardKey::Repo(-1),
        "right" | "down" => CardKey::Repo(1),
        "tab" if m.shift => CardKey::Repo(-1),
        "tab" => CardKey::Repo(1),
        _ => CardKey::Ignored,
    }
}

/// The repo `step` places from `current` in `repos`, wrapping around.
/// `None` when there is nothing else to pick.
pub fn next_repo(repos: &[PathBuf], current: &Path, step: i8) -> Option<PathBuf> {
    if repos.len() < 2 {
        return None;
    }
    let n = repos.len() as isize;
    let at = repos.iter().position(|r| r == current).unwrap_or(0) as isize;
    let to = (at + isize::from(step)).rem_euclid(n);
    Some(repos[to as usize].clone())
}

/// A repo's folder name, for the row.
fn repo_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

// ---------------------------------------------------------------------------
// The view
// ---------------------------------------------------------------------------

pub enum StartCardEvent {
    /// The slot started; `text` is the daemon's account of the steps.
    Started {
        slot: SlotId,
        text: String,
    },
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Busy {
    Idle,
    /// A new draft for another repo is on its way.
    Drafting,
    /// The start is on its way; the card cannot be closed until it answers.
    Starting,
}

pub struct StartCard {
    draft: StartDraft,
    busy: Busy,
    error: Option<String>,
    client: Arc<Client>,
    focus_handle: FocusHandle,
}

impl EventEmitter<StartCardEvent> for StartCard {}

impl Focusable for StartCard {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

fn lost(e: impl std::fmt::Display) -> String {
    format!("{} ({e:#})", crate::terminal_view::LOST_DAEMON)
}

impl StartCard {
    pub fn new(draft: StartDraft, client: Arc<Client>, cx: &mut Context<Self>) -> Self {
        StartCard {
            draft,
            busy: Busy::Idle,
            error: None,
            client,
            focus_handle: cx.focus_handle(),
        }
    }

    /// The client to use from now on, after a reconnect.
    pub fn set_client(&mut self, client: Arc<Client>) {
        self.client = client;
    }

    pub fn slot(&self) -> SlotId {
        self.draft.slot
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        if self.busy == Busy::Starting {
            return;
        }
        cx.emit(StartCardEvent::Closed);
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        match card_key(&ev.keystroke) {
            CardKey::Ignored => return,
            CardKey::Close => self.close(cx),
            CardKey::Start => self.start(cx),
            CardKey::Repo(step) => self.pick_repo(step, cx),
        }
        cx.stop_propagation();
    }

    fn pick_repo(&mut self, step: i8, cx: &mut Context<Self>) {
        if self.busy != Busy::Idle {
            return;
        }
        let Some(repo) = next_repo(&self.draft.repos, &self.draft.repo, step) else {
            return;
        };
        self.busy = Busy::Drafting;
        self.error = None;
        let req = Request::PrepareStart {
            slot: self.draft.slot,
            key: self.draft.key.clone(),
            repo: Some(repo),
        };
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let reply = client.request(req).await;
            let _ = this.update(cx, |this, cx| {
                this.busy = Busy::Idle;
                match reply {
                    Ok(Response::StartDraft(d)) => this.draft = d,
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

    fn start(&mut self, cx: &mut Context<Self>) {
        if self.busy != Busy::Idle {
            return;
        }
        self.busy = Busy::Starting;
        self.error = None;
        let slot = self.draft.slot;
        let req = Request::StartTicket {
            slot,
            key: self.draft.key.clone(),
            repo: self.draft.repo.clone(),
            branch: self.draft.branch.clone(),
        };
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let reply = client.request(req).await;
            let _ = this.update(cx, |this, cx| {
                this.busy = Busy::Idle;
                match reply {
                    Ok(Response::Text { text }) => cx.emit(StartCardEvent::Started { slot, text }),
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
}

fn button(id: &'static str, text: &'static str, primary: bool) -> gpui::Stateful<gpui::Div> {
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

/// One label and value line of the card.
fn row(label: &'static str, value: impl IntoElement) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .flex_none()
                .w(px(72.))
                .text_xs()
                .text_color(rgb(colors::MUTED))
                .child(label),
        )
        .child(div().flex_1().min_w(px(0.)).child(value))
}

impl Render for StartCard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let d = &self.draft;
        let many = d.repos.len() > 1;
        let arrow = |id: &'static str, glyph: &'static str, step: i8| {
            div()
                .id(id)
                .flex_none()
                .px_1()
                .rounded(px(4.))
                .text_color(rgb(colors::ACCENT))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(colors::CARD_HOVER)))
                .child(glyph)
                .on_click(cx.listener(move |this, _, _, cx| this.pick_repo(step, cx)))
        };
        let repo = div()
            .flex()
            .items_center()
            .gap_1()
            .children(many.then(|| arrow("start-card-prev", "\u{2039}", -1)))
            .child(
                div()
                    .min_w(px(0.))
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(repo_name(&d.repo)),
            )
            .children(many.then(|| arrow("start-card-next", "\u{203a}", 1)))
            .children(many.then(|| {
                let at = d.repos.iter().position(|r| *r == d.repo).unwrap_or(0) + 1;
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(rgb(colors::MUTED))
                    .child(format!("{at} of {}", d.repos.len()))
            }));
        let mono = |text: String| div().min_w(px(0.)).truncate().text_xs().child(text);
        let hint: SharedString = match self.busy {
            Busy::Starting => "Starting\u{2026}".into(),
            Busy::Drafting => "Checking the repo\u{2026}".into(),
            Busy::Idle if many => "Left/Right repo · Enter start · Esc cancel".into(),
            Busy::Idle => "Enter start · Esc cancel".into(),
        };
        let card = div()
            .w(px(640.))
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
            .child(div().font_weight(FontWeight::SEMIBOLD).child(format!(
                "Start {} on {}",
                d.key,
                slot_name(d.slot)
            )))
            .children((!d.title.is_empty()).then(|| {
                div()
                    .text_color(rgb(colors::TEXT_DIM))
                    .line_clamp(2)
                    .child(d.title.clone())
            }))
            .child(row("Repo", repo))
            .child(row("Branch", mono(d.branch.clone())))
            .child(row("Worktree", mono(d.worktree.display().to_string())))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(colors::MUTED))
                    .child("Brief"),
            )
            .child(
                div()
                    .id("start-card-brief")
                    .max_h(px(220.))
                    .overflow_y_scroll()
                    .px_2()
                    .py_1()
                    .rounded(px(4.))
                    .bg(rgb(colors::FIELD))
                    .border_1()
                    .border_color(rgb(colors::BORDER))
                    .text_xs()
                    .children(d.brief.lines().map(|l| {
                        // An empty line keeps its height.
                        div().min_h(px(12.)).child(l.to_string())
                    })),
            )
            .children(
                self.error
                    .clone()
                    .map(|e| div().text_color(rgb(colors::RED)).child(e)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .pt_1()
                    .child(
                        div()
                            .mr_auto()
                            .text_xs()
                            .text_color(rgb(colors::MUTED))
                            .child(hint),
                    )
                    .child(
                        button("start-card-cancel", "Cancel", false)
                            .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
                    )
                    .child(
                        button("start-card-start", "Start", true)
                            .on_click(cx.listener(|this, _, _, cx| this.start(cx))),
                    ),
            );
        div()
            .key_context("StartCard")
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

/// What a dragged ticket looks like under the pointer.
#[derive(Clone)]
pub struct DraggedTicket {
    pub key: String,
    pub title: String,
}

impl Render for DraggedTicket {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .max_w(px(260.))
            .flex()
            .gap_2()
            .px_2()
            .py_1()
            .rounded(px(6.))
            .bg(rgb(colors::CARD))
            .border_1()
            .border_color(rgb(colors::ACCENT))
            .text_xs()
            .child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(colors::ACCENT))
                    .child(self.key.clone()),
            )
            .child(
                div()
                    .min_w(px(0.))
                    .truncate()
                    .text_color(rgb(colors::TEXT))
                    .child(self.title.clone()),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> Keystroke {
        Keystroke::parse(s).unwrap()
    }

    #[test]
    fn keys_start_cancel_and_cycle() {
        assert_eq!(card_key(&key("escape")), CardKey::Close);
        assert_eq!(card_key(&key("enter")), CardKey::Start);
        assert_eq!(card_key(&key("left")), CardKey::Repo(-1));
        assert_eq!(card_key(&key("right")), CardKey::Repo(1));
        assert_eq!(card_key(&key("tab")), CardKey::Repo(1));
        assert_eq!(card_key(&key("shift-tab")), CardKey::Repo(-1));
        assert_eq!(card_key(&key("cmd-enter")), CardKey::Ignored);
        assert_eq!(card_key(&key("a")), CardKey::Ignored);
    }

    #[test]
    fn next_repo_wraps_and_needs_two() {
        let r = |s: &str| PathBuf::from(s);
        let repos = [r("/r/demo-api"), r("/r/demo-web"), r("/r/demo-ops")];
        assert_eq!(next_repo(&repos, &repos[0], 1), Some(r("/r/demo-web")));
        assert_eq!(next_repo(&repos, &repos[2], 1), Some(r("/r/demo-api")));
        assert_eq!(next_repo(&repos, &repos[0], -1), Some(r("/r/demo-ops")));
        // A repo no longer listed starts from the first.
        assert_eq!(next_repo(&repos, &r("/gone"), 1), Some(r("/r/demo-web")));
        assert_eq!(next_repo(&repos[..1], &repos[0], 1), None);
        assert_eq!(next_repo(&[], &repos[0], 1), None);
    }

    #[test]
    fn the_repo_row_shows_the_folder_name() {
        assert_eq!(repo_name(Path::new("/r/demo-api")), "demo-api");
    }
}
