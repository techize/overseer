//! The courier: types pending relays into a slot's terminal, but only when
//! the session is ready for them (spec 7.6).
//!
//! A relay goes when the session is idle, or waits at an idle-input
//! `Notification`, and its input box is empty. It waits while the session
//! works, at a permission or question prompt, while Jonathan has text in
//! the box, and whenever the box cannot be found. Every pending relay for a
//! slot goes in one paste, then Return.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use overseer_proto::msg::{slot_name, Sender};
use overseer_proto::{Activity, ActivityKind, Event, Relay, SlotId, SlotInfo, SlotState};
use overseer_term::encode_paste;
use tokio::sync::{broadcast, Notify};
use tokio::time::Instant;

use super::screen::{input_box, screen_of, InputBox};
use crate::ledger::Ledger;
use crate::locks::table::{mark_delivered_on, pending_relays_on};
use crate::locks::LockService;
use crate::manager::{SlotManager, SLOT_COUNT};
use crate::work::store::unix_now;

/// How often slots with pending relays are looked at again.
const TICK: Duration = Duration::from_secs(1);
/// Between the paste and its Return, as for a kickoff.
const RETURN_DELAY: Duration = Duration::from_millis(100);
/// After a delivery the slot is left alone until its activity changes or
/// this passes, so one idle state never gets two pastes.
const COOLDOWN: Duration = Duration::from_secs(10);

const NOT_RUNNING: &str = "slot is not running";
const TYPED: &str = "input box has text";
const NO_BOX: &str = "input box not found";

/// Whether a slot in `state` doing `activity` may be typed into now, else
/// why not.
pub fn ready(state: &SlotState, activity: Option<&Activity>) -> Result<(), &'static str> {
    if !matches!(state, SlotState::Running { .. }) {
        return Err(NOT_RUNNING);
    }
    let a = activity.ok_or("no activity reported yet")?;
    match a.kind {
        ActivityKind::Idle => Ok(()),
        ActivityKind::NeedsYou if a.idle_wait => Ok(()),
        ActivityKind::NeedsYou => Err("waiting at a prompt for Jonathan"),
        ActivityKind::Working => Err("session is working"),
    }
}

/// Overseer's own relays that already read as a message from Overseer.
fn speaks_for_itself(text: &str) -> bool {
    if text.starts_with("Jonathan released") {
        return true;
    }
    let Some(rest) = text.strip_prefix('S') else {
        return false;
    };
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && rest[digits..].starts_with(" has released")
}

/// The text typed into slot `to` for `relays`, one paragraph each.
pub fn compose(relays: &[Relay], to: SlotId) -> String {
    let one = |r: &Relay| match r.from {
        Sender::Slot { slot } => {
            let mut s = format!("Message from {} via Overseer: {}", slot_name(slot), r.text);
            if to != 0 {
                s.push_str("\n(You can answer once with notify_session.)");
            }
            s
        }
        Sender::You => format!("Message from Jonathan via Overseer: {}", r.text),
        Sender::Overseer if speaks_for_itself(&r.text) => r.text.clone(),
        Sender::Overseer => format!("Overseer: {}", r.text),
    };
    relays.iter().map(one).collect::<Vec<_>>().join("\n\n")
}

/// A delivery to a slot: the activity it was made in, and when.
struct Sent {
    activity: Option<Activity>,
    at: Instant,
}

pub struct Courier {
    ledger: Arc<Ledger>,
    manager: Arc<Mutex<SlotManager>>,
    locks: Arc<LockService>,
    wake: Notify,
}

impl Courier {
    /// Starts the delivery loop. `events` should be subscribed before any
    /// slot starts.
    pub fn spawn(
        ledger: Arc<Ledger>,
        manager: Arc<Mutex<SlotManager>>,
        locks: Arc<LockService>,
        events: broadcast::Receiver<Event>,
    ) -> Arc<Courier> {
        let courier = Arc::new(Courier {
            ledger,
            manager,
            locks,
            wake: Notify::new(),
        });
        tokio::spawn(courier.clone().run(events));
        courier
    }

