//! The command palette (#90): Cmd+K, or the board's "Cmd+K" button, lists
//! the common moves (open a slot, relay a message to it, open its PR or
//! ticket, file a bug report, go to the next waiting item, the locks view)
//! built from what the board knows. Type to filter, Up and Down to move,
//! Enter or a click to run, Esc or a click outside to close.
//!
//! "File a bug report" asks for what happened, then runs this binary's
//! `ctl bug-report` and opens the report it writes, to read before posting.
//!
//! The rows, the fuzzy match, the key handling and the bug report's
//! arguments and output are pure (`rows`, `score`, `matching`,
//! `PaletteState`, `bug_args`, `written_path`) and unit tested.

use std::path::{Path, PathBuf};

use gpui::{
    div, prelude::*, px, rgb, rgba, Context, EventEmitter, FocusHandle, Focusable, FontWeight,
    KeyDownEvent, Keystroke, MouseButton, ScrollHandle, Window,
};
use overseer_proto::{slot_name, ItemKind, SlotId, SlotInfo, SlotState, WorkView};

use crate::app::colors;
use crate::sprint_view::{is_http_url, slot_prs, SlotBranch};
use crate::start_dialog::{edit_key, DialogKey, TextField};

// ---------------------------------------------------------------------------
// Pure logic
// ---------------------------------------------------------------------------

/// What a palette row does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    OpenSlot(SlotId),
    /// Opens the slot's session with the message box up.
    Message(SlotId),
    /// A PR, in the browser.
    OpenUrl(String),
    /// A ticket's 2ndBrain folder.
    OpenFolder(PathBuf),
    /// Asks for what happened first; the slot's screen goes in the report.
    BugReport(Option<SlotId>),
    NextWaiting,
    Locks,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub label: String,
    /// Muted text after the label; matched too.
    pub detail: String,
    pub command: Command,
}

fn row(label: impl Into<String>, detail: impl Into<String>, command: Command) -> Row {
    Row {
        label: label.into(),
        detail: detail.into(),
        command,
    }
}

fn running(s: &SlotInfo) -> bool {
    matches!(s.state, SlotState::Running { .. })
}

/// Every row, in the order shown before anything is typed: the slots, the
/// messages to running slots, the slots' tickets, the open PRs, the next
/// waiting item (only when something waits), the locks view, then the bug
/// reports. `on` is each Running or Resumable slot's repo and branch
/// (`slot_branches`), which ties a PR to its slot.
pub fn rows(slots: &[SlotInfo], work: &WorkView, on: &[SlotBranch], waiting: usize) -> Vec<Row> {
    let ticket_of = |id: SlotId| {
        crate::board::owned_by(work, id)
            .into_iter()
            .find(|i| i.kind == ItemKind::Ticket)
    };
    let branch_of = |id: SlotId| on.iter().find(|(s, _)| *s == id).map(|(_, rb)| rb);
    // "label · DEMO-12 · branch", leaving out what the slot does not have.
    let about = |s: &SlotInfo| {
        let mut parts = vec![];
        if !s.label.trim().is_empty() {
            parts.push(s.label.trim().to_string());
        }
        if let Some(t) = ticket_of(s.id) {
            parts.push(t.key.clone());
        }
        if let Some((_, b)) = branch_of(s.id) {
            parts.push(b.clone());
        }
        parts.join(" \u{b7} ")
    };
    let mut out = vec![];
    for s in slots {
        out.push(row(
            format!("Open {}", slot_name(s.id)),
            about(s),
            Command::OpenSlot(s.id),
        ));
    }
    for s in slots.iter().filter(|s| running(s)) {
        out.push(row(
            format!("Message {}\u{2026}", slot_name(s.id)),
            about(s),
            Command::Message(s.id),
        ));
    }
    for s in slots {
        if let Some(t) = ticket_of(s.id) {
            if let Some(folder) = &t.folder {
                out.push(row(
                    format!("Open ticket {}", t.key),
                    format!("{} \u{b7} {}", slot_name(s.id), t.title.trim()),
                    Command::OpenFolder(folder.clone()),
                ));
            }
        }
    }
    for p in &work.prs {
        let Some(url) = p.url.clone().filter(|u| is_http_url(u)) else {
            continue;
        };
        let owner = on
            .iter()
            .find(|(_, rb)| {
                slot_prs(work, rb)
                    .iter()
                    .any(|q| q.id == p.id && q.repo == p.repo)
            })
            .map(|(id, _)| slot_name(*id));
        let mut detail = p.repo.clone();
        if let Some(o) = owner {
            detail = format!("{o} \u{b7} {detail}");
        }
        out.push(row(
            format!("Open PR #{} {}", p.id, p.title.trim()),
            detail,
            Command::OpenUrl(url),
        ));
    }
    if waiting > 0 {
        out.push(row(
            "Next waiting item",
            format!("{waiting} waiting on you"),
            Command::NextWaiting,
        ));
    }
    out.push(row(
        "Locks",
        "every held target and its queue",
        Command::Locks,
    ));
    out.push(row(
        "File a bug report\u{2026}",
        "write it to read before posting",
        Command::BugReport(None),
    ));
    for s in slots.iter().filter(|s| running(s)) {
        out.push(row(
            format!("File a bug report for {}\u{2026}", slot_name(s.id)),
            "with its screen",
            Command::BugReport(Some(s.id)),
        ));
    }
    out
}

