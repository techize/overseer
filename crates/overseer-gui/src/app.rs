//! The Overseer window: screen state, navigation, key bindings, live slot
//! updates and the per-slot terminal views.
//!
//! Navigation keys never take terminal input:
//! - `Cmd+0` to `Cmd+6` open a session and `Cmd+Esc` goes to the board, from
//!   anywhere except the start dialog. Cmd keys never reach the PTY anyway.
//! - Plain `Esc` goes to the board only when focus is in the session view
//!   but not in its terminal (after clicking the tab strip or the context
//!   panel). In the terminal, Esc closes the find bar when it is open and
//!   otherwise goes to Claude Code, which uses it to interrupt.
//! - Plain digits always go to whatever has focus.
//!
//! The routing is GPUI's keymap with the contexts `Overseer` (window),
//! `Board`, `Session`, `Terminal` and `StartDialog`; `key_bindings` is tested
//! against GPUI's `Keymap` directly.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui::{
    actions, div, prelude::*, rgb, App, Context, Entity, FocusHandle, Focusable, KeyBinding,
    Subscription, Task, Window,
};
use overseer_proto::{Client, Event, Frame, Request, Response, SlotId, SlotInfo, SlotState};

use crate::board::Dir;
use crate::session::RestartPlan;
use crate::session::{incarnation, keep_view, restart_begin, restart_on_change, Incarnation};
use crate::start_dialog::{DialogEvent, StartDialog};
use crate::{FrameHub, TerminalView, Theme};

/// Dark palette from the approved mockups.
pub(crate) mod colors {
    pub const BG: u32 = 0x0f1218;
    pub const CARD: u32 = 0x161b26;
    pub const CARD_HOVER: u32 = 0x1c2230;
    pub const BORDER: u32 = 0x232a38;
    pub const BORDER_COORD: u32 = 0x2a3245;
    pub const TEXT: u32 = 0xd7dce4;
    pub const TEXT_DIM: u32 = 0xaab3c2;
    pub const MUTED: u32 = 0x7d8696;
    pub const ACCENT: u32 = 0x9fb4ff;
    pub const ACCENT_BG: u32 = 0x1f2742;
    pub const GREEN: u32 = 0x3fb950;
    pub const GREY: u32 = 0x8b949e;
    pub const EDGE_IDLE: u32 = 0x555d6b;
    pub const ORANGE: u32 = 0xf0883e;
    pub const RED: u32 = 0xf85149;
    pub const BUTTON: u32 = 0x21283a;
    pub const BUTTON_BORDER: u32 = 0x2f3850;
    pub const FIELD: u32 = 0x0b0e14;
    pub const TERM_EMPTY: u32 = 0x05070a;
}

actions!(
    overseer,
    [
        GoBoard,
        GoS0,
        GoS1,
        GoS2,
        GoS3,
        GoS4,
        GoS5,
        GoS6,
        StartSession,
        Quit,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        OpenSelected,
    ]
);

/// What the window shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Board,
    Session(SlotId),
}

/// Navigation keys work anywhere in the window except the start dialog.
const NAV: &str = "Overseer && !StartDialog";

pub fn key_bindings() -> Vec<KeyBinding> {
    let nav = Some(NAV);
    vec![
        KeyBinding::new("cmd-0", GoS0, nav),
        KeyBinding::new("cmd-1", GoS1, nav),
        KeyBinding::new("cmd-2", GoS2, nav),
        KeyBinding::new("cmd-3", GoS3, nav),
        KeyBinding::new("cmd-4", GoS4, nav),
        KeyBinding::new("cmd-5", GoS5, nav),
        KeyBinding::new("cmd-6", GoS6, nav),
        KeyBinding::new("cmd-escape", GoBoard, nav),
        KeyBinding::new("cmd-n", StartSession, nav),
        // Not from the terminal: there Esc closes the find bar or goes to
        // Claude Code.
        KeyBinding::new("escape", GoBoard, Some("Session && !Terminal")),
        KeyBinding::new("left", SelectLeft, Some("Board")),
        KeyBinding::new("right", SelectRight, Some("Board")),
        KeyBinding::new("up", SelectUp, Some("Board")),
        KeyBinding::new("down", SelectDown, Some("Board")),
        KeyBinding::new("enter", OpenSelected, Some("Board")),
        KeyBinding::new("cmd-q", Quit, None),
    ]
}

