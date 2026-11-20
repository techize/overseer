//! Relays: messages queued for a session, from the user, S0, a worker
//! answering one, or Overseer itself. This module decides who may send and
//! queues the relay in the ledger; the courier types it into the terminal.

pub mod courier;
pub mod screen;

use overseer_proto::msg::{slot_name, Sender};
use overseer_proto::{Response, SlotId};

use crate::locks::table::{queue_relay_on, unanswered_from_on, waiting_from_on};
use crate::locks::LockService;
use crate::manager::valid_id;

/// The longest relay text, in characters.
pub const MAX_TEXT: usize = 4000;

fn error(message: impl Into<String>) -> Response {
    Response::Error {
        message: message.into(),
    }
}

/// Whether `from` may message `to`. The user (`None`) and S0 always may.
/// A worker may answer a delivered relay from `to` that it has not answered
/// yet (`unanswered`), and the answer carries that relay as its `reply_to`.
/// A worker may also start a message to S0, but only while none of its
/// messages to S0 is still waiting to be typed in (`waiting`), so it cannot
/// flood the coordinator. Workers never start a conversation with each
/// other.
pub fn may_send(
    from: Option<SlotId>,
    to: SlotId,
    unanswered: Option<i64>,
    waiting: Option<i64>,
) -> Result<Option<i64>, String> {
    match from {
        None => Ok(None),
        Some(f) if f == to => Err("you cannot message yourself".into()),
        Some(0) => Ok(None),
        Some(_) if unanswered.is_some() => Ok(unanswered),
        Some(f) if to == 0 => match waiting {
            None => Ok(None),
            Some(id) => Err(format!(
                "{} already has a message waiting for S0 (relay {id}). Wait until it \
                 is typed in, or drop it with drop_relay and send one message.",
                slot_name(f)
            )),
        },
        Some(f) => Err(format!(
            "{} can message {} only to answer a message from {}. Workers may \
             message S0 at any time.",
            slot_name(f),
            slot_name(to),
            slot_name(to)
        )),
    }
}

/// `SendRelay`: validates, applies the reply rule and queues the relay.
/// `live` says whether `to` has a session (running or resumable); a relay
/// for a slot without one would only wait for the next session, which is
/// not who it was for, so it is refused.
pub fn send(
    locks: &LockService,
    from: Option<SlotId>,
    to: SlotId,
    text: &str,
    live: bool,
    now: i64,
) -> Response {
    if !valid_id(to) {
        return error(format!("invalid slot id {to}"));
    }
    if let Some(f) = from.filter(|f| !valid_id(*f)) {
        return error(format!("invalid slot id {f}"));
    }
    if text.trim().is_empty() {
        return error("the message is empty");
    }
    if text.chars().count() > MAX_TEXT {
        return error(format!("the message is over {MAX_TEXT} characters"));
    }
    let sender = from.map_or(Sender::You, |slot| Sender::Slot { slot });
    let done = locks.change("relay", |conn| {
        let (unanswered, waiting) = match from {
            Some(f) if f != 0 && f != to => (
                unanswered_from_on(conn, f, to)?,
                if to == 0 {
                    waiting_from_on(conn, f, 0)?
                } else {
                    None
                },
            ),
            _ => (None, None),
        };
        match may_send(from, to, unanswered, waiting) {
            Ok(_) if !live => Ok(Err(format!("{} has no session to message.", slot_name(to)))),
            Ok(reply_to) => queue_relay_on(conn, &sender, to, text, reply_to, now).map(|_| Ok(())),
            Err(refusal) => Ok(Err(refusal)),
        }
    });
    match done {
        Ok(Ok(())) => Response::Text {
            text: format!(
                "Queued for {0}. It will be typed in when {0} is idle.",
                slot_name(to)
            ),
        },
        Ok(Err(refusal)) => error(refusal),
        Err(e) => error(format!("ledger: {e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jonathan_and_s0_always_pass_without_a_reply_to() {
        assert_eq!(may_send(None, 3, None, None), Ok(None));
        assert_eq!(may_send(Some(0), 3, None, None), Ok(None));
        assert_eq!(may_send(Some(0), 3, Some(9), Some(4)), Ok(None));
    }

    #[test]
    fn a_slot_cannot_message_itself() {
        assert_eq!(
            may_send(Some(2), 2, Some(1), None),
            Err("you cannot message yourself".into())
        );
        assert_eq!(
            may_send(Some(0), 0, None, None),
            Err("you cannot message yourself".into())
        );
    }

    #[test]
    fn a_worker_answers_with_the_relay_it_replies_to() {
        assert_eq!(may_send(Some(2), 0, Some(7), None), Ok(Some(7)));
        assert_eq!(may_send(Some(2), 0, Some(7), Some(8)), Ok(Some(7)));
        assert_eq!(may_send(Some(2), 1, Some(5), None), Ok(Some(5)));
    }

    #[test]
    fn a_worker_may_start_a_message_to_s0() {
        assert_eq!(may_send(Some(2), 0, None, None), Ok(None));
    }

    #[test]
    fn a_worker_has_one_message_waiting_for_s0_at_a_time() {
        assert_eq!(
            may_send(Some(2), 0, None, Some(8)),
            Err(
                "S2 already has a message waiting for S0 (relay 8). Wait until it is \
                 typed in, or drop it with drop_relay and send one message."
                    .into()
            )
        );
    }

    #[test]
    fn a_worker_cannot_start_a_message_to_another_worker() {
        assert_eq!(
            may_send(Some(2), 1, None, None),
            Err(
                "S2 can message S1 only to answer a message from S1. Workers may \
                 message S0 at any time."
                    .into()
            )
        );
    }
}
