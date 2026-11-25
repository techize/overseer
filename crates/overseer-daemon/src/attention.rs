//! The attention queue: everything waiting on you, derived from daemon
//! state. Pure, so the daemon can rerun it on every change and on a tick.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use overseer_proto::git::repo_branch;
use overseer_proto::{
    ActivityKind, AttentionItem, AttentionKind, AttentionView, BuildState, DayView, Event,
    LockView, PullRequest, RelayState, SlotId, SlotInfo, SlotState, WorkView, JIRA_OFF,
};
use tokio::sync::broadcast;

use crate::ledger::{Ledger, PrSeen};
use crate::locks::LockService;
use crate::manager::SlotManager;
use crate::relay::courier::{NO_BOX, WORKING};
use crate::session_logs::{hhmm, local_minutes};
use crate::timeline_service::TimelineService;
use crate::work::store::{unix_now, NO_JIRA_EMAIL};
use crate::work::WorkStore;

/// Seconds a finished slot with a ticket waits before it is an item.
pub const WAIT_AFTER: i64 = 120;
/// Seconds a held relay may wait before it counts as stuck.
pub const STUCK_AFTER: i64 = 60;

/// What `derive` reads. `branches` maps each slot to the repo and branch its
/// folder is on; `author_set` is true when `pr_author` is configured, so the
/// PR list is already the user's own.
pub struct Inputs<'a> {
    pub slots: &'a [SlotInfo],
    pub locks: &'a LockView,
    pub work: &'a WorkView,
    pub seen: &'a [PrSeen],
    pub branches: &'a HashMap<SlotId, (String, String)>,
    pub author_set: bool,
    /// Today's timeline, for the unlogged gaps.
    pub day: &'a DayView,
    pub now: i64,
}

/// Everything waiting on you, oldest first.
pub fn derive(i: &Inputs) -> Vec<AttentionItem> {
    let mut items = Vec::new();
    for s in i.slots {
        slot_items(i, s, &mut items);
    }
    notice_items(i, &mut items);
    gap_items(i, &mut items);
    for pr in &i.work.prs {
        pr_items(i, pr, &mut items);
    }
    items.sort_by(|a, b| a.since.cmp(&b.since).then_with(|| a.key.cmp(&b.key)));
    items
}

/// Marks the rows the user dismissed. A dismissal holds only while the
/// row's `since` and `text` are what they were when it was dismissed, so a
/// new occurrence or a changed text shows the row again.
pub fn apply_dismissals(items: &mut [AttentionItem], dismissed: &[(String, i64, String)]) {
    for item in items {
        item.dismissed = dismissed.iter().any(|(key, since, text)| {
            *key == item.key && *since == item.since && *text == item.text
        });
    }
}

/// Which of the background sources behind the list have been read since
/// the daemon started. Until one has, a row it raises may be missing only
/// because it was not read yet, so its dismissal must stay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Readiness {
    /// The PR poll succeeded for every repo it covers.
    pub prs: bool,
    /// Beads, and Jira unless it is off, were read: the owned items a
    /// waiting row needs.
    pub items: bool,
    /// The day's timeline was built.
    pub day: bool,
}

/// What has been read since `started` (unix seconds). Slots, locks and
/// notices need no flag: they are read from the daemon's own state before
/// the first list is derived.
pub fn readiness(work: &WorkView, day: &DayView, started: i64) -> Readiness {
    let source = |name: &str| work.sources.iter().find(|s| s.name == name);
    let read =
        |name: &str| source(name).is_some_and(|s| s.ok && s.at.is_some_and(|at| at >= started));
    let jira_off = source("jira")
        .is_some_and(|s| matches!(s.message.as_deref(), Some(JIRA_OFF) | Some(NO_JIRA_EMAIL)));
    Readiness {
        prs: read("prs"),
        items: read("beads") && (jira_off || read("jira")),
        day: !day.day.is_empty(),
    }
}

/// The kind a key was made for, from the part before its first `:`.
fn kind_of_key(key: &str) -> AttentionKind {
    let name = key.split(':').next().unwrap_or_default();
    serde_json::from_value(serde_json::Value::String(name.into())).unwrap_or(AttentionKind::Other)
}

/// True when a row of `key` missing from the list means it is gone: the
/// source that raises it has been read.
fn settled(ready: Readiness, key: &str) -> bool {
    match kind_of_key(key) {
        AttentionKind::PrBuildFailed
        | AttentionKind::PrChangesRequested
        | AttentionKind::PrComments => ready.prs,
        AttentionKind::Waiting => ready.items,
        AttentionKind::LogGap => ready.day,
        // Read before the first list, or a kind nothing raises any more.
        AttentionKind::NeedsYou
        | AttentionKind::RelayStuck
        | AttentionKind::Notice
        | AttentionKind::Other => true,
    }
}

/// The dismissed keys that are no longer in the list and whose source has
/// been read, to delete.
pub fn stale_dismissals<'a>(
    items: &[AttentionItem],
    dismissed: &'a [(String, i64, String)],
    ready: Readiness,
) -> Vec<&'a str> {
    dismissed
        .iter()
        .map(|(key, _, _)| key.as_str())
        .filter(|key| !items.iter().any(|i| i.key == *key) && settled(ready, key))
        .collect()
}

/// How often the list is rederived with no event, so a wait that only
/// ripens with time still shows up.
const TICK: Duration = Duration::from_secs(30);

