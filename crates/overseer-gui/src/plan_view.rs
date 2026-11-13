//! The plan approval view: a modal where Jonathan reviews S0's pending plan,
//! edits it, and approves, rejects or closes it.
//!
//! Edits (dropping a slot, an item, a target or a worktree, or rewording a
//! kickoff) change a copy, `PlanDraft`; Approve sends that copy. Cmd+Enter
//! approves, Esc closes and decides nothing, Tab walks the text fields.
//! Approval can take a while (the daemon creates worktrees and starts
//! sessions), so the request runs in the background and the view shows
//! "Approving" until it is answered.
//!
//! The draft, the rows, the key handling and the reply handling are pure
//! and unit tested.

use std::sync::Arc;

use gpui::{
    div, prelude::*, px, rgb, rgba, Context, EventEmitter, FocusHandle, Focusable, FontWeight,
    KeyDownEvent, Keystroke, MouseButton, ScrollHandle, SharedString, Window,
};
use overseer_proto::{
    slot_name, Client, CoordView, Plan, PlanDecision, PlanRecord, PlanState, Request, Response,
    SlotId,
};

use crate::app::colors;
use crate::start_dialog::{edit_key, DialogKey, TextField};

// ---------------------------------------------------------------------------
// Pure logic
// ---------------------------------------------------------------------------

/// The editable copy of a plan.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanDraft {
    original: Plan,
    current: Plan,
}

impl PlanDraft {
    pub fn new(plan: &Plan) -> Self {
        PlanDraft {
            original: plan.clone(),
            current: plan.clone(),
        }
    }

    fn slot_mut(&mut self, slot: SlotId) -> Option<&mut overseer_proto::PlanSlot> {
        self.current.slots.iter_mut().find(|s| s.slot == slot)
    }

    pub fn remove_slot(&mut self, slot: SlotId) {
        self.current.slots.retain(|s| s.slot != slot);
    }

    pub fn remove_item(&mut self, slot: SlotId, key: &str) {
        if let Some(s) = self.slot_mut(slot) {
            s.items.retain(|k| k != key);
        }
    }

    pub fn remove_target(&mut self, slot: SlotId, target: &str) {
        if let Some(s) = self.slot_mut(slot) {
            s.targets.retain(|t| t.target != target);
        }
    }

    pub fn clear_worktree(&mut self, slot: SlotId) {
        if let Some(s) = self.slot_mut(slot) {
            s.worktree = None;
        }
    }

    pub fn set_kickoff(&mut self, slot: SlotId, text: &str) {
        if let Some(s) = self.slot_mut(slot) {
            s.kickoff = text.to_string();
        }
    }

    /// The plan as edited.
    pub fn plan(&self) -> Plan {
        self.current.clone()
    }

    /// Whether the draft differs from the plan S0 proposed.
    pub fn edited(&self) -> bool {
        self.current != self.original
    }

    /// A plan with every slot dropped has nothing to run.
    pub fn can_approve(&self) -> bool {
        !self.current.slots.is_empty()
    }
}

/// A target as shown: the target, then its reason when it has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetPill {
    pub target: String,
    pub text: String,
}

/// One block of the view, top to bottom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanRow {
    Title(String),
    Summary(String),
    SlotHead {
        slot: SlotId,
        title: String,
    },
    Items {
        slot: SlotId,
        keys: Vec<String>,
    },
    Targets {
        slot: SlotId,
        targets: Vec<TargetPill>,
    },
    Worktree {
        slot: SlotId,
        text: String,
    },
    Kickoff {
        slot: SlotId,
    },
    Rule(String),
    Drop(SlotId),
    Warning(String),
    /// Why approving the plan as edited would be refused.
    Refusal(String),
}

