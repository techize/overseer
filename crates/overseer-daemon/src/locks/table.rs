//! The lock table: claims on write targets, the queues waiting for them,
//! and the relays that tell sessions what happened.
//!
//! Every function takes the `&Connection` that `Ledger::transaction` hands
//! its closure, so a caller's checks and writes land together. Times are
//! unix seconds, passed in by the caller.

use anyhow::{bail, Result};
use overseer_proto::msg::{slot_name, Claim, ClaimKind, Relay, RelayState, Sender, SlotId, Wait};
use rusqlite::types::Type;
use rusqlite::{params, Connection, OptionalExtension, Row};

/// How a slot asked for a target, which decides what it gets when the
/// target is handed to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Via {
    /// A command the `PreToolUse` hook matched; a handoff gives a `Grant`.
    Hook,
    /// The `claim_target` tool; a handoff gives a `Claim`.
    Tool,
}

/// A claim being asked for.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NewClaim<'a> {
    pub target: &'a str,
    pub slot: SlotId,
    pub kind: ClaimKind,
    pub reason: Option<&'a str>,
    pub item: Option<&'a str>,
    pub tool_use_id: Option<&'a str>,
    pub pin_revision: Option<&'a str>,
    pub via: Via,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ClaimOutcome {
    Granted,
    AlreadyHeld,
    /// Held by someone else; the caller is in the queue at `position`
    /// (1 is next).
    Queued {
        holder: Claim,
        position: usize,
    },
}

/// Who is releasing a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Releaser {
    Slot(SlotId),
    /// Jonathan, from the GUI or `ctl force-release`.
    Force,
}

/// A target leaving its holder, and the waiter it went to, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Handoff {
    pub target: String,
    pub from: SlotId,
    pub to: Option<SlotId>,
}

fn kind_str(k: ClaimKind) -> &'static str {
    match k {
        ClaimKind::Command => "command",
        ClaimKind::Grant => "grant",
        ClaimKind::Claim => "claim",
        ClaimKind::Pin => "pin",
    }
}

fn via_str(v: Via) -> &'static str {
    match v {
        Via::Hook => "hook",
        Via::Tool => "tool",
    }
}

fn bad_column(col: usize, what: &str, value: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        col,
        Type::Text,
        format!("unknown {what} {value:?}").into(),
    )
}

fn kind_from(col: usize, s: &str) -> rusqlite::Result<ClaimKind> {
    Ok(match s {
        "command" => ClaimKind::Command,
        "grant" => ClaimKind::Grant,
        "claim" => ClaimKind::Claim,
        "pin" => ClaimKind::Pin,
        other => return Err(bad_column(col, "claim kind", other)),
    })
}

fn via_from(col: usize, s: &str) -> rusqlite::Result<Via> {
    Ok(match s {
        "hook" => Via::Hook,
        "tool" => Via::Tool,
        other => return Err(bad_column(col, "wait via", other)),
    })
}

/// The `sender` and `from_slot` columns for a sender.
fn sender_cols(s: &Sender) -> (&'static str, Option<SlotId>) {
    match s {
        Sender::Overseer => ("overseer", None),
        Sender::You => ("you", None),
        Sender::Slot { slot } => ("slot", Some(*slot)),
    }
}

const CLAIM_COLS: &str = "target, slot, kind, reason, item, since, last_used, pin_revision";

fn claim_row(r: &Row) -> rusqlite::Result<Claim> {
    Ok(Claim {
        target: r.get(0)?,
        slot: r.get(1)?,
        kind: kind_from(2, &r.get::<_, String>(2)?)?,
        reason: r.get(3)?,
        item: r.get(4)?,
        since: r.get(5)?,
        last_used: r.get(6)?,
        pin_revision: r.get(7)?,
    })
}

const RELAY_COLS: &str = "id, at, sender, from_slot, to_slot, text, delivered_at";

fn relay_row(r: &Row) -> rusqlite::Result<Relay> {
    let sender: String = r.get(2)?;
    let from_slot: Option<SlotId> = r.get(3)?;
    let from = match (sender.as_str(), from_slot) {
        ("overseer", _) => Sender::Overseer,
        ("you", _) => Sender::You,
        ("slot", Some(slot)) => Sender::Slot { slot },
        (other, _) => return Err(bad_column(2, "relay sender", other)),
    };
    let delivered_at: Option<i64> = r.get(6)?;
    Ok(Relay {
        id: r.get(0)?,
        at: r.get(1)?,
        from,
        to: r.get(4)?,
        text: r.get(5)?,
        state: if delivered_at.is_some() {
            RelayState::Delivered
        } else {
            RelayState::Pending
        },
        delivered_at,
        held: None,
    })
}