    /// Something may have a relay to deliver: look now.
    pub fn poke(&self) {
        self.wake.notify_one();
    }

    async fn run(self: Arc<Self>, mut events: broadcast::Receiver<Event>) {
        let mut sent: [Option<Sent>; SLOT_COUNT] = Default::default();
        loop {
            let pending = self.round(&mut sent).await;
            let tick = async {
                if pending {
                    tokio::time::sleep(TICK).await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            tokio::pin!(tick);
            loop {
                tokio::select! {
                    _ = self.wake.notified() => break,
                    _ = &mut tick => break,
                    ev = events.recv() => match ev {
                        Ok(Event::SlotChanged(_)) | Err(broadcast::error::RecvError::Lagged(_)) => break,
                        // Including the courier's own LocksChanged.
                        Ok(_) => {}
                        Err(broadcast::error::RecvError::Closed) => return,
                    },
                }
            }
        }
    }

    /// One look at every slot. Returns whether any relay is still pending.
    async fn round(&self, sent: &mut [Option<Sent>; SLOT_COUNT]) -> bool {
        let infos = self.manager.lock().unwrap().list();
        let mut pending = false;
        for info in infos {
            let id = info.id;
            let relays = match self.ledger.transaction(|c| pending_relays_on(c, id)) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("{} pending relays unreadable: {e:#}", slot_name(id));
                    pending = true;
                    continue;
                }
            };
            if relays.is_empty() {
                self.locks.set_held(id, None);
                continue;
            }
            pending = true;
            if let Some(s) = &sent[id as usize] {
                if s.activity == info.activity && s.at.elapsed() < COOLDOWN {
                    continue;
                }
            }
            sent[id as usize] = None;
            match self.deliver(&info, &relays).await {
                Ok(()) => {
                    sent[id as usize] = Some(Sent {
                        activity: info.activity.clone(),
                        at: Instant::now(),
                    })
                }
                Err(reason) => self.locks.set_held(id, Some(reason)),
            }
        }
        pending
    }

    /// Types `relays` into the slot `info` describes if it is ready, and
    /// marks them delivered. Otherwise the reason they wait.
    async fn deliver(&self, info: &SlotInfo, relays: &[Relay]) -> Result<(), &'static str> {
        let id = info.id;
        ready(&info.state, info.activity.as_ref())?;
        let slot = self.manager.lock().unwrap().slot(id).ok_or(NOT_RUNNING)?;
        let (cols, rows) = (info.cols, info.rows);
        // The ring with its mode preamble, as a client attaching would see it.
        let reader = slot.clone();
        let screen = tokio::task::spawn_blocking(move || {
            let (bytes, _) = reader.attach();
            let (snap, modes) = screen_of(&bytes, cols, rows);
            (input_box(&snap), modes)
        })
        .await;
        let modes = match screen {
            Ok((InputBox::Empty, modes)) => modes,
            Ok((InputBox::Typed, _)) => return Err(TYPED),
            Ok((InputBox::NotFound, _)) | Err(_) => return Err(NO_BOX),
        };
        // A hook may have come in while the screen was read.
        let latest = self.manager.lock().unwrap().list().swap_remove(id as usize);
        ready(&latest.state, latest.activity.as_ref())?;
        self.locks.set_held(id, None);
        let paste = encode_paste(&compose(relays, id), &modes);
        let writer = slot.clone();
        let wrote = tokio::task::spawn_blocking(move || writer.write(&paste)).await;
        if let Ok(Err(e)) | Err(e) = wrote.map_err(std::io::Error::other) {
            tracing::warn!("relay to {} not written: {e}", slot_name(id));
            return Err(NOT_RUNNING);
        }
        tokio::time::sleep(RETURN_DELAY).await;
        // The text is in the box now; without its Return it still went.
        if let Err(e) = slot.write(b"\r") {
            tracing::warn!("relay to {} not submitted: {e}", slot_name(id));
        }
        let ids: Vec<i64> = relays.iter().map(|r| r.id).collect();
        let now = unix_now();
        let _ = self
            .locks
            .change("relay delivery", |c| mark_delivered_on(c, &ids, now));
        tracing::info!("{} relay(s) delivered to {}", ids.len(), slot_name(id));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use overseer_proto::RelayState;

