//! The Overseer window: screen state, navigation, key bindings, live slot
//! updates and the per-slot terminal views.
//!
//! Navigation keys never take terminal input:
//! - `Cmd+0` to `Cmd+6` open a session's pop-over and `Cmd+Esc` closes it, from
//!   anywhere except the start dialog and the picker. Cmd keys never reach
//!   the PTY anyway.
//! - Plain `Esc` closes the pop-over only when focus is in the session view
//!   but not in its terminal (after clicking the tab strip or the context
//!   panel). In the terminal, Esc closes the find bar when it is open and
//!   otherwise goes to Claude Code, which uses it to interrupt.
//! - Plain digits always go to whatever has focus.
//! - `a` on the board opens the work-item picker for the selected card.
//! - `l` on the board and `Cmd+Shift+L` anywhere navigation works open the
//!   Locks view.
//! - `Cmd+,` and the app menu's Settings item open the Settings view
//!   anywhere navigation works.
//! - `Tab` on the board or in the conversation pane's reply box selects the
//!   next waiting slot; `Cmd+\` on the board or in the pane hides or shows
//!   the pane. Neither is bound anywhere else, so the dialogs keep Tab.
//!
//! The routing is GPUI's keymap with the contexts `Overseer` (window),
//! `Board`, `Conversation` (the pane beside the board), `ConvReply` (its
//! reply box), `Session`, `Terminal`, `Locks`, `StartDialog`, `Picker`,
//! `PlanView` and `Settings`;
//! `key_bindings` is tested against GPUI's `Keymap` directly.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui::{
    actions, div, prelude::*, px, rgb, App, Context, Entity, FocusHandle, Focusable, KeyBinding,
    Subscription, Task, Window,
};
use overseer_proto::msg::{Answer, ConversationView};
use overseer_proto::{
    AttentionView, BrainView, Client, CoordView, DayView, Event, Frame, LockView, Request,
    Response, SlotId, SlotInfo, SlotState, WorkView,
};

use crate::board::Dir;
use crate::conversation_view::{Conv, SLOT_COUNT};
use crate::daemon_info::DaemonKnown;
use crate::locks_view::LocksState;
use crate::picker::{Picker, PickerEvent};
use crate::plan_view::{PlanEvent, PlanView};
use crate::session::{
    detail_gate, detail_still_owned, fail_detail, reopen, take_reply, DetailGate, OpenDetail,
};
use crate::session::{incarnation, keep_view, restart_begin, restart_on_change, Incarnation};
use crate::session::{reply_note, EntryAction, EntryKind, PanelEntry, RestartPlan};
use crate::settings_view::{SettingsEvent, SettingsPanel};
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
    pub const PURPLE: u32 = 0xd2a8ff;
    pub const BUTTON: u32 = 0x21283a;
    pub const BUTTON_BORDER: u32 = 0x2f3850;
    pub const FIELD: u32 = 0x0b0e14;
    pub const TERM_EMPTY: u32 = 0x05070a;
}

/// Hands `target` (a URL or a path) to macOS `open`. `what` names it in
/// the log when that fails. Waited on off the UI thread so the child does
/// not linger.
pub(crate) fn open_external(target: impl Into<std::ffi::OsString>, what: &'static str) {
    let target = target.into();
    std::thread::spawn(move || {
        if let Err(e) = std::process::Command::new("open").arg(&target).status() {
            tracing::warn!("could not open {what}: {e}");
        }
    });
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
        GoLocks,
        OpenSettings,
        NextWaiting,
        TogglePane,
    ]
);

/// What the window shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Board,
    /// The board with slot id's session in a pop-over over it.
    Session(SlotId),
    /// Every held target and its queue; see `locks_view`.
    Locks,
}

/// Where Esc in the Locks view goes: back to the session it was opened
/// from, or to the board.
pub(crate) fn locks_return(from: Screen) -> Option<SlotId> {
    match from {
        Screen::Session(id) => Some(id),
        Screen::Board | Screen::Locks => None,
    }
}

/// Navigation keys work anywhere in the window except the start dialog, the
/// picker, the plan view and Settings.
const NAV: &str = "Overseer && !StartDialog && !Picker && !PlanView && !Settings";

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
        KeyBinding::new("cmd-shift-l", GoLocks, nav),
        KeyBinding::new("cmd-,", OpenSettings, nav),
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
        KeyBinding::new("l", GoLocks, Some("Board")),
        // The pane is beside the board, not in it, so its reply box gets
        // none of the board's plain keys; Tab is the one it shares.
        KeyBinding::new("tab", NextWaiting, Some("Board || ConvReply")),
        KeyBinding::new("cmd-\\", TogglePane, Some("Board || Conversation")),
        KeyBinding::new("cmd-q", Quit, None),
    ]
}