/// The rows to draw for `rec` as edited in `editor`. Warnings (and a
/// refusal) come last, above the buttons.
pub fn rows(rec: &PlanRecord, editor: &Editor) -> Vec<PlanRow> {
    let plan = &editor.draft.current;
    let mut out = vec![PlanRow::Title(format!("Plan {} from S0", rec.id))];
    if !plan.summary.trim().is_empty() {
        out.push(PlanRow::Summary(plan.summary.clone()));
    }
    for s in &plan.slots {
        let title = if s.label.is_empty() {
            slot_name(s.slot)
        } else {
            format!("{}  {}", slot_name(s.slot), s.label)
        };
        out.push(PlanRow::SlotHead {
            slot: s.slot,
            title,
        });
        if !s.items.is_empty() {
            out.push(PlanRow::Items {
                slot: s.slot,
                keys: s.items.clone(),
            });
        }
        if !s.targets.is_empty() {
            out.push(PlanRow::Targets {
                slot: s.slot,
                targets: s
                    .targets
                    .iter()
                    .map(|t| TargetPill {
                        target: t.target.clone(),
                        text: match t.reason.as_deref().filter(|r| !r.is_empty()) {
                            Some(r) => format!("{} ({r})", t.target),
                            None => t.target.clone(),
                        },
                    })
                    .collect(),
            });
        }
        if let Some(w) = &s.worktree {
            out.push(PlanRow::Worktree {
                slot: s.slot,
                text: format!("worktree {} ({})", w.repo.display(), w.ticket),
            });
        }
        out.push(PlanRow::Kickoff { slot: s.slot });
        for rule in &s.rules {
            out.push(PlanRow::Rule(rule.clone()));
        }
        out.push(PlanRow::Drop(s.slot));
    }
    for w in &editor.warnings {
        out.push(PlanRow::Warning(w.clone()));
    }
    if let Some(r) = &editor.refusal {
        out.push(PlanRow::Refusal(r.clone()));
    }
    out
}

/// The status line when plan `id` is no longer pending in `view`, or None
/// while it still is.
pub fn gone_note(id: i64, view: &CoordView) -> Option<String> {
    let how = match view.plans.iter().find(|p| p.id == id).map(|p| p.state) {
        Some(PlanState::Pending) => return None,
        Some(PlanState::Approved) => "approved",
        Some(PlanState::Rejected) => "rejected",
        Some(PlanState::Superseded) | None => "replaced",
    };
    Some(format!("Plan {id} was {how} elsewhere"))
}

/// A text field of the view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Kickoff(SlotId),
    Reason,
}

/// What a key press means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanAction {
    Redraw,
    Approve,
    Close,
    Paste,
    Ignored,
}

/// How the daemon answered a decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// Decided: the line for the status bar.
    Done(String),
    /// Refused or lost: shown in red, the view stays open.
    Failed(String),
}

/// The state of an open view.
#[derive(Debug, Clone)]
pub struct Editor {
    pub id: i64,
    pub draft: PlanDraft,
    pub kickoffs: Vec<(SlotId, TextField)>,
    pub reason: TextField,
    pub active: Field,
    pub busy: bool,
    pub error: Option<String>,
    /// The newest coordinator view seen while a decision was in flight.
    pub latest: Option<CoordView>,
    /// The warnings for the plan as edited: the record's until the first
    /// check after an edit answers.
    pub warnings: Vec<String>,
    /// Why the daemon would refuse the plan as edited.
    pub refusal: Option<String>,
    /// The number of the newest check sent; older answers are stale.
    checks: u64,
}

impl Editor {
    pub fn new(rec: &PlanRecord) -> Self {
        let kickoffs: Vec<(SlotId, TextField)> = rec
            .plan
            .slots
            .iter()
            .map(|s| (s.slot, TextField::new(&s.kickoff)))
            .collect();
        let active = kickoffs
            .first()
            .map_or(Field::Reason, |(s, _)| Field::Kickoff(*s));
        Editor {
            id: rec.id,
            draft: PlanDraft::new(&rec.plan),
            kickoffs,
            reason: TextField::default(),
            active,
            busy: false,
            error: None,
            latest: None,
            warnings: rec.warnings.clone(),
            refusal: None,
            checks: 0,
        }
    }

    /// A check of the plan as edited, numbered so a late answer to an older
    /// one is dropped.
    pub fn check_request(&mut self) -> (u64, Request) {
        self.checks += 1;
        (
            self.checks,
            Request::CheckPlan {
                plan: self.draft.plan(),
            },
        )
    }

    /// The answer to check `seq`. A lost or odd answer keeps what is shown.
    pub fn checked(&mut self, seq: u64, reply: Result<Response, String>) {
        if seq != self.checks {
            return;
        }
        if let Ok(Response::PlanCheck { warnings, refusal }) = reply {
            self.warnings = warnings;
            self.refusal = refusal;
        }
    }

    fn order(&self) -> Vec<Field> {
        self.kickoffs
            .iter()
            .map(|(s, _)| Field::Kickoff(*s))
            .chain([Field::Reason])
            .collect()
    }

