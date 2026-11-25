//! The courier: types pending relays into a slot's terminal, but only when
//! the session is ready for them (spec 7.6).
//!
//! A relay goes when the session is idle, or waits at an idle-input
//! `Notification`, and its input box is empty. It waits while the session
//! works, at a permission or question prompt, while the user has text in
//! the box, and whenever the box cannot be found. Every pending relay for a
//! slot goes in one paste, then Return.
//!
//! Answering a permission prompt with No ends the turn without a `Stop`
//! hook, so the slot still reads as needing the user. A relay goes anyway
//! once the screen has shown the empty box, with no working line, on two
//! looks in a row.
//!
//! A slot that reads as working, but whose screen has shown the empty box
//! and no working line for `QUIET_FOR`, is idle too: its `Stop` was lost,
//! or something marked it working after the turn ended (#58).
//!
//! Each change in why a slot's relays wait is logged, and a box that cannot
//! be found logs the bottom of the screen, so a stuck relay can be traced.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use overseer_proto::msg::{slot_name, Sender};
use overseer_proto::{Activity, ActivityKind, Event, Relay, SlotId, SlotInfo, SlotState};
use overseer_term::{encode_paste, Snapshot, TermModes};
use tokio::sync::{broadcast, Notify};
use tokio::time::Instant;

use super::screen::{input_box, shows_working, InputBox};
use crate::ledger::Ledger;
use crate::locks::table::{claim_batch_on, claim_stale_on, pending_relays_on, unclaim_on};
use crate::locks::LockService;
use crate::manager::{SlotManager, SLOT_COUNT};
use crate::slot::Slot;
use crate::work::store::unix_now;

/// How often slots with pending relays are looked at again.
const TICK: Duration = Duration::from_secs(1);
/// Between the paste and its Return, as for a kickoff.
pub(crate) const RETURN_DELAY: Duration = Duration::from_millis(100);
/// After a delivery the slot is left alone until its activity changes or
/// this passes, so one idle state never gets two pastes.
const COOLDOWN: Duration = Duration::from_secs(10);
/// How long a slot that reads as working must show the empty box and no
/// working line before it counts as idle. A running turn always draws its
/// working line, so this only has to outlast a redraw.
const QUIET_FOR: Duration = Duration::from_secs(10);
/// A relay pending this long also goes out in the slot's next Overseer
/// MCP tool reply, for when it cannot be typed in (#58).
pub const STALE_AFTER: Duration = Duration::from_secs(5 * 60);
/// Heads relays shown in a tool reply.
const STALE_HEADER: &str =
    "Overseer could not type these messages into your session, so here they are:";
/// Screen rows logged when the input box cannot be found.
const LOGGED_ROWS: usize = 12;

const NOT_RUNNING: &str = "slot is not running";
pub(crate) const WORKING: &str = "session is working";
/// The hold reason for a slot at a prompt. The board shows it as
/// `waiting at a prompt for <user>`: see `held_text`.
pub(crate) const AT_PROMPT: &str = "waiting at a prompt";
const TYPED: &str = "input box has text";
pub(crate) const NO_BOX: &str = "input box not found";
/// A slot attached after a restart, before the redraw its rebuilt screen
/// needs has arrived: the screen cannot be read yet.
const REDRAWING: &str = "screen is being redrawn";
/// Inside the cooldown after a delivery.
const JUST_SENT: &str = "just delivered, waiting for the session";
/// Without bracketed paste every newline is an Enter, and a later one could
/// land on a prompt the first one brought up.
pub(crate) const NO_BRACKETS: &str = "bracketed paste is off";
/// The board is typing into the slot (an answer, reply or stop): the
/// relay goes on a later look, once it is done.
pub(crate) const TYPING: &str = "the board is typing";
/// The ledger could not claim the batch for a paste.
const CLAIM_FAILED: &str = "relays could not be claimed";

/// What typing into a slot needs: its keys and its live screen.
pub(crate) trait Keyboard: Send + Sync {
    fn write(&self, data: &[u8]) -> io::Result<()>;
    /// The live screen and its modes, or `None` while it cannot be read:
    /// a slot attached after a restart, before its redraw has arrived.
    fn screen(&self) -> Option<(Snapshot, TermModes)>;
}

