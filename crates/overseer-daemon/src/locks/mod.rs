//! Write target locks: who holds which shared target, who is queued for
//! it, and the relays that tell sessions about handoffs.
//!
//! `LockService` ties the table rules to requests, hook events and slot
//! exits. Every change is announced with `Event::LocksChanged`.

pub(crate) mod context;
pub(crate) mod table;
pub mod targets;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use overseer_proto::msg::{
    slot_name, BashCheck, Claim, ClaimKind, Event, HookEvent, HookKind, ItemKind, LockView,
    RelayState, Response, SlotId, WorkView, CHECK_ENV,
};
use rusqlite::Connection;
use tokio::sync::broadcast;

use crate::env::BaseEnv;
use crate::ledger::Ledger;
use crate::manager::{valid_id, SLOT_COUNT};
use crate::work::store::unix_now;
use context::SystemContext;
use table::{ClaimOutcome, NewClaim, Releaser, Via};
use targets::{match_command, valid_target, Hit, HitKind, Pattern};

/// How long a `CheckBash` may spend resolving targets. Past it the command
/// is allowed and nothing is recorded, so a late answer never leaves a stray
/// wait behind (the hook gives up after 1 s).
pub const RESOLVE_WITHIN: Duration = Duration::from_millis(700);
/// Delivered relays kept in the lock view.
const RECENT_RELAYS: usize = 50;

pub struct LockService {
    ledger: Arc<Ledger>,
    patterns: Arc<[Pattern]>,
    events: broadcast::Sender<Event>,
    env: BaseEnv,
    flag_after: Duration,
    /// Set by `shutdown`: the daemon is killing every session, and those
    /// exits must not drop their locks.
    stopping: AtomicBool,
    /// Why each slot's pending relays wait, as the courier last found it.
    held: Mutex<[Option<&'static str>; SLOT_COUNT]>,
    /// The slots with no session (empty or exited), from the slot manager.
    sessionless: OnceLock<SessionlessFn>,
}

type SessionlessFn = Box<dyn Fn() -> Vec<SlotId> + Send + Sync>;

/// Local `HH:MM` for unix seconds.
pub fn local_hhmm(t: i64) -> String {
    // SAFETY: localtime_r only writes the `tm` we pass it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let time = t as libc::time_t;
    if unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
        return "?".into();
    }
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

/// `repo:demo-api:main is held by S1 since 08:40 for DEMO-33`, the `for`
/// part naming the claim's item, else its reason, else left out.
fn held_text(c: &Claim) -> String {
    let for_what = c
        .item
        .as_deref()
        .or(c.reason.as_deref())
        .map(|w| format!(" for {w}"))
        .unwrap_or_default();
    format!(
        "{} is held by {} since {}{for_what}",
        c.target,
        slot_name(c.slot),
        local_hhmm(c.since)
    )
}

/// The deny reason for a command whose targets in `held` belong to others.
fn deny_text(held: &[Claim]) -> String {
    let what: Vec<String> = held.iter().map(held_text).collect();
    let queue = if held.len() == 1 {
        "You are in its queue and Overseer will tell you here when you hold it."
    } else {
        "You are in their queues and Overseer will tell you here when you hold them."
    };
    format!(
        "Overseer: {}. {queue} Do not run this again until then.",
        what.join("; ")
    )
}

/// The work items `slot` owns at the top level: its tickets and the beads
/// not under one of its tickets.
pub fn owned_items(view: &WorkView, slot: SlotId) -> Vec<String> {
    let mine = |key: &str| {
        view.items
            .iter()
            .any(|i| i.key == key && i.owner == Some(slot))
    };
    view.items
        .iter()
        .filter(|i| i.owner == Some(slot))
        .filter(|i| {
            !(i.kind == ItemKind::Bead
                && i.parent.as_deref().is_some_and(|p| p != i.key && mine(p)))
        })
        .map(|i| i.key.clone())
        .collect()
}

/// The item a claim is for when none is named: the slot's only item.
fn only_item(owned: &[String]) -> Option<&str> {
    match owned {
        [one] => Some(one.as_str()),
        _ => None,
    }
}

fn error(message: impl Into<String>) -> Response {
    Response::Error {
        message: message.into(),
    }
}

impl LockService {
    pub fn new(
        ledger: Arc<Ledger>,
        patterns: Vec<Pattern>,
        events: broadcast::Sender<Event>,
        env: BaseEnv,
        flag_after: Duration,
    ) -> Arc<LockService> {
        Arc::new(LockService {
            ledger,
            patterns: patterns.into(),
            events,
            env,
            flag_after,
            stopping: AtomicBool::new(false),
            held: Mutex::new([None; SLOT_COUNT]),
            sessionless: OnceLock::new(),
        })
    }

