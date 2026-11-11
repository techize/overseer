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

use std::path::Path;

use gpui::{div, prelude::*, px, rgb, Context, FontWeight, Window};
use overseer_proto::{slot_name, Activity, ActivityKind, SlotId, SlotInfo, SlotState};

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

/// The card for `info` at unix time `now`.
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
            now_doing: None,
            can_resume: false,
        },
        SlotState::Running { pid, since } => {
            let (badge, tone, now_doing) = match &info.activity {
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
            CardModel {
                title,
                badge,
                tone,
                cwd,
                pid: Some(format!("pid {pid}")),
                now_doing,
                can_resume: false,
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
        },
        SlotState::Resumable { .. } => CardModel {
            title,
            badge: "Resumable".into(),
            tone: Tone::Resumable,
            cwd,
            pid: None,
            now_doing: None,
            can_resume: true,
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
        Tone::Idle | Tone::Exited | Tone::Resumable | Tone::Empty => colors::GREY,
    }
}

/// The card's left edge: the badge colour while the slot runs.
fn edge_color(tone: Tone) -> u32 {
    match tone {
        Tone::Running | Tone::Working | Tone::NeedsYou | Tone::Idle => tone_color(tone),
        Tone::Exited | Tone::Resumable | Tone::Empty => colors::EDGE_IDLE,
    }
}

impl OverseerApp {
    fn render_card(&self, id: SlotId, now: i64, cx: &mut Context<Self>) -> impl IntoElement {
        let info = &self.slots[id as usize];
        let m = card_model(info, now, self.home.as_deref());
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
                            .text_color(rgb(if need_you > 0 {
                                colors::ORANGE
                            } else {
                                colors::MUTED
                            }))
                            .child(header_text(running, need_you)),
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
    use overseer_proto::{Activity, ActivityKind};
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
                now_doing: None,
                can_resume: false,
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
        });
        i
    }

    #[test]
    fn card_model_for_each_activity() {
        let now = 2000 + 185;
        let w = card_model(
            &with_activity(ActivityKind::Working, Some("Bash: cargo test"), None),
            now,
            None,
        );
        assert_eq!(w.badge, "● Working 3m 05s");
        assert_eq!(w.tone, Tone::Working);
        assert_eq!(tone_color(w.tone), colors::GREEN);
        assert_eq!(w.now_doing.as_deref(), Some("Bash: cargo test"));
        assert_eq!(w.pid.as_deref(), Some("pid 4321"));

        let n = card_model(
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

        let i = card_model(
            &with_activity(ActivityKind::Idle, Some("Edit"), None),
            2000 + 720,
            None,
        );
        assert_eq!(i.badge, "● Idle 12m 00s");
        assert_eq!(i.tone, Tone::Idle);
        assert_eq!(tone_color(i.tone), colors::GREY);
        assert_eq!(i.now_doing, None);

        // Working before the first tool, or needs you without a reason.
        let bare = card_model(&with_activity(ActivityKind::Working, None, None), now, None);
        assert_eq!(bare.now_doing, None);
        let bare = card_model(
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
        let m = card_model(&info(1, "htop", st), 1000 + 485, None);
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
        assert_eq!(
            card_model(&i, 1000, None).now_doing.as_deref(),
            Some("htop -d 10")
        );
        i.command = Some(vec!["x".repeat(100)]);
        let long = card_model(&i, 1000, None).now_doing.unwrap();
        assert_eq!(long.chars().count(), 81);
        assert!(long.ends_with('…'));
        i.command = Some(vec![]);
        assert_eq!(card_model(&i, 1000, None).now_doing, None);
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
    fn resumable_card_model() {
        let st = SlotState::Resumable {
            session_id: "abc".into(),
            at: 5,
        };
        let m = card_model(&info(3, "eks", st), 9, Some(Path::new(HOME)));
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
        assert!(!card_model(&info(3, "eks", ex), 9, None).can_resume);
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
