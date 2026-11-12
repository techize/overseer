//! The session view: tab strip, the slot's terminal on the left (about 62%)
//! and the context panel on the right.
//!
//! Also the pure rules behind it: tab and panel text, when a slot's
//! `TerminalView` is kept or dropped, and the restart sequence (stop, wait
//! for Exited, start again with the same label, cwd and command).
//!
//! The Owns box lists the work the slot owns with a Release button each,
//! the slot's notices, Assign (the work-item picker) and Send kickoff,
//! which types the daemon's kickoff prompt into an idle session.

use std::path::{Path, PathBuf};

use gpui::{div, prelude::*, px, relative, rgb, Context, FontWeight, SharedString, Window};
use overseer_proto::{
    slot_name, ActivityKind, Notice, Request, SlotId, SlotInfo, SlotState, WorkView,
};

use crate::app::{colors, OverseerApp};
use crate::board::{activity_detail, activity_state, format_elapsed, owned_by, slot_title, tilde};

// ---------------------------------------------------------------------------
// Pure logic
// ---------------------------------------------------------------------------

/// Text of tab `id`: `0 S0`, `3 S3`, with a dot while it runs.
pub fn tab_label(info: &SlotInfo) -> String {
    let name = slot_name(info.id);
    match info.state {
        SlotState::Running { .. } => format!("{} {name} ●", info.id),
        _ => format!("{} {name}", info.id),
    }
}

/// What the context panel shows for a slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelModel {
    pub title: String,
    /// (name, value) rows: label, cwd, pid, state, started, then activity
    /// and its reason or tool when the slot reports one.
    pub rows: Vec<(&'static str, String)>,
    pub can_stop: bool,
    pub can_restart: bool,
    /// Stopped: the start dialog can run the slot again with new settings.
    pub can_start_new: bool,
    /// Resumable: Resume takes the place of Restart and Start.
    pub can_resume: bool,
}

/// `14:05:12 (8m 05s ago)`. `clock` turns unix seconds into local time.
pub fn format_started(since: i64, now: i64, clock: impl Fn(i64) -> String) -> String {
    format!("{} ({} ago)", clock(since), format_elapsed(now - since))
}

pub fn panel_model(
    info: &SlotInfo,
    now: i64,
    home: Option<&Path>,
    restarting: bool,
    clock: impl Fn(i64) -> String,
) -> PanelModel {
    let dash = || "-".to_string();
    let label = if info.label.trim().is_empty() {
        dash()
    } else {
        info.label.trim().to_string()
    };
    let cwd = info
        .cwd
        .as_deref()
        .map(|p| tilde(p, home))
        .unwrap_or_else(dash);
    let (pid, state, started) = match &info.state {
        SlotState::Empty => (dash(), "Empty".to_string(), dash()),
        SlotState::Running { pid, since } => (
            pid.to_string(),
            "Running".to_string(),
            format_started(*since, now, &clock),
        ),
        SlotState::Exited { code, at } => (
            dash(),
            match code {
                Some(c) => format!("Exited (code {c}) at {}", clock(*at)),
                None => format!("Exited at {}", clock(*at)),
            },
            dash(),
        ),
        SlotState::Resumable { at, .. } => (
            dash(),
            "Resumable".to_string(),
            format_started(*at, now, &clock),
        ),
    };
    let state = if restarting {
        format!("{state}, restarting")
    } else {
        state
    };
    let running = matches!(info.state, SlotState::Running { .. });
    let mut rows = vec![
        ("Label", label),
        ("Cwd", cwd),
        ("Pid", pid),
        ("State", state),
        ("Started", started),
    ];
    if let Some(a) = &info.activity {
        rows.push(("Activity", activity_state(a, now)));
        if let Some(detail) = activity_detail(a) {
            let name = match a.kind {
                ActivityKind::NeedsYou => "Reason",
                _ => "Doing",
            };
            rows.push((name, detail));
        }
    }
    PanelModel {
        title: slot_title(info),
        rows,
        can_stop: running && !restarting,
        can_restart: !matches!(info.state, SlotState::Empty | SlotState::Resumable { .. })
            && info.cwd.is_some()
            && !restarting,
        can_start_new: matches!(info.state, SlotState::Exited { .. }) && !restarting,
        can_resume: matches!(info.state, SlotState::Resumable { .. }),
    }
}