/// Holds the current attention list and announces it when it changes.
pub struct AttentionService {
    view: Mutex<AttentionView>,
    ledger: Arc<Ledger>,
    events: broadcast::Sender<Event>,
    /// Held from reading the dismissals to announcing the list, by the task
    /// and by `dismiss`, so neither can store or announce a list the other
    /// has already replaced. Never held across an await.
    turn: Mutex<()>,
    /// Run once by the next `publish` between reading the dismissals and
    /// storing the list, so a test can put a `dismiss` exactly there.
    #[cfg(test)]
    after_read: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl AttentionService {
    fn new(ledger: Arc<Ledger>, events: broadcast::Sender<Event>) -> AttentionService {
        AttentionService {
            view: Mutex::new(AttentionView::default()),
            ledger,
            events,
            turn: Mutex::new(()),
            #[cfg(test)]
            after_read: Mutex::new(None),
        }
    }

    pub fn current(&self) -> AttentionView {
        self.view.lock().unwrap().clone()
    }

    /// Hides the row `key` while its `since` and `text` stay as given. The
    /// current list is marked and announced at once.
    pub fn dismiss(&self, key: &str, since: i64, text: &str) -> anyhow::Result<()> {
        let _turn = self.turn.lock().unwrap_or_else(|e| e.into_inner());
        self.ledger.dismiss_attention(key, since, text)?;
        let dismissed = self.ledger.attention_dismissed()?;
        let mut items = self.current().items;
        apply_dismissals(&mut items, &dismissed);
        self.store(items);
        Ok(())
    }

    /// Marks a freshly derived list with the stored dismissals, deletes the
    /// dismissals of rows that are gone, then stores and announces it.
    fn publish(&self, mut items: Vec<AttentionItem>, ready: Readiness) {
        let _turn = self.turn.lock().unwrap_or_else(|e| e.into_inner());
        let read = self.ledger.attention_dismissed();
        #[cfg(test)]
        if let Some(hook) = self.after_read.lock().unwrap().take() {
            hook();
        }
        match read {
            Ok(dismissed) => {
                apply_dismissals(&mut items, &dismissed);
                let stale = stale_dismissals(&items, &dismissed, ready);
                if !stale.is_empty() {
                    if let Err(e) = self.ledger.delete_attention_dismissed(&stale) {
                        tracing::warn!("attention: pruning dismissals failed: {e:#}");
                    }
                }
            }
            Err(e) => {
                // Keep the marks the last list had rather than show every
                // dismissed row again.
                tracing::warn!("attention: reading dismissals failed: {e:#}");
                let view = self.view.lock().unwrap();
                for item in &mut items {
                    item.dismissed = view.items.iter().any(|old| {
                        old.dismissed
                            && old.key == item.key
                            && old.since == item.since
                            && old.text == item.text
                    });
                }
            }
        }
        self.store(items);
    }

    /// Stores the list and announces it when it changed.
    fn store(&self, items: Vec<AttentionItem>) {
        if self.update(items) {
            // Nobody listening is fine.
            let _ = self.events.send(Event::AttentionChanged(self.current()));
        }
    }

    /// Stores the list; true only when it differs from the last one.
    fn update(&self, items: Vec<AttentionItem>) -> bool {
        let mut view = self.view.lock().unwrap();
        if view.items == items {
            return false;
        }
        view.items = items;
        true
    }

    /// Starts the task that keeps the list current. `author_set` is read
    /// once: settings only take effect when the daemon restarts. The day
    /// comes from `timeline`'s announcements, and from `timeline` itself
    /// when one was missed; it is never asked to rebuild.
    pub fn spawn(
        ledger: Arc<Ledger>,
        manager: Arc<Mutex<SlotManager>>,
        locks: Arc<LockService>,
        store: Arc<WorkStore>,
        timeline: Arc<TimelineService>,
        events: broadcast::Sender<Event>,
        author_set: bool,
    ) -> Arc<AttentionService> {
        let svc = Arc::new(AttentionService::new(ledger.clone(), events.clone()));
        let started = unix_now();
        // Subscribe before seeding so no change falls between the two.
        let mut rx = events.subscribe();
        let reread = move || (manager.lock().unwrap().list(), locks.view(), store.view());
        let (mut slots, mut lock_view, mut work) = reread();
        let mut day = timeline.current();
        let task = svc.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(TICK);
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    ev = rx.recv() => match ev {
                        Ok(Event::SlotChanged(info)) => {
                            match slots.iter_mut().find(|s| s.id == info.id) {
                                Some(s) => *s = info,
                                None => slots.push(info),
                            }
                        }
                        Ok(Event::LocksChanged(v)) => lock_view = v,
                        Ok(Event::WorkChanged(v)) => work = v,
                        Ok(Event::TimelineChanged(v)) => day = v,
                        Ok(_) => continue,
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            (slots, lock_view, work) = reread();
                            day = timeline.current();
                        }
                        Err(broadcast::error::RecvError::Closed) => return,
                    },
                }
                let seen = ledger.pr_seen().unwrap_or_else(|e| {
                    tracing::warn!("attention: reading pr_seen failed: {e:#}");
                    Vec::new()
                });
                let branches = slots
                    .iter()
                    .filter_map(|s| Some((s.id, repo_branch(s.cwd.as_deref()?)?)))
                    .collect();
                let items = derive(&Inputs {
                    slots: &slots,
                    locks: &lock_view,
                    work: &work,
                    seen: &seen,
                    branches: &branches,
                    author_set,
                    day: &day,
                    now: unix_now(),
                });
                task.publish(items, readiness(&work, &day, started));
            }
        });
        svc
    }
}

fn item(kind: AttentionKind, slot: SlotId, since: i64, text: &str) -> AttentionItem {
    AttentionItem {
        key: format!("{}:{slot}", kind_name(kind)),
        kind,
        slot: Some(slot),
        pr: None,
        url: None,
        since,
        text: one_line(text),
        dismissed: false,
    }
}

/// The kind's serde name, so keys match what the app sees.
fn kind_name(kind: AttentionKind) -> &'static str {
    match kind {
        AttentionKind::NeedsYou => "needs_you",
        AttentionKind::Waiting => "waiting",
        AttentionKind::RelayStuck => "relay_stuck",
        AttentionKind::Notice => "notice",
        AttentionKind::PrBuildFailed => "pr_build_failed",
        AttentionKind::PrChangesRequested => "pr_changes_requested",
        AttentionKind::PrComments => "pr_comments",
        AttentionKind::LogGap => "log_gap",
        AttentionKind::Other => "other",
    }
}

fn one_line(text: &str) -> String {
    text.lines().collect::<Vec<_>>().join(" ")
}