    fn act(kind: ActivityKind, idle_wait: bool) -> Activity {
        Activity {
            kind,
            since: 1,
            now_doing: None,
            reason: None,
            idle_wait,
        }
    }

    const RUNNING: SlotState = SlotState::Running { pid: 1, since: 1 };

    #[test]
    fn ready_only_when_running_and_idle_or_idle_waiting() {
        use ActivityKind::*;
        let cases = [
            (Idle, false, Ok(())),
            (Idle, true, Ok(())),
            (NeedsYou, true, Ok(())),
            (NeedsYou, false, Err("waiting at a prompt for Jonathan")),
            (Working, false, Err("session is working")),
            (Working, true, Err("session is working")),
        ];
        for (kind, wait, want) in cases {
            let a = act(kind, wait);
            assert_eq!(ready(&RUNNING, Some(&a)), want, "{kind:?} {wait}");
        }
        assert_eq!(ready(&RUNNING, None), Err("no activity reported yet"));
    }

    #[test]
    fn a_slot_that_is_not_running_is_never_ready() {
        let idle = act(ActivityKind::Idle, false);
        for state in [
            SlotState::Empty,
            SlotState::Exited { code: None, at: 1 },
            SlotState::Resumable {
                session_id: "s".into(),
                at: 1,
            },
        ] {
            assert_eq!(ready(&state, Some(&idle)), Err("slot is not running"));
            assert_eq!(ready(&state, None), Err("slot is not running"));
        }
    }

    fn relay(from: Sender, text: &str) -> Relay {
        Relay {
            id: 1,
            at: 1,
            from,
            to: 2,
            text: text.into(),
            state: RelayState::Pending,
            delivered_at: None,
            held: None,
        }
    }

    #[test]
    fn compose_names_the_sender() {
        assert_eq!(
            compose(&[relay(Sender::Slot { slot: 0 }, "status?")], 2),
            "Message from S0 via Overseer: status?\n(You can answer once with notify_session.)"
        );
        // S0 is not a worker: no reply line.
        assert_eq!(
            compose(&[relay(Sender::Slot { slot: 2 }, "on it")], 0),
            "Message from S2 via Overseer: on it"
        );
        assert_eq!(
            compose(&[relay(Sender::You, "hello")], 2),
            "Message from Jonathan via Overseer: hello"
        );
        assert_eq!(
            compose(&[relay(Sender::You, "hello")], 0),
            "Message from Jonathan via Overseer: hello"
        );
    }

    #[test]
    fn compose_keeps_handoffs_and_prefixes_other_notices() {
        for text in [
            "S1 has released deploy:demo/api. You now hold it.",
            "S12 has released repo:demo-api:main. You now hold it.",
            "Jonathan released custom:z. You now hold it.",
        ] {
            assert_eq!(compose(&[relay(Sender::Overseer, text)], 2), text);
        }
        assert_eq!(
            compose(&[relay(Sender::Overseer, "DEMO-40 was moved to S2")], 1),
            "Overseer: DEMO-40 was moved to S2"
        );
        assert_eq!(
            compose(&[relay(Sender::Overseer, "S has released x")], 1),
            "Overseer: S has released x"
        );
    }

    #[test]
    fn compose_puts_each_relay_in_its_own_paragraph() {
        let text = compose(
            &[
                relay(Sender::Slot { slot: 0 }, "status?"),
                relay(Sender::You, "and the tests"),
                relay(Sender::Overseer, "DEMO-40 was moved to S2"),
            ],
            2,
        );
        assert_eq!(
            text,
            "Message from S0 via Overseer: status?\n(You can answer once with notify_session.)\n\n\
             Message from Jonathan via Overseer: and the tests\n\n\
             Overseer: DEMO-40 was moved to S2"
        );
    }
}