/// One line of the Owns box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnsRow {
    pub key: String,
    pub title: String,
    pub status: String,
    /// The item has left the source's list but is still owned.
    pub gone: bool,
    /// Not in the list only because its source has not answered since the
    /// daemon started (implies `gone`).
    pub not_read_yet: bool,
}

/// What the Owns box lists for `slot`, in `owned_by` order.
pub fn owns_rows(view: &WorkView, slot: SlotId) -> Vec<OwnsRow> {
    owned_by(view, slot)
        .into_iter()
        .map(|i| OwnsRow {
            key: i.key.clone(),
            title: i.title.clone(),
            status: i.status.clone(),
            gone: !i.in_list,
            not_read_yet: view.not_read_yet(i),
        })
        .collect()
}

/// The notices addressed to `slot`.
pub fn slot_notices(view: &WorkView, slot: SlotId) -> Vec<&Notice> {
    view.notices.iter().filter(|n| n.slot == slot).collect()
}

/// Send kickoff types into the session, so only while it waits for input.
pub fn can_kickoff(info: &SlotInfo) -> bool {
    matches!(info.state, SlotState::Running { .. })
        && info
            .activity
            .as_ref()
            .is_some_and(|a| a.kind == ActivityKind::Idle)
}

/// Send kickoff is on offer: the session is idle and no kickoff for it is
/// already on its way (a double click must not type it twice).
pub fn kickoff_enabled(info: &SlotInfo, pending: bool) -> bool {
    can_kickoff(info) && !pending
}

/// The kickoff text as the terminal would get it from a Cmd+V paste: one
/// bracketed paste when the session turned that mode on, newlines as CR.
/// Return is sent on its own afterwards.
pub fn kickoff_bytes(text: &str, modes: &overseer_term::TermModes) -> Vec<u8> {
    overseer_term::encode_paste(text, modes)
}

/// Identifies one run of a slot: the pid and start time while running.
pub type Incarnation = Option<(u32, i64)>;

pub fn incarnation(state: &SlotState) -> Incarnation {
    match state {
        SlotState::Running { pid, since } => Some((*pid, *since)),
        _ => None,
    }
}

/// Whether a `TerminalView` made for run `view` still shows the slot in
/// `state`. An exit keeps it, so the final screen stays readable; a new run
/// (a restart) or an empty slot drops it.
pub fn keep_view(view: Incarnation, state: &SlotState) -> bool {
    match state {
        // No process to show in either.
        SlotState::Empty | SlotState::Resumable { .. } => false,
        SlotState::Running { .. } => view == incarnation(state),
        SlotState::Exited { .. } => true,
    }
}

/// What a restart needs to start the slot again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartPlan {
    pub id: SlotId,
    pub label: String,
    pub cwd: PathBuf,
    pub command: Option<Vec<String>>,
}

impl RestartPlan {
    pub fn start_request(&self) -> Request {
        Request::StartSlot {
            id: self.id,
            label: self.label.clone(),
            cwd: self.cwd.clone(),
            command: self.command.clone(),
            prompt: None,
        }
    }
}

/// First step of a restart: the request to send now, and the plan to keep
/// while waiting for the slot to exit (`None` when it already has).
pub fn restart_begin(info: &SlotInfo) -> Option<(Request, Option<RestartPlan>)> {
    let plan = RestartPlan {
        id: info.id,
        label: info.label.clone(),
        cwd: info.cwd.clone()?,
        command: info.command.clone(),
    };
    match info.state {
        // Restarting would start fresh and lose the session; Resume is the
        // way back in.
        SlotState::Empty | SlotState::Resumable { .. } => None,
        SlotState::Running { .. } => Some((Request::StopSlot { id: info.id }, Some(plan))),
        SlotState::Exited { .. } => Some((plan.start_request(), None)),
    }
}