/// Whether the 1 s ticker is needed: a running slot's elapsed time is on
/// screen.
pub fn needs_tick(screen: Screen, slots: &[SlotInfo]) -> bool {
    let running = |s: &SlotInfo| matches!(s.state, SlotState::Running { .. });
    match screen {
        Screen::Board => slots.iter().any(running),
        Screen::Session(id) => slots.get(id as usize).is_some_and(running),
    }
}

/// Whether the 1 s elapsed-time ticker runs. The ticker stops itself when
/// nothing running is visible, so every change to the screen or the slots
/// must go through `on_change` to start it again.
#[derive(Debug, Default)]
pub struct Ticker {
    running: bool,
}

impl Ticker {
    /// After the screen or the slots changed: true when a ticker task must be
    /// spawned now.
    pub fn on_change(&mut self, screen: Screen, slots: &[SlotInfo]) -> bool {
        if self.running || !needs_tick(screen, slots) {
            return false;
        }
        self.running = true;
        true
    }

    /// On each tick: true to keep ticking. False stops the ticker.
    pub fn on_tick(&mut self, screen: Screen, slots: &[SlotInfo]) -> bool {
        self.running = needs_tick(screen, slots);
        self.running
    }
}

/// The slot `Cmd+N` starts: the selected card on the board, the shown slot in
/// a session. None when that slot is already running.
pub fn start_target(screen: Screen, selected: SlotId, slots: &[SlotInfo]) -> Option<SlotId> {
    let id = match screen {
        Screen::Board => selected,
        Screen::Session(id) => id,
    };
    let info = slots.get(id as usize)?;
    (!matches!(info.state, SlotState::Running { .. })).then_some(id)
}

/// What clicking a card does: open the session, or the start dialog for an
/// empty slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardClick {
    Open(SlotId),
    Start(SlotId),
}

