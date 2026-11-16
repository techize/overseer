//! The sprint kanban and open-PR panel models: pure functions from a
//! `WorkView` (and the clock, passed in as `now`) to what the board draws.

use std::path::PathBuf;

use overseer_proto::{ItemKind, SlotId, Stage, WorkItem, WorkView};

const DAY: i64 = 86_400;
/// Cards a kanban column shows before `+N more`.
const MAX_CARDS: usize = 4;
/// Rows the PR panel shows before `+N more`.
const MAX_PR_ROWS: usize = 8;

/// The tickets in the sprint list: tickets still in the list, without the
/// placeholders made from bead titles.
pub fn sprint_tickets(view: &WorkView) -> Vec<&WorkItem> {
    view.items
        .iter()
        .filter(|i| i.kind == ItemKind::Ticket && i.in_list && !i.is_placeholder())
        .collect()
}

/// Ticket priority as the board shows it; the order is the sort order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Prio {
    Breaker,
    High,
    Med,
    Low,
}

pub fn prio_of(priority: Option<&str>) -> Prio {
    match priority {
        Some("Sprint Breaker") => Prio::Breaker,
        Some("Highest") | Some("High") => Prio::High,
        Some("Low") | Some("Lowest") => Prio::Low,
        _ => Prio::Med,
    }
}