/// Activity and relay items. A slot that is not running has no session to
/// prompt you or take a relay, so it raises none.
fn slot_items(i: &Inputs, s: &SlotInfo, out: &mut Vec<AttentionItem>) {
    if !matches!(s.state, SlotState::Running { .. }) {
        return;
    }
    let act = s.activity.as_ref();
    if let Some(a) = act {
        let finished = a.kind == ActivityKind::Idle || a.idle_wait;
        if a.kind == ActivityKind::NeedsYou && !a.idle_wait {
            let text = a.reason.as_deref().filter(|r| !r.trim().is_empty());
            out.push(item(
                AttentionKind::NeedsYou,
                s.id,
                a.since,
                text.unwrap_or("needs you"),
            ));
        } else if finished
            && i.now.saturating_sub(a.since) >= WAIT_AFTER
            && i.work.items.iter().any(|w| w.owner == Some(s.id))
        {
            out.push(item(
                AttentionKind::Waiting,
                s.id,
                a.since,
                "finished, waiting for you",
            ));
        }
    }
    let idle_long = act.is_some_and(|a| {
        a.kind == ActivityKind::Idle && i.now.saturating_sub(a.since) > STUCK_AFTER
    });
    let stuck = i
        .locks
        .relays
        .iter()
        .filter(|r| r.to == s.id && r.state == RelayState::Pending)
        .filter_map(|r| Some((r.held.as_deref()?, r.held_since?)))
        .filter(|&(held, since)| match held {
            NO_BOX => i.now.saturating_sub(since) > STUCK_AFTER,
            WORKING => idle_long,
            _ => false,
        })
        .min_by_key(|&(_, since)| since);
    if let Some((held, since)) = stuck {
        let text = format!("message not delivered: {held}");
        out.push(item(AttentionKind::RelayStuck, s.id, since, &text));
    }
}

/// One item per slot with notices, from the oldest notice, showing the
/// newest one's text.
fn notice_items(i: &Inputs, out: &mut Vec<AttentionItem>) {
    let mut slots: Vec<SlotId> = i.work.notices.iter().map(|n| n.slot).collect();
    slots.sort_unstable();
    slots.dedup();
    for slot in slots {
        let mine: Vec<_> = i.work.notices.iter().filter(|n| n.slot == slot).collect();
        let oldest = mine.iter().map(|n| n.at).min().unwrap_or_default();
        let newest = mine
            .iter()
            .rev()
            .max_by_key(|n| n.at)
            .map(|n| n.text.as_str());
        let count = match mine.len() {
            1 => "1 notice".to_string(),
            n => format!("{n} notices"),
        };
        let text = format!("{count}: {}", newest.unwrap_or_default());
        out.push(item(AttentionKind::Notice, slot, oldest, &text));
    }
}

/// One item per slot with unlogged gaps today, timed from the end of the
/// first gap and listing the first two.
fn gap_items(i: &Inputs, out: &mut Vec<AttentionItem>) {
    let mut slots: Vec<SlotId> = i.day.rows.iter().map(|r| r.slot).collect();
    slots.sort_unstable();
    slots.dedup();
    for slot in slots {
        let gaps = i.day.gaps(slot);
        let Some(first) = gaps.first() else {
            continue;
        };
        let range = |from: i64, to: i64| {
            format!(
                "{}-{}",
                hhmm(local_minutes(from).1),
                hhmm(local_minutes(to).1)
            )
        };
        let times = match gaps.as_slice() {
            [a] => range(a.from, a.to),
            [a, b] => format!("{} and {}", range(a.from, a.to), range(b.from, b.to)),
            [a, b, rest @ ..] => format!(
                "{} and {} and {} more",
                range(a.from, a.to),
                range(b.from, b.to),
                rest.len()
            ),
            [] => continue,
        };
        let count = match gaps.len() {
            1 => "1 unlogged gap".to_string(),
            n => format!("{n} unlogged gaps"),
        };
        let text = format!("{count} today, {times}");
        out.push(item(AttentionKind::LogGap, slot, first.to, &text));
    }
}

/// Items for one of your PRs, each timed by when Overseer first saw its
/// condition. A PR with no seen row, or a condition with no time, raises
/// nothing. A build or approvals list this poll could not read counts as
/// unchanged.
fn pr_items(i: &Inputs, pr: &PullRequest, out: &mut Vec<AttentionItem>) {
    let slot = slot_of_pr(pr, i.branches);
    if !i.author_set && slot.is_none() {
        return;
    }
    let Some(seen) = i.seen.iter().find(|s| s.repo == pr.repo && s.id == pr.id) else {
        return;
    };
    let mut push = |kind: AttentionKind, since: Option<i64>, what: String| {
        if let Some(since) = since {
            out.push(AttentionItem {
                key: format!("{}:{}#{}", kind_name(kind), pr.repo, pr.id),
                kind,
                slot,
                pr: Some((pr.repo.clone(), pr.id)),
                url: pr.url.clone(),
                since,
                text: one_line(&format!("{}: {what}", pr.title)),
                dismissed: false,
            });
        }
    };
    // An unread build or approvals list trusts the row, whose time the
    // ledger keeps until a poll reads them again.
    if pr.build.is_none_or(|b| b == BuildState::Failed) {
        push(
            AttentionKind::PrBuildFailed,
            seen.build_failed_since,
            "build failed".into(),
        );
    }
    if pr.approvals.is_none_or(|a| a.changes_requested) {
        push(
            AttentionKind::PrChangesRequested,
            seen.changes_since,
            "changes requested".into(),
        );
    }
    // No stored count yet means nothing has been seen to count from.
    let counts = pr.comments.zip(seen.comments).filter(|(n, base)| n > base);
    if let Some((n, base)) = counts {
        let what = match n - base {
            1 => "1 new comment".to_string(),
            n => format!("{n} new comments"),
        };
        push(AttentionKind::PrComments, seen.comments_since, what);
    }
}