    fn field_mut(&mut self) -> &mut TextField {
        match self.active {
            Field::Reason => &mut self.reason,
            Field::Kickoff(s) => match self.kickoffs.iter().position(|(k, _)| *k == s) {
                Some(i) => &mut self.kickoffs[i].1,
                None => &mut self.reason,
            },
        }
    }

    pub fn field(&self, f: Field) -> &TextField {
        match f {
            Field::Reason => &self.reason,
            Field::Kickoff(s) => self
                .kickoffs
                .iter()
                .find(|(k, _)| *k == s)
                .map_or(&self.reason, |(_, t)| t),
        }
    }

    fn sync(&mut self) {
        if let Field::Kickoff(s) = self.active {
            let text = self.field(self.active).text.clone();
            self.draft.set_kickoff(s, &text);
        }
    }

    fn step(&mut self, back: bool) {
        let order = self.order();
        let at = order.iter().position(|f| *f == self.active).unwrap_or(0);
        let n = order.len();
        self.active = order[if back { (at + n - 1) % n } else { (at + 1) % n }];
    }

    /// Drops a slot from the draft and its kickoff field.
    pub fn drop_slot(&mut self, slot: SlotId) {
        self.draft.remove_slot(slot);
        self.kickoffs.retain(|(s, _)| *s != slot);
        if self.active == Field::Kickoff(slot) {
            self.active = Field::Reason;
        }
    }

    pub fn key(&mut self, ks: &Keystroke) -> PlanAction {
        if ks.key == "escape" {
            return PlanAction::Close;
        }
        if self.busy {
            return PlanAction::Ignored;
        }
        let m = &ks.modifiers;
        if m.platform && ks.key == "enter" {
            return if self.draft.can_approve() {
                PlanAction::Approve
            } else {
                PlanAction::Ignored
            };
        }
        let result = edit_key(self.field_mut(), ks);
        match result {
            DialogKey::Edited => {
                self.sync();
                PlanAction::Redraw
            }
            DialogKey::NextField => {
                self.step(false);
                PlanAction::Redraw
            }
            DialogKey::PrevField => {
                self.step(true);
                PlanAction::Redraw
            }
            DialogKey::Paste => PlanAction::Paste,
            DialogKey::Submit | DialogKey::Cancel | DialogKey::Ignored => PlanAction::Ignored,
        }
    }

    pub fn paste(&mut self, text: &str) {
        self.field_mut().insert(text);
        self.sync();
    }

    pub fn approve_request(&self) -> Request {
        Request::DecidePlan {
            id: self.id,
            from: None,
            decision: PlanDecision::Approve {
                plan: self.draft.plan(),
            },
        }
    }

    pub fn reject_request(&self) -> Request {
        Request::DecidePlan {
            id: self.id,
            from: None,
            decision: PlanDecision::Reject {
                reason: self.reason.text.trim().to_string(),
            },
        }
    }

    /// The daemon's answer to Approve (`approving`) or Reject.
    pub fn reply(&self, reply: Result<Response, String>, approving: bool) -> Reply {
        match reply {
            Ok(Response::Text { text }) if approving => {
                Reply::Done(approval_status(self.id, &text))
            }
            Ok(Response::Ok) | Ok(Response::Text { .. }) => Reply::Done(if approving {
                format!("Plan {} approved", self.id)
            } else {
                format!("Plan {} rejected", self.id)
            }),
            // Decided elsewhere while this request was in flight: say so
            // instead of showing the daemon's refusal.
            Ok(Response::Error { message }) | Ok(Response::NeedsConfirm { message }) => self
                .latest
                .as_ref()
                .and_then(|v| gone_note(self.id, v))
                .map_or(Reply::Failed(message), Reply::Done),
            Ok(other) => Reply::Failed(format!("Unexpected reply: {other:?}")),
            Err(e) => Reply::Failed(e),
        }
    }
}

/// The status line after an approval: the first failed step when there is
/// one (with a count of the others), else the first outcome line.
fn approval_status(id: i64, text: &str) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let failed: Vec<&&str> = lines.iter().filter(|l| l.contains(" failed: ")).collect();
    match (failed.first(), lines.first()) {
        (Some(first), _) if failed.len() > 1 => {
            format!("{first} (+{} more failed)", failed.len() - 1)
        }
        (Some(first), _) => (**first).to_string(),
        (None, Some(first)) => (*first).to_string(),
        (None, None) => format!("Plan {id} approved"),
    }
}