impl Keyboard for Slot {
    fn write(&self, data: &[u8]) -> io::Result<()> {
        Slot::write(self, data)
    }

    fn screen(&self) -> Option<(Snapshot, TermModes)> {
        (!self.redraw_pending()).then(|| Slot::screen(self))
    }
}

/// How a paste and its Return went.
pub(crate) enum Pasted {
    Whole,
    /// The text is in the box, but its Return was not written.
    NoReturn(io::Error),
}

/// Types `paste`, then Return `return_delay` later: how a relay or a reply
/// goes in. The caller holds the slot's key lock. `Err` when the paste
/// itself was not written.
pub(crate) async fn paste_then_return(
    keys: Arc<dyn Keyboard>,
    paste: Vec<u8>,
    return_delay: Duration,
) -> io::Result<Pasted> {
    let writer = keys.clone();
    tokio::task::spawn_blocking(move || writer.write(&paste))
        .await
        .map_err(io::Error::other)??;
    tokio::time::sleep(return_delay).await;
    Ok(match keys.write(b"\r") {
        Ok(()) => Pasted::Whole,
        Err(e) => Pasted::NoReturn(e),
    })
}

/// Whether a slot in `state` doing `activity` may be typed into now, else
/// why not. `redraw_pending` is the slot's [`crate::slot::Slot::redraw_pending`].
pub fn ready(
    state: &SlotState,
    activity: Option<&Activity>,
    redraw_pending: bool,
) -> Result<(), &'static str> {
    if !matches!(state, SlotState::Running { .. }) {
        return Err(NOT_RUNNING);
    }
    if redraw_pending {
        return Err(REDRAWING);
    }
    let a = activity.ok_or("no activity reported yet")?;
    match a.kind {
        ActivityKind::Idle => Ok(()),
        ActivityKind::NeedsYou if a.idle_wait => Ok(()),
        ActivityKind::NeedsYou => Err(AT_PROMPT),
        ActivityKind::Working => Err(WORKING),
    }
}

/// A hold reason as the board shows it: the prompt one names `user`.
pub(crate) fn held_text(reason: &str, user: &str) -> String {
    if reason == AT_PROMPT {
        format!("{reason} for {user}")
    } else {
        reason.to_string()
    }
}

/// Overseer's own relays that already read as a message from Overseer.
/// The user's release is told by its shape, not by the name in it:
/// `<user> released <target>, which you held.` or `... You now hold it.`
fn speaks_for_itself(text: &str) -> bool {
    if text.contains(" released ")
        && (text.ends_with(", which you held.") || text.ends_with(". You now hold it."))
    {
        return true;
    }
    let Some(rest) = text.strip_prefix('S') else {
        return false;
    };
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && rest[digits..].starts_with(" has released")
}

/// `text` without control characters other than newline and tab, so a
/// relay can never type a key such as Escape, Ctrl-C or Delete.
pub fn strip_controls(text: &str) -> String {
    text.chars()
        .filter(|&c| (c >= ' ' && c != '\x7f') || c == '\n' || c == '\t')
        .collect()
}

/// The bytes that type `text` with `modes`. Without bracketed paste a Tab
/// is a key (it switches Claude Code's mode), so it goes as spaces.
pub fn typed_paste(text: &str, modes: &TermModes) -> Vec<u8> {
    if modes.bracketed_paste {
        encode_paste(text, modes)
    } else {
        encode_paste(&text.replace('\t', "    "), modes)
    }
}