    /// Sets how to learn which slots have no session, so relays Overseer
    /// queues for them are dropped at once. Set once at start-up; it must
    /// not call back into the lock service.
    pub fn on_sessionless(&self, f: impl Fn() -> Vec<SlotId> + Send + Sync + 'static) {
        let _ = self.sessionless.set(Box::new(f));
    }

    /// The slots with no session now. None while the daemon stops, since
    /// those sessions are only paused for the restart.
    fn sessionless(&self) -> Vec<SlotId> {
        if self.stopping.load(Ordering::SeqCst) {
            return Vec::new();
        }
        self.sessionless.get().map_or_else(Vec::new, |f| f())
    }

    /// Drops the relays pending for slots with no session, such as a
    /// notice the work store just queued for one.
    pub fn drop_sessionless_relays(&self, now: i64) {
        let dead = self.sessionless();
        if dead.is_empty() {
            return;
        }
        let done = self.ledger.transaction(|conn| {
            let mut n = 0;
            for &slot in &dead {
                n += table::drop_relays_to_on(conn, slot, now)?;
            }
            Ok(n)
        });
        match done {
            Ok(0) => {}
            Ok(_) => self.announce(),
            Err(e) => tracing::warn!("dropping relays: {e:#}"),
        }
    }

    pub fn view(&self) -> LockView {
        let flag = i64::try_from(self.flag_after.as_secs()).unwrap_or(i64::MAX);
        let mut view = self
            .ledger
            .lock_view(RECENT_RELAYS, flag)
            .unwrap_or_else(|e| {
                tracing::warn!("lock view unreadable: {e:#}");
                LockView {
                    claims: Vec::new(),
                    waits: Vec::new(),
                    relays: Vec::new(),
                    flag_after_secs: flag,
                }
            });
        let held = *self.held.lock().unwrap();
        for r in &mut view.relays {
            if r.state == RelayState::Pending {
                r.held = held.get(r.to as usize).copied().flatten().map(String::from);
            }
        }
        view
    }

    /// The courier's reason `slot`'s pending relays wait, `None` when they
    /// are not held. A change is announced.
    pub(crate) fn set_held(&self, slot: SlotId, reason: Option<&'static str>) {
        let changed = {
            let mut held = self.held.lock().unwrap();
            let Some(entry) = held.get_mut(slot as usize) else {
                return;
            };
            std::mem::replace(entry, reason) != reason
        };
        if changed {
            self.announce();
        }
    }

    pub(crate) fn announce(&self) {
        let _ = self.events.send(Event::LocksChanged(self.view()));
    }