/// While a restart waits: the start request once the slot has exited.
pub fn restart_on_change(plan: &RestartPlan, info: &SlotInfo) -> Option<Request> {
    (info.id == plan.id && matches!(info.state, SlotState::Exited { .. }))
        .then(|| plan.start_request())
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn button(
    id: impl Into<gpui::ElementId>,
    text: &'static str,
    enabled: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded(px(5.))
        .text_sm()
        .bg(rgb(colors::BUTTON))
        .border_1()
        .border_color(rgb(colors::BUTTON_BORDER))
        .text_color(rgb(if enabled {
            colors::ACCENT
        } else {
            colors::MUTED
        }))
        .when(enabled, |b| {
            b.cursor_pointer().hover(|s| s.bg(rgb(colors::CARD_HOVER)))
        })
        .child(text)
}

fn panel_box(title: &'static str) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .p_2()
        .rounded(px(6.))
        .bg(rgb(colors::CARD))
        .border_1()
        .border_color(rgb(colors::BORDER))
        .child(
            div()
                .text_xs()
                .text_color(rgb(colors::MUTED))
                .child(title.to_uppercase()),
        )
}

fn placeholder(title: &'static str, phase: &'static str) -> gpui::Div {
    panel_box(title).child(
        div()
            .text_sm()
            .text_color(rgb(colors::TEXT_DIM))
            .child(format!("Filled in {phase}")),
    )
}

impl OverseerApp {
    fn render_tabs(&self, current: SlotId, cx: &mut Context<Self>) -> impl IntoElement {
        let board_tab = div()
            .id("tab-board")
            .px_2()
            .py_1()
            .rounded(px(5.))
            .bg(rgb(colors::CARD))
            .border_1()
            .border_color(rgb(colors::BORDER))
            .cursor_pointer()
            .child("Esc board")
            .on_click(cx.listener(|this, _, window, cx| this.go_board(window, cx)));
        let tabs: Vec<_> = self
            .slots
            .iter()
            .map(|info| {
                let id = info.id;
                let on = id == current;
                div()
                    .id(("tab", id as usize))
                    .px_2()
                    .py_1()
                    .rounded(px(5.))
                    .bg(rgb(colors::CARD))
                    .border_1()
                    .border_color(rgb(if on { colors::ORANGE } else { colors::BORDER }))
                    .text_color(rgb(if on { colors::ORANGE } else { colors::TEXT }))
                    .cursor_pointer()
                    .child(tab_label(info))
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.go_session(id, window, cx)),
                    )
            })
            .collect();
        let focus = self.session_focus.clone();
        div()
            .flex()
            .items_center()
            .gap_2()
            .text_xs()
            .on_mouse_down(gpui::MouseButton::Left, move |_, window, _| {
                window.focus(&focus)
            })
            .child(board_tab)
            .children(tabs)
            .child(
                div()
                    .ml_auto()
                    .text_color(rgb(colors::MUTED))
                    .child("Cmd+0 to Cmd+6 switch · Cmd+Esc board"),
            )
    }

    fn render_panel(&self, id: SlotId, now: i64, cx: &mut Context<Self>) -> impl IntoElement {
        let info = &self.slots[id as usize];
        let restarting = self.restarting(id);
        let m = panel_model(info, now, self.home.as_deref(), restarting, local_clock);
        let rows: Vec<_> = m
            .rows
            .into_iter()
            .map(|(k, v)| {
                div()
                    .flex()
                    .gap_2()
                    .text_sm()
                    .child(div().w(px(64.)).text_color(rgb(colors::MUTED)).child(k))
                    .child(div().flex_1().min_w(px(0.)).child(v))
            })
            .collect();
        let stop = button("stop", "Stop", m.can_stop).when(m.can_stop, |b| {
            b.on_click(cx.listener(move |this, _, _, cx| this.stop_slot(id, cx)))
        });
        let restart = button("restart", "Restart", m.can_restart).when(m.can_restart, |b| {
            b.on_click(cx.listener(move |this, _, _, cx| this.restart_slot(id, cx)))
        });
        let start_new =
            button("start-new", "Start\u{2026}", m.can_start_new).when(m.can_start_new, |b| {
                b.on_click(cx.listener(move |this, _, window, cx| this.open_dialog(id, window, cx)))
            });
        let resume = button("resume", "Resume", m.can_resume).when(m.can_resume, |b| {
            b.on_click(cx.listener(move |this, _, window, cx| this.resume_slot(id, window, cx)))
        });
        // Resume takes the place of Restart and Start while it is on offer.
        let buttons = if m.can_resume {
            div().flex().gap_2().pt_1().child(stop).child(resume)
        } else {
            div()
                .flex()
                .gap_2()
                .pt_1()
                .child(stop)
                .child(restart)
                .child(start_new)
        };
        let focus = self.session_focus.clone();
        div()
            .flex_1()
            .min_w(px(0.))
            .flex()
            .flex_col()
            .gap_2()
            .on_mouse_down(gpui::MouseButton::Left, move |_, window, _| {
                window.focus(&focus)
            })
            .child(
                panel_box("Session")
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(m.title))
                    .children(rows)
                    .child(buttons),
            )
            .child(self.render_owns(id, cx))
            .child(placeholder("Holds", "phase 4"))
            .child(placeholder("Progress", "phase 4"))
            .children(
                self.status
                    .clone()
                    .map(|s| div().text_sm().text_color(rgb(colors::ORANGE)).child(s)),
            )
    }

    fn render_owns(&self, id: SlotId, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<_> = owns_rows(&self.work, id)
            .into_iter()
            .enumerate()
            .map(|(n, row)| {
                let key = row.key.clone();
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_sm()
                    .child(
                        div()
                            .flex_shrink()
                            .min_w(px(0.))
                            .truncate()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(row.key),
                    )
                    .child(
                        div()
                            .flex_shrink()
                            .min_w(px(0.))
                            .truncate()
                            .text_color(rgb(colors::MUTED))
                            .child(row.status),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .child(div().truncate().child(row.title))
                            .children(row.gone.then(|| {
                                if row.not_read_yet {
                                    div()
                                        .text_xs()
                                        .text_color(rgb(colors::MUTED))
                                        .child("not read yet")
                                } else {
                                    div()
                                        .text_xs()
                                        .text_color(rgb(colors::ORANGE))
                                        .child("not in the current list")
                                }
                            })),
                    )
                    .child(
                        button(("release", n), "Release", true)
                            .flex_none()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.release_item(key.clone(), cx)
                            })),
                    )
            })
            .collect();
        let notices: Vec<_> = slot_notices(&self.work, id)
            .into_iter()
            .map(|n| {
                div()
                    .text_sm()
                    .text_color(rgb(colors::ORANGE))
                    .child(format!("{} {}", crate::board::local_hhmm(n.at), n.text))
            })
            .collect();
        let has_notices = !notices.is_empty();
        let empty = rows.is_empty();
        let kickoff = kickoff_enabled(&self.slots[id as usize], self.kickoff_pending(id));
        panel_box("Owns")
            .children(empty.then(|| {
                div()
                    .text_sm()
                    .text_color(rgb(colors::TEXT_DIM))
                    .child("Nothing assigned")
            }))
            .children(rows)
            .children(notices)
            .child(
                div()
                    .flex()
                    .gap_2()
                    .pt_1()
                    .child(button("assign", "Assign\u{2026}", true).on_click(
                        cx.listener(move |this, _, window, cx| this.open_picker(id, window, cx)),
                    ))
                    .child(
                        button("kickoff", "Send kickoff", kickoff).when(kickoff, |b| {
                            b.on_click(cx.listener(move |this, _, _, cx| this.send_kickoff(id, cx)))
                        }),
                    )
                    .children(has_notices.then(|| {
                        button("clear-notices", "Clear", true)
                            .on_click(cx.listener(move |this, _, _, cx| this.clear_notices(id, cx)))
                    })),
            )
    }

    pub(crate) fn render_session(
        &mut self,
        id: SlotId,
        now: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let view = self.ensure_terminal(id, window, cx);
        let left = match view {
            Some(view) => div()
                .w(relative(0.62))
                .h_full()
                .rounded(px(6.))
                .border_1()
                .border_color(rgb(colors::BORDER))
                .overflow_hidden()
                .child(view),
            None => div()
                .w(relative(0.62))
                .h_full()
                .rounded(px(6.))
                .border_1()
                .border_color(rgb(colors::BORDER))
                .bg(rgb(colors::TERM_EMPTY))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    div()
                        .text_color(rgb(colors::TEXT_DIM))
                        .child(SharedString::from(format!("{} is empty", slot_name(id)))),
                )
                .child(button("start-empty", "+ start", true).on_click(
                    cx.listener(move |this, _, window, cx| this.open_dialog(id, window, cx)),
                )),
        };
        let resumable = matches!(self.slots[id as usize].state, SlotState::Resumable { .. });
        let left = if resumable {
            // `left` above is the empty-slot pane; this slot has a session
            // to continue, not nothing to start.
            div()
                .w(relative(0.62))
                .h_full()
                .rounded(px(6.))
                .border_1()
                .border_color(rgb(colors::BORDER))
                .bg(rgb(colors::TERM_EMPTY))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    div()
                        .text_color(rgb(colors::TEXT_DIM))
                        .child(SharedString::from(format!(
                            "{} was running when the daemon stopped",
                            slot_name(id)
                        ))),
                )
                .child(button("resume-pane", "Resume", true).on_click(
                    cx.listener(move |this, _, window, cx| this.resume_slot(id, window, cx)),
                ))
        } else {
            left
        };
        let tabs = self.render_tabs(id, cx);
        let panel = self.render_panel(id, now, cx);
        div()
            .key_context("Session")
            .track_focus(&self.session_focus)
            .size_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .child(tabs)
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .gap_2()
                    .child(left)
                    .child(panel),
            )
    }
}