/// The text typed into slot `to` for `relays`, one paragraph each.
pub fn compose(relays: &[Relay], to: SlotId, user: &str) -> String {
    let one = |r: &Relay| match r.from {
        Sender::Slot { slot } => {
            let mut s = format!("Message from {} via Overseer: {}", slot_name(slot), r.text);
            if to != 0 {
                s.push_str("\n(You can answer once with notify_session.)");
            }
            s
        }
        Sender::You => format!("Message from {user} via Overseer: {}", r.text),
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

/// What the courier remembers about one slot between looks.
#[derive(Default)]
struct Watch {
    /// The activity in which a prompt was last seen answered.
    answered: Option<Activity>,
    /// The working activity in which the screen was first seen quiet (the
    /// empty box, no working line), and when.
    quiet: Option<(Activity, Instant)>,
    /// Why the slot's relays waited on the last look, for the log.
    held: Option<&'static str>,
}

/// The bottom `LOGGED_ROWS` non-blank rows of `snap`, trailing blanks cut,
/// one per line.
fn bottom_rows(snap: &overseer_term::Snapshot) -> String {
    let mut rows: Vec<String> = (0..snap.rows)
        .rev()
        .map(|r| {
            let cells = &snap.cells[r * snap.cols..(r + 1) * snap.cols];
            cells
                .iter()
                .map(|c| c.ch)
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .filter(|r| !r.is_empty())
        .take(LOGGED_ROWS)
        .collect();
    rows.reverse();
    rows.join("\n")
}

pub struct Courier {
    ledger: Arc<Ledger>,
    manager: Arc<Mutex<SlotManager>>,
    locks: Arc<LockService>,
    user: String,
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
        user: String,
    ) -> Arc<Courier> {
        let courier = Arc::new(Courier {
            ledger,
            manager,
            locks,
            user,
            wake: Notify::new(),
        });
        tokio::spawn(courier.clone().run(events));
        courier
    }

    /// Something may have a relay to deliver: look now.
    pub fn poke(&self) {
        self.wake.notify_one();
    }

    /// The text of `slot`'s pending relays, marked delivered at `now`, if
    /// the oldest has waited `STALE_AFTER` or more; else empty. For
    /// `TakeStaleRelays`: the session reads them in an MCP tool's reply.
    /// The claim is the courier's own, so a relay is never both pasted and
    /// shown, and a lock notice that no longer holds is dropped.
    pub fn take_stale(&self, slot: SlotId, now: i64) -> Result<String, String> {
        let stale_by = now - STALE_AFTER.as_secs() as i64;
        let relays = self
            .locks
            .change("stale relays", |c| claim_stale_on(c, slot, stale_by, now))
            .map_err(|e| format!("{e:#}"))?;
        if relays.is_empty() {
            return Ok(String::new());
        }
        let ids: Vec<i64> = relays.iter().map(|r| r.id).collect();
        tracing::info!(
            "relays {ids:?} to {} shown in a tool reply after waiting",
            slot_name(slot)
        );
        Ok(format!(
            "{STALE_HEADER}\n\n{}",
            strip_controls(&compose(&relays, slot, &self.user))
        ))
    }

    async fn run(self: Arc<Self>, mut events: broadcast::Receiver<Event>) {
        let mut sent: [Option<Sent>; SLOT_COUNT] = Default::default();
        let mut watch: [Watch; SLOT_COUNT] = Default::default();
        loop {
            let pending = self.round(&mut sent, &mut watch).await;
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
    async fn round(
        &self,
        sent: &mut [Option<Sent>; SLOT_COUNT],
        watch: &mut [Watch; SLOT_COUNT],
    ) -> bool {
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
            let w = &mut watch[id as usize];
            if relays.is_empty() {
                *w = Watch::default();
                self.locks.set_held(id, None);
                continue;
            }
            pending = true;
            if let Some(s) = &sent[id as usize] {
                if s.activity == info.activity && s.at.elapsed() < COOLDOWN {
                    self.locks.set_held(id, Some(JUST_SENT));
                    continue;
                }
            }
            sent[id as usize] = None;
            let held = match self.deliver(&info, &relays, w).await {
                Ok(true) => {
                    sent[id as usize] = Some(Sent {
                        activity: info.activity.clone(),
                        at: Instant::now(),
                    });
                    None
                }
                // Every relay had gone stale: nothing was typed.
                Ok(false) => None,
                Err(reason) => Some(reason),
            };
            self.locks.set_held(id, held);
            if let Some(reason) = held.filter(|r| w.held != Some(*r)) {
                self.log_wait(id, &relays, reason);
            }
            w.held = held;
        }
        pending
    }

    /// Logs why `relays` to `id` wait, and for a box that cannot be found,
    /// what the screen shows instead.
    fn log_wait(&self, id: SlotId, relays: &[Relay], reason: &str) {
        let ids: Vec<i64> = relays.iter().map(|r| r.id).collect();
        tracing::info!("relays {ids:?} to {} wait: {reason}", slot_name(id));
        if reason == NO_BOX {
            if let Some(slot) = self.manager.lock().unwrap().slot(id) {
                let (snap, _) = slot.screen();
                tracing::info!(
                    "{} screen with no input box found:\n{}",
                    slot_name(id),
                    bottom_rows(&snap)
                );
            }
        }
    }

    /// Types `relays` into the slot `info` describes if it is ready, and
    /// marks them delivered: `Ok(true)`. Otherwise the reason they wait.
    ///
    /// Just before the paste the batch is claimed (`claim_batch_on`): a
    /// relay dropped since it was read is left out, and a lock notice whose
    /// target the slot no longer holds is dropped, not typed. `Ok(false)`
    /// says none were left, so nothing was typed. If the paste fails, the
    /// claimed relays go back to pending.
    ///
    /// A slot waiting at a prompt is ready once the prompt is answered: the
    /// screen shows the empty box and no working line on this look and on
    /// the one before, in the same activity. A slot that reads as working
    /// is ready once the screen has shown that for `QUIET_FOR` in the same
    /// activity. `watch` carries both between looks.
    async fn deliver(
        &self,
        info: &SlotInfo,
        relays: &[Relay],
        watch: &mut Watch,
    ) -> Result<bool, &'static str> {
        let id = info.id;
        let slot = self.manager.lock().unwrap().slot(id);
        let redrawing = slot.as_ref().is_some_and(|s| s.redraw_pending());
        let held = match ready(&info.state, info.activity.as_ref(), redrawing) {
            Ok(()) => None,
            Err(why @ (AT_PROMPT | WORKING)) => Some(why),
            Err(why) => {
                watch.answered = None;
                watch.quiet = None;
                return Err(why);
            }
        };
        // Never waited for: the round serves every slot in turn. The screen
        // is read under the lock, so it shows what the board just typed, and
        // nothing else types into the slot until this Return is in.
        let lock = self.manager.lock().unwrap().key_lock(id);
        let Ok(_typing) = lock.try_lock() else {
            return Err(TYPING);
        };
        let (at_prompt, working_hook) = (held == Some(AT_PROMPT), held == Some(WORKING));
        if !at_prompt {
            watch.answered = None;
        }
        if !working_hook {
            watch.quiet = None;
        }
        let slot = slot.ok_or(NOT_RUNNING)?;
        // The live screen, not a replay of the ring: the ring loses the box's
        // rules once it wraps, as they are drawn only once (#21).
        let (snap, modes) = slot.screen();
        let (found, working) = (input_box(&snap), shows_working(&snap));
        if at_prompt {
            if found != InputBox::Empty || working {
                watch.answered = None;
                return Err(AT_PROMPT);
            }
            if watch.answered != info.activity {
                // Seen once; deliver if the next look agrees.
                watch.answered = info.activity.clone();
                return Err(AT_PROMPT);
            }
        }
        if working_hook {
            let quiet = found == InputBox::Empty && !working;
            let now = info.activity.clone().ok_or(WORKING)?;
            match &watch.quiet {
                _ if !quiet => {
                    watch.quiet = None;
                    return Err(WORKING);
                }
                Some((a, since)) if *a == now => {
                    if since.elapsed() < QUIET_FOR {
                        return Err(WORKING);
                    }
                    tracing::info!(
                        "{} reads as working but its screen has been idle for {}s",
                        slot_name(id),
                        since.elapsed().as_secs()
                    );
                }
                _ => {
                    watch.quiet = Some((now, Instant::now()));
                    return Err(WORKING);
                }
            }
        }
        match found {
            InputBox::Empty => {}
            InputBox::Typed => return Err(TYPED),
            InputBox::NotFound => return Err(NO_BOX),
        }
        // The batch can only shrink once claimed, so if all of it fits,
        // what is left does too.
        let whole = strip_controls(&compose(relays, id, &self.user));
        if whole.contains('\n') && !modes.bracketed_paste {
            return Err(NO_BRACKETS);
        }
        // A hook may have come in while the screen was read.
        let latest = self.manager.lock().unwrap().list().swap_remove(id as usize);
        match ready(
            &latest.state,
            latest.activity.as_ref(),
            slot.redraw_pending(),
        ) {
            Err(why @ (AT_PROMPT | WORKING))
                if held == Some(why) && latest.activity == info.activity => {}
            other => other?,
        }
        // From here a drop of a claimed relay is refused as delivered, and
        // one dropped before is left out.
        let relays = self.claim(id, relays)?;
        watch.answered = None;
        watch.quiet = None;
        if relays.is_empty() {
            self.locks.set_held(id, None);
            return Ok(false);
        }
        let ids: Vec<i64> = relays.iter().map(|r| r.id).collect();
        let text = strip_controls(&compose(&relays, id, &self.user));
        if text.contains('\n') && !modes.bracketed_paste {
            self.unclaim(id, &ids);
            return Err(NO_BRACKETS);
        }
        self.locks.set_held(id, None);
        let paste = typed_paste(&text, &modes);
        match paste_then_return(slot.clone(), paste, RETURN_DELAY).await {
            Ok(Pasted::Whole) => {}
            // The text is in the box now; without its Return it still went.
            Ok(Pasted::NoReturn(e)) => {
                tracing::warn!("relay to {} not submitted: {e}", slot_name(id));
            }
            Err(e) => {
                tracing::warn!("relay to {} not written: {e}", slot_name(id));
                self.unclaim(id, &ids);
                return Err(NOT_RUNNING);
            }
        }
        tracing::info!("{} relay(s) delivered to {}", ids.len(), slot_name(id));
        Ok(true)
    }

    /// Of `relays`, read earlier, the ones still pending less stale lock
    /// notices (which are dropped), marked delivered in one transaction.
    fn claim(&self, id: SlotId, relays: &[Relay]) -> Result<Vec<Relay>, &'static str> {
        let now = unix_now();
        self.locks
            .change("relay delivery", |c| claim_batch_on(c, relays, now))
            .map_err(|e| {
                tracing::warn!("relays to {} not claimed: {e:#}", slot_name(id));
                CLAIM_FAILED
            })
    }

    /// Puts relays claimed for a paste that did not happen back to pending.
    /// A write fails mostly because the session went away, and its end
    /// skipped these as delivered: so relays for slots with no session are
    /// then dropped, and a later session never gets the old one's.
    fn unclaim(&self, id: SlotId, ids: &[i64]) {
        if let Err(e) = self.locks.change("relay unclaim", |c| unclaim_on(c, ids)) {
            tracing::error!(
                "relays {ids:?} to {} not put back, so they read as delivered: {e:#}",
                slot_name(id)
            );
        }
        self.locks.drop_sessionless_relays(unix_now());
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
            (NeedsYou, false, Err("waiting at a prompt")),
            (Working, false, Err("session is working")),
            (Working, true, Err("session is working")),
        ];
        for (kind, wait, want) in cases {
            let a = act(kind, wait);
            assert_eq!(ready(&RUNNING, Some(&a), false), want, "{kind:?} {wait}");
        }
        assert_eq!(
            ready(&RUNNING, None, false),
            Err("no activity reported yet")
        );
    }

    #[test]
    fn a_slot_being_redrawn_is_never_ready() {
        for kind in [
            ActivityKind::Idle,
            ActivityKind::NeedsYou,
            ActivityKind::Working,
        ] {
            let a = act(kind, true);
            assert_eq!(ready(&RUNNING, Some(&a), true), Err(REDRAWING));
        }
        assert_eq!(ready(&RUNNING, None, true), Err(REDRAWING));
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
            assert_eq!(ready(&state, Some(&idle), true), Err("slot is not running"));
            assert_eq!(ready(&state, None, false), Err("slot is not running"));
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
            dropped_at: None,
            kind: overseer_proto::RelayKind::Note,
            drop_reason: None,
            held: None,
            held_since: None,
        }
    }

    #[test]
    fn compose_names_the_sender() {
        assert_eq!(
            compose(&[relay(Sender::Slot { slot: 0 }, "status?")], 2, "Demo"),
            "Message from S0 via Overseer: status?\n(You can answer once with notify_session.)"
        );
        // S0 is not a worker: no reply line.
        assert_eq!(
            compose(&[relay(Sender::Slot { slot: 2 }, "on it")], 0, "Demo"),
            "Message from S2 via Overseer: on it"
        );
        assert_eq!(
            compose(&[relay(Sender::You, "hello")], 2, "Demo"),
            "Message from Demo via Overseer: hello"
        );
        assert_eq!(
            compose(&[relay(Sender::You, "hello")], 0, "Demo"),
            "Message from Demo via Overseer: hello"
        );
    }

    #[test]
    fn compose_keeps_handoffs_and_prefixes_other_notices() {
        for text in [
            "S1 has released deploy:demo/api. You now hold it.",
            "S12 has released repo:demo-api:main. You now hold it.",
            "Demo released custom:z. You now hold it.",
            "Demo released custom:z (back on main), which you held.",
        ] {
            assert_eq!(compose(&[relay(Sender::Overseer, text)], 2, "Demo"), text);
        }
        // Routing does not depend on the name, so a renamed user still routes.
        for user in ["Sam", "Mary Ann", "you"] {
            let text = format!("{user} released custom:z. You now hold it.");
            assert_eq!(compose(&[relay(Sender::Overseer, &text)], 2, user), text);
        }
        assert_eq!(
            compose(
                &[relay(Sender::Overseer, "DEMO-40 was moved to S2")],
                1,
                "Demo"
            ),
            "Overseer: DEMO-40 was moved to S2"
        );
        assert_eq!(
            compose(&[relay(Sender::Overseer, "S has released x")], 1, "Demo"),
            "Overseer: S has released x"
        );
    }

    #[test]
    fn bottom_rows_keeps_the_last_non_blank_rows_trimmed() {
        let mut out = String::from("\x1b[2J\x1b[H");
        for i in 0..20 {
            out.push_str(&format!("row {i}   \r\n"));
        }
        out.push_str("\r\n  ? for shortcuts");
        let (snap, _) = super::super::screen::screen_of(out.as_bytes(), 40, 30);
        let rows = bottom_rows(&snap);
        let lines: Vec<&str> = rows.lines().collect();
        assert_eq!(lines.len(), LOGGED_ROWS);
        assert_eq!(lines[0], "row 9");
        assert_eq!(lines[LOGGED_ROWS - 2], "row 19");
        assert_eq!(lines[LOGGED_ROWS - 1], "  ? for shortcuts");
    }

    #[test]
    fn the_prompt_hold_names_the_user() {
        assert_eq!(held_text(AT_PROMPT, "Demo"), "waiting at a prompt for Demo");
        assert_eq!(held_text(TYPED, "Demo"), TYPED);
    }

    #[test]
    fn strip_controls_keeps_only_newline_and_tab() {
        assert_eq!(
            strip_controls("a\x1b[201~b\x03c\r\nd\te\x00\x7f"),
            "a[201~bc\nd\te"
        );
        assert_eq!(strip_controls("plain text"), "plain text");
    }

    #[test]
    fn a_tab_is_typed_as_spaces_without_bracketed_paste() {
        let plain = TermModes::default();
        assert_eq!(typed_paste("a\tb", &plain), b"a    b");
        let bracketed = TermModes {
            bracketed_paste: true,
            ..TermModes::default()
        };
        assert_eq!(typed_paste("a\tb", &bracketed), b"\x1b[200~a\tb\x1b[201~");
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
            "Demo",
        );
        assert_eq!(
            text,
            "Message from S0 via Overseer: status?\n(You can answer once with notify_session.)\n\n\
             Message from Demo via Overseer: and the tests\n\n\
             Overseer: DEMO-40 was moved to S2"
        );
    }
}