fn claims_where(
    conn: &Connection,
    filter: &str,
    args: impl rusqlite::Params,
) -> Result<Vec<Claim>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {CLAIM_COLS} FROM claims {filter} ORDER BY target"
    ))?;
    let rows = stmt.query_map(args, claim_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn holder_on(conn: &Connection, target: &str) -> Result<Option<Claim>> {
    Ok(conn
        .query_row(
            &format!("SELECT {CLAIM_COLS} FROM claims WHERE target = ?1"),
            params![target],
            claim_row,
        )
        .optional()?)
}

/// Every claim, by target.
pub(crate) fn claims_on(conn: &Connection) -> Result<Vec<Claim>> {
    claims_where(conn, "", [])
}

/// Every wait, in the order they joined, so each target's waits are in
/// queue order.
pub(crate) fn waits_on(conn: &Connection) -> Result<Vec<Wait>> {
    let mut stmt = conn.prepare("SELECT target, slot, at, reason FROM waits ORDER BY id")?;
    let rows = stmt.query_map([], |r| {
        Ok(Wait {
            target: r.get(0)?,
            slot: r.get(1)?,
            at: r.get(2)?,
            reason: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Every pending relay plus the `recent` most recently delivered, newest
/// first.
pub(crate) fn relays_on(conn: &Connection, recent: usize) -> Result<Vec<Relay>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {RELAY_COLS} FROM relays
         WHERE delivered_at IS NULL
            OR id IN (SELECT id FROM relays WHERE delivered_at IS NOT NULL
                      ORDER BY delivered_at DESC, id DESC LIMIT ?1)
         ORDER BY id DESC"
    ))?;
    let recent = i64::try_from(recent).unwrap_or(i64::MAX);
    let rows = stmt.query_map(params![recent], relay_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Asks for `c.target`. A free target is granted; one the slot already
/// holds is touched; one held by another slot puts the caller in its
/// queue. Asking again while queued keeps the caller's place.
pub(crate) fn claim_on(conn: &Connection, c: &NewClaim, now: i64) -> Result<ClaimOutcome> {
    match holder_on(conn, c.target)? {
        None => {
            conn.execute(
                "INSERT INTO claims (target, slot, kind, reason, item, tool_use_id,
                                     pin_revision, relay_id, since, last_used)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8, ?8)",
                params![
                    c.target,
                    c.slot,
                    kind_str(c.kind),
                    c.reason,
                    c.item,
                    c.tool_use_id,
                    c.pin_revision,
                    now
                ],
            )?;
            // A free target has no queue worth keeping a place in.
            conn.execute(
                "DELETE FROM waits WHERE target = ?1 AND slot = ?2",
                params![c.target, c.slot],
            )?;
            Ok(ClaimOutcome::Granted)
        }
        Some(held) if held.slot == c.slot => {
            touch_on(conn, c.target, c.slot, now)?;
            Ok(ClaimOutcome::AlreadyHeld)
        }
        Some(holder) => {
            conn.execute(
                "INSERT OR IGNORE INTO waits (target, slot, via, reason, at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![c.target, c.slot, via_str(c.via), c.reason, now],
            )?;
            let position: i64 = conn.query_row(
                "SELECT COUNT(*) FROM waits WHERE target = ?1
                   AND id <= (SELECT id FROM waits WHERE target = ?1 AND slot = ?2)",
                params![c.target, c.slot],
                |r| r.get(0),
            )?;
            Ok(ClaimOutcome::Queued {
                holder,
                position: position as usize,
            })
        }
    }
}

/// Takes `target` from `holder` and gives it to the first waiter, queueing
/// the relays that say so. `back_on` is the revision a pin went back to;
/// `forced` means Jonathan released it.
fn hand_on(
    conn: &Connection,
    target: &str,
    holder: SlotId,
    forced: bool,
    back_on: Option<&str>,
    now: i64,
) -> Result<Handoff> {
    conn.execute("DELETE FROM claims WHERE target = ?1", params![target])?;
    let back = back_on
        .map(|r| format!(" (back on {r})"))
        .unwrap_or_default();
    if forced {
        let text = format!("Jonathan released {target}{back}, which you held.");
        queue_relay_on(conn, &Sender::Overseer, holder, &text, None, now)?;
    }
    let next = conn
        .query_row(
            "SELECT id, slot, via, reason FROM waits WHERE target = ?1 ORDER BY id LIMIT 1",
            params![target],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, SlotId>(1)?,
                    via_from(2, &r.get::<_, String>(2)?)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((wait_id, slot, via, reason)) = next else {
        return Ok(Handoff {
            target: target.into(),
            from: holder,
            to: None,
        });
    };
    conn.execute("DELETE FROM waits WHERE id = ?1", params![wait_id])?;
    let text = if forced {
        format!("Jonathan released {target}{back}. You now hold it.")
    } else {
        format!(
            "{} has released {target}{back}. You now hold it.",
            slot_name(holder)
        )
    };
    let relay = queue_relay_on(conn, &Sender::Overseer, slot, &text, None, now)?;
    let kind = match via {
        Via::Hook => ClaimKind::Grant,
        Via::Tool => ClaimKind::Claim,
    };
    conn.execute(
        "INSERT INTO claims (target, slot, kind, reason, item, tool_use_id,
                             pin_revision, relay_id, since, last_used)
         VALUES (?1, ?2, ?3, ?4, NULL, NULL, NULL, ?5, ?6, ?6)",
        params![target, slot, kind_str(kind), reason, relay, now],
    )?;
    Ok(Handoff {
        target: target.into(),
        from: holder,
        to: Some(slot),
    })
}

/// Releases `target`. Its holder (or `Releaser::Force`) hands it to the
/// first waiter; a slot that only waits on it leaves the queue and gets
/// `Ok(None)`. Anyone else is an error.
pub(crate) fn release_on(
    conn: &Connection,
    target: &str,
    by: Releaser,
    back_on: Option<&str>,
    now: i64,
) -> Result<Option<Handoff>> {
    let holder = holder_on(conn, target)?.map(|h| h.slot);
    match (by, holder) {
        (Releaser::Force, Some(h)) => Ok(Some(hand_on(conn, target, h, true, back_on, now)?)),
        (Releaser::Force, None) => bail!("{target} is not held"),
        (Releaser::Slot(s), Some(h)) if h == s => {
            Ok(Some(hand_on(conn, target, h, false, back_on, now)?))
        }
        (Releaser::Slot(s), _) => {
            let left = conn.execute(
                "DELETE FROM waits WHERE target = ?1 AND slot = ?2",
                params![target, s],
            )?;
            if left == 0 {
                bail!("{target} is not held by {}", slot_name(s));
            }
            Ok(None)
        }
    }
}

/// Records that `slot` used `target` now, if it holds it.
pub(crate) fn touch_on(conn: &Connection, target: &str, slot: SlotId, now: i64) -> Result<()> {
    conn.execute(
        "UPDATE claims SET last_used = ?3 WHERE target = ?1 AND slot = ?2",
        params![target, slot, now],
    )?;
    Ok(())
}

/// Turns the slot's `Grant` (or its own `Pin`, when a command unpins it) on
/// `target` into a `Command` claim for `tool_use_id`, so that command's
/// `PostToolUse` releases it. True if a claim changed.
pub(crate) fn bind_grant_on(
    conn: &Connection,
    target: &str,
    slot: SlotId,
    tool_use_id: &str,
) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE claims SET kind = 'command', tool_use_id = ?3
         WHERE target = ?1 AND slot = ?2 AND kind IN ('grant', 'pin')",
        params![target, slot, tool_use_id],
    )?;
    Ok(changed > 0)
}

/// Releases each of `slot`'s claims on `targets`, in order.
fn release_all(
    conn: &Connection,
    slot: SlotId,
    targets: Vec<String>,
    now: i64,
) -> Result<Vec<Handoff>> {
    targets
        .iter()
        .map(|t| hand_on(conn, t, slot, false, None, now))
        .collect()
}

fn targets_where(
    conn: &Connection,
    filter: &str,
    args: impl rusqlite::Params,
) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT c.target FROM claims c {filter} ORDER BY c.target"
    ))?;
    let rows = stmt.query_map(args, |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// After a command: with an id, the slot's command claims for that id;
/// without one, all its command claims.
pub(crate) fn release_commands_on(
    conn: &Connection,
    slot: SlotId,
    tool_use_id: Option<&str>,
    now: i64,
) -> Result<Vec<Handoff>> {
    let targets = match tool_use_id {
        Some(id) => targets_where(
            conn,
            "WHERE c.slot = ?1 AND c.kind = 'command' AND c.tool_use_id = ?2",
            params![slot, id],
        )?,
        None => targets_where(
            conn,
            "WHERE c.slot = ?1 AND c.kind = 'command'",
            params![slot],
        )?,
    };
    release_all(conn, slot, targets, now)
}

/// At a `Stop`: the slot's command claims, and the grants it was told
/// about but did not use.
pub(crate) fn release_on_stop_on(
    conn: &Connection,
    slot: SlotId,
    now: i64,
) -> Result<Vec<Handoff>> {
    let targets = targets_where(
        conn,
        "LEFT JOIN relays r ON r.id = c.relay_id
         WHERE c.slot = ?1 AND (c.kind = 'command'
            OR (c.kind = 'grant' AND r.delivered_at IS NOT NULL))",
        params![slot],
    )?;
    release_all(conn, slot, targets, now)
}

/// When a slot's session ends: its command claims and every grant are
/// released and it leaves every queue. Tool claims and pins stay.
pub(crate) fn drop_slot_on(conn: &Connection, slot: SlotId, now: i64) -> Result<Vec<Handoff>> {
    conn.execute("DELETE FROM waits WHERE slot = ?1", params![slot])?;
    let targets = targets_where(
        conn,
        "WHERE c.slot = ?1 AND c.kind IN ('command', 'grant')",
        params![slot],
    )?;
    release_all(conn, slot, targets, now)
}

/// Releases every claim made for work item `item`, whoever holds it.
pub(crate) fn release_item_on(conn: &Connection, item: &str, now: i64) -> Result<Vec<Handoff>> {
    claims_where(conn, "WHERE item = ?1", params![item])?
        .iter()
        .map(|c| hand_on(conn, &c.target, c.slot, false, None, now))
        .collect()
}

/// Queues a relay for `to`, returning its id.
pub(crate) fn queue_relay_on(
    conn: &Connection,
    from: &Sender,
    to: SlotId,
    text: &str,
    reply_to: Option<i64>,
    now: i64,
) -> Result<i64> {
    let (sender, from_slot) = sender_cols(from);
    conn.execute(
        "INSERT INTO relays (at, sender, from_slot, to_slot, text, reply_to, delivered_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
        params![now, sender, from_slot, to, text, reply_to],
    )?;
    Ok(conn.last_insert_rowid())
}

/// The relays waiting for `to`, oldest first.
pub(crate) fn pending_relays_on(conn: &Connection, to: SlotId) -> Result<Vec<Relay>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {RELAY_COLS} FROM relays
         WHERE to_slot = ?1 AND delivered_at IS NULL ORDER BY id"
    ))?;
    let rows = stmt.query_map(params![to], relay_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Marks relays delivered at `now`. Ones already delivered keep their time.
pub(crate) fn mark_delivered_on(conn: &Connection, ids: &[i64], now: i64) -> Result<()> {
    let mut stmt =
        conn.prepare("UPDATE relays SET delivered_at = ?2 WHERE id = ?1 AND delivered_at IS NULL")?;
    for id in ids {
        stmt.execute(params![id, now])?;
    }
    Ok(())
}

/// The oldest relay from slot `from` delivered to `worker` that `worker`
/// has not answered (no relay from `worker` names it in `reply_to`).
pub(crate) fn unanswered_from_on(
    conn: &Connection,
    worker: SlotId,
    from: SlotId,
) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT r.id FROM relays r
             WHERE r.sender = 'slot' AND r.from_slot = ?2 AND r.to_slot = ?1
               AND r.delivered_at IS NOT NULL
               AND NOT EXISTS (SELECT 1 FROM relays a
                               WHERE a.sender = 'slot' AND a.from_slot = ?1
                                 AND a.reply_to = r.id)
             ORDER BY r.id LIMIT 1",
            params![worker, from],
            |r| r.get(0),
        )
        .optional()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::Ledger;

    const T: &str = "argo:uat/root";

    fn claim(target: &str, slot: SlotId, kind: ClaimKind, via: Via) -> NewClaim<'_> {
        NewClaim {
            target,
            slot,
            kind,
            reason: None,
            item: None,
            tool_use_id: None,
            pin_revision: None,
            via,
        }
    }

    fn take(c: &Connection, target: &str, slot: SlotId, kind: ClaimKind, now: i64) {
        let out = claim_on(c, &claim(target, slot, kind, Via::Tool), now).unwrap();
        assert_eq!(out, ClaimOutcome::Granted);
    }

    fn queue(c: &Connection, target: &str, slot: SlotId, via: Via, now: i64) -> usize {
        match claim_on(c, &claim(target, slot, ClaimKind::Command, via), now).unwrap() {
            ClaimOutcome::Queued { position, .. } => position,
            other => panic!("expected a queue, got {other:?}"),
        }
    }

    fn holders(c: &Connection) -> Vec<(String, SlotId, ClaimKind)> {
        claims_on(c)
            .unwrap()
            .into_iter()
            .map(|c| (c.target, c.slot, c.kind))
            .collect()
    }

    fn texts(c: &Connection, to: SlotId) -> Vec<String> {
        pending_relays_on(c, to)
            .unwrap()
            .into_iter()
            .map(|r| r.text)
            .collect()
    }

    fn with<T>(f: impl FnOnce(&Connection) -> Result<T>) -> T {
        Ledger::in_memory().transaction(f).unwrap()
    }

    #[test]
    fn claim_free_target() {
        with(|c| {
            let new = NewClaim {
                reason: Some("pushing"),
                item: Some("DEMO-12"),
                tool_use_id: Some("tu-1"),
                ..claim(T, 1, ClaimKind::Command, Via::Hook)
            };
            assert_eq!(claim_on(c, &new, 100)?, ClaimOutcome::Granted);
            assert_eq!(
                claims_on(c)?,
                vec![Claim {
                    target: T.into(),
                    slot: 1,
                    kind: ClaimKind::Command,
                    reason: Some("pushing".into()),
                    item: Some("DEMO-12".into()),
                    since: 100,
                    last_used: 100,
                    pin_revision: None,
                }]
            );
            assert!(waits_on(c)?.is_empty());
            Ok(())
        })
    }

    #[test]
    fn claim_own_target_is_already_held() {
        with(|c| {
            take(c, T, 1, ClaimKind::Claim, 10);
            let again = claim_on(c, &claim(T, 1, ClaimKind::Command, Via::Hook), 20)?;
            assert_eq!(again, ClaimOutcome::AlreadyHeld);
            let held = &claims_on(c)?[0];
            assert_eq!(
                (held.kind, held.since, held.last_used),
                (ClaimKind::Claim, 10, 20)
            );
            assert!(waits_on(c)?.is_empty());
            Ok(())
        })
    }

    #[test]
    fn claim_held_target_queues_fifo() {
        with(|c| {
            take(c, T, 1, ClaimKind::Claim, 10);
            match claim_on(c, &claim(T, 2, ClaimKind::Command, Via::Hook), 11)? {
                ClaimOutcome::Queued { holder, position } => {
                    assert_eq!((holder.slot, holder.target.as_str()), (1, T));
                    assert_eq!(position, 1);
                }
                other => panic!("expected a queue, got {other:?}"),
            }
            assert_eq!(queue(c, T, 3, Via::Tool, 12), 2);
            assert_eq!(queue(c, T, 2, Via::Hook, 13), 1);
            let waits = waits_on(c)?;
            let order: Vec<_> = waits.iter().map(|w| (w.slot, w.at)).collect();
            assert_eq!(order, vec![(2, 11), (3, 12)]);
            Ok(())
        })
    }

    #[test]
    fn release_hands_to_first_waiter_with_relay() {
        with(|c| {
            take(c, T, 1, ClaimKind::Claim, 10);
            queue(c, T, 2, Via::Hook, 11);
            queue(c, T, 3, Via::Hook, 12);
            let h = release_on(c, T, Releaser::Slot(1), None, 20)?;
            assert_eq!(
                h,
                Some(Handoff {
                    target: T.into(),
                    from: 1,
                    to: Some(2)
                })
            );
            assert_eq!(holders(c), vec![(T.into(), 2, ClaimKind::Grant)]);
            let held = &claims_on(c)?[0];
            assert_eq!((held.since, held.last_used), (20, 20));
            assert_eq!(waits_on(c)?.len(), 1);
            let relays = pending_relays_on(c, 2)?;
            assert_eq!(relays.len(), 1);
            assert_eq!(
                relays[0].text,
                "S1 has released argo:uat/root. You now hold it."
            );
            assert_eq!(relays[0].from, Sender::Overseer);
            assert_eq!(relays[0].state, RelayState::Pending);
            assert!(texts(c, 1).is_empty());
            Ok(())
        })
    }

    #[test]
    fn release_with_back_on() {
        with(|c| {
            take(c, T, 1, ClaimKind::Pin, 10);
            queue(c, T, 2, Via::Hook, 11);
            release_on(c, T, Releaser::Slot(1), Some("main"), 20)?;
            assert_eq!(
                texts(c, 2),
                vec!["S1 has released argo:uat/root (back on main). You now hold it."]
            );
            Ok(())
        })
    }

    #[test]
    fn release_with_no_waiter_frees_the_target() {
        with(|c| {
            take(c, T, 1, ClaimKind::Claim, 10);
            let h = release_on(c, T, Releaser::Slot(1), None, 20)?;
            assert_eq!(
                h,
                Some(Handoff {
                    target: T.into(),
                    from: 1,
                    to: None
                })
            );
            assert!(claims_on(c)?.is_empty());
            assert!(relays_on(c, 10)?.is_empty());
            Ok(())
        })
    }

    #[test]
    fn force_release_tells_both() {
        with(|c| {
            take(c, T, 1, ClaimKind::Claim, 10);
            queue(c, T, 2, Via::Tool, 11);
            let h = release_on(c, T, Releaser::Force, None, 20)?;
            assert_eq!(h.map(|h| (h.from, h.to)), Some((1, Some(2))));
            assert_eq!(
                texts(c, 2),
                vec!["Jonathan released argo:uat/root. You now hold it."]
            );
            assert_eq!(
                texts(c, 1),
                vec!["Jonathan released argo:uat/root, which you held."]
            );
            assert_eq!(holders(c), vec![(T.into(), 2, ClaimKind::Claim)]);
            Ok(())
        })
    }

    #[test]
    fn release_by_a_waiter_leaves_the_queue() {
        with(|c| {
            take(c, T, 1, ClaimKind::Claim, 10);
            queue(c, T, 2, Via::Hook, 11);
            queue(c, T, 3, Via::Hook, 12);
            assert_eq!(release_on(c, T, Releaser::Slot(2), None, 20)?, None);
            let left: Vec<_> = waits_on(c)?.iter().map(|w| w.slot).collect();
            assert_eq!(left, vec![3]);
            assert_eq!(holders(c), vec![(T.into(), 1, ClaimKind::Claim)]);
            assert!(relays_on(c, 10)?.is_empty());
            Ok(())
        })
    }

    #[test]
    fn release_not_held_is_an_error() {
        let l = Ledger::in_memory();
        l.transaction(|c| {
            take(c, T, 1, ClaimKind::Claim, 10);
            Ok(())
        })
        .unwrap();
        let err = l
            .transaction(|c| release_on(c, T, Releaser::Slot(2), None, 20))
            .unwrap_err();
        assert_eq!(err.to_string(), "argo:uat/root is not held by S2");
        let err = l
            .transaction(|c| release_on(c, "repo:demo-api:main", Releaser::Slot(2), None, 20))
            .unwrap_err();
        assert_eq!(err.to_string(), "repo:demo-api:main is not held by S2");
        let err = l
            .transaction(|c| release_on(c, "repo:demo-api:main", Releaser::Force, None, 20))
            .unwrap_err();
        assert_eq!(err.to_string(), "repo:demo-api:main is not held");
    }

    #[test]
    fn hook_waiter_gets_a_grant_tool_waiter_a_claim() {
        with(|c| {
            take(c, T, 1, ClaimKind::Claim, 10);
            queue(c, T, 2, Via::Hook, 11);
            queue(c, T, 3, Via::Tool, 12);
            release_on(c, T, Releaser::Slot(1), None, 20)?;
            assert_eq!(holders(c), vec![(T.into(), 2, ClaimKind::Grant)]);
            let relay_2 = pending_relays_on(c, 2)?[0].id;
            let relay_id: Option<i64> =
                c.query_row("SELECT relay_id FROM claims", [], |r| r.get(0))?;
            assert_eq!(relay_id, Some(relay_2));
            release_on(c, T, Releaser::Slot(2), None, 30)?;
            assert_eq!(holders(c), vec![(T.into(), 3, ClaimKind::Claim)]);
            let relay_3 = pending_relays_on(c, 3)?[0].id;
            let relay_id: Option<i64> =
                c.query_row("SELECT relay_id FROM claims", [], |r| r.get(0))?;
            assert_eq!(relay_id, Some(relay_3));
            assert_eq!(
                texts(c, 3),
                vec!["S2 has released argo:uat/root. You now hold it."]
            );
            Ok(())
        })
    }

    #[test]
    fn touch_and_bind_grant() {
        with(|c| {
            take(c, T, 1, ClaimKind::Claim, 10);
            queue(c, T, 2, Via::Hook, 11);
            release_on(c, T, Releaser::Slot(1), None, 20)?;
            touch_on(c, T, 2, 25)?;
            assert_eq!(claims_on(c)?[0].last_used, 25);
            touch_on(c, T, 3, 30)?;
            assert_eq!(claims_on(c)?[0].last_used, 25);

            assert!(!bind_grant_on(c, T, 3, "tu-9")?);
            assert!(bind_grant_on(c, T, 2, "tu-9")?);
            assert_eq!(holders(c), vec![(T.into(), 2, ClaimKind::Command)]);
            let id: Option<String> =
                c.query_row("SELECT tool_use_id FROM claims", [], |r| r.get(0))?;
            assert_eq!(id.as_deref(), Some("tu-9"));

            let pin = NewClaim {
                pin_revision: Some("feature-x"),
                ..claim("argo:uat/demo-api", 4, ClaimKind::Pin, Via::Hook)
            };
            claim_on(c, &pin, 40)?;
            take(c, "repo:demo-api:main", 4, ClaimKind::Claim, 40);
            assert!(bind_grant_on(c, "argo:uat/demo-api", 4, "tu-10")?);
            assert!(!bind_grant_on(c, "repo:demo-api:main", 4, "tu-11")?);
            let kinds: Vec<_> = holders(c).into_iter().filter(|h| h.1 == 4).collect();
            assert_eq!(
                kinds,
                vec![
                    ("argo:uat/demo-api".into(), 4, ClaimKind::Command),
                    ("repo:demo-api:main".into(), 4, ClaimKind::Claim),
                ]
            );
            Ok(())
        })
    }

    #[test]
    fn post_tool_use_releases_by_id_or_all_commands() {
        with(|c| {
            let cmd = |target, id| NewClaim {
                tool_use_id: Some(id),
                ..claim(target, 1, ClaimKind::Command, Via::Hook)
            };
            claim_on(c, &cmd("a", "tu-1"), 10)?;
            claim_on(c, &cmd("b", "tu-2"), 10)?;
            claim_on(c, &cmd("e", "tu-2"), 10)?;
            take(c, "c", 1, ClaimKind::Pin, 10);
            take(c, "d", 1, ClaimKind::Claim, 10);
            take(c, "f", 2, ClaimKind::Command, 10);
            queue(c, "a", 3, Via::Hook, 11);

            let h = release_commands_on(c, 1, Some("tu-1"), 20)?;
            assert_eq!(
                h,
                vec![Handoff {
                    target: "a".into(),
                    from: 1,
                    to: Some(3)
                }]
            );
            let h = release_commands_on(c, 1, None, 21)?;
            let released: Vec<_> = h.iter().map(|h| h.target.as_str()).collect();
            assert_eq!(released, vec!["b", "e"]);
            assert_eq!(
                holders(c),
                vec![
                    ("a".into(), 3, ClaimKind::Grant),
                    ("c".into(), 1, ClaimKind::Pin),
                    ("d".into(), 1, ClaimKind::Claim),
                    ("f".into(), 2, ClaimKind::Command),
                ]
            );
            Ok(())
        })
    }

    /// S2 ends up with a delivered grant on `a`, an undelivered grant on
    /// `b`, a command claim on `c`, a tool claim on `d` and a wait on `e`.
    fn slot_2_with_one_of_each(c: &Connection) {
        take(c, "a", 1, ClaimKind::Claim, 10);
        take(c, "b", 1, ClaimKind::Claim, 10);
        take(c, "e", 1, ClaimKind::Claim, 10);
        queue(c, "a", 2, Via::Hook, 11);
        queue(c, "b", 2, Via::Hook, 11);
        release_on(c, "a", Releaser::Slot(1), None, 12).unwrap();
        let delivered = pending_relays_on(c, 2).unwrap()[0].id;
        mark_delivered_on(c, &[delivered], 13).unwrap();
        release_on(c, "b", Releaser::Slot(1), None, 14).unwrap();
        take(c, "c", 2, ClaimKind::Command, 15);
        take(c, "d", 2, ClaimKind::Claim, 15);
        queue(c, "e", 2, Via::Tool, 16);
    }

    #[test]
    fn stop_releases_delivered_grants_only() {
        with(|c| {
            slot_2_with_one_of_each(c);
            let h = release_on_stop_on(c, 2, 20)?;
            let released: Vec<_> = h.iter().map(|h| h.target.as_str()).collect();
            assert_eq!(released, vec!["a", "c"]);
            let left: Vec<_> = holders(c).into_iter().filter(|h| h.1 == 2).collect();
            assert_eq!(
                left,
                vec![
                    ("b".into(), 2, ClaimKind::Grant),
                    ("d".into(), 2, ClaimKind::Claim)
                ]
            );
            assert_eq!(waits_on(c)?.len(), 1);
            Ok(())
        })
    }

    #[test]
    fn drop_slot_hands_on_and_leaves_queues() {
        with(|c| {
            slot_2_with_one_of_each(c);
            queue(c, "b", 3, Via::Hook, 17);
            let h = drop_slot_on(c, 2, 20)?;
            assert_eq!(
                h,
                vec![
                    Handoff {
                        target: "a".into(),
                        from: 2,
                        to: None
                    },
                    Handoff {
                        target: "b".into(),
                        from: 2,
                        to: Some(3)
                    },
                    Handoff {
                        target: "c".into(),
                        from: 2,
                        to: None
                    },
                ]
            );
            assert_eq!(
                holders(c),
                vec![
                    ("b".into(), 3, ClaimKind::Grant),
                    ("d".into(), 2, ClaimKind::Claim),
                    ("e".into(), 1, ClaimKind::Claim),
                ]
            );
            assert!(waits_on(c)?.is_empty());
            assert_eq!(texts(c, 3), vec!["S2 has released b. You now hold it."]);
            Ok(())
        })
    }

    #[test]
    fn release_item_releases_its_claims() {
        with(|c| {
            let for_item = |target, slot, kind, item| NewClaim {
                item: Some(item),
                ..claim(target, slot, kind, Via::Tool)
            };
            claim_on(c, &for_item("a", 1, ClaimKind::Claim, "DEMO-12"), 10)?;
            claim_on(c, &for_item("b", 1, ClaimKind::Command, "DEMO-12"), 10)?;
            claim_on(c, &for_item("c", 1, ClaimKind::Claim, "DEMO-13"), 10)?;
            take(c, "d", 2, ClaimKind::Claim, 10);
            queue(c, "b", 2, Via::Tool, 11);
            let h = release_item_on(c, "DEMO-12", 20)?;
            assert_eq!(
                h,
                vec![
                    Handoff {
                        target: "a".into(),
                        from: 1,
                        to: None
                    },
                    Handoff {
                        target: "b".into(),
                        from: 1,
                        to: Some(2)
                    },
                ]
            );
            assert_eq!(
                holders(c),
                vec![
                    ("b".into(), 2, ClaimKind::Claim),
                    ("c".into(), 1, ClaimKind::Claim),
                    ("d".into(), 2, ClaimKind::Claim),
                ]
            );
            Ok(())
        })
    }

    #[test]
    fn relays_keep_pending_and_recent_delivered_newest_first() {
        with(|c| {
            let ids: Vec<i64> = (0..4)
                .map(|n| queue_relay_on(c, &Sender::You, 2, &format!("m{n}"), None, 10 + n))
                .collect::<Result<_>>()?;
            mark_delivered_on(c, &ids[..3], 20)?;
            let s = queue_relay_on(c, &Sender::Slot { slot: 0 }, 1, "from s0", None, 30)?;
            let got: Vec<_> = relays_on(c, 2)?
                .into_iter()
                .map(|r| (r.id, r.state))
                .collect();
            assert_eq!(
                got,
                vec![
                    (s, RelayState::Pending),
                    (ids[3], RelayState::Pending),
                    (ids[2], RelayState::Delivered),
                    (ids[1], RelayState::Delivered),
                ]
            );
            let r = &relays_on(c, 0)?[0];
            assert_eq!(r.from, Sender::Slot { slot: 0 });
            assert_eq!((r.to, r.at, r.delivered_at), (1, 30, None));
            assert_eq!(r.held, None);
            assert_eq!(pending_relays_on(c, 2)?.len(), 1);
            let d = relays_on(c, 1)?;
            assert_eq!(d[2].delivered_at, Some(20));
            Ok(())
        })
    }

    #[test]
    fn unanswered_from_counts_one_reply() {
        with(|c| {
            let s0 = Sender::Slot { slot: 0 };
            let s2 = Sender::Slot { slot: 2 };
            assert_eq!(unanswered_from_on(c, 2, 0)?, None);
            let first = queue_relay_on(c, &s0, 2, "check uat/root", None, 10)?;
            assert_eq!(unanswered_from_on(c, 2, 0)?, None);
            mark_delivered_on(c, &[first], 11)?;
            let second = queue_relay_on(c, &s0, 2, "and staging-eu", None, 12)?;
            queue_relay_on(c, &Sender::You, 2, "hi", None, 12)?;
            assert_eq!(unanswered_from_on(c, 2, 0)?, Some(first));
            assert_eq!(unanswered_from_on(c, 2, 1)?, None);
            queue_relay_on(c, &s2, 0, "done", Some(first), 13)?;
            assert_eq!(unanswered_from_on(c, 2, 0)?, None);
            mark_delivered_on(c, &[second], 14)?;
            assert_eq!(unanswered_from_on(c, 2, 0)?, Some(second));
            Ok(())
        })
    }
}
