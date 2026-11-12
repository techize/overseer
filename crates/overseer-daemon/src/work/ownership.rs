//! Ownership rules for assigning a work item to a slot.

use std::collections::HashMap;

use overseer_proto::{slot_name, ItemKind, SlotId, WorkItem};

/// What an assignment request should do.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Assign,
    AlreadyYours,
    Refuse {
        owner: SlotId,
    },
    /// The caller assigns when the request is confirmed and stores a notice for `tell`.
    /// With `from` it is a move from that slot, which gets the move's notice too.
    Warn {
        message: String,
        tell: Vec<SlotId>,
        from: Option<SlotId>,
    },
    Move {
        from: SlotId,
    },
}

pub fn decide(
    items: &[WorkItem],
    owners: &HashMap<String, SlotId>,
    key: &str,
    slot: SlotId,
    _confirm: bool,
    move_owner: bool,
) -> Result<Decision, String> {
    let item = items.iter().find(|i| i.key == key);
    let direct = owners.get(key).copied();
    let Some(item) = item else {
        return match direct {
            Some(owner) => Ok(direct_decision(owner, slot, move_owner)),
            None => Err(format!("no work item {key}")),
        };
    };
    if let Some(owner) = direct {
        return Ok(match direct_decision(owner, slot, move_owner) {
            // Moving a bead under someone else's ticket still asks first.
            Decision::Move { from } if item.kind == ItemKind::Bead => {
                match parent_warning(item, owners, slot, Some(from)) {
                    Some(warn) => warn,
                    None => Decision::Move { from },
                }
            }
            d => d,
        });
    }
    match item.kind {
        ItemKind::Bead => Ok(parent_warning(item, owners, slot, None).unwrap_or(Decision::Assign)),
        ItemKind::Ticket => {
            let held: Vec<(&str, SlotId)> = items
                .iter()
                .filter(|i| i.parent.as_deref() == Some(key))
                .filter_map(|i| owners.get(&i.key).map(|o| (i.key.as_str(), *o)))
                .filter(|(_, o)| *o != slot)
                .collect();
            if held.is_empty() {
                return Ok(Decision::Assign);
            }
            let list = held
                .iter()
                .map(|(k, o)| format!("{k} ({})", slot_name(*o)))
                .collect::<Vec<_>>()
                .join(", ");
            let mut tell: Vec<SlotId> = held.iter().map(|(_, o)| *o).collect();
            tell.sort_unstable();
            tell.dedup();
            Ok(Decision::Warn {
                message: format!(
                    "{key} has beads owned by other slots: {list}. Assign to {} anyway?",
                    slot_name(slot)
                ),
                tell,
                from: None,
            })
        }
    }
}

/// The warning for a bead whose ticket another slot owns, else `None`.
fn parent_warning(
    item: &WorkItem,
    owners: &HashMap<String, SlotId>,
    slot: SlotId,
    from: Option<SlotId>,
) -> Option<Decision> {
    let parent = item.parent.as_deref()?;
    let owner = *owners.get(parent)?;
    (owner != slot).then(|| Decision::Warn {
        message: format!(
            "This bead is under {parent}, owned by {}. Assign to {} anyway?",
            slot_name(owner),
            slot_name(slot)
        ),
        tell: vec![owner],
        from,
    })
}

fn direct_decision(owner: SlotId, slot: SlotId, move_owner: bool) -> Decision {
    if owner == slot {
        Decision::AlreadyYours
    } else if move_owner {
        Decision::Move { from: owner }
    } else {
        Decision::Refuse { owner }
    }
}