/// Local `HH:MM:SS` for unix seconds.
pub fn local_clock(t: i64) -> String {
    // SAFETY: localtime_r only writes the `tm` we pass it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let time: libc::time_t = t as libc::time_t;
    let ok = unsafe { !libc::localtime_r(&time, &mut tm).is_null() };
    if !ok {
        return "?".into();
    }
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(state: SlotState) -> SlotInfo {
        SlotInfo {
            id: 2,
            label: "magento".into(),
            cwd: Some("/Users/me/Repos/m".into()),
            command: Some(vec!["claude".into(), "--resume".into()]),
            state,
            cols: 80,
            rows: 24,
            activity: None,
        }
    }

    fn clock(t: i64) -> String {
        format!("T{t}")
    }

    const RUN: SlotState = SlotState::Running {
        pid: 77,
        since: 100,
    };

    #[test]
    fn tabs() {
        assert_eq!(tab_label(&info(RUN)), "2 S2 ●");
        assert_eq!(tab_label(&info(SlotState::Empty)), "2 S2");
    }

    #[test]
    fn panel_running() {
        let m = panel_model(&info(RUN), 165, Some(Path::new("/Users/me")), false, clock);
        assert_eq!(m.title, "S2 · magento");
        assert_eq!(
            m.rows,
            vec![
                ("Label", "magento".to_string()),
                ("Cwd", "~/Repos/m".to_string()),
                ("Pid", "77".to_string()),
                ("State", "Running".to_string()),
                ("Started", "T100 (1m 05s ago)".to_string()),
            ]
        );
        assert!(m.can_stop && m.can_restart && !m.can_start_new);
    }

    #[test]
    fn panel_resumable_offers_resume_not_restart() {
        let st = SlotState::Resumable {
            session_id: "abc".into(),
            at: 100,
        };
        let m = panel_model(&info(st.clone()), 165, None, false, clock);
        assert!(m.rows.contains(&("State", "Resumable".to_string())));
        assert!(m
            .rows
            .contains(&("Started", "T100 (1m 05s ago)".to_string())));
        assert!(m.can_resume && !m.can_stop && !m.can_restart && !m.can_start_new);
        assert!(restart_begin(&info(st.clone())).is_none());
        assert!(!keep_view(None, &st));
        assert!(!panel_model(&info(RUN), 165, None, false, clock).can_resume);
    }

    #[test]
    fn panel_shows_activity_and_reason() {
        use overseer_proto::{Activity, ActivityKind};
        let mut i = info(RUN);
        i.activity = Some(Activity {
            kind: ActivityKind::NeedsYou,
            since: 105,
            now_doing: Some("Bash".into()),
            reason: Some("Permission to run Bash".into()),
        });
        let m = panel_model(&i, 165, None, false, clock);
        assert_eq!(m.rows[3], ("State", "Running".to_string()));
        assert_eq!(m.rows[5], ("Activity", "Needs you 1m 00s".to_string()));
        assert_eq!(m.rows[6], ("Reason", "Permission to run Bash".to_string()));
        assert_eq!(m.rows.len(), 7);

        i.activity = Some(Activity {
            kind: ActivityKind::Working,
            since: 160,
            now_doing: Some("Edit".into()),
            reason: None,
        });
        let m = panel_model(&i, 165, None, false, clock);
        assert_eq!(m.rows[5], ("Activity", "Working 5s".to_string()));
        assert_eq!(m.rows[6], ("Doing", "Edit".to_string()));

        i.activity = Some(Activity {
            kind: ActivityKind::Idle,
            since: 160,
            now_doing: None,
            reason: None,
        });
        let m = panel_model(&i, 165, None, false, clock);
        assert_eq!(m.rows[5], ("Activity", "Idle 5s".to_string()));
        assert_eq!(m.rows.len(), 6);
    }

    #[test]
    fn panel_exited_and_empty() {
        let ex = SlotState::Exited {
            code: Some(1),
            at: 50,
        };
        let m = panel_model(&info(ex), 60, None, false, clock);
        assert_eq!(m.rows[3].1, "Exited (code 1) at T50");
        assert_eq!(m.rows[2].1, "-");
        assert!(!m.can_stop && m.can_restart && m.can_start_new);

        let mut empty = info(SlotState::Empty);
        empty.cwd = None;
        empty.label.clear();
        let m = panel_model(&empty, 60, None, false, clock);
        assert_eq!(m.title, "S2");
        assert_eq!(m.rows[0].1, "-");
        assert!(!m.can_stop && !m.can_restart && !m.can_start_new);
    }

    #[test]
    fn panel_while_restarting_disables_buttons() {
        let m = panel_model(&info(RUN), 165, None, true, clock);
        assert_eq!(m.rows[3].1, "Running, restarting");
        assert!(!m.can_stop && !m.can_restart);
    }

    #[test]
    fn views_survive_exit_but_not_a_new_run() {
        let inc = incarnation(&RUN);
        assert_eq!(inc, Some((77, 100)));
        assert!(keep_view(inc, &RUN));
        let ex = SlotState::Exited { code: None, at: 9 };
        assert!(keep_view(inc, &ex));
        let again = SlotState::Running {
            pid: 78,
            since: 200,
        };
        assert!(!keep_view(inc, &again));
        assert!(!keep_view(inc, &SlotState::Empty));
        // A view made while exited is dropped when the slot runs again.
        assert!(keep_view(None, &ex));
        assert!(!keep_view(None, &again));
    }

    #[test]
    fn restart_running_stops_then_starts_same_spec() {
        let i = info(RUN);
        let (first, plan) = restart_begin(&i).unwrap();
        assert_eq!(first, Request::StopSlot { id: 2 });
        let plan = plan.unwrap();
        // Still running (or another slot changed): keep waiting.
        assert_eq!(restart_on_change(&plan, &i), None);
        let mut other = info(SlotState::Exited { code: None, at: 1 });
        other.id = 3;
        assert_eq!(restart_on_change(&plan, &other), None);
        let exited = info(SlotState::Exited { code: None, at: 1 });
        assert_eq!(
            restart_on_change(&plan, &exited),
            Some(Request::StartSlot {
                id: 2,
                label: "magento".into(),
                cwd: "/Users/me/Repos/m".into(),
                command: Some(vec!["claude".into(), "--resume".into()]),
                prompt: None,
            })
        );
    }

    #[test]
    fn restart_exited_starts_at_once_and_empty_cannot() {
        let (first, plan) = restart_begin(&info(SlotState::Exited {
            code: Some(0),
            at: 1,
        }))
        .unwrap();
        assert!(matches!(first, Request::StartSlot { id: 2, .. }));
        assert!(plan.is_none());
        assert!(restart_begin(&info(SlotState::Empty)).is_none());
    }

    #[test]
    fn owns_rows_mark_items_off_the_list() {
        use overseer_proto::{ItemKind, WorkItem};
        let item = |key: &str, kind, owner, in_list| WorkItem {
            key: key.into(),
            kind,
            title: format!("title of {key}"),
            status: "In Progress".into(),
            priority: None,
            parent: None,
            owner,
            in_list,
            folder: None,
        };
        let view = WorkView {
            items: vec![
                item("ov-3", ItemKind::Bead, Some(2), true),
                item("DEMO-12", ItemKind::Ticket, Some(2), false),
                item("DEMO-7", ItemKind::Ticket, Some(4), true),
            ],
            sources: vec![],
            notices: vec![
                Notice {
                    slot: 2,
                    at: 5,
                    text: "DEMO-9 moved to S4".into(),
                },
                Notice {
                    slot: 4,
                    at: 6,
                    text: "other".into(),
                },
            ],
        };
        let rows = owns_rows(&view, 2);
        assert_eq!(
            rows.iter()
                .map(|r| (r.key.as_str(), r.gone))
                .collect::<Vec<_>>(),
            [("DEMO-12", true), ("ov-3", false)]
        );
        assert_eq!(rows[0].status, "In Progress");
        assert!(!rows[0].not_read_yet, "no source is waiting to be read");
        let notes = slot_notices(&view, 2);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].text, "DEMO-9 moved to S4");
    }

    #[test]
    fn owns_rows_say_not_read_yet_before_the_first_read() {
        use overseer_proto::{ItemKind, SourceStatus, WorkItem};
        let view = WorkView {
            items: vec![WorkItem {
                key: "ov-3".into(),
                kind: ItemKind::Bead,
                title: "ov-3".into(),
                status: "unknown".into(),
                priority: None,
                parent: None,
                owner: Some(2),
                in_list: false,
                folder: None,
            }],
            sources: vec![SourceStatus {
                name: "beads".into(),
                ok: false,
                count: 0,
                at: None,
                message: Some("not read yet".into()),
            }],
            notices: vec![],
        };
        let rows = owns_rows(&view, 2);
        assert!(rows[0].gone && rows[0].not_read_yet, "{rows:?}");
    }

    #[test]
    fn kickoff_only_when_idle() {
        use overseer_proto::Activity;
        let mut i = info(RUN);
        assert!(!can_kickoff(&i), "no activity reported");
        for (kind, ok) in [
            (ActivityKind::Idle, true),
            (ActivityKind::Working, false),
            (ActivityKind::NeedsYou, false),
        ] {
            i.activity = Some(Activity {
                kind,
                since: 1,
                now_doing: None,
                reason: None,
            });
            assert_eq!(can_kickoff(&i), ok);
        }
        i.state = SlotState::Exited { code: None, at: 2 };
        assert!(!can_kickoff(&i));
    }

    #[test]
    fn kickoff_disabled_while_one_is_in_flight() {
        use overseer_proto::Activity;
        let mut i = info(RUN);
        i.activity = Some(Activity {
            kind: ActivityKind::Idle,
            since: 1,
            now_doing: None,
            reason: None,
        });
        assert!(kickoff_enabled(&i, false));
        assert!(!kickoff_enabled(&i, true));
        i.activity = None;
        assert!(!kickoff_enabled(&i, false));
    }

    #[test]
    fn kickoff_text_is_pasted() {
        use overseer_term::TermModes;
        let text = "Work on DEMO-12\nand ov-3";
        let plain = kickoff_bytes(text, &TermModes::default());
        assert_eq!(plain, b"Work on DEMO-12\rand ov-3");
        let bracketed = TermModes {
            bracketed_paste: true,
            ..TermModes::default()
        };
        assert_eq!(
            kickoff_bytes(text, &bracketed),
            b"\x1b[200~Work on DEMO-12\rand ov-3\x1b[201~"
        );
    }

    #[test]
    fn started_format() {
        assert_eq!(format_started(10, 20, clock), "T10 (10s ago)");
        assert_eq!(local_clock(0).len(), 8);
    }
}
