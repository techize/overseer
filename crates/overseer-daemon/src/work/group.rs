//! Grouping beads under their Jira tickets.

use std::collections::HashMap;

use overseer_proto::{ItemKind, WorkItem};
use regex::Regex;

use super::sources::{Bead, Ticket};

/// The first ticket key in the bead title, else in its external ref.
/// `pattern` is expected to carry word boundaries (see `compile_pattern`).
pub fn ticket_of(bead: &Bead, pattern: &Regex) -> Option<String> {
    pattern
        .find(&bead.title)
        .or_else(|| bead.external_ref.as_deref().and_then(|r| pattern.find(r)))
        .map(|m| m.as_str().to_owned())
}

fn bead_item(b: &Bead, parent: Option<&str>) -> WorkItem {
    WorkItem {
        key: b.id.clone(),
        kind: ItemKind::Bead,
        title: b.title.clone(),
        status: b.status.clone(),
        priority: b.priority.clone(),
        parent: parent.map(str::to_owned),
        owner: None,
        in_list: true,
        folder: None,
    }
}

fn ticket_item(t: &Ticket) -> WorkItem {
    WorkItem {
        key: t.key.clone(),
        kind: ItemKind::Ticket,
        title: t.title.clone(),
        status: t.status.clone(),
        priority: t.priority.clone(),
        parent: None,
        owner: None,
        in_list: true,
        folder: None,
    }
}

/// Tickets in Jira order each followed by their beads, then tickets known only
/// from beads, then beads with no ticket.
pub fn group(tickets: &[Ticket], beads: &[Bead], pattern: &Regex) -> Vec<WorkItem> {
    let mut by_key: HashMap<String, Vec<&Bead>> = HashMap::new();
    let mut key_order: Vec<String> = Vec::new();
    let mut standalone: Vec<&Bead> = Vec::new();
    for b in beads {
        match ticket_of(b, pattern) {
            Some(k) => {
                if !by_key.contains_key(&k) {
                    key_order.push(k.clone());
                }
                by_key.entry(k).or_default().push(b);
            }
            None => standalone.push(b),
        }
    }
    let mut out = Vec::new();
    for t in tickets {
        out.push(ticket_item(t));
        for b in by_key.remove(&t.key).unwrap_or_default() {
            out.push(bead_item(b, Some(&t.key)));
        }
    }
    for k in key_order {
        let Some(bs) = by_key.remove(&k) else {
            continue;
        };
        out.push(ticket_item(&Ticket {
            key: k.clone(),
            title: k.clone(),
            status: "unknown".into(),
            priority: None,
        }));
        for b in bs {
            out.push(bead_item(b, Some(&k)));
        }
    }
    out.extend(standalone.into_iter().map(|b| bead_item(b, None)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::compile_pattern;

    fn bead(id: &str, title: &str, ext: Option<&str>) -> Bead {
        Bead {
            id: id.into(),
            title: title.into(),
            status: "open".into(),
            priority: None,
            external_ref: ext.map(Into::into),
            labels: vec![],
        }
    }
    fn tk(key: &str) -> Ticket {
        Ticket {
            key: key.into(),
            title: format!("{key} title"),
            status: "To Do".into(),
            priority: None,
        }
    }
    fn pat() -> Regex {
        compile_pattern("[A-Z]+-[0-9]+").unwrap()
    }

    #[test]
    fn ticket_in_title_or_external_ref() {
        let p = pat();
        assert_eq!(
            ticket_of(&bead("a", "fix DEMO-12 now", None), &p).as_deref(),
            Some("DEMO-12")
        );
        assert_eq!(
            ticket_of(
                &bead("a", "no key", Some("https://x.test/browse/DEMO-9")),
                &p
            )
            .as_deref(),
            Some("DEMO-9")
        );
        assert_eq!(ticket_of(&bead("a", "no key", None), &p), None);
    }

    #[test]
    fn pattern_does_not_match_a_longer_key() {
        let p = compile_pattern("DEMO-123").unwrap();
        assert_eq!(ticket_of(&bead("a", "DEMO-1234 thing", None), &p), None);
        assert_eq!(
            ticket_of(&bead("a", "DEMO-123 thing", None), &p).as_deref(),
            Some("DEMO-123")
        );
    }

    #[test]
    fn first_of_two_keys_wins() {
        let b = bead("a", "DEMO-2 and DEMO-1", None);
        assert_eq!(ticket_of(&b, &pat()).as_deref(), Some("DEMO-2"));
    }

    #[test]
    fn bad_pattern_is_an_error_not_a_panic() {
        assert!(compile_pattern("[A-Z+").is_err());
    }

    #[test]
    fn beads_group_under_their_ticket_in_jira_order() {
        let items = group(
            &[tk("DEMO-2"), tk("DEMO-1")],
            &[
                bead("b1", "DEMO-1 x", None),
                bead("b2", "DEMO-2 y", None),
                bead("b3", "DEMO-1 z", None),
            ],
            &pat(),
        );
        let keys: Vec<_> = items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, ["DEMO-2", "b2", "DEMO-1", "b1", "b3"]);
        assert_eq!(items[1].parent.as_deref(), Some("DEMO-2"));
        assert!(items
            .iter()
            .all(|i| i.in_list && i.owner.is_none() && i.folder.is_none()));
    }

    #[test]
    fn bead_without_jira_ticket_gets_a_ticket_node() {
        let items = group(&[tk("DEMO-1")], &[bead("b1", "DEMO-9 lone", None)], &pat());
        assert_eq!(items.len(), 3);
        assert_eq!(items[1].key, "DEMO-9");
        assert_eq!(items[1].kind, ItemKind::Ticket);
        assert_eq!(items[1].title, "DEMO-9");
        assert_eq!(items[1].status, "unknown");
        assert_eq!(items[2].parent.as_deref(), Some("DEMO-9"));
    }

    #[test]
    fn standalone_beads_last() {
        let items = group(
            &[tk("DEMO-1")],
            &[bead("s", "plain", None), bead("b1", "DEMO-9 x", None)],
            &pat(),
        );
        let keys: Vec<_> = items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, ["DEMO-1", "DEMO-9", "b1", "s"]);
        assert_eq!(items[3].parent, None);
    }
}
