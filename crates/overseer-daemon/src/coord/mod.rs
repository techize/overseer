//! The coordinator's side of the board: the summary S0 keeps, progress
//! reports and status requests, and the state timeline of every slot.
//!
//! Plans join this service later. Every change is announced with
//! `Event::CoordChanged`.

use std::collections::HashMap;
use std::sync::Arc;

use overseer_proto::msg::{
    slot_name, ActivityKind, CoordView, Event, ReportKind, Response, Sender, SlotId, SlotInfo,
    SlotState,
};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::ledger::Ledger;
use crate::locks::table::queue_relay_on;
use crate::locks::{local_hhmm, LockService};
use crate::manager::valid_id;

/// The longest summary or report, in characters.
pub const MAX_TEXT: usize = 2000;
/// Reports and state changes in `CoordView.reports`.
const VIEW_REPORTS: usize = 100;
/// Plans in `CoordView.plans`.
const VIEW_PLANS: usize = 20;

pub struct CoordService {
    ledger: Arc<Ledger>,
    events: broadcast::Sender<Event>,
}

fn error(message: impl Into<String>) -> Response {
    Response::Error {
        message: message.into(),
    }
}

/// `text` trimmed, em dashes turned into hyphens, and checked to be 1 to
/// 2000 characters. `what` names it in the refusal (`summary`, `report`).
pub fn clean_text(text: &str, what: &str) -> Result<String, String> {
    let text = text.replace('\u{2014}', "-");
    let text = text.trim();
    if text.is_empty() {
        return Err(format!("the {what} is empty"));
    }
    if text.chars().count() > MAX_TEXT {
        return Err(format!("the {what} is longer than {MAX_TEXT} characters"));
    }
    Ok(text.to_string())
}

/// Refuses anyone but S0 and Jonathan (`from` of `None`, through `ctl`).
/// `what` completes the refusal, such as `set the board summary`.
pub fn coordinator_only(from: Option<SlotId>, what: &str) -> Result<(), String> {
    match from {
        None | Some(0) => Ok(()),
        Some(_) => Err(format!("only the coordinator (S0) can {what}")),
    }
}

impl CoordService {
    pub fn new(ledger: Arc<Ledger>, events: broadcast::Sender<Event>) -> Arc<Self> {
        Arc::new(CoordService { ledger, events })
    }

    pub fn view(&self) -> CoordView {
        fn or_default<T: Default>(what: &str, r: anyhow::Result<T>) -> T {
            r.unwrap_or_else(|e| {
                tracing::warn!("coordinator {what} unreadable: {e:#}");
                T::default()
            })
        }
        CoordView {
            summary: or_default("summary", self.ledger.summary()),
            plans: or_default("plans", self.ledger.plans(VIEW_PLANS)),
            reports: or_default("reports", self.ledger.reports(VIEW_REPORTS)),
            status_asked: or_default("status requests", self.ledger.status_asked()),
        }
    }

    fn announce(&self) {
        let _ = self.events.send(Event::CoordChanged(self.view()));
    }

    /// Queues a relay from Overseer for `to`; announced as a lock change.
    fn tell(&self, locks: &LockService, to: SlotId, text: &str, now: i64) -> anyhow::Result<()> {
        locks.change("coordinator relay", |conn| {
            queue_relay_on(conn, &Sender::Overseer, to, text, None, now).map(|_| ())
        })
    }

    /// `set_board_summary`: S0 (or Jonathan) replaces the summary.
    pub fn set_summary(&self, from: Option<SlotId>, text: &str, now: i64) -> Response {
        if let Err(message) = coordinator_only(from, "set the board summary") {
            return error(message);
        }
        let text = match clean_text(text, "summary") {
            Ok(t) => t,
            Err(message) => return error(message),
        };
        if let Err(e) = self.ledger.set_summary(&text, now) {
            return error(format!("ledger: {e:#}"));
        }
        self.announce();
        Response::Ok
    }