    /// Runs `f` in one ledger transaction and announces the result. An
    /// error is logged with `what` and returned.
    pub(crate) fn change<T>(
        &self,
        what: &str,
        f: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<T> {
        let done = self.ledger.transaction(f);
        match &done {
            Ok(_) => self.announce(),
            Err(e) => tracing::warn!("{what}: {e:#}"),
        }
        done
    }

    /// `claim_target`: holds `target` for `slot`, or queues it.
    pub fn claim(
        &self,
        slot: SlotId,
        target: &str,
        reason: Option<&str>,
        item: Option<&str>,
        owned: &[String],
        now: i64,
    ) -> Response {
        if !valid_id(slot) {
            return error(format!("invalid slot id {slot}"));
        }
        if !valid_target(target) {
            return error(format!(
                "{target:?} is not a target: it needs a repo:, argo:, deploy:, tf:, kube: or custom: prefix and no spaces"
            ));
        }
        let c = NewClaim {
            target,
            slot,
            kind: ClaimKind::Claim,
            reason,
            item: item.or(only_item(owned)),
            tool_use_id: None,
            pin_revision: None,
            via: Via::Tool,
        };
        match self.change("claim", |conn| table::claim_on(conn, &c, now)) {
            Ok(ClaimOutcome::Granted) => Response::Text {
                text: format!("You hold {target}."),
            },
            Ok(ClaimOutcome::AlreadyHeld) => Response::Text {
                text: format!("You already hold {target}."),
            },
            Ok(ClaimOutcome::Queued { holder, position }) => Response::Queued {
                message: format!(
                    "{}. You are number {position} in the queue; Overseer will tell you here when you hold it.",
                    held_text(&holder)
                ),
            },
            Err(e) => error(format!("ledger: {e:#}")),
        }
    }

    /// `release_target`: the holder hands `target` on; a waiter leaves its
    /// queue.
    pub fn release(&self, slot: SlotId, target: &str, now: i64) -> Response {
        if !valid_id(slot) {
            return error(format!("invalid slot id {slot}"));
        }
        let done = self
            .ledger
            .transaction(|conn| table::release_on(conn, target, Releaser::Slot(slot), None, now));
        self.released(target, done)
    }

    /// Jonathan's release of `target`: from whatever holds it when
    /// `holder` is `None`, else only while `holder` still does. The notice
    /// to a holder with no session is queued already dropped.
    pub fn force_release(&self, target: &str, holder: Option<SlotId>, now: i64) -> Response {
        let dead = self.sessionless();
        let done = self.ledger.transaction(|conn| {
            let h = table::release_on(conn, target, Releaser::Force(holder), None, now)?;
            for &slot in &dead {
                table::drop_relays_to_on(conn, slot, now)?;
            }
            Ok(h)
        });
        self.released(target, done)
    }

    fn released(&self, target: &str, done: Result<Option<table::Handoff>>) -> Response {
        match done {
            Ok(handoff) => {
                self.announce();
                let text = match handoff {
                    None => format!("You are no longer queued for {target}."),
                    Some(h) => match h.to {
                        Some(to) => format!("Released {target}. {} holds it now.", slot_name(to)),
                        None => format!("Released {target}."),
                    },
                };
                Response::Text { text }
            }
            Err(e) => error(format!("{e:#}")),
        }
    }

    /// The `PreToolUse` check of a Bash command. Always a `Verdict`; any
    /// failure or a resolution past `RESOLVE_WITHIN` allows the command and
    /// records nothing.
    pub async fn check_bash(&self, check: BashCheck, owned: Vec<String>) -> Response {
        let allow = Response::Verdict {
            deny: None,
            rewrite: None,
        };
        if !valid_id(check.slot) {
            return allow;
        }
        let deadline = Instant::now() + RESOLVE_WITHIN;
        let patterns = self.patterns.clone();
        let base = self.env.clone();
        let command = check.command.clone();
        let cwd = check.cwd.clone().unwrap_or_else(|| PathBuf::from("/"));
        let env: Vec<(String, String)> = check
            .env
            .iter()
            .filter(|(k, _)| CHECK_ENV.contains(&k.as_str()))
            .cloned()
            .collect();
        let job = tokio::task::spawn_blocking(move || {
            let ctx = SystemContext {
                env,
                deadline,
                base,
            };
            match_command(&patterns, &command, &cwd, &ctx)
        });
        let hits = match tokio::time::timeout(RESOLVE_WITHIN, job).await {
            // A lookup killed at the deadline can still leave other hits.
            Ok(Ok(hits)) if Instant::now() < deadline => hits,
            Ok(Ok(_)) | Err(_) => {
                tracing::warn!(
                    "{}: targets not resolved within {RESOLVE_WITHIN:?}; allowing {:?}",
                    slot_name(check.slot),
                    check.command
                );
                return allow;
            }
            Ok(Err(e)) => {
                tracing::warn!("target resolution failed: {e}");
                return allow;
            }
        };
        if hits.is_empty() {
            return allow;
        }
        let item = only_item(&owned);
        let now = unix_now();
        match self.change("bash check", |conn| bash_on(conn, &check, &hits, item, now)) {
            Ok(deny) => Response::Verdict {
                deny,
                rewrite: None,
            },
            Err(_) => allow,
        }
    }

    /// Releases the claims a hook event ends: a command's claims at its
    /// `PostToolUse`, the turn's at `Stop`.
    pub fn on_hook(&self, ev: &HookEvent, now: i64) {
        if !valid_id(ev.slot) {
            return;
        }
        let done = match ev.event {
            HookKind::PostToolUse => self.ledger.transaction(|conn| {
                table::release_commands_on(conn, ev.slot, ev.tool_use_id.as_deref(), now)
            }),
            HookKind::Stop => self
                .ledger
                .transaction(|conn| table::release_on_stop_on(conn, ev.slot, now)),
            _ => return,
        };
        self.after_release("hook release", done);
    }

    /// A slot's session ended (it exited, a resumable one was declined, or
    /// a fresh start replaces it): its command claims and grants go, it
    /// leaves every queue and its pending relays are dropped. Ignored once
    /// `shutdown` was called, so those sessions keep all of it for a resume.
    pub fn on_session_ended(&self, slot: SlotId, now: i64) {
        if !valid_id(slot) || self.stopping.load(Ordering::SeqCst) {
            return;
        }
        // Leaving a queue hands nothing on, so announce either way.
        let _ = self.change("session end", |conn| table::end_session_on(conn, slot, now));
    }

    /// For slots whose end may not have been seen (lost exit events, or a
    /// daemon start): each of `ended` (slots with no session now) that
    /// still has a command claim, a grant, a wait or a pending relay is
    /// treated as ended. Slots with nothing to drop are left alone.
    pub fn reconcile_exits(&self, ended: &[SlotId], now: i64) {
        let v = self.view();
        for &slot in ended {
            let stale =
                v.claims.iter().any(|c| {
                    c.slot == slot && matches!(c.kind, ClaimKind::Command | ClaimKind::Grant)
                }) || v.waits.iter().any(|w| w.slot == slot)
                    || v.pending_for(slot) > 0;
            if stale {
                self.on_session_ended(slot, now);
            }
        }
    }

    /// A work item was released or moved: the targets claimed for it go.
    pub fn on_item_released(&self, key: &str, now: i64) {
        let done = self
            .ledger
            .transaction(|conn| table::release_item_on(conn, key, now));
        self.after_release("item release", done);
    }

    fn after_release(&self, what: &str, done: Result<Vec<table::Handoff>>) {
        match done {
            Ok(h) if h.is_empty() => {}
            Ok(_) => self.announce(),
            Err(e) => tracing::warn!("{what}: {e:#}"),
        }
    }

    /// The daemon is stopping: session exits from here on keep their locks.
    pub fn shutdown(&self) {
        self.stopping.store(true, Ordering::SeqCst);
    }
}

/// The ledger side of a Bash check. Any hit held by another slot queues
/// the caller on every such hit and denies; otherwise free hits are
/// claimed, the slot's own grants (and its pin, for an unpin) are bound to
/// this command and its other holds are touched. Returns the deny reason.
fn bash_on(
    conn: &Connection,
    check: &BashCheck,
    hits: &[Hit],
    item: Option<&str>,
    now: i64,
) -> Result<Option<String>> {
    let slot = check.slot;
    let tool_use_id = check.tool_use_id.as_deref();
    let mut held = Vec::new();
    for h in hits {
        if let Some(c) = table::holder_on(conn, &h.target)? {
            if c.slot != slot {
                held.push(c);
            }
        }
    }
    if !held.is_empty() {
        for c in &held {
            let wait = NewClaim {
                target: &c.target,
                slot,
                kind: ClaimKind::Command,
                reason: None,
                item,
                tool_use_id,
                pin_revision: None,
                via: Via::Hook,
            };
            table::claim_on(conn, &wait, now)?;
        }
        return Ok(Some(deny_text(&held)));
    }
    for h in hits {
        match table::holder_on(conn, &h.target)? {
            None => {
                let (kind, pin_revision) = match &h.kind {
                    HitKind::Pin { revision } => (ClaimKind::Pin, Some(revision.as_str())),
                    HitKind::Command | HitKind::Unpin { .. } => (ClaimKind::Command, None),
                };
                let c = NewClaim {
                    target: &h.target,
                    slot,
                    kind,
                    reason: None,
                    item,
                    tool_use_id,
                    pin_revision,
                    via: Via::Hook,
                };
                table::claim_on(conn, &c, now)?;
            }
            Some(mine) => {
                let id = tool_use_id.unwrap_or("");
                match (mine.kind, &h.kind) {
                    // A pin keeps its lock past the command, whether the
                    // target was granted or claimed with the tool.
                    (
                        ClaimKind::Grant | ClaimKind::Claim | ClaimKind::Pin,
                        HitKind::Pin { revision },
                    ) => {
                        table::pin_on(conn, &h.target, slot, revision)?;
                    }
                    (ClaimKind::Grant, _) => {
                        table::bind_grant_on(conn, &h.target, slot, id, None)?;
                    }
                    (ClaimKind::Pin, HitKind::Unpin { revision }) => {
                        table::bind_grant_on(conn, &h.target, slot, id, Some(revision))?;
                    }
                    _ => {}
                }
                table::touch_on(conn, &h.target, slot, now)?;
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use overseer_proto::msg::WorkItem;

    fn claim(item: Option<&str>, reason: Option<&str>) -> Claim {
        Claim {
            target: "repo:demo-api:main".into(),
            slot: 1,
            kind: ClaimKind::Command,
            reason: reason.map(String::from),
            item: item.map(String::from),
            since: 0,
            last_used: 0,
            pin_revision: None,
        }
    }

    #[test]
    fn held_text_names_the_item_else_the_reason() {
        let at = local_hhmm(0);
        assert_eq!(
            held_text(&claim(Some("DEMO-33"), Some("release"))),
            format!("repo:demo-api:main is held by S1 since {at} for DEMO-33")
        );
        assert_eq!(
            held_text(&claim(None, Some("release"))),
            format!("repo:demo-api:main is held by S1 since {at} for release")
        );
        assert_eq!(
            held_text(&claim(None, None)),
            format!("repo:demo-api:main is held by S1 since {at}")
        );
    }

    #[test]
    fn deny_text_for_one_and_for_several() {
        let at = local_hhmm(0);
        assert_eq!(
            deny_text(&[claim(Some("DEMO-33"), None)]),
            format!("Overseer: repo:demo-api:main is held by S1 since {at} for DEMO-33. You are in its queue and Overseer will tell you here when you hold it. Do not run this again until then.")
        );
        let two = deny_text(&[claim(None, None), claim(None, None)]);
        assert!(two.contains("; repo:demo-api:main"), "{two}");
        assert!(two.contains("You are in their queues"), "{two}");
    }

    fn service() -> Arc<LockService> {
        let (events, _) = broadcast::channel(16);
        LockService::new(
            Arc::new(Ledger::in_memory()),
            Vec::new(),
            events,
            BaseEnv::ready(Vec::new()),
            Duration::from_secs(3600),
        )
    }

    /// A service where S1 holds `custom:a` through a command.
    fn holding() -> Arc<LockService> {
        let locks = service();
        let check = BashCheck {
            slot: 1,
            session_id: None,
            tool_use_id: Some("tu".into()),
            command: String::new(),
            cwd: None,
            env: Vec::new(),
        };
        let hit = Hit {
            target: "custom:a".into(),
            kind: HitKind::Command,
            pattern: "p".into(),
        };
        let deny = locks
            .ledger
            .transaction(|c| bash_on(c, &check, &[hit], None, 10))
            .unwrap();
        assert_eq!(deny, None);
        assert_eq!(locks.view().claims.len(), 1);
        locks
    }

    #[test]
    fn exits_after_shutdown_keep_their_locks() {
        let locks = holding();
        locks.on_session_ended(1, 20);
        assert!(locks.view().claims.is_empty());
        let locks = holding();
        locks.shutdown();
        locks.on_session_ended(1, 20);
        assert_eq!(locks.view().claims.len(), 1);
    }

    #[test]
    fn reconcile_drops_only_stopped_slots_with_something_to_drop() {
        let locks = holding();
        assert!(matches!(
            locks.claim(2, "custom:a", None, None, &[], 11),
            Response::Queued { .. }
        ));
        assert!(matches!(
            locks.claim(3, "custom:b", None, None, &[], 11),
            Response::Text { .. }
        ));
        // S1 is still running: untouched. S3 holds only a tool claim.
        locks.reconcile_exits(&[2, 3], 20);
        let v = locks.view();
        assert!(v.waits.is_empty(), "{v:?}");
        assert_eq!(v.claims.len(), 2);
        locks.reconcile_exits(&[1], 21);
        let v = locks.view();
        assert_eq!(v.claims.len(), 1);
        assert_eq!(v.claims[0].target, "custom:b");
    }

    #[test]
    fn held_reasons_reach_only_that_slots_pending_relays() {
        let (events, mut rx) = broadcast::channel(16);
        let locks = LockService::new(
            Arc::new(Ledger::in_memory()),
            Vec::new(),
            events,
            BaseEnv::ready(Vec::new()),
            Duration::from_secs(3600),
        );
        let queue = |to: SlotId, text: &str| {
            locks
                .change("test", |c| {
                    table::queue_relay_on(c, &overseer_proto::Sender::You, to, text, None, 1)
                })
                .unwrap()
        };
        let done = queue(2, "done");
        locks
            .change("test", |c| table::mark_delivered_on(c, &[done], 2))
            .unwrap();
        queue(2, "waiting");
        queue(3, "other");
        while rx.try_recv().is_ok() {}
        locks.set_held(2, Some("session is working"));
        assert!(matches!(rx.try_recv(), Ok(Event::LocksChanged(_))));
        // The same reason again is no change.
        locks.set_held(2, Some("session is working"));
        assert!(rx.try_recv().is_err());
        let held: Vec<_> = locks
            .view()
            .relays
            .iter()
            .map(|r| (r.text.clone(), r.held.clone()))
            .collect();
        assert_eq!(
            held,
            [
                ("other".to_string(), None),
                (
                    "waiting".to_string(),
                    Some("session is working".to_string())
                ),
                ("done".to_string(), None),
            ]
        );
        locks.set_held(2, None);
        assert!(matches!(rx.try_recv(), Ok(Event::LocksChanged(_))));
        assert!(locks.view().relays.iter().all(|r| r.held.is_none()));
    }

    #[test]
    fn hhmm_is_two_digit_fields() {
        let t = local_hhmm(1_700_000_000);
        assert_eq!(t.len(), 5, "{t}");
        assert_eq!(&t[2..3], ":");
    }

    fn item(key: &str, kind: ItemKind, parent: Option<&str>, owner: Option<SlotId>) -> WorkItem {
        WorkItem {
            key: key.into(),
            kind,
            title: key.into(),
            status: "open".into(),
            priority: None,
            parent: parent.map(String::from),
            owner,
            in_list: true,
            folder: None,
        }
    }

    #[test]
    fn owned_items_leave_out_beads_under_an_owned_ticket() {
        let view = WorkView {
            items: vec![
                item("DEMO-12", ItemKind::Ticket, None, Some(1)),
                item("ov-2", ItemKind::Bead, Some("DEMO-12"), Some(1)),
                item("ov-3", ItemKind::Bead, Some("DEMO-40"), Some(1)),
                item("ov-4", ItemKind::Bead, None, Some(2)),
            ],
            sources: Vec::new(),
            notices: Vec::new(),
        };
        assert_eq!(owned_items(&view, 1), ["DEMO-12", "ov-3"]);
        assert_eq!(owned_items(&view, 2), ["ov-4"]);
        assert!(owned_items(&view, 3).is_empty());
        assert_eq!(only_item(&owned_items(&view, 2)), Some("ov-4"));
        assert_eq!(only_item(&owned_items(&view, 1)), None);
    }
}