pub fn card_click(info: &SlotInfo) -> CardClick {
    match info.state {
        SlotState::Empty => CardClick::Start(info.id),
        _ => CardClick::Open(info.id),
    }
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn empty_slots() -> Vec<SlotInfo> {
    (0..7)
        .map(|id| SlotInfo {
            id,
            label: String::new(),
            cwd: None,
            command: None,
            state: SlotState::Empty,
            cols: 80,
            rows: 24,
        })
        .collect()
}

struct TermEntry {
    view: Entity<TerminalView>,
    /// The run it was made for; see `keep_view`.
    incarnation: Incarnation,
}

pub struct OverseerApp {
    client: Arc<Client>,
    theme: Theme,
    pub(crate) slots: Vec<SlotInfo>,
    pub(crate) screen: Screen,
    pub(crate) selected: SlotId,
    pub(crate) home: Option<PathBuf>,
    /// Last error from a request, shown until the next action.
    pub(crate) status: Option<String>,
    terminals: HashMap<SlotId, TermEntry>,
    dialog: Option<(Entity<StartDialog>, Subscription)>,
    restarts: HashMap<SlotId, RestartPlan>,
    /// Focus this slot's terminal as soon as it exists.
    focus_when_ready: Option<SlotId>,
    root_focus: FocusHandle,
    pub(crate) board_focus: FocusHandle,
    pub(crate) session_focus: FocusHandle,
    ticker: Ticker,
    _ticker: Option<Task<()>>,
    _events: Task<()>,
}

impl OverseerApp {
    pub fn new(
        client: Arc<Client>,
        theme: Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let hub = cx.global::<FrameHub>().clone();
        let mut frames = hub.take_events();
        let events = cx.spawn_in(window, async move |this, cx| {
            let Some(frames) = frames.as_mut() else {
                return;
            };
            while let Some(frame) = frames.recv().await {
                let alive = this.update_in(cx, |this, window, cx| this.on_frame(frame, window, cx));
                if alive.is_err() {
                    return;
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.status = Some(crate::terminal_view::LOST_DAEMON.into());
                cx.notify();
            });
        });
        // The list comes back through the event stream (a marked reply), so
        // it is applied in order with the events around it.
        let lister = client.clone();
        cx.spawn(async move |this, cx| {
            if let Err(e) = lister.request_marked(Request::ListSlots, |_| true).await {
                let _ = this.update(cx, |this, cx| {
                    this.status = Some(format!("{} ({e:#})", crate::terminal_view::LOST_DAEMON));
                    cx.notify();
                });
            }
        })
        .detach();
        let board_focus = cx.focus_handle();
        window.focus(&board_focus);
        OverseerApp {
            client,
            theme,
            slots: empty_slots(),
            screen: Screen::Board,
            selected: 1,
            home: std::env::var_os("HOME").map(PathBuf::from),
            status: None,
            terminals: HashMap::new(),
            dialog: None,
            restarts: HashMap::new(),
            focus_when_ready: None,
            root_focus: cx.focus_handle(),
            board_focus,
            session_focus: cx.focus_handle(),
            ticker: Ticker::default(),
            _ticker: None,
            _events: events,
        }
    }

    fn on_frame(&mut self, frame: Frame, window: &mut Window, cx: &mut Context<Self>) {
        match frame {
            Frame::Event(Event::SlotChanged(info)) => self.apply_info(info, window, cx),
            Frame::Response {
                resp: Response::Slots(list),
                ..
            } => {
                for info in list {
                    self.apply_info(info, window, cx);
                }
            }
            _ => {}
        }
    }

    fn apply_info(&mut self, info: SlotInfo, window: &mut Window, cx: &mut Context<Self>) {
        let id = info.id;
        if id as usize >= self.slots.len() {
            return;
        }
        let dropped = match self.terminals.get(&id) {
            Some(t) if !keep_view(t.incarnation, &info.state) => {
                let had_focus = t.view.focus_handle(cx).contains_focused(window, cx);
                self.terminals.remove(&id);
                had_focus
            }
            _ => false,
        };
        if let Some(plan) = self.restarts.get(&id) {
            if let Some(req) = restart_on_change(plan, &info) {
                self.restarts.remove(&id);
                self.send(req, cx);
            }
        }
        self.slots[id as usize] = info;
        if self.screen == Screen::Session(id) && self.dialog.is_none() {
            if let Some(view) = self.ensure_terminal(id, window, cx) {
                // The old terminal had focus, or the user is waiting for
                // this slot's terminal (just started or restarted).
                if dropped || self.focus_when_ready == Some(id) {
                    self.focus_when_ready = None;
                    view.read(cx).focus(window);
                }
            }
        }
        self.ensure_ticker(cx);
        cx.notify();
    }

    /// The slot's terminal view, made on first use. None for an empty slot.
    pub(crate) fn ensure_terminal(
        &mut self,
        id: SlotId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<TerminalView>> {
        let state = self.slots.get(id as usize)?.state.clone();
        if state == SlotState::Empty {
            return None;
        }
        let client = self.client.clone();
        let theme = self.theme.clone();
        let entry = self.terminals.entry(id).or_insert_with(|| TermEntry {
            view: TerminalView::new(id, client, theme, window, cx),
            incarnation: incarnation(&state),
        });
        Some(entry.view.clone())
    }

    fn ensure_ticker(&mut self, cx: &mut Context<Self>) {
        if !self.ticker.on_change(self.screen, &self.slots) {
            return;
        }
        self._ticker = Some(cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            let more = this
                .update(cx, |this, cx| {
                    let more = this.ticker.on_tick(this.screen, &this.slots);
                    if more {
                        cx.notify();
                    }
                    more
                })
                .unwrap_or(false);
            if !more {
                break;
            }
        }));
    }

    /// Sends `req`, showing an error reply in the status line.
    fn send(&mut self, req: Request, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let restart_slot = match &req {
            Request::StopSlot { id } => Some(*id),
            _ => None,
        };
        cx.spawn(async move |this, cx| {
            let result = client.request(req).await;
            let _ = this.update(cx, |this, cx| {
                let err = match result {
                    Ok(Response::Error { message }) => Some(message),
                    Err(e) => Some(format!("{} ({e:#})", crate::terminal_view::LOST_DAEMON)),
                    Ok(_) => None,
                };
                if let Some(e) = err {
                    // A failed stop abandons its restart.
                    if let Some(id) = restart_slot {
                        this.restarts.remove(&id);
                    }
                    this.status = Some(e);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(crate) fn restarting(&self, id: SlotId) -> bool {
        self.restarts.contains_key(&id)
    }

    pub(crate) fn stop_slot(&mut self, id: SlotId, cx: &mut Context<Self>) {
        self.status = None;
        self.send(Request::StopSlot { id }, cx);
    }

    /// Stop, wait for Exited, then start with the same label, cwd and
    /// command. The new run replaces the terminal view (see `apply_info`).
    pub(crate) fn restart_slot(&mut self, id: SlotId, cx: &mut Context<Self>) {
        self.status = None;
        let Some((req, plan)) = self.slots.get(id as usize).and_then(restart_begin) else {
            return;
        };
        if let Some(plan) = plan {
            self.restarts.insert(id, plan);
        }
        self.focus_when_ready = Some(id);
        self.send(req, cx);
        cx.notify();
    }

    /// The only place the screen changes, so the ticker always restarts when
    /// a running slot becomes visible again.
    fn set_screen(&mut self, screen: Screen, cx: &mut Context<Self>) {
        self.screen = screen;
        self.ensure_ticker(cx);
    }

    pub(crate) fn go_board(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Screen::Session(id) = self.screen {
            self.selected = id;
        }
        self.set_screen(Screen::Board, cx);
        self.focus_when_ready = None;
        window.focus(&self.board_focus);
        cx.notify();
    }

    pub(crate) fn go_session(&mut self, id: SlotId, window: &mut Window, cx: &mut Context<Self>) {
        self.set_screen(Screen::Session(id), cx);
        self.selected = id;
        match self.ensure_terminal(id, window, cx) {
            Some(view) => {
                self.focus_when_ready = None;
                view.read(cx).focus(window)
            }
            None => {
                // Just started: the Running event may still be on its way.
                self.focus_when_ready = Some(id);
                window.focus(&self.session_focus)
            }
        }
        cx.notify();
    }

    pub(crate) fn open_card(&mut self, id: SlotId, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = id;
        match card_click(&self.slots[id as usize]) {
            CardClick::Open(id) => self.go_session(id, window, cx),
            CardClick::Start(id) => self.open_dialog(id, window, cx),
        }
    }

    pub(crate) fn move_selection(&mut self, dir: Dir, cx: &mut Context<Self>) {
        self.selected = crate::board::move_selection(self.selected, dir);
        cx.notify();
    }

    pub(crate) fn open_dialog(&mut self, id: SlotId, window: &mut Window, cx: &mut Context<Self>) {
        self.status = None;
        let client = self.client.clone();
        let dialog = cx.new(|cx| StartDialog::new(id, client, cx));
        let sub = cx.subscribe_in(&dialog, window, |this, _, ev: &DialogEvent, window, cx| {
            this.dialog = None;
            match ev {
                DialogEvent::Started(id) => this.go_session(*id, window, cx),
                DialogEvent::Cancelled => this.restore_focus(window, cx),
            }
            cx.notify();
        });
        window.focus(&dialog.focus_handle(cx));
        self.dialog = Some((dialog, sub));
        cx.notify();
    }

    fn restore_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.screen {
            Screen::Board => window.focus(&self.board_focus),
            Screen::Session(id) => match self.terminals.get(&id) {
                Some(t) => t.view.read(cx).focus(window),
                None => window.focus(&self.session_focus),
            },
        }
    }
}

impl Render for OverseerApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = now_secs();
        let screen = match self.screen {
            Screen::Board => self.render_board(now, window, cx).into_any_element(),
            Screen::Session(id) => self.render_session(id, now, window, cx).into_any_element(),
        };
        div()
            .key_context("Overseer")
            .track_focus(&self.root_focus)
            .on_action(cx.listener(|this, _: &GoBoard, window, cx| this.go_board(window, cx)))
            .on_action(cx.listener(|this, _: &GoS0, w, cx| this.go_session(0, w, cx)))
            .on_action(cx.listener(|this, _: &GoS1, w, cx| this.go_session(1, w, cx)))
            .on_action(cx.listener(|this, _: &GoS2, w, cx| this.go_session(2, w, cx)))
            .on_action(cx.listener(|this, _: &GoS3, w, cx| this.go_session(3, w, cx)))
            .on_action(cx.listener(|this, _: &GoS4, w, cx| this.go_session(4, w, cx)))
            .on_action(cx.listener(|this, _: &GoS5, w, cx| this.go_session(5, w, cx)))
            .on_action(cx.listener(|this, _: &GoS6, w, cx| this.go_session(6, w, cx)))
            .on_action(cx.listener(|this, _: &StartSession, window, cx| {
                if let Some(id) = start_target(this.screen, this.selected, &this.slots) {
                    this.open_dialog(id, window, cx);
                }
            }))
            .relative()
            .size_full()
            .bg(rgb(colors::BG))
            .text_color(rgb(colors::TEXT))
            .child(screen)
            .children(self.dialog.as_ref().map(|(d, _)| d.clone()))
    }
}

/// Registers the key bindings, the Quit action and the app menu.
pub fn install(cx: &mut App) {
    cx.bind_keys(key_bindings());
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.set_menus(vec![gpui::Menu {
        name: "Overseer".into(),
        items: vec![gpui::MenuItem::action("Quit Overseer", Quit)],
    }]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{KeyContext, Keymap, Keystroke};

    /// The action GPUI's keymap picks for `key` with focus inside
    /// `contexts` (outermost first), or None when the key goes on to the
    /// focused element's own key handling.
    fn routed(key: &str, contexts: &[&str]) -> Option<&'static str> {
        let keymap = Keymap::new(key_bindings());
        let stack: Vec<KeyContext> = contexts
            .iter()
            .map(|c| KeyContext::parse(c).unwrap())
            .collect();
        let ks = Keystroke::parse(key).unwrap();
        let (bindings, _) = keymap.bindings_for_input(&[ks], &stack);
        bindings.first().map(|b| b.action().name())
    }

    const TERMINAL: &[&str] = &["Overseer", "Session", "Terminal"];
    const CHROME: &[&str] = &["Overseer", "Session"];
    const BOARD: &[&str] = &["Overseer", "Board"];
    const DIALOG: &[&str] = &["Overseer", "StartDialog"];

    #[test]
    fn cmd_digits_navigate_from_everywhere_but_the_dialog() {
        assert_eq!(routed("cmd-3", TERMINAL), Some("overseer::GoS3"));
        assert_eq!(routed("cmd-0", BOARD), Some("overseer::GoS0"));
        assert_eq!(routed("cmd-6", CHROME), Some("overseer::GoS6"));
        assert_eq!(routed("cmd-1", DIALOG), None);
    }

    #[test]
    fn plain_digits_reach_the_terminal() {
        for d in ["0", "1", "6"] {
            assert_eq!(routed(d, TERMINAL), None);
            assert_eq!(routed(d, DIALOG), None);
        }
    }

    #[test]
    fn escape_stays_with_the_terminal() {
        // Find bar or Claude Code gets it.
        assert_eq!(routed("escape", TERMINAL), None);
        assert_eq!(routed("escape", CHROME), Some("overseer::GoBoard"));
        assert_eq!(routed("escape", BOARD), None);
        assert_eq!(routed("escape", DIALOG), None);
        assert_eq!(routed("cmd-escape", TERMINAL), Some("overseer::GoBoard"));
        assert_eq!(routed("cmd-escape", DIALOG), None);
    }

    #[test]
    fn other_bindings() {
        assert_eq!(routed("cmd-n", TERMINAL), Some("overseer::StartSession"));
        assert_eq!(routed("cmd-n", DIALOG), None);
        assert_eq!(routed("cmd-q", TERMINAL), Some("overseer::Quit"));
        assert_eq!(routed("cmd-q", DIALOG), Some("overseer::Quit"));
        assert_eq!(routed("left", BOARD), Some("overseer::SelectLeft"));
        assert_eq!(routed("enter", BOARD), Some("overseer::OpenSelected"));
        // Arrows and Enter in a terminal are Claude Code's.
        assert_eq!(routed("left", TERMINAL), None);
        assert_eq!(routed("enter", TERMINAL), None);
    }

    fn slots_with(running: &[SlotId]) -> Vec<SlotInfo> {
        let mut s = empty_slots();
        for &id in running {
            s[id as usize].state = SlotState::Running { pid: 1, since: 0 };
        }
        s
    }

    #[test]
    fn ticker_only_while_a_running_slot_is_visible() {
        assert!(!needs_tick(Screen::Board, &slots_with(&[])));
        assert!(needs_tick(Screen::Board, &slots_with(&[3])));
        assert!(needs_tick(Screen::Session(3), &slots_with(&[3])));
        assert!(!needs_tick(Screen::Session(2), &slots_with(&[3])));
        assert!(!needs_tick(Screen::Session(9), &slots_with(&[3])));
    }

    #[test]
    fn ticker_restarts_when_returning_to_the_board() {
        let s = slots_with(&[1]);
        let mut t = Ticker::default();
        assert!(t.on_change(Screen::Board, &s), "starts on the board");
        assert!(!t.on_change(Screen::Board, &s), "only one ticker");
        // An empty session hides every running slot, so the ticker ends.
        assert!(!t.on_tick(Screen::Session(2), &s));
        // Back on the board the running card is visible again.
        assert!(t.on_change(Screen::Board, &s));
        assert!(t.on_tick(Screen::Board, &s));
        // Nothing running anywhere: no ticker.
        let mut idle = Ticker::default();
        assert!(!idle.on_change(Screen::Board, &slots_with(&[])));
    }

    #[test]
    fn start_target_skips_running_slots() {
        let s = slots_with(&[1]);
        assert_eq!(start_target(Screen::Board, 5, &s), Some(5));
        assert_eq!(start_target(Screen::Board, 1, &s), None);
        assert_eq!(start_target(Screen::Session(1), 5, &s), None);
        assert_eq!(start_target(Screen::Session(4), 1, &s), Some(4));
    }

    #[test]
    fn card_clicks() {
        let s = slots_with(&[1]);
        assert_eq!(card_click(&s[1]), CardClick::Open(1));
        assert_eq!(card_click(&s[5]), CardClick::Start(5));
        let mut ex = s[2].clone();
        ex.state = SlotState::Exited { code: None, at: 0 };
        assert_eq!(card_click(&ex), CardClick::Open(2));
    }
}