/// How well `query` fuzzy-matches `text`, ignoring case: every query
/// character in order, with runs of consecutive characters and characters
/// at the start of a word scoring higher. `None` when it does not match;
/// an empty query matches everything with 0.
pub fn score(query: &str, text: &str) -> Option<i32> {
    let query: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    if query.is_empty() {
        return Some(0);
    }
    let text: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    let mut total = 0;
    let mut q = 0;
    let mut last: Option<usize> = None;
    for (i, c) in text.iter().enumerate() {
        if q == query.len() {
            break;
        }
        if *c != query[q] {
            continue;
        }
        total += 1;
        if last.is_some_and(|l| l + 1 == i) {
            total += 5;
        }
        if i == 0 || !text[i - 1].is_alphanumeric() {
            total += 3;
        }
        if let Some(l) = last {
            // A small cost for each character skipped since the last match.
            total -= (i - l - 1).min(3) as i32;
        }
        last = Some(i);
        q += 1;
    }
    (q == query.len()).then_some(total)
}

/// The indexes of the rows `query` matches, best first; rows that score
/// the same keep their order.
pub fn matching(rows: &[Row], query: &str) -> Vec<usize> {
    let mut hits: Vec<(i32, usize)> = rows
        .iter()
        .enumerate()
        .filter_map(|(n, r)| {
            let label = score(query, &r.label);
            let both = score(query, &format!("{} {}", r.label, r.detail));
            // A match in the label alone counts a little more.
            let best = match (label, both) {
                (Some(l), Some(b)) => Some((l + 2).max(b)),
                (l, b) => l.map(|l| l + 2).or(b),
            };
            best.map(|s| (s, n))
        })
        .collect();
    hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    hits.into_iter().map(|(_, n)| n).collect()
}

/// The arguments for this binary's bug report: what happened, and the
/// slot whose screen to include.
pub fn bug_args(what: &str, slot: Option<SlotId>) -> Vec<String> {
    let mut args = vec![
        "ctl".to_string(),
        "bug-report".to_string(),
        what.to_string(),
    ];
    if let Some(s) = slot {
        args.push("--slot".into());
        args.push(s.to_string());
    }
    args
}

/// The report file `ctl bug-report` says it wrote ("Wrote ~/..."), with
/// `~` put back to `home`.
pub fn written_path(stdout: &str, home: Option<&Path>) -> Option<PathBuf> {
    let shown = stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("Wrote "))?
        .trim();
    match (shown.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => Some(home.join(rest)),
        _ => Some(PathBuf::from(shown)),
    }
}

/// What a key press in the palette means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteAction {
    Redraw,
    /// Run the selected row.
    Run(Command),
    /// Write the bug report with the text typed.
    Report {
        slot: Option<SlotId>,
        what: String,
    },
    Close,
    Paste,
    Ignored,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    List,
    /// Asking what happened, for a bug report.
    Describe(Option<SlotId>),
}