/// The slot working on `pr`: its repo and branch exactly equal the PR's
/// repo and head branch. The lowest slot id wins when several do.
pub fn slot_of_pr(
    pr: &PullRequest,
    branches: &HashMap<SlotId, (String, String)>,
) -> Option<SlotId> {
    branches
        .iter()
        .filter(|(_, (repo, branch))| *repo == pr.repo && *branch == pr.branch)
        .map(|(&slot, _)| slot)
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;
    use overseer_proto::{
        Activity, ActivityKind, Approvals, BuildState, DayRow, DayStretch, ItemKind, Notice,
        PullRequest, Relay, RelayKind, RelayState, Sender, SlotState, SourceStatus, Span, SpanKind,
        WorkItem,
    };

    const NOW: i64 = 10_000;

    fn slot(id: SlotId, act: Option<Activity>) -> SlotInfo {
        SlotInfo {
            id,
            label: String::new(),
            cwd: None,
            command: None,
            state: SlotState::Running { pid: 1, since: 1 },
            cols: 80,
            rows: 24,
            activity: act,
            session_note: None,
            held: false,
        }
    }

    fn act(kind: ActivityKind, since: i64) -> Option<Activity> {
        Some(Activity {
            kind,
            since,
            now_doing: None,
            reason: None,
            idle_wait: false,
        })
    }

    fn idle_wait(since: i64) -> Option<Activity> {
        let mut a = act(ActivityKind::Idle, since);
        a.as_mut().unwrap().idle_wait = true;
        a
    }

    fn locks(relays: Vec<Relay>) -> LockView {
        LockView {
            claims: vec![],
            waits: vec![],
            relays,
            flag_after_secs: 600,
        }
    }

    fn relay(id: i64, to: SlotId, held: &str, held_since: i64) -> Relay {
        Relay {
            id,
            at: held_since,
            from: Sender::You,
            to,
            text: "hello".into(),
            state: RelayState::Pending,
            delivered_at: None,
            dropped_at: None,
            kind: RelayKind::Note,
            drop_reason: None,
            held: Some(held.into()),
            held_since: Some(held_since),
        }
    }

    fn work(items: Vec<WorkItem>, notices: Vec<Notice>, prs: Vec<PullRequest>) -> WorkView {
        WorkView {
            items,
            sources: vec![],
            notices,
            sprint: None,
            prs,
            sprint_day: None,
            notes_off: None,
        }
    }

    fn owned(key: &str, owner: SlotId) -> WorkItem {
        WorkItem {
            key: key.into(),
            kind: ItemKind::Ticket,
            title: "Demo ticket".into(),
            status: "In Progress".into(),
            priority: None,
            parent: None,
            owner: Some(owner),
            in_list: true,
            folder: None,
            stage: None,
            stage_since: None,
        }
    }

    fn pr(repo: &str, id: u64, branch: &str) -> PullRequest {
        PullRequest {
            repo: repo.into(),
            id,
            title: format!("DEMO-{id} change"),
            branch: branch.into(),
            url: Some(format!("https://example.test/{repo}/{id}")),
            author: Some("demo".into()),
            created: Some(1),
            ticket: None,
            approvals: None,
            comments: None,
            build: None,
            head: None,
        }
    }

    fn seen(repo: &str, id: u64) -> PrSeen {
        PrSeen {
            repo: repo.into(),
            id,
            comments: Some(0),
            build_failed_since: None,
            changes_since: None,
            comments_since: None,
        }
    }

    struct World {
        slots: Vec<SlotInfo>,
        locks: LockView,
        work: WorkView,
        seen: Vec<PrSeen>,
        branches: HashMap<SlotId, (String, String)>,
        author_set: bool,
        day: DayView,
        now: i64,
    }

    impl World {
        fn new() -> Self {
            World {
                slots: vec![],
                locks: locks(vec![]),
                work: work(vec![], vec![], vec![]),
                seen: vec![],
                branches: HashMap::new(),
                author_set: true,
                day: DayView::default(),
                now: NOW,
            }
        }

        fn derive(&self) -> Vec<AttentionItem> {
            derive(&Inputs {
                slots: &self.slots,
                locks: &self.locks,
                work: &self.work,
                seen: &self.seen,
                branches: &self.branches,
                author_set: self.author_set,
                day: &self.day,
                now: self.now,
            })
        }

        fn keys(&self) -> Vec<String> {
            self.derive().into_iter().map(|i| i.key).collect()
        }
    }

    fn span(from: i64, to: i64, kind: SpanKind) -> Span {
        Span { from, to, kind }
    }

    /// A day with one stretch for `slot` holding `spans`.
    fn day_of(slot: SlotId, spans: Vec<Span>) -> DayView {
        DayView {
            day: "2026-11-24".into(),
            start: 0,
            now: NOW,
            rows: vec![DayRow {
                slot,
                stretches: vec![DayStretch {
                    session: "demo".into(),
                    first: 0,
                    last: NOW,
                    running: true,
                    spans,
                    logs: vec![],
                }],
            }],
        }
    }

    fn gap(from: i64, to: i64) -> Span {
        span(from, to, SpanKind::Gap)
    }

    #[test]
    fn log_gap_raised_per_slot_with_gaps() {
        let mut w = World::new();
        w.day = day_of(2, vec![gap(100, 800)]);
        w.day.rows.push(DayRow {
            slot: 5,
            stretches: vec![DayStretch {
                session: "demo".into(),
                first: 0,
                last: NOW,
                running: false,
                spans: vec![gap(1000, 1700)],
                logs: vec![],
            }],
        });
        assert_eq!(w.keys(), ["log_gap:2", "log_gap:5"]);
        let items = w.derive();
        assert!(items.iter().all(|i| i.kind == AttentionKind::LogGap));
        assert_eq!(items[0].slot, Some(2));
    }

    #[test]
    fn log_gap_text_lists_two_then_more() {
        // Day start at 0 makes local times zone-dependent, so build the
        // expected text from the same formatter.
        let at = |t: i64| {
            let (_, m) = crate::session_logs::local_minutes(t);
            crate::session_logs::hhmm(m)
        };
        let g1 = gap(1_000_000, 1_000_700);
        let g2 = gap(1_010_000, 1_010_700);
        let g3 = gap(1_020_000, 1_020_700);
        let range = |g: &Span| format!("{}-{}", at(g.from), at(g.to));
        let mut w = World::new();

        w.day = day_of(1, vec![g1.clone()]);
        assert_eq!(
            w.derive()[0].text,
            format!("1 unlogged gap today, {}", range(&g1))
        );

        w.day = day_of(1, vec![g1.clone(), g2.clone()]);
        assert_eq!(
            w.derive()[0].text,
            format!("2 unlogged gaps today, {} and {}", range(&g1), range(&g2))
        );

        w.day = day_of(1, vec![g1.clone(), g2.clone(), g3.clone()]);
        assert_eq!(
            w.derive()[0].text,
            format!(
                "3 unlogged gaps today, {} and {} and 1 more",
                range(&g1),
                range(&g2)
            )
        );
    }

    #[test]
    fn log_gap_since_is_end_of_first_gap() {
        let mut w = World::new();
        // Out of order on purpose: gaps() sorts by start.
        w.day = day_of(1, vec![gap(5000, 5700), gap(100, 900)]);
        assert_eq!(w.derive()[0].since, 900);
    }

    #[test]
    fn no_gaps_no_item() {
        let mut w = World::new();
        assert!(w.derive().is_empty());
        w.day = day_of(
            1,
            vec![
                span(0, 500, SpanKind::Logged),
                span(500, 900, SpanKind::Unlogged),
                span(900, 1200, SpanKind::Wait),
            ],
        );
        assert!(w.derive().is_empty());
    }

    #[test]
    fn log_gap_clears_when_covered() {
        let mut w = World::new();
        w.day = day_of(1, vec![gap(100, 800)]);
        assert_eq!(w.keys(), ["log_gap:1"]);
        w.day = day_of(1, vec![span(100, 800, SpanKind::Logged)]);
        assert!(w.derive().is_empty());
    }

    #[test]
    fn update_reports_only_changes() {
        let svc = AttentionService::new(Arc::new(Ledger::in_memory()), broadcast::channel(4).0);
        let one = |since| vec![item(AttentionKind::NeedsYou, 1, since, "needs you")];
        assert!(svc.update(one(10)));
        assert!(!svc.update(one(10)));
        assert!(svc.update(one(20)));
        assert_eq!(svc.current().items, one(20));
        assert!(svc.update(Vec::new()));
        assert!(!svc.update(Vec::new()));
    }

    fn dismissal(key: &str, since: i64, text: &str) -> (String, i64, String) {
        (key.into(), since, text.into())
    }

    #[test]
    fn a_dismissal_hides_the_row_only_while_since_and_text_match() {
        let mut items = vec![
            item(AttentionKind::NeedsYou, 1, 100, "permission prompt"),
            item(AttentionKind::NeedsYou, 2, 100, "permission prompt"),
        ];
        let d = vec![dismissal("needs_you:1", 100, "permission prompt")];
        apply_dismissals(&mut items, &d);
        assert!(items[0].dismissed);
        assert!(!items[1].dismissed);

        // A new occurrence of the same key has a new since: it comes back.
        let mut again = vec![item(AttentionKind::NeedsYou, 1, 200, "permission prompt")];
        apply_dismissals(&mut again, &d);
        assert!(!again[0].dismissed);

        // Same since, changed text (more notices, a new gap): it comes back.
        let mut more = vec![item(AttentionKind::NeedsYou, 1, 100, "another prompt")];
        apply_dismissals(&mut more, &d);
        assert!(!more[0].dismissed);

        // A row marked before is unmarked once nothing matches it.
        apply_dismissals(&mut items, &[]);
        assert!(items.iter().all(|i| !i.dismissed));
    }

    fn source(name: &str, ok: bool, at: Option<i64>, message: Option<&str>) -> SourceStatus {
        SourceStatus {
            name: name.into(),
            ok,
            count: 0,
            at,
            message: message.map(String::from),
        }
    }

    /// A work view with only these source statuses.
    fn sources(list: Vec<SourceStatus>) -> WorkView {
        let mut v = work(vec![], vec![], vec![]);
        v.sources = list;
        v
    }

    const ALL_READY: Readiness = Readiness {
        prs: true,
        items: true,
        day: true,
    };

    #[test]
    fn keys_name_their_kind() {
        assert_eq!(
            kind_of_key("pr_comments:demo-api#7"),
            AttentionKind::PrComments
        );
        assert_eq!(kind_of_key("log_gap:2"), AttentionKind::LogGap);
        assert_eq!(kind_of_key("from_the_future:1"), AttentionKind::Other);
        assert_eq!(kind_of_key(""), AttentionKind::Other);
    }

    #[test]
    fn stale_dismissals_wait_for_the_source_behind_each_kind() {
        let items = vec![item(AttentionKind::Notice, 3, 100, "one notice")];
        let d = vec![
            dismissal("notice:3", 50, "older text"),
            dismissal("needs_you:1", 100, "permission prompt"),
            dismissal("pr_comments:demo-api#7", 100, "DEMO-1: 2 new comments"),
            dismissal("waiting:2", 100, "finished, waiting for you"),
            dismissal("log_gap:4", 100, "1 unlogged gap today"),
        ];
        // Nothing read yet: only the kinds read before the first list go.
        assert_eq!(
            stale_dismissals(&items, &d, Readiness::default()),
            ["needs_you:1"]
        );
        let prs_only = Readiness {
            prs: true,
            ..Readiness::default()
        };
        assert_eq!(
            stale_dismissals(&items, &d, prs_only),
            ["needs_you:1", "pr_comments:demo-api#7"]
        );
        assert_eq!(
            stale_dismissals(&items, &d, ALL_READY),
            [
                "needs_you:1",
                "pr_comments:demo-api#7",
                "waiting:2",
                "log_gap:4"
            ]
        );
        // A listed key is never stale.
        assert!(stale_dismissals(&items, &d[..1], ALL_READY).is_empty());
    }

    #[test]
    fn readiness_needs_a_good_read_since_start() {
        let start = 1000;
        let day = DayView::default();
        let at = |n: &str, ok, t| source(n, ok, Some(t), None);

        // Not read yet, as the store starts.
        let r = readiness(
            &sources(vec![
                source("beads", false, None, Some("not read yet")),
                source("jira", false, None, Some("not read yet")),
                source("prs", false, None, Some("not read yet")),
            ]),
            &day,
            start,
        );
        assert_eq!(r, Readiness::default());

        // A failed poll keeps the time of the last good read, from before.
        let r = readiness(&sources(vec![at("prs", false, start - 10)]), &day, start);
        assert!(!r.prs);
        // A good read from before start does not count.
        let r = readiness(&sources(vec![at("prs", true, start - 10)]), &day, start);
        assert!(!r.prs);
        let r = readiness(&sources(vec![at("prs", true, start + 5)]), &day, start);
        assert!(r.prs);

        // Items need beads, and Jira unless it is off.
        let r = readiness(
            &sources(vec![
                at("beads", true, start + 5),
                source("jira", false, None, Some("not read yet")),
            ]),
            &day,
            start,
        );
        assert!(!r.items);
        for off in [JIRA_OFF, NO_JIRA_EMAIL] {
            let r = readiness(
                &sources(vec![
                    at("beads", true, start + 5),
                    source("jira", false, None, Some(off)),
                ]),
                &day,
                start,
            );
            assert!(r.items, "{off}");
        }
        let r = readiness(
            &sources(vec![
                at("beads", true, start + 5),
                at("jira", true, start + 6),
            ]),
            &day,
            start,
        );
        assert!(r.items);

        // The day is loaded once the timeline has built one.
        let r = readiness(&sources(vec![]), &day_of(1, vec![]), start);
        assert!(r.day);
    }

    #[test]
    fn a_failed_or_empty_pr_poll_keeps_pr_dismissals_and_a_good_one_prunes() {
        let start = 1000;
        let ledger = Arc::new(Ledger::in_memory());
        let svc = AttentionService::new(ledger.clone(), broadcast::channel(16).0);
        let key = "pr_comments:demo-api#7";
        ledger
            .dismiss_attention(key, 100, "DEMO-1: 2 new comments")
            .unwrap();
        let day = DayView::default();

        // The poll has not run, or it failed: the PR list is empty, but
        // that says nothing about the PR.
        for prs in [
            source("prs", false, None, Some("not read yet")),
            source("prs", false, Some(start - 10), Some("demo-api: 401")),
            source("prs", false, Some(start + 5), Some("repos: timed out")),
        ] {
            let view = sources(vec![prs]);
            svc.publish(vec![], readiness(&view, &day, start));
            assert_eq!(ledger.attention_dismissed().unwrap().len(), 1);
        }

        // A good poll without the PR: it is gone, so the dismissal goes.
        let view = sources(vec![source("prs", true, Some(start + 30), None)]);
        svc.publish(vec![], readiness(&view, &day, start));
        assert!(ledger.attention_dismissed().unwrap().is_empty());
    }

    #[test]
    fn publish_keeps_a_dismissal_whose_row_is_listed() {
        let ledger = Arc::new(Ledger::in_memory());
        let svc = AttentionService::new(ledger.clone(), broadcast::channel(16).0);
        ledger
            .dismiss_attention("notice:3", 100, "one notice")
            .unwrap();
        ledger
            .dismiss_attention("needs_you:1", 100, "gone")
            .unwrap();
        svc.publish(
            vec![item(AttentionKind::Notice, 3, 100, "one notice")],
            Readiness::default(),
        );
        assert!(svc.current().items[0].dismissed);
        assert_eq!(
            ledger.attention_dismissed().unwrap(),
            [dismissal("notice:3", 100, "one notice")]
        );
    }

    #[test]
    fn a_derive_racing_dismiss_cannot_leave_the_row_shown() {
        // The task has read the dismissals (none yet) for a fresh list and
        // not stored it when `dismiss` comes in. Without the turn lock the
        // dismissal would be announced and then overwritten by that list.
        let ledger = Arc::new(Ledger::in_memory());
        let (tx, mut rx) = broadcast::channel(64);
        let svc = Arc::new(AttentionService::new(ledger, tx));
        let row = item(AttentionKind::NeedsYou, 1, 100, "permission prompt");
        svc.publish(vec![row.clone()], ALL_READY);

        let (read_tx, read_rx) = std::sync::mpsc::channel::<()>();
        let read_tx = Mutex::new(read_tx);
        *svc.after_read.lock().unwrap() = Some(Box::new(move || {
            let _ = read_tx.lock().unwrap().send(());
            // Long enough for an unserialised dismiss to finish first.
            std::thread::sleep(Duration::from_millis(200));
        }));
        let deriver = {
            let svc = svc.clone();
            let row = row.clone();
            std::thread::spawn(move || svc.publish(vec![row], ALL_READY))
        };
        read_rx.recv().unwrap();
        svc.dismiss(&row.key, row.since, &row.text).unwrap();
        deriver.join().unwrap();

        assert!(svc.current().items[0].dismissed);
        let mut last = None;
        while let Ok(ev) = rx.try_recv() {
            if let Event::AttentionChanged(v) = ev {
                last = Some(v);
            }
        }
        let last = last.expect("announced");
        assert!(
            last.items[0].dismissed,
            "the last announcement shows the row"
        );
    }

    #[test]
    fn dismiss_marks_the_current_list_and_announces_it() {
        let ledger = Arc::new(Ledger::in_memory());
        let (tx, mut rx) = broadcast::channel(4);
        let svc = AttentionService::new(ledger.clone(), tx);
        let row = item(AttentionKind::NeedsYou, 1, 100, "permission prompt");
        svc.update(vec![row.clone()]);
        svc.dismiss(&row.key, row.since, &row.text).unwrap();
        assert!(svc.current().items[0].dismissed);
        assert_eq!(svc.current().count(), 0);
        match rx.try_recv() {
            Ok(Event::AttentionChanged(v)) => assert!(v.items[0].dismissed),
            other => panic!("unexpected {other:?}"),
        }
        // Stored, so the next derivation keeps it hidden.
        assert_eq!(
            ledger.attention_dismissed().unwrap(),
            [dismissal("needs_you:1", 100, "permission prompt")]
        );
    }

    #[test]
    fn plain_idle_and_idle_wait_needs_you_both_wait_after_two_minutes() {
        let mut w = World::new();
        w.work = work(vec![owned("DEMO-1", 2)], vec![], vec![]);
        // Finished after a Stop: plain Idle, no idle-input wait.
        w.slots = vec![slot(2, act(ActivityKind::Idle, NOW - WAIT_AFTER + 1))];
        assert!(w.derive().is_empty());
        w.slots = vec![slot(2, act(ActivityKind::Idle, NOW - WAIT_AFTER))];
        assert_eq!(w.keys(), ["waiting:2"]);

        // An idle-input wait reported as NeedsYou.
        let mut a = act(ActivityKind::NeedsYou, NOW - WAIT_AFTER + 1);
        a.as_mut().unwrap().idle_wait = true;
        w.slots = vec![slot(2, a)];
        assert!(w.derive().is_empty());
        let mut a = act(ActivityKind::NeedsYou, NOW - WAIT_AFTER);
        a.as_mut().unwrap().idle_wait = true;
        w.slots = vec![slot(2, a)];
        let items = w.derive();
        assert_eq!(items.len(), 1);
        assert_eq!(
            (items[0].key.as_str(), items[0].since),
            ("waiting:2", NOW - WAIT_AFTER)
        );
    }

    #[test]
    fn needs_you_raises_and_idle_wait_does_not() {
        let mut w = World::new();
        let mut a = act(ActivityKind::NeedsYou, 50);
        a.as_mut().unwrap().reason = Some("Claude needs your permission\nto use Bash".into());
        w.slots = vec![slot(3, a)];
        let items = w.derive();
        assert_eq!(items.len(), 1);
        let i = &items[0];
        assert_eq!(i.key, "needs_you:3");
        assert_eq!(
            (i.kind, i.slot, i.since),
            (AttentionKind::NeedsYou, Some(3), 50)
        );
        assert_eq!(i.text, "Claude needs your permission to use Bash");

        // No reason falls back to a plain text.
        w.slots = vec![slot(3, act(ActivityKind::NeedsYou, 50))];
        assert_eq!(w.derive()[0].text, "needs you");

        // An idle-input wait is not a prompt for you.
        let mut a = act(ActivityKind::NeedsYou, 50);
        a.as_mut().unwrap().idle_wait = true;
        w.slots = vec![slot(3, a)];
        assert!(!w.keys().contains(&"needs_you:3".to_string()));

        // Clears when the activity moves on.
        w.slots = vec![slot(3, act(ActivityKind::Working, 60))];
        assert!(w.derive().is_empty());
    }

    #[test]
    fn waiting_needs_two_minutes_and_an_owned_item() {
        let mut w = World::new();
        w.work = work(vec![owned("DEMO-1", 2)], vec![], vec![]);
        w.slots = vec![slot(2, idle_wait(NOW - 119))];
        assert!(w.derive().is_empty(), "119 s is too soon");

        w.slots = vec![slot(2, idle_wait(NOW - 121))];
        w.work = work(vec![owned("DEMO-1", 5)], vec![], vec![]);
        assert!(w.derive().is_empty(), "no item owned by the slot");

        w.work = work(vec![owned("DEMO-1", 2)], vec![], vec![]);
        let items = w.derive();
        assert_eq!(items.len(), 1);
        let i = &items[0];
        assert_eq!(i.key, "waiting:2");
        assert_eq!(
            (i.kind, i.slot, i.since),
            (AttentionKind::Waiting, Some(2), NOW - 121)
        );
        assert_eq!(i.text, "finished, waiting for you");

        // Clears when the slot gets input.
        w.slots = vec![slot(2, act(ActivityKind::Working, NOW))];
        assert!(w.derive().is_empty());
    }

    #[test]
    fn slots_that_are_not_running_raise_no_activity_items() {
        let mut w = World::new();
        w.work = work(vec![owned("DEMO-1", 1)], vec![], vec![]);
        let mut s = slot(1, act(ActivityKind::NeedsYou, 5));
        s.state = SlotState::Exited {
            code: Some(0),
            at: 9,
        };
        let mut t = slot(2, idle_wait(5));
        t.state = SlotState::Empty;
        w.slots = vec![s, t];
        assert!(w.derive().is_empty());
    }

    #[test]
    fn relay_stuck_no_box_after_a_minute() {
        let mut w = World::new();
        w.slots = vec![slot(4, act(ActivityKind::Idle, NOW - 30))];
        w.locks = locks(vec![relay(7, 4, NO_BOX, NOW - 60)]);
        assert!(w.derive().is_empty(), "60 s is not more than a minute");

        w.locks = locks(vec![
            relay(7, 4, NO_BOX, NOW - 61),
            relay(8, 4, NO_BOX, NOW - 90),
        ]);
        let items = w.derive();
        assert_eq!(items.len(), 1, "one item per slot");
        let i = &items[0];
        assert_eq!(i.key, "relay_stuck:4");
        assert_eq!(
            (i.kind, i.slot, i.since),
            (AttentionKind::RelayStuck, Some(4), NOW - 90)
        );
        assert_eq!(i.text, "message not delivered: input box not found");

        // Clears when the relay is delivered.
        let mut r = relay(7, 4, NO_BOX, NOW - 61);
        r.state = RelayState::Delivered;
        r.held = None;
        w.locks = locks(vec![r]);
        assert!(w.derive().is_empty());
    }

    #[test]
    fn a_relay_waiting_on_the_board_typing_is_never_stuck() {
        let mut w = World::new();
        w.slots = vec![slot(4, act(ActivityKind::Idle, NOW - 900))];
        w.locks = locks(vec![relay(7, 4, crate::relay::courier::TYPING, NOW - 900)]);
        assert!(w.derive().is_empty());
    }

    #[test]
    fn relay_stuck_working_while_idle_is_the_58_case() {
        let mut w = World::new();
        w.locks = locks(vec![relay(1, 6, WORKING, NOW - 500)]);
        w.slots = vec![slot(6, act(ActivityKind::Working, NOW - 400))];
        assert!(
            w.derive().is_empty(),
            "a working slot holds its relay normally"
        );

        w.slots = vec![slot(6, act(ActivityKind::Idle, NOW - 60))];
        assert!(w.derive().is_empty(), "idle for only 60 s");

        w.slots = vec![slot(6, act(ActivityKind::Idle, NOW - 61))];
        let items = w.derive();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].key, "relay_stuck:6");
        assert_eq!(items[0].since, NOW - 500);
        assert_eq!(items[0].text, "message not delivered: session is working");

        // Other hold reasons, such as a prompt, are not stuck.
        w.locks = locks(vec![relay(1, 6, "waiting at a prompt for demo", NOW - 500)]);
        assert!(w.derive().is_empty());
    }

    #[test]
    fn notices_are_one_item_per_slot_from_the_oldest() {
        let mut w = World::new();
        let mut s = slot(1, None);
        s.state = SlotState::Exited {
            code: Some(0),
            at: 9,
        };
        w.slots = vec![s];
        let n = |slot, at, text: &str| Notice {
            slot,
            at,
            text: text.into(),
        };
        w.work = work(
            vec![],
            vec![
                n(1, 300, "newest\nnotice"),
                n(1, 100, "oldest notice"),
                n(1, 200, "middle notice"),
                n(2, 400, "only notice"),
            ],
            vec![],
        );
        let items = w.derive();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].key, "notice:1");
        assert_eq!(
            (items[0].kind, items[0].slot, items[0].since),
            (AttentionKind::Notice, Some(1), 100)
        );
        assert_eq!(items[0].text, "3 notices: newest notice");
        assert_eq!(items[1].key, "notice:2");
        assert_eq!(items[1].since, 400);
        assert_eq!(items[1].text, "1 notice: only notice");

        // Clears when the notices are cleared.
        w.work = work(vec![], vec![], vec![]);
        assert!(w.derive().is_empty());
    }

    fn failing(mut p: PullRequest) -> PullRequest {
        p.build = Some(BuildState::Failed);
        p.comments = Some(0);
        p
    }

    fn failed_seen(repo: &str, id: u64, at: i64) -> PrSeen {
        PrSeen {
            build_failed_since: Some(at),
            ..seen(repo, id)
        }
    }

    #[test]
    fn pr_items_need_author_or_a_matching_slot_branch() {
        let mut w = World::new();
        w.work = work(
            vec![],
            vec![],
            vec![
                failing(pr("demo-api", 1, "feature/a")),
                failing(pr("demo-api", 2, "feature/b")),
            ],
        );
        w.seen = vec![
            failed_seen("demo-api", 1, 10),
            failed_seen("demo-api", 2, 20),
        ];
        w.branches
            .insert(3, ("demo-api".into(), "feature/b".into()));
        assert_eq!(
            w.keys(),
            ["pr_build_failed:demo-api#1", "pr_build_failed:demo-api#2"]
        );

        w.author_set = false;
        assert_eq!(w.keys(), ["pr_build_failed:demo-api#2"]);
    }

    #[test]
    fn pr_items_take_since_from_pr_seen_and_name_the_slot() {
        let mut w = World::new();
        let mut p = pr("demo-api", 9, "feature/x");
        p.build = Some(BuildState::Failed);
        p.approvals = Some(Approvals {
            changes_requested: true,
            ..Approvals::default()
        });
        p.comments = Some(5);
        w.work = work(vec![], vec![], vec![p]);
        w.seen = vec![PrSeen {
            comments: Some(4),
            build_failed_since: Some(30),
            changes_since: Some(20),
            comments_since: Some(10),
            ..seen("demo-api", 9)
        }];
        w.branches
            .insert(5, ("demo-api".into(), "feature/x".into()));
        w.branches
            .insert(2, ("demo-api".into(), "feature/x".into()));
        w.branches.insert(1, ("demo-ws".into(), "feature/x".into()));

        let items = w.derive();
        let keys: Vec<_> = items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "pr_comments:demo-api#9",
                "pr_changes_requested:demo-api#9",
                "pr_build_failed:demo-api#9"
            ]
        );
        for i in &items {
            assert_eq!(
                i.slot,
                Some(2),
                "the lowest slot on the PR's repo and branch"
            );
            assert_eq!(i.pr, Some(("demo-api".to_string(), 9)));
            assert_eq!(i.url.as_deref(), Some("https://example.test/demo-api/9"));
        }
        assert_eq!(items[0].kind, AttentionKind::PrComments);
        assert_eq!(
            (items[0].since, items[0].text.as_str()),
            (10, "DEMO-9 change: 1 new comment")
        );
        assert_eq!(items[1].kind, AttentionKind::PrChangesRequested);
        assert_eq!(
            (items[1].since, items[1].text.as_str()),
            (20, "DEMO-9 change: changes requested")
        );
        assert_eq!(items[2].kind, AttentionKind::PrBuildFailed);
        assert_eq!(
            (items[2].since, items[2].text.as_str()),
            (30, "DEMO-9 change: build failed")
        );

        // More comments are counted against the seen count.
        w.work.prs[0].comments = Some(7);
        assert_eq!(w.derive()[0].text, "DEMO-9 change: 3 new comments");

        // Each clears with its condition: build passed, changes no longer
        // requested, comments seen.
        w.work.prs[0].build = Some(BuildState::Passed);
        w.work.prs[0].approvals = Some(Approvals::default());
        w.work.prs[0].comments = Some(4);
        assert!(w.derive().is_empty());

        // A PR with no seen row raises nothing, and one with no matching
        // slot names none.
        w.branches.clear();
        w.work.prs[0].build = Some(BuildState::Failed);
        assert_eq!(w.derive()[0].slot, None);
        w.seen.clear();
        assert!(w.derive().is_empty());
    }

    #[test]
    fn pr_items_trust_the_row_while_build_or_approvals_are_unknown() {
        let mut w = World::new();
        // A poll that could not read the build or the approvals.
        w.work = work(vec![], vec![], vec![pr("demo-api", 3, "feature/z")]);
        w.seen = vec![PrSeen {
            build_failed_since: Some(40),
            changes_since: Some(50),
            ..seen("demo-api", 3)
        }];
        let items = w.derive();
        let got: Vec<_> = items.iter().map(|i| (i.key.as_str(), i.since)).collect();
        assert_eq!(
            got,
            [
                ("pr_build_failed:demo-api#3", 40),
                ("pr_changes_requested:demo-api#3", 50)
            ]
        );

        // A known passing build and no changes requested clear them.
        w.work.prs[0].build = Some(BuildState::Passed);
        w.work.prs[0].approvals = Some(Approvals::default());
        assert!(w.derive().is_empty());
    }

    #[test]
    fn pr_comments_need_a_stored_count_to_count_from() {
        let mut w = World::new();
        let mut p = pr("demo-api", 6, "feature/c");
        p.build = Some(BuildState::Passed);
        p.approvals = Some(Approvals::default());
        p.comments = Some(5);
        w.work = work(vec![], vec![], vec![p]);
        w.seen = vec![PrSeen {
            comments: None,
            comments_since: Some(1),
            ..seen("demo-api", 6)
        }];
        assert!(w.derive().is_empty());

        w.seen[0].comments = Some(4);
        assert_eq!(w.derive()[0].text, "DEMO-6 change: 1 new comment");
    }

    #[test]
    fn bb_prs_with_no_counts_raise_nothing() {
        let mut w = World::new();
        w.work = work(vec![], vec![], vec![pr("demo-api", 4, "feature/y")]);
        w.seen = vec![PrSeen {
            comments: Some(0),
            comments_since: Some(1),
            ..seen("demo-api", 4)
        }];
        assert!(w.derive().is_empty());
    }

    #[test]
    fn items_sort_oldest_first_and_future_times_do_not_panic() {
        let mut w = World::new();
        w.work = work(vec![owned("DEMO-1", 1)], vec![], vec![]);
        w.slots = vec![
            slot(0, act(ActivityKind::NeedsYou, 500)),
            slot(1, idle_wait(200)),
            slot(2, act(ActivityKind::NeedsYou, 200)),
            slot(3, act(ActivityKind::NeedsYou, i64::MAX)),
            slot(4, idle_wait(i64::MAX)),
            slot(5, act(ActivityKind::Idle, i64::MIN)),
        ];
        w.locks = locks(vec![
            relay(1, 5, WORKING, i64::MIN),
            relay(2, 4, NO_BOX, i64::MAX),
        ]);
        assert_eq!(
            w.keys(),
            [
                "relay_stuck:5",
                "needs_you:2",
                "waiting:1",
                "needs_you:0",
                "needs_you:3"
            ]
        );
    }
}