/// Beads that were standalone and owned in `before` and now sit under a ticket
/// owned by a different slot in `after`.
pub fn promotions(
    before: &[WorkItem],
    after: &[WorkItem],
    owners: &HashMap<String, SlotId>,
) -> Vec<(String, SlotId, SlotId)> {
    after
        .iter()
        .filter(|i| i.kind == ItemKind::Bead)
        .filter_map(|bead| {
            let parent = bead.parent.as_deref()?;
            let was_standalone = before
                .iter()
                .any(|b| b.key == bead.key && b.kind == ItemKind::Bead && b.parent.is_none());
            if !was_standalone {
                return None;
            }
            let a = *owners.get(&bead.key)?;
            let b = *owners.get(parent)?;
            (a != b).then(|| (bead.key.clone(), a, b))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(key: &str, kind: ItemKind, parent: Option<&str>) -> WorkItem {
        WorkItem {
            key: key.into(),
            kind,
            title: key.into(),
            status: "open".into(),
            priority: None,
            parent: parent.map(str::to_owned),
            owner: None,
            in_list: true,
            folder: None,
        }
    }
    fn ticket(k: &str) -> WorkItem {
        item(k, ItemKind::Ticket, None)
    }
    fn bead(k: &str, p: Option<&str>) -> WorkItem {
        item(k, ItemKind::Bead, p)
    }
    fn owners(v: &[(&str, SlotId)]) -> HashMap<String, SlotId> {
        v.iter().map(|(k, s)| (k.to_string(), *s)).collect()
    }

    #[test]
    fn unknown_key_is_an_error() {
        let r = decide(
            &[ticket("DEMO-12")],
            &owners(&[]),
            "DEMO-99",
            4,
            false,
            false,
        );
        assert_eq!(r, Err("no work item DEMO-99".into()));
    }

    #[test]
    fn free_ticket_is_assigned() {
        let r = decide(
            &[ticket("DEMO-12")],
            &owners(&[]),
            "DEMO-12",
            4,
            false,
            false,
        );
        assert_eq!(r, Ok(Decision::Assign));
    }

    #[test]
    fn owned_by_this_slot_is_already_yours() {
        let o = owners(&[("DEMO-12", 4)]);
        let r = decide(&[ticket("DEMO-12")], &o, "DEMO-12", 4, false, false);
        assert_eq!(r, Ok(Decision::AlreadyYours));
    }

    #[test]
    fn ticket_owned_by_another_is_refused_or_moved() {
        let o = owners(&[("DEMO-12", 3)]);
        let items = [ticket("DEMO-12")];
        assert_eq!(
            decide(&items, &o, "DEMO-12", 4, false, false),
            Ok(Decision::Refuse { owner: 3 })
        );
        assert_eq!(
            decide(&items, &o, "DEMO-12", 4, true, true),
            Ok(Decision::Move { from: 3 })
        );
    }

    #[test]
    fn standalone_bead_owned_by_another_is_refused_or_moved() {
        let o = owners(&[("ov-3", 2)]);
        let items = [bead("ov-3", None)];
        assert_eq!(
            decide(&items, &o, "ov-3", 4, false, false),
            Ok(Decision::Refuse { owner: 2 })
        );
        assert_eq!(
            decide(&items, &o, "ov-3", 4, false, true),
            Ok(Decision::Move { from: 2 })
        );
    }

    #[test]
    fn owned_item_missing_from_the_list_is_still_known() {
        let o = owners(&[("ov-3", 2)]);
        let r = decide(&[], &o, "ov-3", 4, false, false);
        assert_eq!(r, Ok(Decision::Refuse { owner: 2 }));
    }

    #[test]
    fn bead_under_a_ticket_owned_elsewhere_warns() {
        let items = [ticket("DEMO-12"), bead("ov-3", Some("DEMO-12"))];
        let o = owners(&[("DEMO-12", 3)]);
        let expect = Decision::Warn {
            message: "This bead is under DEMO-12, owned by S3. Assign to S4 anyway?".into(),
            tell: vec![3],
            from: None,
        };
        assert_eq!(
            decide(&items, &o, "ov-3", 4, false, false),
            Ok(expect.clone())
        );
        assert_eq!(decide(&items, &o, "ov-3", 4, true, false), Ok(expect));
    }

    #[test]
    fn moving_a_bead_under_a_ticket_owned_elsewhere_warns() {
        let items = [ticket("DEMO-12"), bead("ov-3", Some("DEMO-12"))];
        let o = owners(&[("DEMO-12", 3), ("ov-3", 2)]);
        let expect = Decision::Warn {
            message: "This bead is under DEMO-12, owned by S3. Assign to S4 anyway?".into(),
            tell: vec![3],
            from: Some(2),
        };
        assert_eq!(
            decide(&items, &o, "ov-3", 4, false, true),
            Ok(expect.clone())
        );
        assert_eq!(decide(&items, &o, "ov-3", 4, true, true), Ok(expect));
        // Without --move it is still a refusal.
        assert_eq!(
            decide(&items, &o, "ov-3", 4, false, false),
            Ok(Decision::Refuse { owner: 2 })
        );
        // Moving it to the ticket's owner needs no question.
        assert_eq!(
            decide(&items, &o, "ov-3", 3, false, true),
            Ok(Decision::Move { from: 2 })
        );
    }

    #[test]
    fn bead_under_a_ticket_owned_by_this_slot_is_assigned() {
        let items = [ticket("DEMO-12"), bead("ov-3", Some("DEMO-12"))];
        let o = owners(&[("DEMO-12", 4)]);
        assert_eq!(
            decide(&items, &o, "ov-3", 4, false, false),
            Ok(Decision::Assign)
        );
    }

    #[test]
    fn ticket_with_beads_owned_elsewhere_warns() {
        let items = [
            ticket("DEMO-12"),
            bead("ov-3", Some("DEMO-12")),
            bead("ov-4", Some("DEMO-12")),
            bead("ov-5", Some("DEMO-12")),
        ];
        let o = owners(&[("ov-3", 3), ("ov-4", 5), ("ov-5", 4)]);
        let expect = Decision::Warn {
            message: "DEMO-12 has beads owned by other slots: ov-3 (S3), ov-4 (S5). \
                      Assign to S4 anyway?"
                .into(),
            tell: vec![3, 5],
            from: None,
        };
        assert_eq!(
            decide(&items, &o, "DEMO-12", 4, false, false),
            Ok(expect.clone())
        );
        assert_eq!(decide(&items, &o, "DEMO-12", 4, true, false), Ok(expect));
    }

    #[test]
    fn ticket_with_only_own_beads_is_assigned() {
        let items = [ticket("DEMO-12"), bead("ov-5", Some("DEMO-12"))];
        let o = owners(&[("ov-5", 4)]);
        assert_eq!(
            decide(&items, &o, "DEMO-12", 4, false, false),
            Ok(Decision::Assign)
        );
    }

    #[test]
    fn promotion_detected_once() {
        let before = [bead("ov-3", None)];
        let after = [ticket("DEMO-12"), bead("ov-3", Some("DEMO-12"))];
        let o = owners(&[("ov-3", 2), ("DEMO-12", 5)]);
        assert_eq!(
            promotions(&before, &after, &o),
            vec![("ov-3".to_string(), 2, 5)]
        );
        assert!(promotions(&after, &after, &o).is_empty());
    }

    #[test]
    fn no_promotion_when_owners_match() {
        let before = [bead("ov-3", None)];
        let after = [ticket("DEMO-12"), bead("ov-3", Some("DEMO-12"))];
        let o = owners(&[("ov-3", 2), ("DEMO-12", 2)]);
        assert!(promotions(&before, &after, &o).is_empty());
    }
}