    /// `report_progress`: records `slot`'s report. When S0 had asked that
    /// slot for a status and is live (`live_s0`), the answer is relayed.
    pub fn report(
        &self,
        slot: SlotId,
        text: &str,
        live_s0: bool,
        locks: &LockService,
        now: i64,
    ) -> Response {
        if !valid_id(slot) {
            return error(format!("invalid slot id {slot}"));
        }
        let text = match clean_text(text, "report") {
            Ok(t) => t,
            Err(message) => return error(message),
        };
        let answered = self
            .ledger
            .add_report(slot, ReportKind::Report, &text, now)
            .and_then(|_| self.ledger.answer_status(slot));
        let answered = match answered {
            Ok(a) => a,
            Err(e) => return error(format!("ledger: {e:#}")),
        };
        if answered && live_s0 && slot != 0 {
            let relay = format!("{} reports: {text}", slot_name(slot));
            if let Err(e) = self.tell(locks, 0, &relay, now) {
                return error(format!("ledger: {e:#}"));
            }
        }
        self.announce();
        Response::Ok
    }

    /// `request_status`: asks `slot` (`running` says it has a live session)
    /// for a short status, once until it answers.
    pub fn request_status(
        &self,
        from: Option<SlotId>,
        slot: SlotId,
        running: bool,
        locks: &LockService,
        now: i64,
    ) -> Response {
        if let Err(message) = coordinator_only(from, "ask for a status") {
            return error(message);
        }
        if !valid_id(slot) {
            return error(format!("invalid slot id {slot}"));
        }
        if slot == 0 {
            return error("S0 is the coordinator; ask S1 to S6 for a status");
        }
        if !running {
            return error(format!("{} is not running", slot_name(slot)));
        }
        let before = match self.ledger.ask_status(slot, now) {
            Ok(b) => b,
            Err(e) => return error(format!("ledger: {e:#}")),
        };
        if let Some(asked) = before {
            self.announce();
            return Response::Text {
                text: format!("already asked {} at {}", slot_name(slot), local_hhmm(asked)),
            };
        }
        let ask = "S0 asks for a short status. Answer with the report_progress tool in two or three lines.";
        if let Err(e) = self.tell(locks, slot, ask, now) {
            // Nothing was sent, so nothing is waiting for an answer.
            let _ = self.ledger.answer_status(slot);
            return error(format!("ledger: {e:#}"));
        }
        self.announce();
        Response::Text {
            text: format!(
                "Asked {} for a status. Its answer will be typed in here.",
                slot_name(slot)
            ),
        }
    }

    /// Starts the timeline task: a row for each change of a slot's state or
    /// activity. It holds no lock across an await. Abort the handle to stop.
    pub fn watch(self: &Arc<Self>, mut events: broadcast::Receiver<Event>) -> JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            // The last (state, activity) seen per slot, to spot changes.
            let mut last: HashMap<SlotId, (u8, Option<ActivityKind>)> = HashMap::new();
            loop {
                match events.recv().await {
                    Ok(Event::SlotChanged(info)) => this.note_change(&mut last, &info),
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("coordinator timeline lagged by {n} events");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }

    fn note_change(&self, last: &mut HashMap<SlotId, (u8, Option<ActivityKind>)>, info: &SlotInfo) {
        let (state, activity) = match &info.state {
            SlotState::Running { .. } => (1, info.activity.as_ref()),
            SlotState::Exited { .. } => (2, None),
            // No session to speak of: forget the slot's last state.
            SlotState::Empty | SlotState::Resumable { .. } => {
                last.remove(&info.id);
                return;
            }
        };
        let key = (state, activity.map(|a| a.kind));
        let before = last.insert(info.id, key);
        if before == Some(key) {
            return;
        }
        let text = match (state, activity) {
            (2, _) => "exited".to_string(),
            (_, None) => {
                // Activity cleared while running is not a new start.
                if before.is_some_and(|(s, _)| s == 1) {
                    return;
                }
                "started".to_string()
            }
            (_, Some(a)) => match a.kind {
                ActivityKind::Working => "working".to_string(),
                ActivityKind::Idle => "idle".to_string(),
                ActivityKind::NeedsYou => match a.reason.as_deref().filter(|r| !r.is_empty()) {
                    Some(reason) => format!("needs you: {reason}"),
                    None => "needs you".to_string(),
                },
            },
        };
        let now = crate::work::store::unix_now();
        match self
            .ledger
            .add_report(info.id, ReportKind::State, &text, now)
        {
            Ok(_) => self.announce(),
            Err(e) => tracing::warn!("timeline row for {}: {e:#}", slot_name(info.id)),
        }
    }
}
