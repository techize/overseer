//! The Locks view: every held write target with its holder, kind, age,
//! reason and queue, plus the targets nobody holds but somebody waits for.
//! `l` on the board and `Cmd+Shift+L` elsewhere open it.
//!
//! A force release names the holder the confirm showed, so a target that
//! changed hands while you read the prompt is never released from the new
//! holder.
//!
//! `lock_rows` and `LocksState` are pure, so the rows, the selection and
//! the confirm are tested without a window.

use gpui::{div, prelude::*, px, rgb, Context, FontWeight, KeyDownEvent, Window};
use overseer_proto::{slot_name, ClaimKind, LockView, Request, SlotId, SlotInfo};

use crate::app::{colors, OverseerApp};

/// One target in the view.
#[derive(Debug, Clone, PartialEq)]
pub struct LockRow {
    pub target: String,
    /// None for a target with only waiters.
    pub holder: Option<SlotId>,
    /// `S1 demo lane`, `S1` for a slot with no label, `nobody`.
    pub holder_text: String,
    /// `claim`, `command`, `grant` or `pin`; empty when nobody holds it.
    pub kind: &'static str,
    /// `since 08:40 (2h 5m)`; empty when nobody holds it.
    pub since: String,
    /// The claim's start time, which tells a renewed claim from the same one.
    pub claimed_at: Option<i64>,
    /// `for DEMO-33`: the claim's work item, else its reason.
    pub why: Option<String>,
    /// `at <revision>` for a pin.
    pub pin: Option<String>,
    pub long_held: bool,
    /// `1. S4 since 09:10, for sync`, first in line first.
    pub waiters: Vec<String>,
}

