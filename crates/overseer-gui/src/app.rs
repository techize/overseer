//! The Overseer window: screen state, navigation, key bindings, live slot
//! updates and the per-slot terminal views.
//!
//! Navigation keys never take terminal input:
//! - `Cmd+0` to `Cmd+6` open a session and `Cmd+Esc` goes to the board, from
//!   anywhere except the start dialog and the picker. Cmd keys never reach
//!   the PTY anyway.
//! - Plain `Esc` goes to the board only when focus is in the session view
//!   but not in its terminal (after clicking the tab strip or the context
//!   panel). In the terminal, Esc closes the find bar when it is open and
//!   otherwise goes to Claude Code, which uses it to interrupt.
//! - Plain digits always go to whatever has focus.
//! - `a` on the board opens the work-item picker for the selected card.
//!
//! The routing is GPUI's keymap with the contexts `Overseer` (window),
//! `Board`, `Session`, `Terminal`, `StartDialog` and `Picker`;
//! `key_bindings` is tested against GPUI's `Keymap` directly.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui::{
    actions, div, prelude::*, rgb, App, Context, Entity, FocusHandle, Focusable, KeyBinding,
    Subscription, Task, Window,
};
use overseer_proto::{
    BrainView, Client, CoordView, Event, Frame, LockView, Request, Response, SlotId, SlotInfo,
    SlotState, WorkView,
};

use crate::board::Dir;
use crate::picker::{Picker, PickerEvent};
use crate::plan_view::{PlanEvent, PlanView};
use crate::session::{incarnation, keep_view, restart_begin, restart_on_change, Incarnation};
use crate::session::{reply_note, EntryAction, EntryKind, PanelEntry, RestartPlan};
use crate::start_dialog::{DialogEvent, Prefill, StartDialog};
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
        ResumeSelected,
        AssignSelected,
        OpenMessage,
        OpenRelease,
        OpenNote,
        OpenPlan,
    ]
);

/// What the window shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Board,
    Session(SlotId),
}

/// Navigation keys work anywhere in the window except the start dialog, the
/// picker and the plan view.
const NAV: &str = "Overseer && !StartDialog && !Picker && !PlanView";

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
        // Nor from an open Holds or Relays entry row, which takes Esc itself.
        KeyBinding::new(
            "escape",
            GoBoard,
            Some("Session && !Terminal && !PanelEntry"),
        ),
        // Cmd+Shift keys never reach Claude Code, so the terminal loses
        // nothing; plain m and r still go to it.
        KeyBinding::new("cmd-shift-m", OpenMessage, Some("Session")),
        KeyBinding::new("cmd-shift-r", OpenRelease, Some("Session")),
        // Plain n goes to the terminal; outside it (the panel has focus) it
        // opens the session note too.
        KeyBinding::new("cmd-shift-n", OpenNote, Some("Session")),
        KeyBinding::new("n", OpenNote, Some("Session && !Terminal && !PanelEntry")),
        KeyBinding::new("left", SelectLeft, Some("Board")),
        KeyBinding::new("right", SelectRight, Some("Board")),
        KeyBinding::new("up", SelectUp, Some("Board")),
        KeyBinding::new("down", SelectDown, Some("Board")),
        KeyBinding::new("enter", OpenSelected, Some("Board")),
        KeyBinding::new("r", ResumeSelected, Some("Board")),
        KeyBinding::new("a", AssignSelected, Some("Board")),
        KeyBinding::new("p", OpenPlan, Some("Board")),
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