/// Whether the 1 s ticker is needed: a running slot's elapsed time is on
/// screen.
pub fn needs_tick(screen: Screen, slots: &[SlotInfo]) -> bool {
    let running = |s: &SlotInfo| matches!(s.state, SlotState::Running { .. });
    match screen {
        // The pop-over leaves the board visible behind it.
        Screen::Board | Screen::Session(_) => slots.iter().any(running),
        Screen::Locks => false,
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
/// a session. None when that slot is already running, and in the Locks
/// view, which shows no slot.
pub fn start_target(screen: Screen, selected: SlotId, slots: &[SlotInfo]) -> Option<SlotId> {
    let id = match screen {
        Screen::Board => selected,
        Screen::Session(id) => id,
        Screen::Locks => return None,
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

/// What clicking a ticket card does: open its owner's session (Running or
/// Resumable), start work in a slot, start it in an idle slot alongside the
/// work that slot already owns, or report that every slot has a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketClick {
    Open(SlotId),
    Start(SlotId),
    Join(SlotId),
    NoSlot,
}

/// The slot starting work on a ticket uses. With an owner: that owner when
/// it is Empty or Exited and not S0, else None (a Running or Resumable owner
/// is opened, never started over). With no owner: the lowest Empty or
/// Exited slot in S1 to S6 that owns no work, so a click piles a ticket onto
/// another ticket's slot only when it must; failing that, the lowest Empty
/// or Exited slot in S1 to S6, whose kickoff then names its other work too.
/// S0, Running and Resumable slots are never picked.
pub fn start_slot(owner: Option<SlotId>, slots: &[SlotInfo], work: &WorkView) -> Option<SlotId> {
    let free = |i: &&SlotInfo| matches!(i.state, SlotState::Empty | SlotState::Exited { .. });
    match owner {
        Some(o) => slots
            .get(o as usize)
            .filter(|i| i.id != 0)
            .filter(free)
            .map(|i| i.id),
        None => {
            let idle = || {
                slots
                    .iter()
                    .filter(|i| (1..=6).contains(&i.id))
                    .filter(free)
            };
            idle()
                .find(|i| crate::board::owned_by(work, i.id).is_empty())
                .or_else(|| idle().next())
                .map(|i| i.id)
        }
    }
}

/// A Running or Resumable owner opens: its pop-over offers Resume, and a
/// fresh start would throw that session away. An S0 owner opens too, since
/// the coordinator is never started from a ticket.
pub fn ticket_click(owner: Option<SlotId>, slots: &[SlotInfo], work: &WorkView) -> TicketClick {
    let open = owner.and_then(|o| slots.get(o as usize)).filter(|i| {
        i.id == 0
            || matches!(
                i.state,
                SlotState::Running { .. } | SlotState::Resumable { .. }
            )
    });
    if let Some(info) = open {
        return TicketClick::Open(info.id);
    }
    match start_slot(owner, slots, work) {
        Some(slot) if owner != Some(slot) && !crate::board::owned_by(work, slot).is_empty() => {
            TicketClick::Join(slot)
        }
        Some(slot) => TicketClick::Start(slot),
        None => TicketClick::NoSlot,
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
        notes_off: None,
        items: Vec::new(),
        sources: Vec::new(),
        notices: Vec::new(),
        sprint: None,
        prs: Vec::new(),
        sprint_day: None,
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
        remote: None,
        last_push: None,
        on: false,
        entries: Vec::new(),
        last_backup: None,
        last_sync: None,
        push_problem: None,
        brain_dir: None,
        notes_dir: None,
    }
}

/// Where a new session starts: the notes folder, whether or not the
/// writer is on, else the writer's folder (a daemon that predates
/// `notes_dir`).
pub fn dialog_notes_dir(v: &BrainView) -> Option<std::path::PathBuf> {
    v.notes_dir.clone().or_else(|| v.brain_dir.clone())
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

/// `slots` when a list reply on this connection filled them, else `None`:
/// the empty slots the app starts with, or stale ones after a lost
/// connection, would claim no session is running.
fn known_slots(known: bool, slots: &[SlotInfo]) -> Option<&[SlotInfo]> {
    known.then_some(slots)
}

/// The terminal theme and editor in the config file at `path`. A missing
/// file gives the defaults.
fn app_settings(path: &std::path::Path) -> Result<(Theme, String), String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let theme = Theme::from_toml(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((theme, crate::terminal_view::editor_from_toml(&text)))
}

pub(crate) fn empty_convs() -> Vec<Conv> {
    (0..SLOT_COUNT as SlotId).map(Conv::new).collect()
}

pub(crate) fn empty_slots() -> Vec<SlotInfo> {
    (0..SLOT_COUNT as SlotId)
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
            held: false,
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
    pub(crate) theme: Theme,
    pub(crate) slots: Vec<SlotInfo>,
    /// Work items, owners, source health and notices from the daemon.
    pub(crate) work: WorkView,
    /// Claims, waits and relays from the daemon.
    pub(crate) locks: LockView,
    /// The 2ndBrain queue and push state, for the board's bottom strip.
    pub(crate) brain: BrainView,
    /// S0's summary, the plans and the timeline from the daemon.
    pub(crate) coord: CoordView,
    /// What is waiting on you, from the daemon; see `attention_view`.
    pub(crate) attention: AttentionView,
    /// Today's sessions on a shared time axis, from the daemon; see
    /// `timeline_view`.
    pub(crate) timeline: DayView,
    /// Each slot's conversation, draft and picked answers, by slot id.
    pub(crate) convs: Vec<Conv>,
    /// The conversation pane beside the board is hidden; kept in `gui.toml`.
    pub(crate) pane_hidden: bool,
    /// The pane's reply box.
    pub(crate) reply_focus: FocusHandle,
    /// The pane's entries, kept at the bottom as they grow unless scrolled up.
    pub(crate) pane_scroll: gpui::ScrollHandle,
    /// The slot, session and entry count the pane last drew, so new entries
    /// can be told from a redraw.
    pub(crate) pane_seen: (SlotId, String, usize),
    /// The Today panel shows its bars, not just its one line.
    pub(crate) timeline_open: bool,
    /// The push problem text last announced; see `push_problem_note`.
    notified_push: Option<String>,
    /// The strip's list shows while hovered, or until clicked away.
    pub(crate) brain_hover: bool,
    pub(crate) brain_pinned: bool,
    /// The kanban's dragged height; `None` fills the space left.
    pub(crate) kanban_height: Option<f32>,
    /// While its edge is dragged: where the pointer went down and the
    /// kanban's height then.
    pub(crate) kanban_drag: Option<(f32, f32)>,
    /// The kanban's height as last laid out.
    pub(crate) kanban_measured: std::rc::Rc<std::cell::Cell<f32>>,
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
    /// The last detail request number, so a late reply can be told apart.
    detail_req: u64,
    /// The owned item whose detail the session panel shows instead of its
    /// boxes; see `open_detail`.
    pub(crate) detail: Option<OpenDetail>,
    terminals: HashMap<SlotId, TermEntry>,
    dialog: Option<(Entity<StartDialog>, Subscription)>,
    picker: Option<(Entity<Picker>, Subscription)>,
    /// The open plan approval view.
    plan_view: Option<(Entity<PlanView>, Subscription)>,
    /// Plan views closed with a decision still on its way, kept for the
    /// answer.
    plan_leaving: Vec<(Entity<PlanView>, Subscription)>,
    /// The open Settings view.
    settings: Option<(Entity<SettingsPanel>, Subscription)>,
    /// Settings views closed with a save on its way, kept for the answer.
    settings_leaving: Vec<(Entity<SettingsPanel>, Subscription)>,
    /// What the connected daemon said about itself; see `probe_daemon`.
    pub(crate) daemon: DaemonKnown,
    /// A build probe is on its way, so `daemon` may still change from
    /// `Unknown`.
    daemon_probing: bool,
    /// A detail click held until the probe answers: its `refresh`. See
    /// `open_detail`.
    detail_waiting: Option<bool>,
    /// A `ctl restart --yes` from Settings is running.
    restarting_daemon: bool,
    /// The slots came from a list reply on this connection. Until then,
    /// and after the connection is lost, `slots` may hide running sessions.
    slots_known: bool,
    /// A notification click that came before the slot list (L2): it
    /// opens its session once the list is in.
    pending_click: Option<crate::notify::Click>,
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
    /// The Locks view's selection and force-release confirm.
    pub(crate) locks_state: LocksState,
    pub(crate) locks_focus: FocusHandle,
    /// The session the Locks view was opened from, which Esc goes back to.
    pub(crate) locks_from: Option<SlotId>,
    ticker: Ticker,
    _ticker: Option<Task<()>>,
    _events: Task<()>,
    _clicks: Task<()>,
    /// A reconnect loop is running; see `connection_lost`.
    reconnecting: bool,
    /// A Refresh of the work sources is waiting for the daemon.
    pub(crate) refreshing_work: bool,
}

impl OverseerApp {
    pub fn new(
        client: Arc<Client>,
        theme: Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let events = Self::listen(window, cx);
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
        Self::ask_for_lists(client.clone(), cx);
        let board_focus = cx.focus_handle();
        window.focus(&board_focus);
        let mut app = OverseerApp {
            client,
            theme,
            slots: empty_slots(),
            work: empty_work(),
            locks: empty_locks(),
            brain: empty_brain(),
            coord: empty_coord(),
            attention: AttentionView::default(),
            timeline: DayView::default(),
            convs: empty_convs(),
            pane_hidden: crate::kanban_size::pane_hidden_from(&crate::kanban_size::path()),
            reply_focus: cx.focus_handle(),
            pane_scroll: gpui::ScrollHandle::new(),
            pane_seen: (SlotId::MAX, String::new(), 0),
            timeline_open: false,
            notified_push: None,
            brain_hover: false,
            brain_pinned: false,
            kanban_height: crate::kanban_size::load_from(&crate::kanban_size::path()),
            kanban_drag: None,
            kanban_measured: Default::default(),
            screen: Screen::Board,
            selected: 1,
            home: std::env::var_os("HOME").map(PathBuf::from),
            status: None,
            entry: None,
            entry_note: None,
            entry_focus: cx.focus_handle(),
            detail_req: 0,
            terminals: HashMap::new(),
            dialog: None,
            picker: None,
            plan_view: None,
            plan_leaving: Vec::new(),
            settings: None,
            settings_leaving: Vec::new(),
            daemon: DaemonKnown::Unknown,
            daemon_probing: false,
            detail_waiting: None,
            restarting_daemon: false,
            slots_known: false,
            pending_click: None,
            leaving: Vec::new(),
            restarts: HashMap::new(),
            kickoffs: HashSet::new(),
            detail: None,
            focus_when_ready: None,
            root_focus: cx.focus_handle(),
            board_focus,
            session_focus: cx.focus_handle(),
            locks_state: LocksState::default(),
            locks_from: None,
            locks_focus: cx.focus_handle(),
            ticker: Ticker::default(),
            _ticker: None,
            _events: events,
            _clicks: clicked,
            reconnecting: false,
            refreshing_work: false,
        };
        app.probe_daemon(cx);
        app.request_conversation(app.selected, cx);
        app
    }

    /// Reads the current frame hub's events into `on_frame`. When they end
    /// (the daemon went away) the app starts reconnecting.
    fn listen(window: &mut Window, cx: &mut Context<Self>) -> Task<()> {
        let mut frames = cx.global::<FrameHub>().take_events();
        cx.spawn_in(window, async move |this, cx| {
            let Some(frames) = frames.as_mut() else {
                return;
            };
            while let Some(frame) = frames.recv().await {
                let alive = this.update_in(cx, |this, window, cx| this.on_frame(frame, window, cx));
                if alive.is_err() {
                    return;
                }
            }
            let _ = this.update_in(cx, |this, window, cx| this.connection_lost(window, cx));
        })
    }

    /// Asks for every list. The answers come back through the event stream
    /// (marked replies), so they are applied in order with the events around
    /// them, and as a first sync: nothing in them notifies.
    fn ask_for_lists(lister: Arc<Client>, cx: &mut Context<Self>) {
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
            if result.is_ok() {
                result = lister.request_marked(Request::Attention, |_| true).await;
            }
            if result.is_ok() {
                result = lister.request_marked(Request::Timeline, |_| true).await;
            }
            if let Err(e) = result {
                let _ = this.update(cx, |this, cx| {
                    this.status = Some(format!("{} ({e:#})", crate::terminal_view::LOST_DAEMON));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Asks the daemon which build it runs, on a connection of its own (see
    /// `settings_view::probe_daemon`), and says when it differs from the
    /// app's.
    fn probe_daemon(&mut self, cx: &mut Context<Self>) {
        self.daemon_probing = true;
        let runtime = cx.global::<FrameHub>().runtime().clone();
        let socket = overseer_proto::paths::socket_path();
        cx.spawn(async move |this, cx| {
            // A probe that did not finish (the runtime is shutting down)
            // still ends the wait, as no answer.
            let known = runtime
                .spawn(crate::daemon_info::probe_daemon(socket))
                .await
                .unwrap_or(DaemonKnown::Unknown);
            let _ = this.update(cx, |this, cx| this.daemon_known(known, cx));
        })
        .detach();
    }

    /// Takes the probe's answer (`Unknown` when it timed out) and sends or
    /// refuses a detail click held for it, so the detail never stays on
    /// Loading.
    fn daemon_known(&mut self, known: DaemonKnown, cx: &mut Context<Self>) {
        self.daemon_probing = false;
        let notice = match &known {
            DaemonKnown::Info(info) => {
                overseer_proto::build_notice(overseer_proto::current_build(), &info.build)
            }
            DaemonKnown::Old => Some(crate::daemon_info::OLD_DAEMON_NOTICE.to_string()),
            DaemonKnown::Unknown => None,
        };
        if notice.is_some() {
            self.status = notice;
        }
        if let Some((s, _)) = &self.settings {
            s.update(cx, |s, cx| s.daemon_known(&known, cx));
        }
        self.daemon = known;
        if let Some(refresh) = self.detail_waiting.take() {
            if self.detail.is_some() {
                self.gate_detail(refresh, cx);
            }
        }
        cx.notify();
    }

    /// Shows that the daemon is gone and tries to reach it again, with a
    /// growing pause, until it answers.
    fn connection_lost(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.status = Some(crate::terminal_view::LOST_DAEMON.into());
        self.slots_known = false;
        self.forward_slots(cx);
        // The list is the daemon's; with it gone, nothing is known to wait.
        // Reconnecting asks for it again.
        self.attention = AttentionView::default();
        self.timeline = DayView::default();
        for conv in &mut self.convs {
            conv.clear();
        }
        crate::notify::set_dock_badge(0);
        cx.notify();
        if self.reconnecting {
            return;
        }
        self.reconnecting = true;
        let runtime = cx.global::<FrameHub>().runtime().clone();
        let socket = overseer_proto::paths::socket_path();
        cx.spawn_in(window, async move |this, cx| {
            let found = runtime.spawn(crate::reconnect::reconnect(socket)).await;
            let _ = this.update_in(cx, |this, window, cx| match found {
                Ok(client) => this.reconnected(Arc::new(client), runtime, window, cx),
                // The runtime is shutting down with the app.
                Err(_) => this.reconnecting = false,
            });
        })
        .detach();
    }

    /// Starts over on a new connection: a new frame hub, every list asked
    /// for again, new terminal views (the old ones read the dead
    /// connection), and the open dialogs on the new client.
    fn reconnected(
        &mut self,
        client: Arc<Client>,
        runtime: tokio::runtime::Handle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.reconnecting = false;
        cx.set_global(FrameHub::start(&client, runtime));
        self.client = client.clone();
        if let Some((d, _)) = &self.dialog {
            d.update(cx, |d, _| d.set_client(client.clone()));
        }
        if let Some((p, _)) = &self.picker {
            p.update(cx, |p, _| p.set_client(client.clone()));
        }
        if let Some((v, _)) = &self.plan_view {
            v.update(cx, |v, _| v.set_client(client.clone()));
        }
        if let Some((s, _)) = &self.settings {
            s.update(cx, |s, _| s.set_client(client.clone()));
        }
        // A new daemon may be another build; ask again.
        self.daemon = DaemonKnown::Unknown;
        self.probe_daemon(cx);
        // Requests in flight on the old connection have failed.
        self.restarts.clear();
        self.kickoffs.clear();
        let had_focus = self
            .terminals
            .values()
            .any(|t| t.view.focus_handle(cx).contains_focused(window, cx));
        self.terminals.clear();
        if let (Screen::Session(id), true) = (self.screen, had_focus) {
            self.focus_when_ready = Some(id);
        }
        let lost = crate::terminal_view::LOST_DAEMON;
        if self.status.as_deref().is_some_and(|s| s.starts_with(lost)) {
            self.status = None;
        }
        self._events = Self::listen(window, cx);
        Self::ask_for_lists(client, cx);
        self.request_conversation(self.selected, cx);
        cx.notify();
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
                self.slots_known = true;
                for info in list {
                    self.apply_info(info, true, window, cx);
                }
                self.forward_slots(cx);
                if let Some(click) = self.pending_click.take() {
                    self.notification_clicked(click, window, cx);
                }
            }
            // The list asked for on connecting is not news, so it never
            // notifies; a change does.
            Frame::Response {
                resp: Response::Locks(view),
                ..
            } => self.locks_changed(view, cx),
            Frame::Event(Event::LocksChanged(view)) => {
                for note in crate::notify::new_waits(&self.locks, &view) {
                    crate::notify::post(&note);
                }
                self.locks_changed(view, cx);
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
            // The list asked for on connecting and every change replace it
            // whole; the Dock badge follows.
            Frame::Event(Event::AttentionChanged(view))
            | Frame::Response {
                resp: Response::Attention(view),
                ..
            } => {
                crate::notify::set_dock_badge(view.count());
                self.attention = view;
                cx.notify();
            }
            // Today's timeline, asked for on connecting and resent whole when
            // it changes.
            Frame::Event(Event::TimelineChanged(view))
            | Frame::Response {
                resp: Response::Timeline(view),
                ..
            } => {
                self.timeline = view;
                cx.notify();
            }
            Frame::Event(Event::ConversationChanged(view)) => self.conversation_pushed(view, cx),
            Frame::Event(Event::WorkChanged(view))
            | Frame::Response {
                resp: Response::Work(view),
                ..
            } => {
                if let Some((p, _)) = &self.picker {
                    p.update(cx, |p, cx| p.set_view(view.clone(), cx));
                }
                self.work = view;
                if self
                    .detail
                    .as_ref()
                    .is_some_and(|d| !detail_still_owned(d, &self.work))
                {
                    self.detail = None;
                }
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
        self.forward_slots(cx);
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

    /// Reads every work source now (the header's Refresh). The new items
    /// come back as `WorkChanged` events, one per source as it finishes;
    /// the reply only ends the wait, or puts a failure in the status line.
    pub(crate) fn refresh_work(&mut self, cx: &mut Context<Self>) {
        if self.refreshing_work {
            return;
        }
        self.refreshing_work = true;
        self.status = None;
        cx.notify();
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let result = client.request(Request::RefreshWorkItems).await;
            let _ = this.update(cx, |this, cx| {
                this.refreshing_work = false;
                match result {
                    Ok(Response::Error { message }) => this.status = Some(message),
                    Err(e) => {
                        this.status = Some(format!("{} ({e:#})", crate::terminal_view::LOST_DAEMON))
                    }
                    Ok(_) => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Selects `id`, and asks for its conversation when that is a change.
    pub(crate) fn select(&mut self, id: SlotId, cx: &mut Context<Self>) {
        if self.selected != id {
            self.selected = id;
            self.request_conversation(id, cx);
        }
    }

    /// Takes in a slot's conversation the daemon pushed. When it does not
    /// fit what the slot holds, asks for the whole of it again.
    fn conversation_pushed(&mut self, view: ConversationView, cx: &mut Context<Self>) {
        let slot = view.slot;
        let Some(conv) = self.convs.get_mut(slot as usize) else {
            return;
        };
        if !conv.pushed(view) {
            self.request_conversation(slot, cx);
        }
        cx.notify();
    }

    /// Asks the daemon for `slot`'s whole conversation. An error reply is
    /// left alone: a slot with nothing running has no conversation to show.
    /// A reply that an event pushed meanwhile may have overtaken is dropped
    /// and asked for again; one that does not fit is asked for again too.
    pub(crate) fn request_conversation(&mut self, slot: SlotId, cx: &mut Context<Self>) {
        let Some(asked) = self.convs.get(slot as usize).map(Conv::asked) else {
            return;
        };
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let reply = client.request(Request::Conversation { slot }).await;
            let _ = this.update(cx, |this, cx| match reply {
                Ok(Response::Conversation(view)) => {
                    let Some(conv) = this.convs.get_mut(slot as usize) else {
                        return;
                    };
                    if conv.answered(asked, view) != Some(true) {
                        this.request_conversation(slot, cx);
                    }
                    cx.notify();
                }
                Ok(_) => {}
                Err(e) => {
                    this.status = Some(format!("{} ({e:#})", crate::terminal_view::LOST_DAEMON));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Sends `slot`'s draft to its session. The draft is cleared once the
    /// daemon says `Ok`, unless it was edited meanwhile; a refusal such as
    /// `S2 moved on, not sent` shows in the slot's status.
    pub(crate) fn send_reply(&mut self, slot: SlotId, cx: &mut Context<Self>) {
        let Some(conv) = self.convs.get_mut(slot as usize) else {
            return;
        };
        let Some(req) = conv.reply_request() else {
            cx.notify();
            return;
        };
        let sent = conv.draft.text.clone();
        cx.notify();
        self.send_conv(slot, req, Some(sent), cx);
    }

    /// Answers `slot`'s open prompt, if it has one and nothing else is on
    /// its way.
    pub(crate) fn send_answer(&mut self, slot: SlotId, answer: Answer, cx: &mut Context<Self>) {
        let Some(conv) = self.convs.get_mut(slot as usize) else {
            return;
        };
        if let Some(req) = conv.answer_request(answer) {
            conv.status = None;
            self.send_conv(slot, req, None, cx);
        }
    }

    /// Interrupts `slot`'s session.
    pub(crate) fn interrupt(&mut self, slot: SlotId, cx: &mut Context<Self>) {
        if let Some(conv) = self.convs.get_mut(slot as usize) {
            conv.status = None;
            self.send_conv(slot, Request::Interrupt { slot }, None, cx);
        }
    }

    /// Sends a conversation request; the slot's status shows how it went
    /// wrong. `sent` is the draft this request carries. A Reply or Answer
    /// holds the slot's next one until it is answered.
    fn send_conv(
        &mut self,
        slot: SlotId,
        req: Request,
        sent: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let holds = matches!(req, Request::Reply { .. } | Request::Answer { .. });
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let reply = client.request(req).await;
            let _ = this.update(cx, |this, cx| {
                let Some(conv) = this.convs.get_mut(slot as usize) else {
                    return;
                };
                if holds {
                    conv.sending = false;
                }
                match reply {
                    Ok(Response::Ok) => {
                        if sent.as_ref().is_some_and(|s| *s == conv.draft.text) {
                            conv.draft = Default::default();
                        }
                    }
                    Ok(Response::Error { message }) => conv.status = Some(message),
                    Ok(other) => conv.status = Some(format!("unexpected reply: {other:?}")),
                    Err(e) => {
                        conv.status = Some(format!("{} ({e:#})", crate::terminal_view::LOST_DAEMON))
                    }
                }
                cx.notify();
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

    /// Marks a pull request's comments and build as seen. Its attention
    /// item goes when the daemon sends the list again, not here.
    pub(crate) fn pr_seen(&mut self, repo: String, id: u64, cx: &mut Context<Self>) {
        self.send(Request::PrSeen { repo, id }, cx);
    }

    /// Drops pending relay `id` unsent, as the user. The new state comes
    /// back as a `LocksChanged` event; a refusal shows in the status line.
    pub(crate) fn drop_relay(&mut self, id: i64, cx: &mut Context<Self>) {
        self.status = None;
        self.send(
            Request::DropRelays {
                from: None,
                id: Some(id),
                to: None,
            },
            cx,
        );
    }

    /// Shows `key`'s detail in `slot`'s panel and asks the daemon for it;
    /// `refresh` skips the daemon's cache and keeps a loaded detail shown
    /// until the reply. The cached fields show at once.
    pub(crate) fn open_detail(
        &mut self,
        slot: SlotId,
        key: String,
        refresh: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.detail_req += 1;
        // The hidden Claim/Message entry must not keep the keyboard focus.
        self.entry = None;
        window.focus(&self.session_focus);
        self.detail = Some(reopen(
            self.detail.as_ref(),
            slot,
            &key,
            self.detail_req,
            refresh,
        ));
        self.detail_waiting = None;
        self.gate_detail(refresh, cx);
        cx.notify();
    }

    /// Sends the open detail's request, holds it for the build probe, or
    /// says why it cannot be sent (an older daemon hangs up on a request it
    /// cannot decode).
    fn gate_detail(&mut self, refresh: bool, cx: &mut Context<Self>) {
        match detail_gate(
            &self.daemon,
            self.daemon_probing,
            overseer_proto::current_build(),
        ) {
            DetailGate::Send => self.send_detail(refresh, cx),
            DetailGate::Wait => self.detail_waiting = Some(refresh),
            DetailGate::Refuse(why) => {
                if let Some(d) = self.detail.as_mut() {
                    fail_detail(d, why);
                }
            }
        }
    }

    /// Asks the daemon for the open detail; the reply goes through
    /// `take_reply`, so one for a detail since closed or reopened is dropped.
    fn send_detail(&mut self, refresh: bool, cx: &mut Context<Self>) {
        let Some(open) = self.detail.as_ref() else {
            return;
        };
        let key = open.key.clone();
        let req_no = open.req;
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let req = Request::GetItemDetail {
                key: key.clone(),
                refresh,
            };
            let resp = match client.request(req).await {
                Ok(Response::ItemDetail(d)) => Ok(d),
                Ok(Response::Error { message }) => Err(message),
                Ok(other) => Err(format!("unexpected reply to the detail request: {other:?}")),
                Err(e) => Err(format!("{} ({e:#})", crate::terminal_view::LOST_DAEMON)),
            };
            let _ = this.update(cx, |this, cx| {
                take_reply(&mut this.detail, &key, req_no, resp);
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn close_detail(&mut self, cx: &mut Context<Self>) {
        self.detail = None;
        self.detail_waiting = None;
        cx.notify();
    }

    /// Shows the whole body, or back to the first lines.
    pub(crate) fn toggle_detail_all(&mut self, cx: &mut Context<Self>) {
        if let Some(d) = self.detail.as_mut() {
            d.show_all = !d.show_all;
            cx.notify();
        }
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
            open_external(url, "the session note");
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

    /// The start dialog, the picker, the plan view or Settings is up;
    /// navigation waits for it.
    pub(crate) fn modal_open(&self) -> bool {
        self.dialog.is_some()
            || self.picker.is_some()
            || self.plan_view.is_some()
            || self.settings.is_some()
    }

    /// The slots for the Settings view's restart question, or `None` when
    /// this connection has not listed them.
    fn known_slots(&self) -> Option<&[SlotInfo]> {
        known_slots(self.slots_known, &self.slots)
    }

    fn forward_slots(&mut self, cx: &mut Context<Self>) {
        if let Some((s, _)) = &self.settings {
            let slots = known_slots(self.slots_known, &self.slots);
            s.update(cx, |s, cx| s.set_slots(slots, cx));
        }
    }

    /// `Cmd+,` and the app menu: the Settings view. With a daemon that
    /// predates Settings it opens without asking, since such a daemon hangs
    /// up on a request it does not know.
    pub(crate) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        self.status = None;
        let client = self.client.clone();
        let slots = self.known_slots().map(<[SlotInfo]>::to_vec);
        let old = self.daemon == DaemonKnown::Old;
        let panel = cx.new(|cx| SettingsPanel::new(client, slots, old, cx));
        let sub = cx.subscribe_in(
            &panel,
            window,
            |this, panel, ev: &SettingsEvent, window, cx| {
                match ev {
                    SettingsEvent::Saved { text, app_changed } => {
                        this.settings_leaving.retain(|(p, _)| p != panel);
                        this.status = Some(text.clone());
                        if *app_changed {
                            let path = panel.read(cx).config_path();
                            this.reload_theme(path, cx);
                        }
                    }
                    SettingsEvent::Closed => {
                        let front = this.settings.take_if(|(p, _)| p == panel);
                        match front {
                            // Kept until its save is answered.
                            Some(open) => {
                                if open.0.read(cx).busy() {
                                    this.settings_leaving.push(open);
                                }
                                this.restore_focus(window, cx);
                            }
                            // A closed view's save failed.
                            None => {
                                this.settings_leaving.retain(|(p, _)| p != panel);
                                if let Some(e) = panel.read(cx).error() {
                                    this.status = Some(format!("Settings not saved: {e}"));
                                }
                            }
                        }
                    }
                    SettingsEvent::Restart => this.restart_daemon(cx),
                }
                cx.notify();
            },
        );
        window.focus(&panel.focus_handle(cx));
        self.settings = Some((panel, sub));
        cx.notify();
    }

    /// Terminal or editor settings were saved: every terminal view takes
    /// them up at once (Decision 9), and new ones start with them.
    /// The file is the one the view saved to; a broken one leaves the
    /// terminals as they are and says why on the status line.
    fn reload_theme(&mut self, path: Option<PathBuf>, cx: &mut Context<Self>) {
        let Some(path) = path.or_else(Theme::config_path) else {
            return;
        };
        let (theme, editor) = match app_settings(&path) {
            Ok(both) => both,
            Err(e) => {
                self.status = Some(format!("Saved, but the terminals kept their settings: {e}"));
                return;
            }
        };
        self.theme = theme.clone();
        for t in self.terminals.values() {
            t.view.update(cx, |v, cx| {
                v.set_editor(editor.clone());
                v.set_theme(theme.clone(), cx);
            });
        }
    }

    /// Settings' Restart the daemon, confirmed: runs `<this binary> ctl
    /// restart --yes` and says how it went. The reconnect brings the app
    /// back on the new daemon by itself.
    fn restart_daemon(&mut self, cx: &mut Context<Self>) {
        if self.restarting_daemon {
            self.status = Some("The daemon is already restarting.".to_string());
            return;
        }
        self.restarting_daemon = true;
        self.status = Some("Restarting the daemon".to_string());
        let exe = std::env::current_exe();
        cx.spawn(async move |this, cx| {
            let text = cx
                .background_executor()
                .spawn(async move { crate::daemon_info::run_restart(exe) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.restarting_daemon = false;
                if let Some((s, _)) = &this.settings {
                    s.update(cx, |s, cx| s.restart_done(&text, cx));
                }
                this.status = Some(text);
                cx.notify();
            });
        })
        .detach();
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

    /// A click on a ticket card that is not Running anywhere: assign the
    /// ticket to `start_slot` (unless that slot owns it already), then fetch
    /// the kickoff and open the start dialog, as the picker does.
    pub(crate) fn start_ticket(
        &mut self,
        key: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.modal_open() {
            return;
        }
        let owner = self
            .work
            .items
            .iter()
            .find(|i| i.key == key)
            .and_then(|i| i.owner);
        let Some(slot) = start_slot(owner, &self.slots, &self.work) else {
            self.status = Some(format!(
                "No free slot for {key}: S1 to S6 all have sessions. Stop one, or assign {key} from a slot's card."
            ));
            cx.notify();
            return;
        };
        self.status = None;
        if owner == Some(slot) {
            self.assign_and_start(slot, key, window, cx);
            return;
        }
        let client = self.client.clone();
        let req = Request::Assign {
            key: key.clone(),
            slot,
            // A daemon warning (beads under the ticket owned by other slots)
            // goes to the status line instead of being confirmed silently.
            confirm: false,
            move_owner: false,
        };
        cx.spawn_in(window, async move |this, cx| {
            let reply = client.request(req).await;
            let _ = this.update_in(cx, |this, window, cx| {
                match reply {
                    // The slot may have started while the Assign was out.
                    Ok(Response::Ok)
                        if this.slots.get(slot as usize).is_some_and(assign_starts) =>
                    {
                        this.assign_and_start(slot, key, window, cx)
                    }
                    Ok(Response::Ok) => {
                        this.status = Some(format!(
                            "{key} assigned to {}, which started meanwhile.",
                            overseer_proto::slot_name(slot)
                        ))
                    }
                    Ok(Response::NeedsConfirm { message }) | Ok(Response::Error { message }) => {
                        this.status = Some(message)
                    }
                    Ok(other) => {
                        this.status = Some(format!("unexpected reply to assign: {other:?}"))
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

    /// A click on a ticket card that is not Done.
    pub(crate) fn click_ticket(
        &mut self,
        key: String,
        owner: Option<SlotId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Before the first slot list every slot looks Empty.
        if !self.slots_known {
            return;
        }
        match ticket_click(owner, &self.slots, &self.work) {
            TicketClick::Open(id) => {
                if !self.modal_open() {
                    self.open_card(id, window, cx);
                }
            }
            TicketClick::Start(_) | TicketClick::Join(_) | TicketClick::NoSlot => {
                self.start_ticket(key, window, cx)
            }
        }
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
    /// Leaving the Locks view forgets its selection and any open confirm, so
    /// it opens fresh next time.
    fn set_screen(&mut self, screen: Screen, cx: &mut Context<Self>) {
        if self.screen == Screen::Locks && screen != Screen::Locks {
            self.locks_state = LocksState::default();
        }
        if self
            .detail
            .as_ref()
            .is_some_and(|d| screen != Screen::Session(d.slot))
        {
            self.detail = None;
        }
        self.screen = screen;
        crate::notify::set_viewing(match screen {
            Screen::Session(id) => Some(id),
            _ => None,
        });
        self.entry = None;
        self.ensure_ticker(cx);
    }

    /// Sets the kanban's height (`None` fills) and saves it; a failed save
    /// only costs the height on the next start, so it goes to the status line.
    pub(crate) fn set_kanban_height(&mut self, height: Option<f32>) {
        self.kanban_height = height;
        if let Err(e) = crate::kanban_size::save_to(&crate::kanban_size::path(), height) {
            self.status = Some(format!("cannot save the kanban height: {e}"));
        }
    }

    /// The pointer moved while the kanban's edge is held: resize it. A move
    /// with the button already up (released outside the window) ends the drag.
    pub(crate) fn drag_kanban(
        &mut self,
        ev: &gpui::MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((start_y, start_h)) = self.kanban_drag else {
            return;
        };
        if !ev.dragging() {
            self.end_kanban_drag(cx);
            return;
        }
        let max: f32 = window.viewport_size().height.into();
        let y: f32 = ev.position.y.into();
        self.kanban_height = Some(crate::kanban_size::dragged(start_h, start_y, y, max));
        cx.notify();
    }

    /// The edge was let go: keep the height the kanban actually got, so a
    /// height the window could not fit is not saved.
    pub(crate) fn end_kanban_drag(&mut self, cx: &mut Context<Self>) {
        if self.kanban_drag.take().is_some() {
            let got = self.kanban_measured.get();
            if got > 0. {
                self.set_kanban_height(Some(got));
            }
            cx.notify();
        }
    }

    pub(crate) fn go_board(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Screen::Session(id) = self.screen {
            self.select(id, cx);
        }
        self.set_screen(Screen::Board, cx);
        self.focus_when_ready = None;
        window.focus(&self.board_focus);
        cx.notify();
    }

    pub(crate) fn go_session(&mut self, id: SlotId, window: &mut Window, cx: &mut Context<Self>) {
        self.set_screen(Screen::Session(id), cx);
        self.select(id, cx);
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
        if !self.slots_known {
            self.pending_click = Some(click);
            return;
        }
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

    /// The daemon's new lock view: keep it, and let an open force-release
    /// confirm notice that its target changed hands.
    fn locks_changed(&mut self, view: LockView, cx: &mut Context<Self>) {
        self.locks = view;
        let rows = self.lock_rows_now(now_secs());
        if let Some(text) = self.locks_state.on_change(&rows) {
            self.status = Some(text);
        }
        cx.notify();
    }

    /// `l` on the board, `Cmd+Shift+L` and a click on the board's lock
    /// strip: the Locks view.
    pub(crate) fn go_locks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        self.status = None;
        if self.screen != Screen::Locks {
            self.locks_from = locks_return(self.screen);
        }
        self.set_screen(Screen::Locks, cx);
        self.focus_when_ready = None;
        window.focus(&self.locks_focus);
        cx.notify();
    }

    /// Sends the Locks view's force release; the daemon's reply (who holds
    /// the target now, or why nothing was released) goes to the status line
    /// as it is.
    pub(crate) fn force_release(&mut self, req: Request, cx: &mut Context<Self>) {
        self.status = None;
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let reply = client
                .request(req)
                .await
                .map_err(|e| format!("{} ({e:#})", crate::terminal_view::LOST_DAEMON));
            let _ = this.update(cx, |this, cx| {
                this.status = reply_note(&reply);
                cx.notify();
            });
        })
        .detach();
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

    /// `p` on the board, a click on its plan line and a clicked plan notice:
    /// the plan approval view for the plan waiting for Jonathan.
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
        let sub = cx.subscribe_in(&view, window, |this, view, ev: &PlanEvent, window, cx| {
            let open = this.plan_view.take_if(|(v, _)| v == view);
            let was_front = open.is_some();
            match ev {
                PlanEvent::Closed => {
                    // Kept until its decision is answered.
                    if let Some(open) = open {
                        if open.0.read(cx).busy() {
                            this.plan_leaving.push(open);
                        }
                    }
                }
                PlanEvent::Decided(line) => {
                    this.plan_leaving.retain(|(v, _)| v != view);
                    this.status = Some(line.clone());
                }
            }
            if was_front {
                this.restore_focus(window, cx);
            }
            cx.notify();
        });
        window.focus(&view.focus_handle(cx));
        self.plan_view = Some((view, sub));
        cx.notify();
    }

    pub(crate) fn open_card(&mut self, id: SlotId, window: &mut Window, cx: &mut Context<Self>) {
        self.select(id, cx);
        match card_click(&self.slots[id as usize]) {
            CardClick::Open(id) => self.go_session(id, window, cx),
            CardClick::Start(id) => self.open_dialog(id, window, cx),
        }
    }

    pub(crate) fn move_selection(&mut self, dir: Dir, cx: &mut Context<Self>) {
        self.select(crate::board::move_selection(self.selected, dir), cx);
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
        let notes_dir = dialog_notes_dir(&self.brain);
        let dialog =
            cx.new(|cx| StartDialog::new(id, client, last, prefill, notes_dir.as_deref(), cx));
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
            Screen::Locks => window.focus(&self.locks_focus),
        }
    }
}

impl Render for OverseerApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = now_secs();
        let screen = match self.screen {
            Screen::Board | Screen::Session(_) if self.pane_hidden => {
                self.render_board(now, window, cx).into_any_element()
            }
            // The pane beside the board, not inside it, so the reply box's
            // key context stack has no Board.
            Screen::Board | Screen::Session(_) => div()
                .size_full()
                .flex()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .h_full()
                        .child(self.render_board(now, window, cx)),
                )
                .child(self.render_conversation(window, cx))
                .into_any_element(),
            Screen::Locks => self.render_locks(now, window, cx).into_any_element(),
        };
        // Beside the board, never inside it, so the terminal's key context
        // stack has no Board and the board's keys stay out of the session.
        let popover = match self.screen {
            Screen::Session(id) => Some(self.render_popover(id, now, window, cx)),
            _ => None,
        };
        div()
            .key_context("Overseer")
            .track_focus(&self.root_focus)
            .on_action(cx.listener(|this, _: &GoBoard, window, cx| this.go_board(window, cx)))
            .on_action(cx.listener(|this, _: &GoLocks, window, cx| this.go_locks(window, cx)))
            .on_action(
                cx.listener(|this, _: &OpenSettings, window, cx| this.open_settings(window, cx)),
            )
            .on_action(cx.listener(|this, _: &GoS0, w, cx| this.go_session(0, w, cx)))
            .on_action(cx.listener(|this, _: &GoS1, w, cx| this.go_session(1, w, cx)))
            .on_action(cx.listener(|this, _: &GoS2, w, cx| this.go_session(2, w, cx)))
            .on_action(cx.listener(|this, _: &GoS3, w, cx| this.go_session(3, w, cx)))
            .on_action(cx.listener(|this, _: &GoS4, w, cx| this.go_session(4, w, cx)))
            .on_action(cx.listener(|this, _: &GoS5, w, cx| this.go_session(5, w, cx)))
            .on_action(cx.listener(|this, _: &GoS6, w, cx| this.go_session(6, w, cx)))
            .on_action(cx.listener(|this, _: &NextWaiting, _, cx| this.next_waiting_slot(cx)))
            .on_action(cx.listener(|this, _: &TogglePane, window, cx| this.toggle_pane(window, cx)))
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
            .children(popover)
            .children(self.dialog.as_ref().map(|(d, _)| d.clone()))
            .children(self.picker.as_ref().map(|(p, _)| p.clone()))
            .children(self.plan_view.as_ref().map(|(p, _)| p.clone()))
            .children(self.settings.as_ref().map(|(s, _)| s.clone()))
    }
}

/// Registers the key bindings, the Quit action and the app menu (Settings
/// and Quit).
pub fn install(cx: &mut App) {
    cx.bind_keys(key_bindings());
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.set_menus(vec![gpui::Menu {
        name: "Overseer".into(),
        items: vec![
            gpui::MenuItem::action("Settings\u{2026}", OpenSettings),
            gpui::MenuItem::separator(),
            gpui::MenuItem::action("Quit Overseer", Quit),
        ],
    }]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{KeyContext, Keymap, Keystroke};

    #[test]
    fn a_new_session_starts_in_the_notes_folder_with_the_writer_off() {
        use crate::start_dialog::{initial_fields, FALLBACK_CWD};
        let home = Some(std::path::Path::new("/Users/demo"));
        // Writer off, vault present: the daemon still names the folder.
        let off = BrainView {
            notes_dir: Some("/Users/demo/Repos/2ndBrain".into()),
            ..empty_brain()
        };
        let dir = dialog_notes_dir(&off);
        assert_eq!(
            initial_fields(0, None, home, dir.as_deref())[1],
            "~/Repos/2ndBrain"
        );
        // A daemon that predates `notes_dir` still gives the writer's folder.
        let old = BrainView {
            on: true,
            brain_dir: Some("/Users/demo/notes".into()),
            ..empty_brain()
        };
        assert_eq!(dialog_notes_dir(&old), Some("/Users/demo/notes".into()));
        // Neither: home.
        let none = dialog_notes_dir(&empty_brain());
        assert_eq!(
            initial_fields(0, None, home, none.as_deref())[1],
            FALLBACK_CWD
        );
    }

    #[test]
    fn slots_before_the_first_list_are_unknown() {
        let start = empty_slots();
        assert_eq!(known_slots(false, &start), None);
        assert_eq!(
            crate::settings_view::restart_confirm(known_slots(false, &start)),
            crate::settings_view::RESTART_UNLISTED
        );
        assert_eq!(known_slots(true, &start).map(<[SlotInfo]>::len), Some(7));
    }

    #[test]
    fn saved_app_settings_reload_from_the_saved_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        // Missing: the defaults.
        let (theme, _) = app_settings(&path).unwrap();
        assert_eq!(theme.font_size, Theme::default().font_size);
        std::fs::write(&path, "[terminal]\nfont_size = 17.0\n").unwrap();
        let (theme, _) = app_settings(&path).unwrap();
        assert_eq!(theme.font_size, 17.0);
        std::fs::write(&path, "[terminal\n").unwrap();
        let e = app_settings(&path).unwrap_err();
        assert!(e.contains("config.toml"), "{e}");
    }

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
    const SETTINGS: &[&str] = &["Overseer", "Settings"];

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
        // Cmd+A (select all) too: no binding takes it from the text fields.
        for k in ["tab", "enter", "a", "p", "cmd-v", "cmd-a"] {
            assert_eq!(routed(k, PLAN), None, "{k}");
            assert_eq!(routed(k, DIALOG), None, "{k}");
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
        // The board stays visible behind the pop-over, so any running slot
        // keeps the ticker going.
        assert!(needs_tick(Screen::Session(2), &slots_with(&[3])));
        assert!(needs_tick(Screen::Session(9), &slots_with(&[3])));
        assert!(!needs_tick(Screen::Session(2), &slots_with(&[])));
    }

    #[test]
    fn board_keys_stay_out_of_the_popover() {
        // The pop-over is drawn beside the board, not inside it, so the
        // focused terminal's context stack has no Board.
        const POPOVER: &[&str] = &["Overseer", "Session", "Terminal"];
        for k in ["left", "enter", "r", "a", "p", "l"] {
            assert_eq!(routed(k, POPOVER), None, "{k}");
        }
        assert_eq!(routed("cmd-escape", POPOVER), Some("overseer::GoBoard"));
        assert_eq!(routed("escape", POPOVER), None);
    }

    #[test]
    fn ticker_restarts_when_returning_to_the_board() {
        let s = slots_with(&[1]);
        let mut t = Ticker::default();
        assert!(t.on_change(Screen::Board, &s), "starts on the board");
        assert!(!t.on_change(Screen::Board, &s), "only one ticker");
        // A pop-over on an empty slot still shows the running card behind it.
        assert!(t.on_tick(Screen::Session(2), &s));
        // The slot stops: nothing running is visible, so the ticker ends.
        assert!(!t.on_tick(Screen::Session(2), &slots_with(&[])));
        // It runs again: the running card is visible again.
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
            stage: None,
            stage_since: None,
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
    fn ticket_clicks() {
        let w = empty_work();
        let exited = SlotState::Exited { code: None, at: 0 };
        // A Running owner opens its pop-over.
        let s = slots_with(&[4]);
        assert_eq!(ticket_click(Some(4), &s, &w), TicketClick::Open(4));
        assert_eq!(start_slot(Some(4), &s, &w), None, "never a Running owner");
        // An Exited owner is the target, even when a lower slot is free.
        let mut s = empty_slots();
        s[4].state = exited.clone();
        assert_eq!(ticket_click(Some(4), &s, &w), TicketClick::Start(4));
        // A Resumable owner opens its pop-over, where Resume is offered.
        s[4].state = SlotState::Resumable {
            session_id: "demo-session".into(),
            at: 0,
        };
        assert_eq!(ticket_click(Some(4), &s, &w), TicketClick::Open(4));
        assert_eq!(start_slot(Some(4), &s, &w), None, "never a Resumable owner");
        // An Empty owner still starts in place.
        s[4].state = SlotState::Empty;
        assert_eq!(ticket_click(Some(4), &s, &w), TicketClick::Start(4));
        // No owner: the lowest free slot, skipping Running ones.
        let s = slots_with(&[1]);
        assert_eq!(ticket_click(None, &s, &w), TicketClick::Start(2));
        // S0 is never picked.
        let s = slots_with(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(s[0].state, SlotState::Empty);
        assert_eq!(ticket_click(None, &s, &w), TicketClick::NoSlot);
        assert_eq!(start_slot(None, &s, &w), None);
        // An Exited slot counts as free.
        let mut s = slots_with(&[1, 2]);
        s[3].state = exited;
        assert_eq!(ticket_click(None, &s, &w), TicketClick::Start(3));
        assert_eq!(start_slot(None, &s, &w), Some(3));
    }

    #[test]
    fn s0_owner_opens_the_coordinator() {
        let w = empty_work();
        let mut s = empty_slots();
        assert_eq!(ticket_click(Some(0), &s, &w), TicketClick::Open(0));
        s[0].state = SlotState::Exited { code: None, at: 0 };
        assert_eq!(ticket_click(Some(0), &s, &w), TicketClick::Open(0));
        assert_eq!(start_slot(Some(0), &s, &w), None);
    }

    #[test]
    fn start_slot_skips_free_slots_that_own_work() {
        use overseer_proto::{ItemKind, WorkItem};
        let mut w = empty_work();
        w.items = vec![WorkItem {
            key: "DEMO-5".into(),
            kind: ItemKind::Ticket,
            title: String::new(),
            status: "open".into(),
            priority: None,
            parent: None,
            owner: Some(1),
            in_list: true,
            folder: None,
            stage: None,
            stage_since: None,
        }];
        // S1 Exited owning DEMO-5, S2 Empty: S2.
        let mut s = empty_slots();
        s[1].state = SlotState::Exited { code: None, at: 0 };
        assert_eq!(start_slot(None, &s, &w), Some(2));
        assert_eq!(ticket_click(None, &s, &w), TicketClick::Start(2));
        // Only S1 free, and it owns DEMO-5: S1 after all, alongside DEMO-5.
        let mut s = slots_with(&[2, 3, 4, 5, 6]);
        s[1].state = SlotState::Exited { code: None, at: 0 };
        assert_eq!(start_slot(None, &s, &w), Some(1));
        assert_eq!(ticket_click(None, &s, &w), TicketClick::Join(1));
        // An Empty S1 that was given DEMO-5 (a cancelled start) is the same.
        s[1].state = SlotState::Empty;
        assert_eq!(start_slot(None, &s, &w), Some(1));
        assert_eq!(ticket_click(None, &s, &w), TicketClick::Join(1));
        // A Resumable S1 is never joined: no slot left.
        s[1].state = SlotState::Resumable {
            session_id: "demo-session".into(),
            at: 0,
        };
        assert_eq!(start_slot(None, &s, &w), None);
        assert_eq!(ticket_click(None, &s, &w), TicketClick::NoSlot);
        s[1].state = SlotState::Empty;
        // DEMO-5's own click still restarts it in S1.
        assert_eq!(ticket_click(Some(1), &s, &w), TicketClick::Start(1));
    }

    /// Contexts that take the keyboard for themselves; window-wide
    /// navigation waits while one is open. A new modal is one line here.
    const MODALS: &[&[&str]] = &[DIALOG, PICKER, PLAN, SETTINGS];

    #[test]
    fn l_opens_locks_from_the_board_only() {
        assert_eq!(routed("l", BOARD), Some("overseer::GoLocks"));
        assert_eq!(routed("l", TERMINAL), None);
        assert_eq!(routed("l", CHROME), None);
    }

    #[test]
    fn cmd_shift_l_opens_locks_everywhere_but_modals() {
        for ctx in [TERMINAL, CHROME, BOARD] {
            assert_eq!(routed("cmd-shift-l", ctx), Some("overseer::GoLocks"));
        }
        for ctx in MODALS {
            assert_eq!(routed("cmd-shift-l", ctx), None, "{ctx:?}");
        }
    }

    #[test]
    fn esc_in_locks_goes_back_where_it_came_from() {
        assert_eq!(locks_return(Screen::Session(3)), Some(3));
        assert_eq!(locks_return(Screen::Board), None);
        assert_eq!(locks_return(Screen::Locks), None);
    }

    #[test]
    fn locks_keys_reach_the_view() {
        const LOCKS: &[&str] = &["Overseer", "Locks"];
        for k in ["escape", "enter", "f", "up", "down", "l"] {
            assert_eq!(routed(k, LOCKS), None, "{k}");
        }
        assert_eq!(routed("cmd-1", LOCKS), Some("overseer::GoS1"));
        assert_eq!(routed("cmd-escape", LOCKS), Some("overseer::GoBoard"));
    }

    #[test]
    fn cmd_comma_opens_settings_from_everywhere_but_modals() {
        for ctx in [TERMINAL, CHROME, BOARD, &["Overseer", "Locks"][..]] {
            assert_eq!(
                routed("cmd-,", ctx),
                Some("overseer::OpenSettings"),
                "{ctx:?}"
            );
        }
        for ctx in MODALS {
            assert_eq!(routed("cmd-,", ctx), None, "{ctx:?}");
        }
    }

    #[test]
    fn keys_reach_the_settings_view() {
        for k in [
            "escape",
            "cmd-s",
            "tab",
            "shift-tab",
            "cmd-a",
            "cmd-v",
            "enter",
            "space",
        ] {
            assert_eq!(routed(k, SETTINGS), None, "{k}");
        }
        assert_eq!(routed("cmd-q", SETTINGS), Some("overseer::Quit"));
    }

    #[test]
    fn cmd_digits_do_not_route_while_settings_open() {
        for d in [
            "cmd-0",
            "cmd-3",
            "cmd-6",
            "cmd-n",
            "cmd-escape",
            "cmd-shift-l",
        ] {
            assert_eq!(routed(d, SETTINGS), None, "{d}");
        }
    }

    #[test]
    fn the_pane_keys() {
        const REPLY: &[&str] = &["Overseer", "Conversation", "ConvReply"];
        // Tab jumps from the board and the reply box, and nowhere else: the
        // dialogs, Settings, the picker and the terminal keep it.
        assert_eq!(routed("tab", BOARD), Some("overseer::NextWaiting"));
        assert_eq!(routed("tab", REPLY), Some("overseer::NextWaiting"));
        for ctx in [TERMINAL, CHROME, DIALOG, PICKER, PLAN, SETTINGS] {
            assert_eq!(routed("tab", ctx), None, "{ctx:?}");
        }
        for ctx in [BOARD, REPLY] {
            assert_eq!(
                routed("cmd-\\", ctx),
                Some("overseer::TogglePane"),
                "{ctx:?}"
            );
        }
        for ctx in [TERMINAL, DIALOG, PICKER, PLAN, SETTINGS] {
            assert_eq!(routed("cmd-\\", ctx), None, "{ctx:?}");
        }
        // The board's plain keys stay out of the reply box, which takes
        // Enter, Esc and typing itself.
        for k in [
            "a",
            "r",
            "p",
            "l",
            "n",
            "left",
            "up",
            "enter",
            "shift-enter",
            "escape",
            "shift-tab",
            "cmd-v",
            "cmd-a",
        ] {
            assert_eq!(routed(k, REPLY), None, "{k}");
        }
        assert_eq!(routed("cmd-3", REPLY), Some("overseer::GoS3"));
    }

    #[test]
    fn p_routes_on_the_board_only() {
        assert_eq!(routed("p", BOARD), Some("overseer::OpenPlan"));
        for ctx in [TERMINAL, CHROME, DIALOG, PICKER] {
            assert_eq!(routed("p", ctx), None);
        }
    }
}