/// `12m`, `2h 5m`, `3d 4h`. Negative input (clock skew) shows `0m`.
pub fn age_text(secs: i64) -> String {
    let s = secs.max(0);
    let (d, h, m) = (s / 86400, s % 86400 / 3600, s % 3600 / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

fn kind_name(kind: ClaimKind) -> &'static str {
    match kind {
        ClaimKind::Command => "command",
        ClaimKind::Grant => "grant",
        ClaimKind::Claim => "claim",
        ClaimKind::Pin => "pin",
    }
}

fn holder_text(slot: SlotId, slots: &[SlotInfo]) -> String {
    match slots.iter().find(|s| s.id == slot) {
        Some(s) if !s.label.is_empty() => format!("{} {}", slot_name(slot), s.label),
        _ => slot_name(slot),
    }
}

/// The queue for `target`, one line per waiter: `1. S4 since 09:10, for sync`.
fn waiter_lines(view: &LockView, target: &str, clock: &impl Fn(i64) -> String) -> Vec<String> {
    view.waits
        .iter()
        .filter(|w| w.target == target)
        .enumerate()
        .map(|(i, w)| {
            let line = format!("{}. {} since {}", i + 1, slot_name(w.slot), clock(w.at));
            match &w.reason {
                Some(why) => format!("{line}, for {why}"),
                None => line,
            }
        })
        .collect()
}

/// The slot first in line, read back from the row's first waiter line.
fn next_in_line(row: &LockRow) -> Option<&str> {
    row.waiters.first()?.split(' ').nth(1)
}

/// Every claim, oldest first, then the targets with only waiters in the
/// order of their first wait. `clock` renders a time.
pub fn lock_rows(
    view: &LockView,
    slots: &[SlotInfo],
    now: i64,
    clock: impl Fn(i64) -> String,
) -> Vec<LockRow> {
    let mut claims: Vec<_> = view.claims.iter().collect();
    claims.sort_by_key(|c| c.since);
    let mut rows: Vec<LockRow> = claims
        .into_iter()
        .map(|c| LockRow {
            target: c.target.clone(),
            holder: Some(c.slot),
            holder_text: holder_text(c.slot, slots),
            kind: kind_name(c.kind),
            since: format!("since {} ({})", clock(c.since), age_text(now - c.since)),
            claimed_at: Some(c.since),
            why: c
                .item
                .as_deref()
                .or(c.reason.as_deref())
                .map(|w| format!("for {w}")),
            pin: c.pin_revision.as_ref().map(|r| format!("at {r}")),
            long_held: view.flagged(c, now),
            waiters: waiter_lines(view, &c.target, &clock),
        })
        .collect();
    let mut unheld: Vec<(&str, i64)> = Vec::new();
    for w in &view.waits {
        let held = view.claims.iter().any(|c| c.target == w.target);
        if !held && !unheld.iter().any(|(t, _)| *t == w.target) {
            unheld.push((&w.target, w.at));
        }
    }
    unheld.sort_by_key(|(_, first)| *first);
    rows.extend(unheld.into_iter().map(|(target, _)| LockRow {
        target: target.to_string(),
        holder: None,
        holder_text: "nobody".into(),
        kind: "",
        since: String::new(),
        claimed_at: None,
        why: None,
        pin: None,
        long_held: false,
        waiters: waiter_lines(view, target, &clock),
    }));
    rows
}

/// `3 held, 1 waiting`, `1 held`, or a sentence when the view is empty.
pub fn locks_summary(view: &LockView) -> String {
    let (held, waiting) = (view.claims.len(), view.waits.len());
    match (held, waiting) {
        (0, 0) => "Nothing is held and nobody is waiting.".into(),
        (h, 0) => format!("{h} held"),
        (h, w) => format!("{h} held, {w} waiting"),
    }
}

/// The board's link to the view: `Locks: 3 held, 1 waiting (l)`.
pub fn locks_strip(view: &LockView) -> String {
    if view.claims.is_empty() && view.waits.is_empty() {
        "Locks: none held (l)".into()
    } else {
        format!("Locks: {} (l)", locks_summary(view))
    }
}

#[derive(Debug, PartialEq)]
pub enum LocksAction {
    Redraw,
    Send(Request),
    Back,
    Status(String),
    Ignored,
}

/// An open force-release confirm: the claim it showed, by holder and start.
struct Confirm {
    target: String,
    holder: SlotId,
    claimed_at: Option<i64>,
    prompt: String,
}

impl Confirm {
    /// The confirm for `row`; None when nobody holds it.
    fn for_row(row: &LockRow) -> Option<Confirm> {
        let holder = row.holder?;
        let queue = match next_in_line(row) {
            Some(next) => format!("{next} is next in the queue."),
            None => "Nobody is waiting.".into(),
        };
        let prompt = format!(
            "Release {}'s {}? {queue} Enter / Esc",
            slot_name(holder),
            row.target
        );
        Some(Confirm {
            target: row.target.clone(),
            holder,
            claimed_at: row.claimed_at,
            prompt,
        })
    }
}

/// The view's selection and confirm. The selection is kept by target name,
/// so it stays on its target when the rows reorder.
#[derive(Default)]
pub struct LocksState {
    target: Option<String>,
    /// Where the selection was, for when its target leaves the list.
    index: usize,
    confirm: Option<Confirm>,
}

impl LocksState {
    /// Applies a key, with `rows` the list currently shown.
    pub fn key(&mut self, key: &str, rows: &[LockRow]) -> LocksAction {
        if let Some(c) = self.confirm.take() {
            return match key {
                "enter" => LocksAction::Send(Request::ForceRelease {
                    target: c.target,
                    holder: Some(c.holder),
                }),
                "escape" => LocksAction::Redraw,
                _ => {
                    self.confirm = Some(c);
                    LocksAction::Ignored
                }
            };
        }
        let current = self.selected(rows);
        match (key, current) {
            ("up", Some(i)) => {
                self.select(i.saturating_sub(1), rows);
                LocksAction::Redraw
            }
            ("down", Some(i)) => {
                self.select((i + 1).min(rows.len() - 1), rows);
                LocksAction::Redraw
            }
            ("enter" | "f", Some(i)) => match Confirm::for_row(&rows[i]) {
                Some(c) => {
                    self.select(i, rows);
                    self.confirm = Some(c);
                    LocksAction::Redraw
                }
                None => LocksAction::Status(format!(
                    "Nobody holds {}; nothing to release",
                    rows[i].target
                )),
            },
            ("escape", _) => LocksAction::Back,
            _ => LocksAction::Ignored,
        }
    }

    /// The open confirm's question.
    pub fn prompt(&self) -> Option<String> {
        self.confirm.as_ref().map(|c| c.prompt.clone())
    }

    /// After the daemon's view changed: keeps the selection on its target,
    /// and closes an open confirm whose target now has another holder, a
    /// renewed claim by the same one, or none, saying so.
    pub fn on_change(&mut self, rows: &[LockRow]) -> Option<String> {
        if let Some(i) = self.selected(rows) {
            self.select(i, rows);
        }
        let open = self.confirm.take()?;
        let fresh = rows
            .iter()
            .find(|r| r.target == open.target)
            .and_then(Confirm::for_row);
        match fresh {
            Some(c) if c.holder == open.holder && c.claimed_at == open.claimed_at => {
                self.confirm = Some(c);
                None
            }
            _ => Some(format!("{} changed hands; look again", open.target)),
        }
    }

    /// The selected row: the selected target, or the nearest row when it
    /// has left the list. None for an empty list.
    pub fn selected(&self, rows: &[LockRow]) -> Option<usize> {
        let last = rows.len().checked_sub(1)?;
        let by_name = self
            .target
            .as_ref()
            .and_then(|t| rows.iter().position(|r| &r.target == t));
        Some(by_name.unwrap_or(self.index.min(last)))
    }

    fn select(&mut self, i: usize, rows: &[LockRow]) {
        self.index = i;
        self.target = Some(rows[i].target.clone());
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// The keyboard help under the list.
const FOOTER: &str = "Up and Down pick a target · Enter or f releases it · Esc goes back";

impl OverseerApp {
    pub(crate) fn lock_rows_now(&self, now: i64) -> Vec<LockRow> {
        lock_rows(&self.locks, &self.slots, now, overseer_proto::local_hhmm)
    }

    pub(crate) fn on_locks_key(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let m = &ev.keystroke.modifiers;
        if m.platform || m.control || m.alt || m.function {
            return;
        }
        let rows = self.lock_rows_now(crate::app::now_secs());
        match self.locks_state.key(&ev.keystroke.key, &rows) {
            LocksAction::Ignored => return,
            LocksAction::Redraw => self.status = None,
            LocksAction::Send(req) => self.force_release(req, cx),
            LocksAction::Back => self.go_board(window, cx),
            LocksAction::Status(text) => self.status = Some(text),
        }
        cx.stop_propagation();
        cx.notify();
    }

    pub(crate) fn render_locks(
        &mut self,
        now: i64,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let rows = self.lock_rows_now(now);
        let selected = self.locks_state.selected(&rows);
        let prompt = self.locks_state.prompt();
        let list = rows.into_iter().enumerate().map(|(i, row)| {
            let line = [row.kind, &row.since]
                .into_iter()
                .chain(row.why.as_deref())
                .chain(row.pin.as_deref())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("  ");
            div()
                .flex()
                .flex_col()
                .gap_1()
                .p_2()
                .rounded(px(6.))
                .bg(rgb(colors::CARD))
                .border_1()
                .border_color(rgb(if selected == Some(i) {
                    colors::ACCENT
                } else {
                    colors::BORDER
                }))
                .child(
                    div()
                        .flex()
                        .gap_3()
                        .items_baseline()
                        .child(div().font_weight(FontWeight::SEMIBOLD).child(row.target))
                        .child(
                            div()
                                .text_color(rgb(colors::TEXT_DIM))
                                .child(row.holder_text),
                        )
                        .child(div().text_sm().text_color(rgb(colors::MUTED)).child(line))
                        .children(row.long_held.then(|| {
                            div()
                                .text_sm()
                                .text_color(rgb(colors::ORANGE))
                                .child("held long")
                        })),
                )
                .children(row.waiters.into_iter().map(|w| {
                    div()
                        .pl_4()
                        .text_sm()
                        .text_color(rgb(colors::TEXT_DIM))
                        .child(w)
                }))
        });
        div()
            .key_context("Locks")
            .track_focus(&self.locks_focus)
            .on_key_down(cx.listener(Self::on_locks_key))
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_3()
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .gap_3()
                    .child(div().text_lg().font_weight(FontWeight::BOLD).child("Locks"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(colors::MUTED))
                            .child(locks_summary(&self.locks)),
                    ),
            )
            .child(
                div()
                    .id("locks-list")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(list),
            )
            .children(
                self.status
                    .clone()
                    .map(|s| div().text_sm().text_color(rgb(colors::ORANGE)).child(s)),
            )
            .children(prompt.map(|p| {
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(colors::ORANGE))
                    .child(p)
            }))
            .child(div().text_xs().text_color(rgb(colors::MUTED)).child(FOOTER))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use overseer_proto::{Claim, ClaimKind, SlotState, Wait};

    /// `HH:MM` of the seconds since midnight, as a fixed local clock.
    fn clock(t: i64) -> String {
        format!("{:02}:{:02}", t / 3600 % 24, t / 60 % 60)
    }

    fn at(h: i64, m: i64) -> i64 {
        h * 3600 + m * 60
    }

    fn slot(id: SlotId, label: &str) -> SlotInfo {
        SlotInfo {
            id,
            label: label.into(),
            cwd: None,
            command: None,
            state: SlotState::Empty,
            cols: 80,
            rows: 24,
            activity: None,
            session_note: None,
        }
    }

    fn slots() -> Vec<SlotInfo> {
        vec![
            slot(0, ""),
            slot(1, "demo lane"),
            slot(2, ""),
            slot(3, "uat fixes"),
            slot(4, ""),
        ]
    }

    fn claim(slot: SlotId, target: &str, kind: ClaimKind, since: i64) -> Claim {
        Claim {
            target: target.into(),
            slot,
            kind,
            reason: None,
            item: None,
            since,
            last_used: NOW,
            pin_revision: None,
        }
    }

    fn wait(slot: SlotId, target: &str, at: i64, reason: Option<&str>) -> Wait {
        Wait {
            target: target.into(),
            slot,
            at,
            reason: reason.map(Into::into),
        }
    }

    fn view(claims: Vec<Claim>, waits: Vec<Wait>) -> LockView {
        let mut v = crate::app::empty_locks();
        v.claims = claims;
        v.waits = waits;
        v
    }

    const NOW: i64 = 10 * 3600 + 45 * 60;

    /// S1 holds argo:uat/root with S4 waiting; S3 holds repo:demo-api:main.
    fn demo() -> LockView {
        let mut root = claim(1, "argo:uat/root", ClaimKind::Claim, at(8, 40));
        root.item = Some("DEMO-33".into());
        root.reason = Some("apply".into());
        let mut repo = claim(3, "repo:demo-api:main", ClaimKind::Command, at(9, 30));
        repo.reason = Some("push".into());
        view(
            vec![repo, root],
            vec![wait(4, "argo:uat/root", at(9, 10), Some("sync"))],
        )
    }

    fn rows(v: &LockView) -> Vec<LockRow> {
        lock_rows(v, &slots(), NOW, clock)
    }

    #[test]
    fn rows_list_holders_kinds_reasons_and_waiters_in_order() {
        let r = rows(&demo());
        assert_eq!(r.len(), 2);
        assert_eq!(
            r[0],
            LockRow {
                target: "argo:uat/root".into(),
                holder: Some(1),
                holder_text: "S1 demo lane".into(),
                kind: "claim",
                since: "since 08:40 (2h 5m)".into(),
                claimed_at: Some(at(8, 40)),
                why: Some("for DEMO-33".into()),
                pin: None,
                long_held: false,
                waiters: vec!["1. S4 since 09:10, for sync".into()],
            }
        );
        assert_eq!(r[1].target, "repo:demo-api:main");
        assert_eq!(r[1].holder_text, "S3 uat fixes");
        assert_eq!(r[1].kind, "command");
        assert_eq!(r[1].since, "since 09:30 (1h 15m)");
        assert_eq!(r[1].why.as_deref(), Some("for push"), "reason without item");
        assert!(r[1].waiters.is_empty());
        // A slot with no label shows its name only.
        let mut v = demo();
        v.claims[0].slot = 2;
        v.claims[0].kind = ClaimKind::Grant;
        v.waits.push(wait(2, "argo:uat/root", at(9, 20), None));
        let r = rows(&v);
        assert_eq!(r[1].holder_text, "S2");
        assert_eq!(r[1].kind, "grant");
        assert_eq!(
            r[0].waiters,
            ["1. S4 since 09:10, for sync", "2. S2 since 09:20"]
        );
    }

    #[test]
    fn waiter_only_target_comes_last_held_by_nobody() {
        let mut v = demo();
        v.waits.insert(0, wait(2, "staging-eu", at(9, 50), None));
        v.waits.push(wait(4, "uat/root", at(9, 40), Some("apply")));
        let r = rows(&v);
        let targets: Vec<_> = r.iter().map(|r| r.target.as_str()).collect();
        assert_eq!(
            targets,
            [
                "argo:uat/root",
                "repo:demo-api:main",
                "uat/root",
                "staging-eu"
            ]
        );
        assert_eq!(r[2].holder, None);
        assert_eq!(r[2].holder_text, "nobody");
        assert_eq!(r[2].waiters, ["1. S4 since 09:40, for apply"]);
        assert!(!r[2].long_held);
    }

    #[test]
    fn long_held_rows_are_flagged() {
        let mut v = demo();
        v.flag_after_secs = 3600;
        v.claims[0].last_used = NOW - 2 * 3600;
        v.claims[1].last_used = NOW - 60;
        let r = rows(&v);
        let flagged: Vec<_> = r.iter().map(|r| (r.target.as_str(), r.long_held)).collect();
        assert_eq!(
            flagged,
            [("argo:uat/root", false), ("repo:demo-api:main", true)]
        );
    }

    #[test]
    fn pin_shows_its_revision() {
        let mut pin = claim(2, "argo:uat/payments", ClaimKind::Pin, at(7, 0));
        pin.pin_revision = Some("a1b2c3d".into());
        let r = rows(&view(vec![pin], vec![]));
        assert_eq!(r[0].kind, "pin");
        assert_eq!(r[0].pin.as_deref(), Some("at a1b2c3d"));
        assert_eq!(r[0].why, None);
    }

    #[test]
    fn age_text_steps() {
        assert_eq!(age_text(59), "0m");
        assert_eq!(age_text(12 * 60), "12m");
        assert_eq!(age_text(3600), "1h 0m");
        assert_eq!(age_text(2 * 3600 + 5 * 60), "2h 5m");
        assert_eq!(age_text(90000), "1d 1h");
        assert_eq!(age_text(3 * 86400 + 4 * 3600 + 59), "3d 4h");
        assert_eq!(age_text(-5), "0m", "clock skew");
    }

    #[test]
    fn summary_and_strip_texts() {
        let none = view(vec![], vec![]);
        assert_eq!(
            locks_summary(&none),
            "Nothing is held and nobody is waiting."
        );
        assert_eq!(locks_strip(&none), "Locks: none held (l)");
        let one = view(vec![claim(1, "a", ClaimKind::Claim, 0)], vec![]);
        assert_eq!(locks_summary(&one), "1 held");
        assert_eq!(locks_strip(&one), "Locks: 1 held (l)");
        let three = view(
            vec![
                claim(1, "a", ClaimKind::Claim, 0),
                claim(2, "b", ClaimKind::Pin, 0),
                claim(3, "c", ClaimKind::Command, 0),
            ],
            vec![wait(4, "a", 0, None)],
        );
        assert_eq!(locks_summary(&three), "3 held, 1 waiting");
        assert_eq!(locks_strip(&three), "Locks: 3 held, 1 waiting (l)");
    }

    const PROMPT: &str = "Release S1's argo:uat/root? S4 is next in the queue. Enter / Esc";

    #[test]
    fn confirm_sends_the_holder_it_showed() {
        let r = rows(&demo());
        let mut s = LocksState::default();
        assert_eq!(s.selected(&r), Some(0));
        assert_eq!(s.prompt(), None);
        assert_eq!(s.key("enter", &r), LocksAction::Redraw);
        assert_eq!(s.prompt().as_deref(), Some(PROMPT));
        assert_eq!(
            s.key("enter", &r),
            LocksAction::Send(Request::ForceRelease {
                target: "argo:uat/root".into(),
                holder: Some(1),
            })
        );
        assert_eq!(s.prompt(), None, "sending closes the confirm");
        // f opens it too; with nobody waiting the prompt says so.
        assert_eq!(s.key("down", &r), LocksAction::Redraw);
        assert_eq!(s.key("f", &r), LocksAction::Redraw);
        assert_eq!(
            s.prompt().as_deref(),
            Some("Release S3's repo:demo-api:main? Nobody is waiting. Enter / Esc")
        );
    }

    #[test]
    fn confirm_drops_when_the_target_changes_hands() {
        let mut s = LocksState::default();
        let before = rows(&demo());
        s.key("enter", &before);
        assert_eq!(s.prompt().as_deref(), Some(PROMPT));
        let mut v = demo();
        v.claims[1].slot = 4;
        v.waits.clear();
        let after = rows(&v);
        assert_eq!(
            s.on_change(&after).as_deref(),
            Some("argo:uat/root changed hands; look again")
        );
        assert_eq!(s.prompt(), None);
        assert_eq!(s.key("enter", &after), LocksAction::Redraw, "not sent");
        assert_eq!(
            s.prompt().as_deref(),
            Some("Release S4's argo:uat/root? Nobody is waiting. Enter / Esc")
        );
        // Released altogether: the confirm drops too.
        let gone = rows(&view(vec![], vec![]));
        assert_eq!(
            s.on_change(&gone).as_deref(),
            Some("argo:uat/root changed hands; look again")
        );
        // A change that keeps the holder keeps the confirm, with the queue
        // brought up to date.
        let mut s = LocksState::default();
        s.key("enter", &before);
        let mut v = demo();
        v.waits.clear();
        assert_eq!(s.on_change(&rows(&v)), None);
        assert_eq!(
            s.prompt().as_deref(),
            Some("Release S1's argo:uat/root? Nobody is waiting. Enter / Esc")
        );
    }

    #[test]
    fn confirm_drops_when_the_same_slot_renews_the_claim() {
        let mut s = LocksState::default();
        s.key("enter", &rows(&demo()));
        assert_eq!(s.prompt().as_deref(), Some(PROMPT));
        // S1 released and took the target again: same slot, new since.
        let mut v = demo();
        v.claims[1].since = at(10, 30);
        let after = rows(&v);
        assert_eq!(
            s.on_change(&after).as_deref(),
            Some("argo:uat/root changed hands; look again")
        );
        assert_eq!(s.prompt(), None);
        assert_eq!(s.key("enter", &after), LocksAction::Redraw, "nothing sent");
    }

    #[test]
    fn selection_follows_the_target_when_rows_reorder() {
        let mut s = LocksState::default();
        let r = rows(&demo());
        s.key("down", &r);
        assert_eq!(s.selected(&r), Some(1));
        s.key("down", &r);
        assert_eq!(s.selected(&r), Some(1), "clamped at the bottom");
        // An older claim arrives first in the list.
        let mut v = demo();
        v.claims
            .push(claim(2, "staging-eu", ClaimKind::Claim, at(7, 0)));
        let r2 = rows(&v);
        assert_eq!(s.on_change(&r2), None);
        assert_eq!(r2[s.selected(&r2).unwrap()].target, "repo:demo-api:main");
        s.key("up", &r2);
        s.key("up", &r2);
        s.key("up", &r2);
        assert_eq!(s.selected(&r2), Some(0), "clamped at the top");
        // The selected target goes: the selection stays in the list.
        let mut s = LocksState::default();
        s.key("down", &r);
        let last_gone = rows(&view(vec![demo().claims[1].clone()], vec![]));
        s.on_change(&last_gone);
        assert_eq!(s.selected(&last_gone), Some(0));
        assert_eq!(s.selected(&[]), None);
    }

    #[test]
    fn escape_cancels_the_confirm_then_goes_back() {
        let r = rows(&demo());
        let mut s = LocksState::default();
        s.key("enter", &r);
        assert_eq!(s.key("escape", &r), LocksAction::Redraw);
        assert_eq!(s.prompt(), None);
        assert_eq!(s.key("escape", &r), LocksAction::Back);
        assert_eq!(s.key("x", &r), LocksAction::Ignored);
    }

    #[test]
    fn waiter_only_row_has_nothing_to_release() {
        let r = rows(&view(vec![], vec![wait(4, "uat/root", 0, None)]));
        let mut s = LocksState::default();
        assert_eq!(
            s.key("enter", &r),
            LocksAction::Status("Nobody holds uat/root; nothing to release".into())
        );
        assert_eq!(s.prompt(), None);
        assert_eq!(s.key("enter", &[]), LocksAction::Ignored, "empty view");
    }
}