/// The slot `r` resumes: the selected card, when it is resumable.
pub fn resume_target(selected: SlotId, slots: &[SlotInfo]) -> Option<SlotId> {
    slots
        .get(selected as usize)
        .filter(|i| matches!(i.state, SlotState::Resumable { .. }))
        .map(|i| i.id)
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

/// Whether assigning to this slot goes on to start it with the kickoff
/// ("Assign and start"): it has no session running.
pub fn assign_starts(info: &SlotInfo) -> bool {
    matches!(info.state, SlotState::Empty | SlotState::Exited { .. })
}

/// The keys a fresh kickoff for `slot` names, in card order: what the view
/// says it owns, plus `assigned` when the view has not caught up yet.
pub fn kickoff_keys(work: &WorkView, slot: SlotId, assigned: &str) -> Vec<String> {
    let mut keys: Vec<String> = crate::board::owned_by(work, slot)
        .into_iter()
        .map(|i| i.key.clone())
        .collect();
    if !keys.iter().any(|k| k == assigned) {
        keys.push(assigned.to_string());
    }
    keys
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// How long after the kickoff paste its Return is sent.
const KICKOFF_RETURN_DELAY: Duration = Duration::from_millis(100);

/// The work view before the daemon's first answer.
pub fn empty_work() -> WorkView {
    WorkView {
        items: Vec::new(),
        sources: Vec::new(),
        notices: Vec::new(),
    }
}

/// The lock view before the daemon's first answer.
pub fn empty_locks() -> LockView {
    LockView {
        claims: Vec::new(),
        waits: Vec::new(),
        relays: Vec::new(),
        flag_after_secs: 3600,
    }
}

/// The 2ndBrain view before the daemon's first answer.
pub fn empty_brain() -> BrainView {
    BrainView {
        on: false,
        entries: Vec::new(),
        last_backup: None,
        last_sync: None,
        push_problem: None,
        brain_dir: None,
    }
}

/// The coordinator view before the daemon's first answer.
pub fn empty_coord() -> CoordView {
    CoordView {
        summary: None,
        plans: Vec::new(),
        reports: Vec::new(),
        status_asked: Vec::new(),
    }
}

/// A plan record for tests in this crate.
#[cfg(test)]
pub(crate) fn test_plan(id: i64, state: overseer_proto::PlanState) -> overseer_proto::PlanRecord {
    overseer_proto::PlanRecord {
        id,
        at: 0,
        plan: overseer_proto::Plan {
            summary: String::new(),
            slots: Vec::new(),
        },
        state,
        decided_at: None,
        edited: false,
        warnings: Vec::new(),
        outcome: Vec::new(),
    }
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
            activity: None,
            session_note: None,
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
    /// Work items, owners, source health and notices from the daemon.
    pub(crate) work: WorkView,
    /// Claims, waits and relays from the daemon.
    pub(crate) locks: LockView,
    /// The 2ndBrain queue and push state, for the board's bottom strip.
    pub(crate) brain: BrainView,
    /// S0's summary, the plans and the timeline from the daemon.
    pub(crate) coord: CoordView,
    /// The push problem text last announced; see `push_problem_note`.
    notified_push: Option<String>,
    /// The strip's list shows while hovered, or until clicked away.
    pub(crate) brain_hover: bool,
    pub(crate) brain_pinned: bool,
    pub(crate) screen: Screen,
    pub(crate) selected: SlotId,
    pub(crate) home: Option<PathBuf>,
    /// Last error from a request, shown until the next action.
    pub(crate) status: Option<String>,
    /// The open Release confirm, Claim entry or Message entry of the panel.
    pub(crate) entry: Option<PanelEntry>,
    /// The daemon's last answer to an entry: (slot, is for Holds, text).
    pub(crate) entry_note: Option<(SlotId, bool, String)>,
    pub(crate) entry_focus: FocusHandle,
    terminals: HashMap<SlotId, TermEntry>,
    dialog: Option<(Entity<StartDialog>, Subscription)>,
    picker: Option<(Entity<Picker>, Subscription)>,
    /// The open plan approval view.
    plan_view: Option<(Entity<PlanView>, Subscription)>,
    /// Pickers closed with an assign on its way; see `PickerEvent::Late`.
    leaving: Vec<(Entity<Picker>, Subscription)>,
    restarts: HashMap<SlotId, RestartPlan>,
    /// Slots with a kickoff on its way; see `send_kickoff`.
    kickoffs: HashSet<SlotId>,
    /// Focus this slot's terminal as soon as it exists.
    focus_when_ready: Option<SlotId>,
    root_focus: FocusHandle,
    pub(crate) board_focus: FocusHandle,
    pub(crate) session_focus: FocusHandle,
    ticker: Ticker,
    _ticker: Option<Task<()>>,
    _events: Task<()>,
    _clicks: Task<()>,
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
        let mut clicks = crate::notify::clicks();
        let clicked = cx.spawn_in(window, async move |this, cx| {
            while let Some(click) = clicks.recv().await {
                let alive = this.update_in(cx, |this, window, cx| {
                    this.notification_clicked(click, window, cx)
                });
                if alive.is_err() {
                    return;
                }
            }
        });
        // The lists come back through the event stream (marked replies), so
        // they are applied in order with the events around them.
        let lister = client.clone();
        cx.spawn(async move |this, cx| {
            let mut result = lister.request_marked(Request::ListSlots, |_| true).await;
            if result.is_ok() {
                result = lister
                    .request_marked(Request::ListWorkItems, |_| true)
                    .await;
            }
            if result.is_ok() {
                result = lister.request_marked(Request::ListLocks, |_| true).await;
            }
            if result.is_ok() {
                result = lister.request_marked(Request::BrainStatus, |_| true).await;
            }
            if result.is_ok() {
                result = lister.request_marked(Request::CoordStatus, |_| true).await;
            }
            if let Err(e) = result {
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
            work: empty_work(),
            locks: empty_locks(),
            brain: empty_brain(),
            coord: empty_coord(),
            notified_push: None,
            brain_hover: false,
            brain_pinned: false,
            screen: Screen::Board,
            selected: 1,
            home: std::env::var_os("HOME").map(PathBuf::from),
            status: None,
            entry: None,
            entry_note: None,
            entry_focus: cx.focus_handle(),
            terminals: HashMap::new(),
            dialog: None,
            picker: None,
            plan_view: None,
            leaving: Vec::new(),
            restarts: HashMap::new(),
            kickoffs: HashSet::new(),
            focus_when_ready: None,
            root_focus: cx.focus_handle(),
            board_focus,
            session_focus: cx.focus_handle(),
            ticker: Ticker::default(),
            _ticker: None,
            _events: events,
            _clicks: clicked,
        }
    }

    fn on_frame(&mut self, frame: Frame, window: &mut Window, cx: &mut Context<Self>) {
        match frame {
            Frame::Event(Event::SlotChanged(info)) => self.apply_info(info, false, window, cx),
            // The GUI asks for the list once, right after connecting, so a
            // NeedsYou in it was already waiting and does not notify.
            Frame::Response {
                resp: Response::Slots(list),
                ..
            } => {
                for info in list {
                    self.apply_info(info, true, window, cx);
                }
            }
            // The list asked for on connecting is not news, so it never
            // notifies; a change does.
            Frame::Response {
                resp: Response::Locks(view),
                ..
            } => {
                self.locks = view;
                cx.notify();
            }
            Frame::Event(Event::LocksChanged(view)) => {
                for note in crate::notify::new_waits(&self.locks, &view) {
                    crate::notify::post(&note);
                }
                self.locks = view;
                cx.notify();
            }
            // A push problem waits until a push succeeds, so one already
            // there on connecting is announced too, once.
            Frame::Response {
                resp: Response::Brain(view),
                ..
            }
            | Frame::Event(Event::BrainChanged(view)) => {
                if let Some(note) = crate::notify::push_problem_note(
                    &mut self.notified_push,
                    view.push_problem.as_deref(),
                ) {
                    crate::notify::post(&note);
                }
                self.brain = view;
                cx.notify();
            }
            // The view asked for on connecting is not news: a plan already
            // waiting shows on the board but does not notify.
            Frame::Response {
                resp: Response::Coord(view),
                ..
            } => {
                self.coord_changed(view, cx);
            }
            Frame::Event(Event::CoordChanged(view)) => {
                if let Some(note) = crate::notify::plan_note(&self.coord, &view) {
                    crate::notify::post(&note);
                }
                self.coord_changed(view, cx);
            }
            Frame::Event(Event::WorkChanged(view))
            | Frame::Response {
                resp: Response::Work(view),
                ..
            } => {
                if let Some((p, _)) = &self.picker {
                    p.update(cx, |p, cx| p.set_view(view.clone(), cx));
                }
                self.work = view;
                cx.notify();
            }
            _ => {}
        }
    }

    fn apply_info(
        &mut self,
        info: SlotInfo,
        first_sync: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = info.id;
        if id as usize >= self.slots.len() {
            return;
        }
        if crate::notify::should_notify(self.slots.get(id as usize), &info, first_sync) {
            crate::notify::post(&crate::notify::note_for(&info));
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
        if self.screen == Screen::Session(id) && !self.modal_open() {
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
        // No process to attach to in either.
        if matches!(state, SlotState::Empty | SlotState::Resumable { .. }) {
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

    /// Relaunches a resumable slot with its session continued and opens it.
    /// The slot is still Resumable here, so `go_session` leaves the terminal
    /// to be focused once the Running event arrives.
    pub(crate) fn resume_slot(&mut self, id: SlotId, window: &mut Window, cx: &mut Context<Self>) {
        self.status = None;
        self.send(Request::ResumeSlot { id }, cx);
        self.go_session(id, window, cx);
    }

    /// Gives up the slot's hold on a work item. The new state comes back as
    /// a `WorkChanged` event.
    pub(crate) fn release_item(&mut self, key: String, cx: &mut Context<Self>) {
        self.status = None;
        self.send(Request::Release { key }, cx);
    }

    pub(crate) fn clear_notices(&mut self, slot: SlotId, cx: &mut Context<Self>) {
        self.status = None;
        self.send(Request::ClearNotices { slot }, cx);
    }

    pub(crate) fn kickoff_pending(&self, slot: SlotId) -> bool {
        self.kickoffs.contains(&slot)
    }

    /// Asks the daemon for the slot's kickoff prompt and pastes it into the
    /// session, then presses Return. Return goes a moment after the paste so
    /// it lands once the paste has closed instead of inside it. The button
    /// stays off until this is done.
    pub(crate) fn send_kickoff(&mut self, slot: SlotId, cx: &mut Context<Self>) {
        if !self.kickoffs.insert(slot) {
            return;
        }
        self.status = None;
        cx.notify();
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let reply = client.request(Request::Kickoff { slot }).await;
            let mut err = match reply {
                Ok(Response::Text { text }) => {
                    let modes = this
                        .update(cx, |this, cx| this.term_modes(slot, cx))
                        .unwrap_or_default();
                    let bytes = crate::session::kickoff_bytes(&text, &modes);
                    let mut typed = client.send_input(slot, &bytes).await;
                    if typed.is_ok() {
                        cx.background_executor().timer(KICKOFF_RETURN_DELAY).await;
                        typed = client.send_input(slot, b"\r").await;
                    }
                    typed
                        .err()
                        .map(|e| format!("{} ({e})", crate::terminal_view::LOST_DAEMON))
                }
                Ok(Response::Error { message }) => Some(message),
                Ok(other) => Some(format!("unexpected reply to kickoff: {other:?}")),
                Err(e) => Some(format!("{} ({e:#})", crate::terminal_view::LOST_DAEMON)),
            };
            let _ = this.update(cx, |this, cx| {
                this.kickoffs.remove(&slot);
                if let Some(e) = err.take() {
                    this.status = Some(e);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Opens `entry` in the panel and takes the keyboard for it.
    fn open_entry(&mut self, entry: PanelEntry, window: &mut Window, cx: &mut Context<Self>) {
        self.status = None;
        self.entry_note = None;
        self.entry = Some(entry);
        window.focus(&self.entry_focus);
        cx.notify();
    }

    fn close_entry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.entry = None;
        self.restore_focus(window, cx);
        cx.notify();
    }

    /// The Release confirm for hold `picked` of `slot`.
    pub(crate) fn open_release(
        &mut self,
        slot: SlotId,
        picked: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let targets: Vec<String> =
            crate::session::holds_rows(&self.locks, slot, 0, crate::board::local_hhmm)
                .into_iter()
                .map(|h| h.target)
                .collect();
        match PanelEntry::release(slot, targets, picked) {
            Some(entry) => self.open_entry(entry, window, cx),
            None => {
                self.entry_note = Some((slot, true, "Nothing held".into()));
                cx.notify();
            }
        }
    }

    pub(crate) fn open_claim(&mut self, slot: SlotId, window: &mut Window, cx: &mut Context<Self>) {
        self.open_entry(PanelEntry::claim(slot), window, cx);
    }

    pub(crate) fn open_message(
        &mut self,
        slot: SlotId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_entry(PanelEntry::message(slot), window, cx);
    }

    /// Cmd+Shift+R in the session view: release confirm for the first hold.
    pub(crate) fn open_release_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Screen::Session(id) = self.screen {
            self.open_release(id, 0, window, cx);
        }
    }

    /// `n` in the session view: opens the slot's session note in Obsidian.
    /// With no note yet it does nothing; the panel says so.
    pub(crate) fn open_note_key(&mut self) {
        let Screen::Session(id) = self.screen else {
            return;
        };
        let target =
            crate::session::note_target(&self.slots[id as usize], self.brain.brain_dir.as_deref());
        if let Some(url) = target {
            // Waited on off the UI thread so the child does not linger.
            std::thread::spawn(move || {
                match std::process::Command::new("open").arg(&url).status() {
                    Ok(_) => {}
                    Err(e) => tracing::warn!("could not open the session note: {e}"),
                }
            });
        }
    }

    pub(crate) fn open_message_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Screen::Session(id) = self.screen {
            self.open_message(id, window, cx);
        }
    }

    pub(crate) fn on_entry_key(
        &mut self,
        ev: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.entry.as_mut() else {
            return;
        };
        match entry.key(&ev.keystroke) {
            EntryAction::Ignored => return,
            EntryAction::Redraw => {}
            EntryAction::Cancel => self.close_entry(window, cx),
            EntryAction::Paste => {
                if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                    entry.paste(&text);
                }
            }
            EntryAction::Submit => self.submit_entry(window, cx),
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// Sends the entry's request. A request the entry cannot make stays
    /// open with the reason; otherwise the row closes and the daemon's
    /// answer (an error, `Queued`, a result) shows under the box.
    fn submit_entry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.entry.as_mut() else {
            return;
        };
        let req = match entry.request() {
            Ok(req) => req,
            Err(why) => {
                entry.error = Some(why);
                return;
            }
        };
        let (slot, holds) = (entry.slot, !matches!(entry.kind, EntryKind::Message));
        self.close_entry(window, cx);
        self.entry_note = None;
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let reply = client
                .request(req)
                .await
                .map_err(|e| format!("{} ({e:#})", crate::terminal_view::LOST_DAEMON));
            let _ = this.update(cx, |this, cx| {
                if let Some(text) = reply_note(&reply) {
                    this.entry_note = Some((slot, holds, text));
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The modes the slot's session set on its terminal (bracketed paste
    /// and so on). Defaults when no terminal view was made for it.
    fn term_modes(&self, slot: SlotId, cx: &App) -> overseer_term::TermModes {
        self.terminals
            .get(&slot)
            .map(|t| t.view.read(cx).term_modes())
            .unwrap_or_default()
    }

    /// The start dialog, the picker or the plan view is up; navigation waits for it.
    fn modal_open(&self) -> bool {
        self.dialog.is_some() || self.picker.is_some() || self.plan_view.is_some()
    }

    /// The work-item picker for `slot`: `a` on the board, `+ assign` on an
    /// empty card, or Assign in the session panel.
    pub(crate) fn open_picker(
        &mut self,
        slot: SlotId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.modal_open() {
            return;
        }
        self.status = None;
        let client = self.client.clone();
        let view = self.work.clone();
        let picker = cx.new(|cx| Picker::new(slot, client, view, cx));
        let sub = cx.subscribe_in(
            &picker,
            window,
            |this, picker, ev: &PickerEvent, window, cx| {
                match ev {
                    PickerEvent::Assigned { slot, key } => {
                        this.picker = None;
                        this.restore_focus(window, cx);
                        let start = this.slots.get(*slot as usize).is_some_and(assign_starts);
                        if start {
                            this.assign_and_start(*slot, key.clone(), window, cx);
                        }
                    }
                    PickerEvent::Closed => {
                        // Kept until its request is answered, for `Late`.
                        if let Some(open) = this.picker.take() {
                            if open.0.read(cx).busy() {
                                this.leaving.push(open);
                            }
                        }
                        this.restore_focus(window, cx);
                    }
                    PickerEvent::Late(line) => {
                        this.leaving.retain(|(p, _)| p != picker);
                        if !line.is_empty() {
                            this.status = Some(line.clone());
                        }
                    }
                }
                cx.notify();
            },
        );
        window.focus(&picker.focus_handle(cx));
        self.picker = Some((picker, sub));
        cx.notify();
    }

    /// After assigning to a slot with no session: fetch its kickoff and
    /// open the start dialog with it as the first prompt.
    fn assign_and_start(
        &mut self,
        slot: SlotId,
        key: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let reply = client.request(Request::Kickoff { slot }).await;
            let _ = this.update_in(cx, |this, window, cx| {
                match reply {
                    Ok(Response::Text { text }) => {
                        let keys = kickoff_keys(&this.work, slot, &key);
                        let prefill = Prefill {
                            label: keys.first().cloned().unwrap_or(key),
                            prompt: text,
                            keys,
                        };
                        // Something else opened meanwhile; the item stays
                        // assigned and Send kickoff still works later.
                        if !this.modal_open() {
                            this.open_dialog_with(slot, Some(prefill), window, cx);
                        }
                    }
                    Ok(Response::Error { message }) => this.status = Some(message),
                    Ok(other) => {
                        this.status = Some(format!("unexpected reply to kickoff: {other:?}"))
                    }
                    Err(e) => {
                        this.status = Some(format!("{} ({e:#})", crate::terminal_view::LOST_DAEMON))
                    }
                }
                cx.notify();
            });
        })
        .detach();
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
        self.entry = None;
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

    /// A notification was clicked: bring the window forward on that
    /// session. With the start dialog open only the window comes forward,
    /// as Cmd+digit does nothing there either.
    fn notification_clicked(
        &mut self,
        click: crate::notify::Click,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.activate(true);
        window.activate_window();
        if !self.modal_open() {
            match click {
                crate::notify::Click::Slot(id) => self.go_session(id, window, cx),
                crate::notify::Click::Board => self.go_board(window, cx),
                crate::notify::Click::Plan => {
                    self.go_board(window, cx);
                    self.open_plan(window, cx);
                }
            }
        }
    }

    /// The daemon's new coordinator view: keep it, and let an open plan
    /// view notice that its plan was decided elsewhere.
    fn coord_changed(&mut self, view: CoordView, cx: &mut Context<Self>) {
        if let Some((p, _)) = &self.plan_view {
            p.update(cx, |p, cx| p.coord_changed(&view, cx));
        }
        self.coord = view;
        cx.notify();
    }

    /// `p` on the board, and a clicked plan notice: the plan approval view
    /// for the plan waiting for Jonathan.
    pub(crate) fn open_plan(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        let Some(rec) = self.coord.pending_plan().cloned() else {
            self.status = Some("No plan is waiting".to_string());
            cx.notify();
            return;
        };
        self.status = None;
        let client = self.client.clone();
        let view = cx.new(|cx| PlanView::new(&rec, client, cx));
        let sub = cx.subscribe_in(&view, window, |this, _, ev: &PlanEvent, window, cx| {
            this.plan_view = None;
            if let PlanEvent::Decided(line) = ev {
                this.status = Some(line.clone());
            }
            this.restore_focus(window, cx);
            cx.notify();
        });
        window.focus(&view.focus_handle(cx));
        self.plan_view = Some((view, sub));
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
        self.open_dialog_with(id, None, window, cx);
    }

    fn open_dialog_with(
        &mut self,
        id: SlotId,
        prefill: Option<Prefill>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.status = None;
        let client = self.client.clone();
        // A stopped slot reopens with its last settings, to change them.
        let last = self
            .slots
            .get(id as usize)
            .filter(|i| matches!(i.state, SlotState::Exited { .. }));
        let dialog = cx.new(|cx| StartDialog::new(id, client, last, prefill, cx));
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
            .children(self.picker.as_ref().map(|(p, _)| p.clone()))
            .children(self.plan_view.as_ref().map(|(p, _)| p.clone()))
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
    const PICKER: &[&str] = &["Overseer", "Picker"];
    const PLAN: &[&str] = &["Overseer", "PlanView"];

    #[test]
    fn cmd_digits_navigate_from_everywhere_but_the_dialog() {
        assert_eq!(routed("cmd-3", TERMINAL), Some("overseer::GoS3"));
        assert_eq!(routed("cmd-0", BOARD), Some("overseer::GoS0"));
        assert_eq!(routed("cmd-6", CHROME), Some("overseer::GoS6"));
        assert_eq!(routed("cmd-1", DIALOG), None);
        assert_eq!(routed("cmd-1", PICKER), None);
    }

    #[test]
    fn cmd_enter_and_escape_route_to_plan_view() {
        // Neither is a global binding: both reach the view's own handler.
        assert_eq!(routed("cmd-enter", PLAN), None);
        assert_eq!(routed("escape", PLAN), None);
        for k in ["tab", "enter", "a", "p", "cmd-v"] {
            assert_eq!(routed(k, PLAN), None, "{k}");
        }
        assert_eq!(routed("cmd-q", PLAN), Some("overseer::Quit"));
    }

    #[test]
    fn cmd_digits_do_not_route_while_plan_view_open() {
        for d in ["cmd-0", "cmd-3", "cmd-6", "cmd-n", "cmd-escape"] {
            assert_eq!(routed(d, PLAN), None, "{d}");
        }
        assert_eq!(routed("cmd-3", BOARD), Some("overseer::GoS3"));
    }

    #[test]
    fn the_picker_keeps_its_keys() {
        // Typing, arrows, Enter and Esc go to the picker's own handler.
        for k in [
            "a",
            "r",
            "up",
            "down",
            "enter",
            "escape",
            "cmd-n",
            "cmd-escape",
        ] {
            assert_eq!(routed(k, PICKER), None, "{k}");
        }
        assert_eq!(routed("cmd-q", PICKER), Some("overseer::Quit"));
    }

    #[test]
    fn cmd_shift_m_and_r_open_the_entries_in_the_session_view_only() {
        const ENTRY: &[&str] = &["Overseer", "Session", "PanelEntry"];
        for ctx in [TERMINAL, CHROME] {
            assert_eq!(routed("cmd-shift-m", ctx), Some("overseer::OpenMessage"));
            assert_eq!(routed("cmd-shift-r", ctx), Some("overseer::OpenRelease"));
        }
        for ctx in [BOARD, DIALOG, PICKER] {
            assert_eq!(routed("cmd-shift-m", ctx), None);
            assert_eq!(routed("cmd-shift-r", ctx), None);
        }
        // Plain keys still go to the terminal, and the entry keeps Esc.
        for k in ["m", "r", "shift-m", "cmd-m", "cmd-r"] {
            assert_eq!(routed(k, TERMINAL), None, "{k}");
        }
        assert_eq!(routed("escape", ENTRY), None);
    }

    #[test]
    fn n_opens_the_note_but_never_steals_the_terminals_key() {
        const ENTRY: &[&str] = &["Overseer", "Session", "PanelEntry"];
        assert_eq!(routed("n", CHROME), Some("overseer::OpenNote"));
        assert_eq!(routed("n", TERMINAL), None);
        assert_eq!(routed("n", ENTRY), None);
        assert_eq!(routed("n", BOARD), None);
        assert_eq!(routed("cmd-shift-n", TERMINAL), Some("overseer::OpenNote"));
        assert_eq!(routed("cmd-shift-n", BOARD), None);
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
        assert_eq!(routed("r", BOARD), Some("overseer::ResumeSelected"));
        assert_eq!(routed("r", TERMINAL), None);
        assert_eq!(routed("a", BOARD), Some("overseer::AssignSelected"));
        assert_eq!(routed("a", TERMINAL), None);
        assert_eq!(routed("a", CHROME), None);
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
    fn r_resumes_only_a_selected_resumable_card() {
        let mut s = slots_with(&[1]);
        s[2].state = SlotState::Resumable {
            session_id: "abc".into(),
            at: 0,
        };
        assert_eq!(resume_target(2, &s), Some(2));
        assert_eq!(resume_target(1, &s), None);
        assert_eq!(resume_target(4, &s), None);
        assert_eq!(resume_target(9, &s), None);
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
    fn assign_starts_only_without_a_session() {
        let mut s = slots_with(&[1]);
        s[2].state = SlotState::Exited { code: None, at: 0 };
        s[3].state = SlotState::Resumable {
            session_id: "abc".into(),
            at: 0,
        };
        assert!(assign_starts(&s[5]), "empty");
        assert!(assign_starts(&s[2]), "exited");
        assert!(!assign_starts(&s[1]), "running");
        assert!(!assign_starts(&s[3]), "resumable has a session to resume");
    }

    #[test]
    fn kickoff_keys_include_the_new_item() {
        use overseer_proto::{ItemKind, WorkItem};
        let item = |key: &str, kind, owner| WorkItem {
            key: key.into(),
            kind,
            title: String::new(),
            status: "open".into(),
            priority: None,
            parent: None,
            owner,
            in_list: true,
            folder: None,
        };
        let mut work = empty_work();
        work.items = vec![
            item("ov-3", ItemKind::Bead, Some(4)),
            item("DEMO-12", ItemKind::Ticket, Some(4)),
            item("DEMO-40", ItemKind::Ticket, Some(2)),
        ];
        assert_eq!(kickoff_keys(&work, 4, "DEMO-12"), ["DEMO-12", "ov-3"]);
        assert_eq!(
            kickoff_keys(&work, 4, "ov-9"),
            ["DEMO-12", "ov-3", "ov-9"],
            "not in the view yet"
        );
        assert_eq!(kickoff_keys(&work, 5, "ov-9"), ["ov-9"]);
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
    #[test]
    fn p_routes_on_the_board_only() {
        assert_eq!(routed("p", BOARD), Some("overseer::OpenPlan"));
        for ctx in [TERMINAL, CHROME, DIALOG, PICKER] {
            assert_eq!(routed("p", ctx), None);
        }
    }
}
