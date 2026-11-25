//! A slot's stretches of work, one per Claude transcript (#80).
//!
//! A slot's process can outlive several transcripts: `/clear` starts a new
//! one in the same session, and its work needs a session log of its own.
//! The run times recorded when a session ends cannot show where one
//! stretch stops and the next starts, so each stretch is tracked from the
//! hooks of the slot's main conversation: from its first prompt, tool call
//! or turn end to its last. A `SessionStart` with a new transcript id ends
//! the stretch before it, and so does the session's end.
//!
//! Tracked in memory and stored in the ledger as it goes: a daemon restart
//! seeds the open stretch back from its row, so it keeps its start.

use overseer_proto::{HookKind, SlotId};
use std::collections::HashMap;

/// A hook event the slot manager applied to a slot's main conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub slot: SlotId,
    /// The transcript the event belongs to.
    pub session: String,
    pub kind: HookKind,
    /// Unix seconds.
    pub at: i64,
}

/// One transcript's work in a slot, unix seconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stretch {
    pub slot: SlotId,
    pub session: String,
    pub first: i64,
    pub last: i64,
}

/// Events that are work: a prompt, a tool call, a turn's end. A session
/// starting, or a notification, is not.
fn is_work(kind: HookKind) -> bool {
    matches!(
        kind,
        HookKind::UserPromptSubmit | HookKind::PostToolUse | HookKind::Stop
    )
}

#[derive(Debug, Default)]
pub struct Stretches {
    /// Per slot: the open transcript and the times of its first and last
    /// work, once there was any.
    open: HashMap<SlotId, (String, Option<(i64, i64)>)>,
}

impl Stretches {
    /// Takes in one event. Returns the stretch it ended (the slot moved to
    /// a new transcript), when that stretch had any work.
    pub fn seen(&mut self, s: &Seen) -> Option<Stretch> {
        let mut ended = None;
        match self.open.get_mut(&s.slot) {
            Some(open) if open.0 != s.session => {
                let (session, times) = std::mem::replace(open, (s.session.clone(), None));
                ended = times.map(|(first, last)| Stretch {
                    slot: s.slot,
                    session,
                    first,
                    last,
                });
            }
            Some(_) => {}
            None => {
                self.open.insert(s.slot, (s.session.clone(), None));
            }
        }
        if is_work(s.kind) {
            let times = &mut self.open.get_mut(&s.slot).expect("inserted above").1;
            *times = Some(match *times {
                Some((first, _)) => (first, s.at),
                None => (s.at, s.at),
            });
        }
        ended
    }

    /// The slot's open stretch, if it has had any work.
    pub fn open(&self, slot: SlotId) -> Option<Stretch> {
        let (session, times) = self.open.get(&slot)?;
        times.map(|(first, last)| Stretch {
            slot,
            session: session.clone(),
            first,
            last,
        })
    }

    /// Puts back a stored stretch that is still running, so its next work
    /// extends it rather than starting a new one.
    pub fn seed(&mut self, s: &Stretch) {
        self.open
            .insert(s.slot, (s.session.clone(), Some((s.first, s.last))));
    }

    /// The slot's session ended: its open stretch, if it had any work.
    pub fn ended(&mut self, slot: SlotId) -> Option<Stretch> {
        let (session, times) = self.open.remove(&slot)?;
        times.map(|(first, last)| Stretch {
            slot,
            session,
            first,
            last,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(slot: SlotId, session: &str, kind: HookKind, at: i64) -> Seen {
        Seen {
            slot,
            session: session.into(),
            kind,
            at,
        }
    }

    #[test]
    fn a_new_transcript_ends_the_stretch_before_it() {
        let mut s = Stretches::default();
        assert_eq!(s.seen(&ev(2, "a", HookKind::SessionStart, 100)), None);
        assert_eq!(s.seen(&ev(2, "a", HookKind::UserPromptSubmit, 110)), None);
        assert_eq!(s.seen(&ev(2, "a", HookKind::PostToolUse, 150)), None);
        assert_eq!(s.seen(&ev(2, "a", HookKind::Stop, 200)), None);
        // A notification after the turn is not work.
        assert_eq!(s.seen(&ev(2, "a", HookKind::Notification, 900)), None);
        let ended = s.seen(&ev(2, "b", HookKind::SessionStart, 1000));
        assert_eq!(
            ended,
            Some(Stretch {
                slot: 2,
                session: "a".into(),
                first: 110,
                last: 200
            })
        );
        s.seen(&ev(2, "b", HookKind::UserPromptSubmit, 1010));
        s.seen(&ev(2, "b", HookKind::Stop, 1500));
        assert_eq!(
            s.ended(2),
            Some(Stretch {
                slot: 2,
                session: "b".into(),
                first: 1010,
                last: 1500
            })
        );
        assert_eq!(s.ended(2), None);
    }

    #[test]
    fn open_reports_the_running_stretch() {
        let mut s = Stretches::default();
        s.seen(&ev(2, "a", HookKind::SessionStart, 100));
        assert_eq!(s.open(2), None, "no work yet");
        s.seen(&ev(2, "a", HookKind::UserPromptSubmit, 110));
        s.seen(&ev(2, "a", HookKind::Stop, 150));
        assert_eq!(
            s.open(2),
            Some(Stretch {
                slot: 2,
                session: "a".into(),
                first: 110,
                last: 150
            })
        );
        assert_eq!(s.open(3), None);
        s.ended(2);
        assert_eq!(s.open(2), None);
    }

    #[test]
    fn seeded_stretch_is_extended_not_restarted() {
        let mut s = Stretches::default();
        s.seed(&Stretch {
            slot: 2,
            session: "a".into(),
            first: 100,
            last: 200,
        });
        assert_eq!(s.seen(&ev(2, "a", HookKind::Stop, 300)), None);
        assert_eq!(s.open(2).map(|x| (x.first, x.last)), Some((100, 300)));
    }

    #[test]
    fn a_transcript_with_no_work_is_no_stretch_and_slots_are_apart() {
        let mut s = Stretches::default();
        s.seen(&ev(1, "a", HookKind::SessionStart, 100));
        s.seen(&ev(3, "c", HookKind::UserPromptSubmit, 120));
        assert_eq!(s.seen(&ev(1, "b", HookKind::SessionStart, 200)), None);
        assert_eq!(s.ended(1), None);
        assert_eq!(s.ended(3).map(|x| (x.first, x.last)), Some((120, 120)));
    }
}