/// `12m`, `5h`, `3d`: the largest whole unit. Negative counts as 0.
pub fn age_text(secs: i64) -> String {
    let s = secs.max(0);
    if s < 3600 {
        format!("{}m", s / 60)
    } else if s < DAY {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / DAY)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgeTone {
    Fresh,
    Ageing,
    Old,
}

/// A PR under 2 days old is fresh, from 2 days ageing, from 5 days old.
pub fn pr_age_tone(secs: i64) -> AgeTone {
    if secs >= 5 * DAY {
        AgeTone::Old
    } else if secs >= 2 * DAY {
        AgeTone::Ageing
    } else {
        AgeTone::Fresh
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TicketCard {
    pub key: String,
    pub title: String,
    pub prio: Prio,
    /// How long it has been in its stage, when the daemon saw it move.
    pub age: Option<String>,
    pub slot: Option<SlotId>,
    /// `#<id>` of the oldest linked PR and its tone.
    pub pr: Option<(String, AgeTone)>,
    /// Beads grouped under the ticket.
    pub beads: usize,
    /// The Jira status, when the ticket is In progress only because no
    /// stage list named that status.
    pub raw_status: Option<String>,
    pub folder: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub stage: Stage,
    pub total: usize,
    pub cards: Vec<TicketCard>,
    pub more: usize,
}

fn card(view: &WorkView, t: &WorkItem, now: i64) -> TicketCard {
    let mut linked: Vec<_> = view
        .prs
        .iter()
        .filter(|p| p.ticket.as_deref() == Some(t.key.as_str()))
        .collect();
    // Oldest first; a PR with no date last.
    linked.sort_by_key(|p| (p.created.is_none(), p.created));
    let pr = linked.first().map(|p| {
        let tone = p.created.map_or(AgeTone::Fresh, |c| pr_age_tone(now - c));
        (format!("#{}", p.id), tone)
    });
    let raw = t.status.trim();
    let raw_status = (t.stage == Some(Stage::InProgress)
        && !raw.is_empty()
        && raw.to_lowercase() != "in progress")
        .then(|| t.status.clone());
    TicketCard {
        key: t.key.clone(),
        title: t.title.clone(),
        prio: prio_of(t.priority.as_deref()),
        age: t.stage_since.map(|s| age_text(now - s)),
        slot: t.owner,
        pr,
        beads: view
            .items
            .iter()
            .filter(|b| b.kind == ItemKind::Bead && b.parent.as_deref() == Some(t.key.as_str()))
            .count(),
        raw_status,
        folder: t.folder.clone(),
    }
}

/// Six columns in `Stage::ALL` order. Cards sort by priority, then by the
/// time in stage (oldest first, unknown last), and are cut to four. The
/// Done column carries no cards: the board shows its count and `last_done`.
pub fn columns(view: &WorkView, now: i64) -> Vec<Column> {
    let tickets = sprint_tickets(view);
    Stage::ALL
        .iter()
        .map(|&stage| {
            let mut mine: Vec<&WorkItem> = tickets
                .iter()
                .copied()
                .filter(|t| t.stage == Some(stage))
                .collect();
            let total = mine.len();
            if stage == Stage::Done {
                return Column {
                    stage,
                    total,
                    cards: Vec::new(),
                    more: 0,
                };
            }
            mine.sort_by_key(|t| {
                (
                    prio_of(t.priority.as_deref()),
                    t.stage_since.is_none(),
                    t.stage_since,
                )
            });
            let more = total.saturating_sub(MAX_CARDS);
            mine.truncate(MAX_CARDS);
            Column {
                stage,
                total,
                cards: mine.into_iter().map(|t| card(view, t, now)).collect(),
                more,
            }
        })
        .collect()
}

/// The Done ticket that moved last, else the first Done ticket.
pub fn last_done(view: &WorkView) -> Option<String> {
    let done: Vec<_> = sprint_tickets(view)
        .into_iter()
        .filter(|t| t.stage == Some(Stage::Done))
        .collect();
    done.iter()
        .filter(|t| t.stage_since.is_some())
        .max_by_key(|t| t.stage_since)
        .or_else(|| done.first())
        .map(|t| t.key.clone())
}

#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
    /// Tickets per stage, in `Stage::ALL` order.
    pub per_stage: [usize; 6],
    /// Tickets that should be done by today at an even pace.
    pub expected: Option<usize>,
    /// Sprint Breaker tickets not yet done.
    pub breakers_open: usize,
}

/// `None` when the sprint has no tickets.
pub fn progress(view: &WorkView) -> Option<Progress> {
    let tickets = sprint_tickets(view);
    let total = tickets.len();
    if total == 0 {
        return None;
    }
    let mut per_stage = [0; 6];
    for t in &tickets {
        if let Some(i) = t
            .stage
            .and_then(|s| Stage::ALL.iter().position(|&x| x == s))
        {
            per_stage[i] += 1;
        }
    }
    let done = per_stage[Stage::ALL.len() - 1];
    let expected = view
        .sprint_day
        .filter(|d| d.days > 0)
        .map(|d| total * d.day as usize / d.days as usize);
    let breakers_open = tickets
        .iter()
        .filter(|t| t.stage != Some(Stage::Done) && prio_of(t.priority.as_deref()) == Prio::Breaker)
        .count();
    Some(Progress {
        done,
        total,
        per_stage,
        expected,
        breakers_open,
    })
}

/// The warning when fewer tickets are done than an even pace expects.
pub fn behind_text(p: &Progress) -> Option<String> {
    let expected = p.expected?;
    (p.done < expected).then(|| format!("Behind pace: {expected} expected done by today"))
}

#[derive(Debug, Clone, PartialEq)]
pub struct PrRow {
    pub repo: String,
    pub id: u64,
    pub ticket: Option<String>,
    pub title: String,
    pub age: Option<String>,
    pub tone: AgeTone,
    pub url: Option<String>,
}

/// Open PRs oldest first (no date last), at most 8, and how many are left.
pub fn pr_rows(view: &WorkView, now: i64) -> (Vec<PrRow>, usize) {
    let mut prs: Vec<_> = view.prs.iter().collect();
    prs.sort_by_key(|p| (p.created.is_none(), p.created));
    let more = prs.len().saturating_sub(MAX_PR_ROWS);
    prs.truncate(MAX_PR_ROWS);
    let rows = prs
        .into_iter()
        .map(|p| PrRow {
            repo: p.repo.clone(),
            id: p.id,
            ticket: p.ticket.clone(),
            title: p.title.clone(),
            age: p.created.map(|c| age_text(now - c)),
            tone: p.created.map_or(AgeTone::Fresh, |c| pr_age_tone(now - c)),
            url: p.url.clone(),
        })
        .collect();
    (rows, more)
}

/// `OPEN PRS · 3`, or `OPEN PRS · 3 · oldest 6d` when any PR has a date.
pub fn pr_header(view: &WorkView, now: i64) -> String {
    let mut text = format!("OPEN PRS · {}", view.prs.len());
    if let Some(oldest) = view.prs.iter().filter_map(|p| p.created).min() {
        text.push_str(&format!(" · oldest {}", age_text(now - oldest)));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::done_count;
    use overseer_proto::{PullRequest, SprintDay};

    fn ticket(key: &str, stage: Stage, prio: Option<&str>) -> WorkItem {
        WorkItem {
            key: key.into(),
            kind: ItemKind::Ticket,
            title: format!("{key} title"),
            status: "In Progress".into(),
            priority: prio.map(String::from),
            parent: None,
            owner: None,
            in_list: true,
            folder: None,
            stage: Some(stage),
            stage_since: None,
        }
    }

    fn pr(id: u64, created: Option<i64>) -> PullRequest {
        PullRequest {
            repo: "demo-api".into(),
            id,
            title: format!("PR {id}"),
            branch: "demo-branch".into(),
            url: None,
            author: None,
            created,
            ticket: None,
        }
    }

    fn view(items: Vec<WorkItem>) -> WorkView {
        WorkView {
            items,
            ..crate::app::empty_work()
        }
    }

    #[test]
    fn age_text_and_tones() {
        assert_eq!(age_text(59), "0m");
        assert_eq!(age_text(3600 * 5), "5h");
        assert_eq!(age_text(86_400 * 3), "3d");
        assert_eq!(age_text(-30), "0m");
        assert_eq!(pr_age_tone(86_400 * 2 - 1), AgeTone::Fresh);
        assert_eq!(pr_age_tone(86_400 * 2), AgeTone::Ageing);
        assert_eq!(pr_age_tone(86_400 * 5 - 1), AgeTone::Ageing);
        assert_eq!(pr_age_tone(86_400 * 5), AgeTone::Old);
    }

    #[test]
    fn prio_mapping() {
        assert_eq!(prio_of(Some("Sprint Breaker")), Prio::Breaker);
        assert_eq!(prio_of(Some("Highest")), Prio::High);
        assert_eq!(prio_of(Some("High")), Prio::High);
        assert_eq!(prio_of(Some("Medium")), Prio::Med);
        assert_eq!(prio_of(Some("Low")), Prio::Low);
        assert_eq!(prio_of(Some("Lowest")), Prio::Low);
        assert_eq!(prio_of(None), Prio::Med);
        assert_eq!(prio_of(Some("Whatever")), Prio::Med);
    }

    #[test]
    fn columns_sort_and_overflow() {
        let mut items = vec![
            ticket("DEMO-1", Stage::InProgress, Some("High")),
            ticket("DEMO-2", Stage::InProgress, Some("High")),
            ticket("DEMO-3", Stage::InProgress, Some("Sprint Breaker")),
            ticket("DEMO-4", Stage::InProgress, None),
            ticket("DEMO-5", Stage::InProgress, None),
            ticket("DEMO-6", Stage::InProgress, Some("Low")),
        ];
        items[0].stage_since = Some(100);
        items[1].stage_since = Some(50);
        let cols = columns(&view(items), 1000);
        assert_eq!(cols.len(), 6);
        assert_eq!(
            cols.iter().map(|c| c.stage).collect::<Vec<_>>(),
            Stage::ALL.to_vec()
        );
        let c = &cols[1];
        assert_eq!((c.total, c.cards.len(), c.more), (6, 4, 2));
        assert_eq!(c.cards[0].key, "DEMO-3");
        assert_eq!(c.cards[1].key, "DEMO-2");
        assert_eq!(c.cards[1].age.as_deref(), Some("15m"));
        assert_eq!(c.cards[2].key, "DEMO-1");
        assert_eq!(c.cards[3].age, None);
        assert_eq!(cols[0].total, 0);
    }

    #[test]
    fn done_column_has_count_and_last_done_but_no_cards() {
        let mut items = vec![
            ticket("DEMO-1", Stage::Done, None),
            ticket("DEMO-2", Stage::Done, None),
            ticket("DEMO-3", Stage::Done, None),
        ];
        let v = view(items.clone());
        assert_eq!(last_done(&v).as_deref(), Some("DEMO-1"));
        items[1].stage_since = Some(10);
        items[2].stage_since = Some(20);
        let v = view(items);
        assert_eq!(last_done(&v).as_deref(), Some("DEMO-3"));
        let done = &columns(&v, 100)[5];
        assert_eq!((done.total, done.cards.len(), done.more), (3, 0, 0));
        assert_eq!(last_done(&view(vec![])), None);
    }

    #[test]
    fn card_links_prs_and_beads() {
        let mut v = view(vec![ticket("DEMO-1", Stage::InReview, None)]);
        let mut bead = ticket("demo-b1", Stage::InReview, None);
        bead.kind = ItemKind::Bead;
        bead.parent = Some("DEMO-1".into());
        v.items.push(bead);
        let mut newer = pr(8, Some(1000));
        newer.ticket = Some("DEMO-1".into());
        let mut older = pr(7, Some(1000 - 3 * 86_400));
        older.ticket = Some("DEMO-1".into());
        v.prs = vec![newer, older, pr(9, Some(0))];
        let c = &columns(&v, 1000)[2].cards[0];
        assert_eq!(c.pr, Some(("#7".to_string(), AgeTone::Ageing)));
        assert_eq!(c.beads, 1);
    }

    #[test]
    fn unmapped_status_shows_raw() {
        let mut odd = ticket("DEMO-1", Stage::InProgress, None);
        odd.status = "Ready for Release".into();
        let plain = ticket("DEMO-2", Stage::InProgress, None);
        let mut review = ticket("DEMO-3", Stage::InReview, None);
        review.status = "Code Review".into();
        let cols = columns(&view(vec![odd, plain, review]), 0);
        assert_eq!(
            cols[1].cards[0].raw_status.as_deref(),
            Some("Ready for Release")
        );
        assert_eq!(cols[1].cards[1].raw_status, None);
        assert_eq!(cols[2].cards[0].raw_status, None);
    }

    #[test]
    fn progress_and_pace() {
        let mut items: Vec<_> = (0..10)
            .map(|i| {
                let stage = if i < 3 { Stage::Done } else { Stage::ToDo };
                ticket(&format!("DEMO-{i}"), stage, None)
            })
            .collect();
        items[5].priority = Some("Sprint Breaker".into());
        items[0].priority = Some("Sprint Breaker".into());
        let mut v = view(items);
        v.sprint_day = Some(SprintDay { day: 6, days: 10 });
        let p = progress(&v).unwrap();
        assert_eq!((p.done, p.total), (3, 10));
        assert_eq!(p.per_stage, [7, 0, 0, 0, 0, 3]);
        assert_eq!(p.expected, Some(6));
        assert_eq!(p.breakers_open, 1);
        assert_eq!(
            behind_text(&p).as_deref(),
            Some("Behind pace: 6 expected done by today")
        );
        v.sprint_day = None;
        let p = progress(&v).unwrap();
        assert_eq!(p.expected, None);
        assert_eq!(behind_text(&p), None);
        assert!(progress(&view(vec![])).is_none());
    }

    #[test]
    fn pr_rows_oldest_first_capped() {
        let now = 10 * 86_400;
        let mut v = view(vec![]);
        v.prs = (1..=9)
            .map(|i| pr(i, Some(now - i as i64 * 86_400)))
            .collect();
        v.prs.push(pr(10, None));
        let (rows, more) = pr_rows(&v, now);
        assert_eq!((rows.len(), more), (8, 2));
        assert_eq!(rows[0].id, 9);
        assert_eq!(rows[0].tone, AgeTone::Old);
        assert_eq!(rows[7].id, 2);
        let (rows, _) = pr_rows(
            &WorkView {
                prs: vec![pr(1, None), pr(2, Some(now))],
                ..view(vec![])
            },
            now,
        );
        assert_eq!((rows[0].id, rows[1].id), (2, 1));
        assert_eq!((rows[1].age.clone(), rows[1].tone), (None, AgeTone::Fresh));
        v.prs = (1..=10)
            .map(|i| pr(i, Some(now - if i == 3 { 6 * 86_400 } else { 3_600 })))
            .collect();
        assert_eq!(pr_header(&v, now), "OPEN PRS · 10 · oldest 6d");
        assert_eq!(pr_header(&view(vec![]), now), "OPEN PRS · 0");
    }

    #[test]
    fn done_count_uses_stage_when_present() {
        let mut shipped = ticket("DEMO-1", Stage::Done, None);
        shipped.status = "Shipped".into();
        let v = view(vec![shipped]);
        assert_eq!(done_count(&v), (1, 1));
        // A stage-less ticket (old daemon) still falls back to the status.
        let mut old = ticket("DEMO-2", Stage::Done, None);
        old.stage = None;
        old.status = "Closed".into();
        assert_eq!(done_count(&view(vec![old])), (1, 1));
    }
}