#[derive(Debug, Clone)]
pub struct PaletteState {
    pub rows: Vec<Row>,
    /// The filter in the list, what happened in `Describe`.
    pub field: TextField,
    pub mode: Mode,
    /// Index into `shown()`.
    pub selected: usize,
    pub error: Option<String>,
    /// A bug report is being written.
    pub busy: bool,
}

impl PaletteState {
    pub fn new(rows: Vec<Row>) -> Self {
        PaletteState {
            rows,
            field: TextField::default(),
            mode: Mode::List,
            selected: 0,
            error: None,
            busy: false,
        }
    }

    /// The rows the filter matches, best first, as indexes into `rows`.
    pub fn shown(&self) -> Vec<usize> {
        matching(&self.rows, &self.field.text)
    }

    /// Runs shown row `n`: a bug report asks for what happened first.
    pub fn pick(&mut self, n: usize) -> PaletteAction {
        let Some(&i) = self.shown().get(n) else {
            return PaletteAction::Ignored;
        };
        match self.rows[i].command.clone() {
            Command::BugReport(slot) => {
                self.mode = Mode::Describe(slot);
                self.field = TextField::default();
                self.error = None;
                PaletteAction::Redraw
            }
            c => PaletteAction::Run(c),
        }
    }

    pub fn key(&mut self, ks: &Keystroke) -> PaletteAction {
        if self.busy {
            return PaletteAction::Ignored;
        }
        let m = &ks.modifiers;
        let plain = !(m.platform || m.control || m.alt || m.shift || m.function);
        if plain && self.mode == Mode::List {
            let last = self.shown().len().saturating_sub(1);
            match ks.key.as_str() {
                "up" => {
                    self.selected = self.selected.saturating_sub(1);
                    return PaletteAction::Redraw;
                }
                "down" => {
                    self.selected = (self.selected + 1).min(last);
                    return PaletteAction::Redraw;
                }
                _ => {}
            }
        }
        let before = self.field.text.clone();
        match edit_key(&mut self.field, ks) {
            DialogKey::Edited => {
                if self.field.text != before {
                    self.selected = 0;
                    self.error = None;
                }
                PaletteAction::Redraw
            }
            DialogKey::Submit => match &self.mode {
                Mode::List => self.pick(self.selected),
                Mode::Describe(slot) => {
                    let what = self.field.text.trim().to_string();
                    if what.is_empty() {
                        self.error = Some("Say what happened first.".into());
                        return PaletteAction::Redraw;
                    }
                    PaletteAction::Report { slot: *slot, what }
                }
            },
            DialogKey::Cancel => PaletteAction::Close,
            DialogKey::Paste => PaletteAction::Paste,
            DialogKey::NextField | DialogKey::PrevField | DialogKey::Ignored => {
                PaletteAction::Ignored
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The view
// ---------------------------------------------------------------------------

pub enum PaletteEvent {
    Run(Command),
    Closed,
}

pub struct Palette {
    state: PaletteState,
    home: Option<PathBuf>,
    scroll: ScrollHandle,
    focus_handle: FocusHandle,
}

impl EventEmitter<PaletteEvent> for Palette {}

impl Focusable for Palette {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Palette {
    pub fn new(rows: Vec<Row>, home: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
        Palette {
            state: PaletteState::new(rows),
            home,
            scroll: ScrollHandle::new(),
            focus_handle: cx.focus_handle(),
        }
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        if !self.state.busy {
            cx.emit(PaletteEvent::Closed);
        }
    }

    fn act(&mut self, action: PaletteAction, cx: &mut Context<Self>) {
        match action {
            PaletteAction::Ignored => return,
            PaletteAction::Redraw => {}
            PaletteAction::Run(c) => cx.emit(PaletteEvent::Run(c)),
            PaletteAction::Report { slot, what } => self.report(slot, what, cx),
            PaletteAction::Close => self.close(cx),
            PaletteAction::Paste => {
                if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                    self.state.field.insert(&text);
                    self.state.selected = 0;
                }
            }
        }
        self.scroll.scroll_to_item(self.state.selected);
        cx.notify();
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let action = self.state.key(&ev.keystroke);
        if action != PaletteAction::Ignored {
            cx.stop_propagation();
        }
        self.act(action, cx);
    }

    fn click_row(&mut self, n: usize, cx: &mut Context<Self>) {
        if self.state.busy {
            return;
        }
        self.state.selected = n;
        let action = self.state.pick(n);
        self.act(action, cx);
    }

    /// Runs `ctl bug-report` off the UI thread and opens what it wrote.
    fn report(&mut self, slot: Option<SlotId>, what: String, cx: &mut Context<Self>) {
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(e) => {
                self.state.error = Some(format!("could not find overseer: {e}"));
                return;
            }
        };
        self.state.busy = true;
        self.state.error = None;
        let args = bug_args(&what, slot);
        let home = self.home.clone();
        cx.spawn(async move |this, cx| {
            let out = cx
                .background_executor()
                .spawn(async move { std::process::Command::new(exe).args(args).output() })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.state.busy = false;
                let written = match &out {
                    Ok(o) if o.status.success() => {
                        written_path(&String::from_utf8_lossy(&o.stdout), home.as_deref())
                    }
                    _ => None,
                };
                match (written, out) {
                    (Some(path), _) => {
                        crate::app::open_external(path, "the bug report");
                        cx.emit(PaletteEvent::Closed);
                    }
                    (None, Ok(o)) => {
                        let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
                        this.state.error = Some(if err.is_empty() {
                            "The bug report was not written.".into()
                        } else {
                            err
                        });
                    }
                    (None, Err(e)) => {
                        this.state.error = Some(format!("could not run overseer: {e}"))
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for Palette {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);
        let field = self.state.field.display(focused);
        let describe = match self.state.mode {
            Mode::Describe(slot) => Some(slot),
            Mode::List => None,
        };
        let shown = self.state.shown();
        let empty = shown.is_empty();
        let list: Vec<_> = shown
            .into_iter()
            .enumerate()
            .map(|(n, i)| {
                let r = &self.state.rows[i];
                let on = n == self.state.selected;
                div()
                    .id(("palette-row", n))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py(px(3.))
                    .rounded(px(4.))
                    .when(on, |d| d.bg(rgb(colors::ACCENT_BG)))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(colors::CARD_HOVER)))
                    .child(
                        div()
                            .flex_none()
                            .max_w(px(360.))
                            .truncate()
                            .text_color(rgb(if on { colors::ACCENT } else { colors::TEXT }))
                            .child(r.label.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_xs()
                            .text_color(rgb(colors::MUTED))
                            .child(r.detail.clone()),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.click_row(n, cx)))
            })
            .collect();
        let title = match describe {
            None => "Command palette".to_string(),
            Some(None) => "File a bug report".to_string(),
            Some(Some(s)) => format!("File a bug report for {}", slot_name(s)),
        };
        let hint = if self.state.busy {
            "Writing the report\u{2026}"
        } else if describe.is_some() {
            "Say what happened · Enter writes the report to read before posting · Esc close"
        } else {
            "Type to filter · Up/Down move · Enter run · Esc close"
        };
        let card = div()
            .w(px(640.))
            .mt(px(80.))
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
            .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
            .child(
                div()
                    .px_2()
                    .py_1()
                    .min_h(px(24.))
                    .rounded(px(4.))
                    .bg(rgb(colors::FIELD))
                    .border_1()
                    .border_color(rgb(colors::ACCENT))
                    .child(field),
            )
            .when(describe.is_none(), |c| {
                c.child(
                    div()
                        .id("palette-rows")
                        .flex()
                        .flex_col()
                        .max_h(px(360.))
                        .overflow_y_scroll()
                        .track_scroll(&self.scroll)
                        .children(list)
                        .children(empty.then(|| {
                            div()
                                .px_2()
                                .text_color(rgb(colors::TEXT_DIM))
                                .child("Nothing matches")
                        })),
                )
            })
            .children(
                self.state
                    .error
                    .clone()
                    .map(|e| div().text_color(rgb(colors::RED)).child(e)),
            )
            .child(div().text_xs().text_color(rgb(colors::MUTED)).child(hint));
        div()
            .key_context("Palette")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
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
    use overseer_proto::{PullRequest, WorkItem};

    fn slot(id: SlotId, label: &str, state: SlotState) -> SlotInfo {
        SlotInfo {
            id,
            label: label.into(),
            cwd: None,
            command: None,
            state,
            cols: 80,
            rows: 24,
            activity: None,
            session_note: None,
            held: false,
        }
    }

    fn live() -> SlotState {
        SlotState::Running { pid: 1, since: 0 }
    }

    fn ticket(key: &str, owner: SlotId, folder: Option<&str>) -> WorkItem {
        WorkItem {
            key: key.into(),
            kind: ItemKind::Ticket,
            title: " Billing export ".into(),
            status: "In Progress".into(),
            priority: None,
            parent: None,
            owner: Some(owner),
            in_list: true,
            folder: folder.map(PathBuf::from),
            stage: None,
            stage_since: None,
        }
    }

    fn pr(id: u64, branch: &str, url: Option<&str>) -> PullRequest {
        PullRequest {
            repo: "demo-api".into(),
            id,
            title: "Add the export".into(),
            branch: branch.into(),
            url: url.map(String::from),
            author: None,
            created: None,
            ticket: None,
            approvals: None,
            comments: None,
            build: None,
            head: None,
        }
    }

    fn labels(rows: &[Row]) -> Vec<&str> {
        rows.iter().map(|r| r.label.as_str()).collect()
    }

    fn key(k: &str, ch: Option<&str>) -> Keystroke {
        let mut ks = Keystroke::parse(k).unwrap();
        ks.key_char = ch.map(String::from);
        ks
    }

    fn typed(p: &mut PaletteState, text: &str) {
        for c in text.chars() {
            let s = c.to_string();
            p.key(&key(&s, Some(&s)));
        }
    }

    fn board() -> (Vec<SlotInfo>, WorkView, Vec<SlotBranch>) {
        let slots = vec![
            slot(0, "coordinator", live()),
            slot(1, "", SlotState::Empty),
            slot(2, "export", live()),
        ];
        let work = WorkView {
            notes_off: None,
            items: vec![ticket("DEMO-12", 2, Some("/vault/DEMO-12"))],
            sources: vec![],
            notices: vec![],
            sprint: None,
            prs: vec![
                pr(7, "demo-export", Some("https://example.test/pr/7")),
                pr(8, "other", Some("https://example.test/pr/8")),
                pr(9, "no-link", None),
            ],
            sprint_day: None,
        };
        let on = vec![(2, ("demo-api".to_string(), "demo-export".to_string()))];
        (slots, work, on)
    }

    #[test]
    fn rows_cover_slots_messages_tickets_prs_and_reports() {
        let (slots, work, on) = board();
        let all = rows(&slots, &work, &on, 0);
        assert_eq!(
            labels(&all),
            [
                "Open S0",
                "Open S1",
                "Open S2",
                "Message S0\u{2026}",
                "Message S2\u{2026}",
                "Open ticket DEMO-12",
                "Open PR #7 Add the export",
                "Open PR #8 Add the export",
                "Locks",
                "File a bug report\u{2026}",
                "File a bug report for S0\u{2026}",
                "File a bug report for S2\u{2026}",
            ]
        );
        let s2 = &all[2];
        assert_eq!(s2.detail, "export \u{b7} DEMO-12 \u{b7} demo-export");
        assert_eq!(all[1].detail, "");
        assert_eq!(all[5].detail, "S2 \u{b7} Billing export");
        assert_eq!(all[5].command, Command::OpenFolder("/vault/DEMO-12".into()));
        // A PR on a slot's branch names the slot; a PR with no link is left out.
        assert_eq!(all[6].detail, "S2 \u{b7} demo-api");
        assert_eq!(all[7].detail, "demo-api");
        assert_eq!(
            all[6].command,
            Command::OpenUrl("https://example.test/pr/7".into())
        );
    }

    #[test]
    fn the_next_waiting_row_shows_only_when_something_waits() {
        let (slots, work, on) = board();
        assert!(!labels(&rows(&slots, &work, &on, 0)).contains(&"Next waiting item"));
        let all = rows(&slots, &work, &on, 2);
        let next = all
            .iter()
            .find(|r| r.command == Command::NextWaiting)
            .unwrap();
        assert_eq!(next.detail, "2 waiting on you");
    }

    #[test]
    fn score_wants_every_character_in_order() {
        assert_eq!(score("", "anything"), Some(0));
        assert!(score("s2", "Open S2").is_some());
        assert!(score("demo12", "Open ticket DEMO-12").is_some());
        assert_eq!(score("21", "Open S2"), None);
        assert_eq!(score("xyz", "Open S2"), None);
    }

    #[test]
    fn score_prefers_runs_and_word_starts() {
        let run = score("exp", "export").unwrap();
        let spread = score("exp", "e x p").unwrap();
        assert!(run > spread, "{run} {spread}");
        let start = score("m", "Message").unwrap();
        let inside = score("m", "Open ticket DEMO").unwrap();
        assert!(start > inside, "{start} {inside}");
    }

    #[test]
    fn matching_puts_the_best_match_first_and_matches_details() {
        let (slots, work, on) = board();
        let all = rows(&slots, &work, &on, 0);
        let best = matching(&all, "msg s2");
        assert_eq!(all[best[0]].label, "Message S2\u{2026}");
        // The ticket key is in Open S2's detail and in the ticket row.
        let demo: Vec<&str> = matching(&all, "DEMO-12")
            .into_iter()
            .map(|n| all[n].label.as_str())
            .collect();
        assert!(demo.contains(&"Open ticket DEMO-12"));
        assert!(demo.contains(&"Open S2"));
        // A PR title matches.
        let pr = matching(&all, "add export");
        assert_eq!(all[pr[0]].label, "Open PR #7 Add the export");
        assert!(matching(&all, "zzz").is_empty());
    }

    #[test]
    fn keys_filter_move_and_run() {
        let (slots, work, on) = board();
        let mut p = PaletteState::new(rows(&slots, &work, &on, 0));
        assert_eq!(p.key(&key("down", None)), PaletteAction::Redraw);
        assert_eq!(p.selected, 1);
        assert_eq!(
            p.key(&key("enter", None)),
            PaletteAction::Run(Command::OpenSlot(1))
        );
        // Typing starts the selection again at the best match.
        typed(&mut p, "locks");
        assert_eq!(p.selected, 0);
        assert_eq!(
            p.key(&key("enter", None)),
            PaletteAction::Run(Command::Locks)
        );
        // Up stops at the top, Down at the last row shown.
        p.key(&key("up", None));
        assert_eq!(p.selected, 0);
        for _ in 0..5 {
            p.key(&key("down", None));
        }
        assert_eq!(p.selected, p.shown().len() - 1);
        assert_eq!(p.key(&key("escape", None)), PaletteAction::Close);
        assert_eq!(p.key(&key("cmd-v", None)), PaletteAction::Paste);
    }

    #[test]
    fn a_bug_report_asks_what_happened_before_it_runs() {
        let (slots, work, on) = board();
        let mut p = PaletteState::new(rows(&slots, &work, &on, 0));
        typed(&mut p, "bug report for s2");
        assert_eq!(p.key(&key("enter", None)), PaletteAction::Redraw);
        assert_eq!(p.mode, Mode::Describe(Some(2)));
        assert_eq!(p.field.text, "");
        // An empty report is refused.
        p.key(&key("enter", None));
        assert!(p.error.is_some());
        typed(&mut p, "the relay stuck");
        assert_eq!(
            p.key(&key("enter", None)),
            PaletteAction::Report {
                slot: Some(2),
                what: "the relay stuck".into()
            }
        );
        // While it is written the keys wait.
        p.busy = true;
        assert_eq!(p.key(&key("escape", None)), PaletteAction::Ignored);
    }

    #[test]
    fn bug_args_name_the_slot_when_there_is_one() {
        assert_eq!(
            bug_args("it broke", None),
            ["ctl", "bug-report", "it broke"]
        );
        assert_eq!(
            bug_args("-x", Some(3)),
            ["ctl", "bug-report", "-x", "--slot", "3"]
        );
    }

    #[test]
    fn written_path_reads_the_wrote_line() {
        let out = "Wrote ~/Library/Logs/Overseer/bug-report-1.md\nRead it before posting\n";
        assert_eq!(
            written_path(out, Some(Path::new("/Users/demo"))),
            Some(PathBuf::from(
                "/Users/demo/Library/Logs/Overseer/bug-report-1.md"
            ))
        );
        assert_eq!(
            written_path("Wrote /tmp/r.md\n", None),
            Some(PathBuf::from("/tmp/r.md"))
        );
        assert_eq!(written_path("nothing here\n", None), None);
    }
}