// ---------------------------------------------------------------------------
// The entity
// ---------------------------------------------------------------------------

/// What the app hears from the view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanEvent {
    /// Decided here or found decided elsewhere: close, with a status line.
    Decided(String),
    /// Closed without deciding.
    Closed,
}

pub struct PlanView {
    rec: PlanRecord,
    editor: Editor,
    client: Arc<Client>,
    scroll: ScrollHandle,
    focus_handle: FocusHandle,
    /// Closed with a request on its way; its answer goes to the status.
    closed: bool,
}

impl EventEmitter<PlanEvent> for PlanView {}

impl Focusable for PlanView {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl PlanView {
    pub fn new(rec: &PlanRecord, client: Arc<Client>, cx: &mut Context<Self>) -> Self {
        PlanView {
            rec: rec.clone(),
            editor: Editor::new(rec),
            client,
            scroll: ScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            closed: false,
        }
    }

    /// The daemon's new coordinator view. While a decision is on its way the
    /// reply settles things; otherwise a plan no longer pending closes the
    /// view.
    pub fn coord_changed(&mut self, view: &CoordView, cx: &mut Context<Self>) {
        if self.editor.busy {
            self.editor.latest = Some(view.clone());
            return;
        }
        if let Some(note) = gone_note(self.editor.id, view) {
            cx.emit(PlanEvent::Decided(note));
        }
    }

    /// A decision is on its way; the app keeps a closed view until it is
    /// answered.
    pub fn busy(&self) -> bool {
        self.editor.busy
    }

    /// The client to use from now on, after a reconnect.
    pub fn set_client(&mut self, client: Arc<Client>) {
        self.client = client;
    }

    /// Closes without cancelling a request in flight: the daemon has no
    /// timeout for it, so Close must not wait for the answer.
    fn close(&mut self, cx: &mut Context<Self>) {
        self.closed = true;
        cx.emit(PlanEvent::Closed);
    }

