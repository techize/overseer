//! The board: header, coordinator panel with the S0 card, and a 3 by 2 grid
//! of S1 to S6 cards (layout A from the design).
//!
//! The card contents are worked out by `card_model`, a pure function, so the
//! badge text, elapsed time and empty, running and exited cards are tested
//! without a window.

use std::path::Path;

use gpui::{div, prelude::*, px, rgb, Context, FontWeight, Window};
use overseer_proto::{slot_name, SlotId, SlotInfo, SlotState};

use crate::app::{colors, OverseerApp};

// ---------------------------------------------------------------------------
// Pure logic
// ---------------------------------------------------------------------------

/// Colour family of a card's state badge and left edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Running,
    Exited,
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

/// The card for `info` at unix time `now`.
pub fn card_model(info: &SlotInfo, now: i64, home: Option<&Path>) -> CardModel {
    let cwd = info.cwd.as_deref().map(|p| tilde(p, home));
    let title = slot_title(info);
    match &info.state {
        SlotState::Empty => CardModel {
            title,
            badge: "+ start".into(),
            tone: Tone::Empty,
            cwd: None,
            pid: None,
        },
        SlotState::Running { pid, since } => CardModel {
            title,
            badge: format!("● Running {}", format_elapsed(now - since)),
            tone: Tone::Running,
            cwd,
            pid: Some(format!("pid {pid}")),
        },
        SlotState::Exited { code, .. } => CardModel {
            title,
            badge: match code {
                Some(c) => format!("Exited (code {c})"),
                None => "Exited".into(),
            },
            tone: Tone::Exited,
            cwd,
            pid: None,
        },
    }
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

fn tone_color(tone: Tone) -> u32 {
    match tone {
        Tone::Running => colors::GREEN,
        Tone::Exited => colors::GREY,
        Tone::Empty => colors::GREY,
    }
}

impl OverseerApp {
    fn render_card(&self, id: SlotId, now: i64, cx: &mut Context<Self>) -> impl IntoElement {
        let info = &self.slots[id as usize];
        let m = card_model(info, now, self.home.as_deref());
        let selected = self.selected == id;
        let edge = match m.tone {
            Tone::Running => colors::GREEN,
            _ => colors::EDGE_IDLE,
        };
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
                            .gap_2()
                            .child(div().w(px(4.)).h(px(16.)).rounded(px(2.)).bg(rgb(edge)))
                            .child(div().font_weight(FontWeight::SEMIBOLD).child(m.title)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(tone_color(m.tone)))
                            .child(m.badge),
                    ),
            )
            .children(
                m.cwd
                    .map(|c| div().text_xs().text_color(rgb(colors::TEXT_DIM)).child(c)),
            )
            .children(
                m.pid
                    .map(|p| div().text_xs().text_color(rgb(colors::MUTED)).child(p)),
            )
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
                            .text_sm()
                            .text_color(rgb(colors::MUTED))
                            .child(header_count(running)),
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
            .child(
                div()
                    .mt_auto()
                    .text_xs()
                    .text_color(rgb(colors::MUTED))
                    .child("Click a card or press Enter to open it · Cmd+0 to Cmd+6 open a session · Cmd+N starts the selected slot"),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        }
    }

    const HOME: &str = "/Users/me";

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
        let m = card_model(&info(5, "", SlotState::Empty), 100, None);
        assert_eq!(
            m,
            CardModel {
                title: "S5".into(),
                badge: "+ start".into(),
                tone: Tone::Empty,
                cwd: None,
                pid: None,
            }
        );
    }

    #[test]
    fn running_card() {
        let st = SlotState::Running {
            pid: 4321,
            since: 1000,
        };
        let m = card_model(
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

    #[test]
    fn exited_card() {
        let st = SlotState::Exited {
            code: Some(3),
            at: 5,
        };
        let m = card_model(&info(3, "", st), 9, None);
        assert_eq!(m.title, "S3");
        assert_eq!(m.badge, "Exited (code 3)");
        assert_eq!(m.tone, Tone::Exited);
        assert_eq!(m.cwd.as_deref(), Some("/Users/me/Repos/eks"));
        assert_eq!(m.pid, None);
        let killed = SlotState::Exited { code: None, at: 5 };
        assert_eq!(card_model(&info(3, "", killed), 9, None).badge, "Exited");
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