    fn decide(&mut self, approving: bool, cx: &mut Context<Self>) {
        if self.closed || self.editor.busy || (approving && !self.editor.draft.can_approve()) {
            return;
        }
        let req = if approving {
            self.editor.approve_request()
        } else {
            self.editor.reject_request()
        };
        self.editor.busy = true;
        self.editor.error = None;
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let reply = client
                .request(req)
                .await
                .map_err(|e| format!("{} ({e:#})", crate::terminal_view::LOST_DAEMON));
            let _ = this.update(cx, |this, cx| {
                match this.editor.reply(reply, approving) {
                    Reply::Done(line) => cx.emit(PlanEvent::Decided(line)),
                    Reply::Failed(e) if this.closed => cx.emit(PlanEvent::Decided(format!(
                        "Plan {} not decided: {e}",
                        this.editor.id
                    ))),
                    Reply::Failed(e) => {
                        this.editor.busy = false;
                        this.editor.error = Some(e);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// The draft changed shape: ask the daemon for its warnings now.
    fn recheck(&mut self, cx: &mut Context<Self>) {
        let (seq, req) = self.editor.check_request();
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let reply = client.request(req).await.map_err(|e| format!("{e:#}"));
            let _ = this.update(cx, |this, cx| {
                this.editor.checked(seq, reply);
                cx.notify();
            });
        })
        .detach();
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let action = self.editor.key(&ev.keystroke);
        if action == PlanAction::Ignored {
            return;
        }
        cx.stop_propagation();
        match action {
            PlanAction::Approve => self.decide(true, cx),
            PlanAction::Close => self.close(cx),
            PlanAction::Paste => {
                if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                    self.editor.paste(&text);
                }
                cx.notify();
            }
            PlanAction::Redraw | PlanAction::Ignored => cx.notify(),
        }
    }

    fn field_box(
        &self,
        f: Field,
        id: (&'static str, usize),
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let on = self.editor.active == f;
        let tf = self.editor.field(f);
        let text = if on {
            tf.with_cursor()
        } else {
            tf.text.clone()
        };
        div()
            .id(id)
            .px_2()
            .py_1()
            .rounded(px(4.))
            .bg(rgb(colors::FIELD))
            .border_1()
            .border_color(rgb(if on { colors::ACCENT } else { colors::BORDER }))
            .cursor_text()
            .child(SharedString::from(text))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.editor.active = f;
                cx.notify();
            }))
    }
}

fn button(id: &'static str, text: &str, primary: bool, enabled: bool) -> gpui::Stateful<gpui::Div> {
    let b = div()
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
        .text_color(rgb(if !enabled {
            colors::MUTED
        } else if primary {
            colors::ACCENT
        } else {
            colors::TEXT
        }))
        .child(text.to_string());
    if enabled {
        b.cursor_pointer().hover(|s| s.bg(rgb(colors::CARD_HOVER)))
    } else {
        b
    }
}

fn pill(
    id: (&'static str, usize),
    text: String,
    cx_click: impl Fn(&mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_1()
        .px_2()
        .rounded(px(10.))
        .bg(rgb(colors::ACCENT_BG))
        .text_color(rgb(colors::ACCENT))
        .child(text)
        .child(
            div()
                .cursor_pointer()
                .text_color(rgb(colors::MUTED))
                .hover(|s| s.text_color(rgb(colors::RED)))
                .child("x")
                .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                    cx_click(window, cx);
                    cx.stop_propagation();
                }),
        )
}

impl Render for PlanView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rec_rows = rows(&self.rec, &self.editor);
        let busy = self.editor.busy;
        let can_approve = self.editor.draft.can_approve() && !busy;
        let mut body: Vec<gpui::AnyElement> = Vec::new();
        for row in rec_rows {
            let el = match row {
                PlanRow::Title(t) => div()
                    .text_base()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(t)
                    .into_any_element(),
                PlanRow::Summary(t) => div()
                    .text_color(rgb(colors::TEXT_DIM))
                    .child(t)
                    .into_any_element(),
                PlanRow::SlotHead { title, .. } => div()
                    .mt_2()
                    .pt_2()
                    .border_t_1()
                    .border_color(rgb(colors::BORDER))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title)
                    .into_any_element(),
                PlanRow::Items { slot, keys } => div()
                    .flex()
                    .flex_wrap()
                    .gap_1()
                    .children(keys.into_iter().enumerate().map(|(i, key)| {
                        let entity = cx.entity();
                        let k = key.clone();
                        pill(("item", usize::from(slot) * 100 + i), key, move |_, cx| {
                            entity.update(cx, |this, cx| {
                                this.editor.draft.remove_item(slot, &k);
                                this.recheck(cx);
                                cx.notify();
                            })
                        })
                    }))
                    .into_any_element(),
                PlanRow::Targets { slot, targets } => div()
                    .flex()
                    .flex_wrap()
                    .gap_1()
                    .children(targets.into_iter().enumerate().map(|(i, t)| {
                        let entity = cx.entity();
                        let target = t.target.clone();
                        pill(
                            ("target", usize::from(slot) * 100 + i),
                            t.text,
                            move |_, cx| {
                                entity.update(cx, |this, cx| {
                                    this.editor.draft.remove_target(slot, &target);
                                    this.recheck(cx);
                                    cx.notify();
                                })
                            },
                        )
                    }))
                    .into_any_element(),
                PlanRow::Worktree { slot, text } => {
                    let entity = cx.entity();
                    div()
                        .flex()
                        .child(pill(("worktree", usize::from(slot)), text, move |_, cx| {
                            entity.update(cx, |this, cx| {
                                this.editor.draft.clear_worktree(slot);
                                this.recheck(cx);
                                cx.notify();
                            })
                        }))
                        .into_any_element()
                }
                PlanRow::Kickoff { slot } => self
                    .field_box(Field::Kickoff(slot), ("kickoff", usize::from(slot)), cx)
                    .into_any_element(),
                PlanRow::Rule(t) => div()
                    .text_xs()
                    .text_color(rgb(colors::MUTED))
                    .child(format!("Rule: {t}"))
                    .into_any_element(),
                PlanRow::Drop(slot) => div()
                    .flex()
                    .child(
                        button("drop", &format!("Drop {}", slot_name(slot)), false, !busy)
                            .id(("drop", usize::from(slot)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.editor.busy {
                                    this.editor.drop_slot(slot);
                                    this.recheck(cx);
                                }
                                cx.notify();
                            })),
                    )
                    .into_any_element(),
                PlanRow::Warning(t) => div()
                    .text_color(rgb(colors::ORANGE))
                    .child(t)
                    .into_any_element(),
                PlanRow::Refusal(t) => div()
                    .text_color(rgb(colors::RED))
                    .child(format!("Approving would fail: {t}"))
                    .into_any_element(),
            };
            body.push(el);
        }
        let error = self.editor.error.clone();
        let hint = if busy {
            "Approving\u{2026}"
        } else {
            "Cmd+Enter approve · Tab next field · Esc close"
        };
        let card = div()
            .w(px(720.))
            .max_h(px(640.))
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
            .child(
                div()
                    .id("plan-rows")
                    .flex()
                    .flex_col()
                    .gap_1()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .children(body),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_none()
                            .text_color(rgb(colors::MUTED))
                            .child("Reject reason"),
                    )
                    .child(self.field_box(Field::Reason, ("reason", 0), cx).flex_1()),
            )
            .children(error.map(|e| div().text_color(rgb(colors::RED)).child(e)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .child(
                        div()
                            .mr_auto()
                            .text_xs()
                            .text_color(rgb(colors::MUTED))
                            .child(hint),
                    )
                    .child(
                        button("plan-close", "Close", false, true)
                            .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
                    )
                    .child(
                        button("plan-reject", "Reject", false, !busy)
                            .on_click(cx.listener(|this, _, _, cx| this.decide(false, cx))),
                    )
                    .child(
                        button("plan-approve", "Approve", true, can_approve)
                            .on_click(cx.listener(|this, _, _, cx| this.decide(true, cx))),
                    ),
            );
        div()
            .key_context("PlanView")
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
            .child(card)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_plan;
    use overseer_proto::{Plan, PlanSlot, PlanState, PlanTarget, PlanWorktree};

    fn slot(n: SlotId) -> PlanSlot {
        PlanSlot {
            slot: n,
            label: "demo lane".into(),
            items: vec!["DEMO-12".into(), "ov-3".into()],
            targets: vec![PlanTarget {
                target: "repo:demo-api:main".into(),
                reason: Some("merge".into()),
            }],
            worktree: Some(PlanWorktree {
                repo: "/work/demo-api".into(),
                ticket: "DEMO-12".into(),
            }),
            cwd: None,
            kickoff: "Start with the schema".into(),
            rules: vec!["Do not deploy before S2 merges".into()],
        }
    }

    fn record() -> overseer_proto::PlanRecord {
        let mut rec = test_plan(4, PlanState::Pending);
        rec.plan = Plan {
            summary: "Split the demo work".into(),
            slots: vec![slot(3), slot(5)],
        };
        rec.warnings = vec!["S3 is running; it will be told to work in /x but not moved".into()];
        rec
    }

    #[test]
    fn draft_starts_equal_and_unedited() {
        let rec = record();
        let d = PlanDraft::new(&rec.plan);
        assert_eq!(d.plan(), rec.plan);
        assert!(!d.edited());
        assert!(d.can_approve());
    }

    #[test]
    fn removing_an_item_marks_edited_and_drops_it_from_the_plan() {
        let mut d = PlanDraft::new(&record().plan);
        d.remove_item(3, "ov-3");
        assert!(d.edited());
        assert_eq!(d.plan().slots[0].items, vec!["DEMO-12".to_string()]);
        assert_eq!(d.plan().slots[1].items.len(), 2);
        d.remove_target(3, "repo:demo-api:main");
        assert!(d.plan().slots[0].targets.is_empty());
        d.clear_worktree(3);
        assert!(d.plan().slots[0].worktree.is_none());
        d.remove_slot(5);
        assert_eq!(d.plan().slots.len(), 1);
    }

    #[test]
    fn dropping_every_slot_disables_approve() {
        let mut d = PlanDraft::new(&record().plan);
        d.remove_slot(3);
        assert!(d.can_approve());
        d.remove_slot(5);
        assert!(!d.can_approve());
        assert!(d.edited());
    }

    #[test]
    fn kickoff_edit_round_trips() {
        let mut d = PlanDraft::new(&record().plan);
        d.set_kickoff(5, "Read the notes first");
        assert_eq!(d.plan().slots[1].kickoff, "Read the notes first");
        assert!(d.edited());
        d.set_kickoff(5, "Start with the schema");
        assert!(!d.edited());
    }

    #[test]
    fn rows_show_warnings_and_worktree() {
        let rec = record();
        let mut e = Editor::new(&rec);
        let rows = rows(&rec, &e);
        assert_eq!(rows[0], PlanRow::Title("Plan 4 from S0".into()));
        assert_eq!(rows[1], PlanRow::Summary("Split the demo work".into()));
        assert!(rows.contains(&PlanRow::SlotHead {
            slot: 3,
            title: "S3  demo lane".into()
        }));
        assert!(rows.contains(&PlanRow::Items {
            slot: 3,
            keys: vec!["DEMO-12".into(), "ov-3".into()]
        }));
        assert!(rows.contains(&PlanRow::Worktree {
            slot: 3,
            text: "worktree /work/demo-api (DEMO-12)".into()
        }));
        assert!(rows.contains(&PlanRow::Rule("Do not deploy before S2 merges".into())));
        assert!(rows.contains(&PlanRow::Drop(3)));
        assert_eq!(
            rows.last(),
            Some(&PlanRow::Warning(
                "S3 is running; it will be told to work in /x but not moved".into()
            ))
        );
        e.draft.clear_worktree(3);
        e.draft.remove_slot(5);
        let rows = rows_of(&rec, &e);
        assert!(!rows.iter().any(|r| matches!(r, PlanRow::Worktree { .. })));
        assert!(!rows.contains(&PlanRow::Drop(5)));
    }

    fn rows_of(rec: &overseer_proto::PlanRecord, e: &Editor) -> Vec<PlanRow> {
        rows(rec, e)
    }

    fn ks(s: &str) -> gpui::Keystroke {
        let mut k = gpui::Keystroke::parse(s).unwrap();
        if k.key.chars().count() == 1 && !k.modifiers.platform {
            k.key_char = Some(k.key.clone());
        }
        k
    }

    #[test]
    fn keys_approve_close_and_edit_the_kickoff() {
        let mut e = Editor::new(&record());
        assert_eq!(e.key(&ks("cmd-enter")), PlanAction::Approve);
        assert_eq!(e.key(&ks("escape")), PlanAction::Close);
        // Typing goes to the first kickoff and into the draft.
        assert_eq!(e.key(&ks("x")), PlanAction::Redraw);
        assert!(e.draft.plan().slots[0].kickoff.ends_with('x'));
        assert!(e.draft.edited());
        // Tab walks the kickoffs, then the reason.
        e.key(&ks("tab"));
        e.key(&ks("tab"));
        e.key(&ks("n"));
        assert_eq!(e.reason.text, "n");
        // Nothing to approve once every slot is dropped.
        e.drop_slot(3);
        e.drop_slot(5);
        assert_eq!(e.key(&ks("cmd-enter")), PlanAction::Ignored);
    }

    #[test]
    fn a_busy_editor_ignores_decisions_but_can_close() {
        let mut e = Editor::new(&record());
        e.busy = true;
        assert_eq!(e.key(&ks("cmd-enter")), PlanAction::Ignored);
        assert_eq!(e.key(&ks("escape")), PlanAction::Close);
    }

    #[test]
    fn a_refusal_after_an_outside_decision_reads_as_decided_elsewhere() {
        let mut e = Editor::new(&record());
        let refusal = || {
            Ok(Response::Error {
                message: "plan 4 is no longer pending".into(),
            })
        };
        // No newer view seen: the refusal is shown as is.
        assert_eq!(
            e.reply(refusal(), true),
            Reply::Failed("plan 4 is no longer pending".into())
        );
        // Still pending in the latest view: still the refusal.
        let mut view = overseer_proto::CoordView {
            summary: None,
            plans: vec![test_plan(4, PlanState::Pending)],
            reports: Vec::new(),
            status_asked: Vec::new(),
        };
        e.latest = Some(view.clone());
        assert!(matches!(e.reply(refusal(), true), Reply::Failed(_)));
        // Decided elsewhere meanwhile.
        view.plans[0].state = PlanState::Approved;
        e.latest = Some(view);
        assert_eq!(
            e.reply(refusal(), true),
            Reply::Done("Plan 4 was approved elsewhere".into())
        );
    }

    #[test]
    fn requests_and_replies() {
        let mut e = Editor::new(&record());
        e.draft.remove_slot(5);
        match e.approve_request() {
            Request::DecidePlan {
                id: 4,
                from: None,
                decision: PlanDecision::Approve { plan },
            } => assert_eq!(plan.slots.len(), 1),
            other => panic!("{other:?}"),
        }
        e.reason.set("too wide");
        assert!(matches!(
            e.reject_request(),
            Request::DecidePlan {
                id: 4,
                from: None,
                decision: PlanDecision::Reject { reason },
            } if reason == "too wide"
        ));
        assert_eq!(
            e.reply(
                Ok(Response::Text {
                    text: "S3: assigned DEMO-12\nS3: started in /x".into()
                }),
                true
            ),
            Reply::Done("S3: assigned DEMO-12".into())
        );
        assert_eq!(
            e.reply(
                Ok(Response::Text {
                    text: String::new()
                }),
                true
            ),
            Reply::Done("Plan 4 approved".into())
        );
        assert_eq!(
            e.reply(Ok(Response::Ok), false),
            Reply::Done("Plan 4 rejected".into())
        );
        assert_eq!(
            e.reply(
                Ok(Response::Error {
                    message: "plan 4 is no longer pending".into()
                }),
                true
            ),
            Reply::Failed("plan 4 is no longer pending".into())
        );
        assert!(matches!(e.reply(Err("lost".into()), true), Reply::Failed(m) if m == "lost"));
    }

    #[test]
    fn warnings_follow_the_edited_plan() {
        let rec = record();
        let mut e = Editor::new(&rec);
        // The record's warnings until a check answers.
        assert_eq!(e.warnings, rec.warnings);
        e.drop_slot(3);
        let (first, req) = e.check_request();
        assert_eq!(
            req,
            Request::CheckPlan {
                plan: e.draft.plan()
            }
        );
        // A newer check supersedes the first; its late answer is ignored.
        let (second, _) = e.check_request();
        e.checked(
            second,
            Ok(Response::PlanCheck {
                warnings: vec![],
                refusal: None,
            }),
        );
        assert!(e.warnings.is_empty());
        assert!(!rows(&rec, &e)
            .iter()
            .any(|r| matches!(r, PlanRow::Warning(_))));
        e.checked(
            first,
            Ok(Response::PlanCheck {
                warnings: vec!["stale".into()],
                refusal: None,
            }),
        );
        assert!(e.warnings.is_empty());
        // A refusal is shown last; a failed check keeps what was there.
        let (third, _) = e.check_request();
        e.checked(
            third,
            Ok(Response::PlanCheck {
                warnings: vec![],
                refusal: Some("demo refusal".into()),
            }),
        );
        assert_eq!(
            rows(&rec, &e).last(),
            Some(&PlanRow::Refusal("demo refusal".into()))
        );
        let (fourth, _) = e.check_request();
        e.checked(fourth, Err("lost".into()));
        assert_eq!(e.refusal.as_deref(), Some("demo refusal"));
    }

    #[test]
    fn approval_status_names_failures() {
        let text = "S3: assigned DEMO-12\nS4: worktree failed: no repo; the rest of S4's plan was skipped\nS5: start failed: busy; the rest of S5's plan was skipped";
        assert_eq!(
            approval_status(4, text),
            "S4: worktree failed: no repo; the rest of S4's plan was skipped (+1 more failed)"
        );
        let one = "S3: assigned DEMO-12\nS4: start failed: busy; the rest of S4's plan was skipped";
        assert_eq!(
            approval_status(4, one),
            "S4: start failed: busy; the rest of S4's plan was skipped"
        );
        assert_eq!(
            approval_status(4, "S3: assigned DEMO-12"),
            "S3: assigned DEMO-12"
        );
    }

    #[test]
    fn a_plan_decided_elsewhere_closes_the_view() {
        let mut view = overseer_proto::CoordView {
            summary: None,
            plans: vec![test_plan(4, PlanState::Pending)],
            reports: Vec::new(),
            status_asked: Vec::new(),
        };
        assert_eq!(gone_note(4, &view), None);
        view.plans[0].state = PlanState::Approved;
        assert_eq!(
            gone_note(4, &view).as_deref(),
            Some("Plan 4 was approved elsewhere")
        );
        view.plans[0].state = PlanState::Rejected;
        assert_eq!(
            gone_note(4, &view).as_deref(),
            Some("Plan 4 was rejected elsewhere")
        );
        view.plans[0].state = PlanState::Superseded;
        assert_eq!(
            gone_note(4, &view).as_deref(),
            Some("Plan 4 was replaced elsewhere")
        );
        view.plans.clear();
        assert_eq!(
            gone_note(4, &view).as_deref(),
            Some("Plan 4 was replaced elsewhere")
        );
    }
}
