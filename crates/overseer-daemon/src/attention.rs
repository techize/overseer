//! The attention queue: everything waiting on you, derived from daemon
//! state. Pure, so the daemon can rerun it on every change and on a tick.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use overseer_proto::{
    ActivityKind, AttentionItem, AttentionKind, AttentionView, BuildState, Event, LockView,
    PullRequest, RelayState, SlotId, SlotInfo, SlotState, WorkView,
};
use tokio::sync::broadcast;

use crate::ledger::{Ledger, PrSeen};
use crate::locks::LockService;
use crate::manager::SlotManager;
use crate::relay::courier::{NO_BOX, WORKING};
use crate::work::store::unix_now;
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
    pub now: i64,
}

/// Everything waiting on you, oldest first.
pub fn derive(i: &Inputs) -> Vec<AttentionItem> {
    let mut items = Vec::new();
    for s in i.slots {
        slot_items(i, s, &mut items);
    }
    notice_items(i, &mut items);
    for pr in &i.work.prs {
        pr_items(i, pr, &mut items);
    }
    items.sort_by(|a, b| a.since.cmp(&b.since).then_with(|| a.key.cmp(&b.key)));
    items
}

/// How often the list is rederived with no event, so a wait that only
/// ripens with time still shows up.
const TICK: Duration = Duration::from_secs(30);

/// Holds the current attention list and announces it when it changes.
pub struct AttentionService {
    view: Mutex<AttentionView>,
}

impl AttentionService {
    pub fn current(&self) -> AttentionView {
        self.view.lock().unwrap().clone()
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
    /// once: settings only take effect when the daemon restarts.
    pub fn spawn(
        ledger: Arc<Ledger>,
        manager: Arc<Mutex<SlotManager>>,
        locks: Arc<LockService>,
        store: Arc<WorkStore>,
        events: broadcast::Sender<Event>,
        author_set: bool,
    ) -> Arc<AttentionService> {
        let svc = Arc::new(AttentionService {
            view: Mutex::new(AttentionView::default()),
        });
        // Subscribe before seeding so no change falls between the two.
        let mut rx = events.subscribe();
        let reread = move || (manager.lock().unwrap().list(), locks.view(), store.view());
        let (mut slots, mut lock_view, mut work) = reread();
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
                        Ok(_) => continue,
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            (slots, lock_view, work) = reread();
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
                    now: unix_now(),
                });
                if task.update(items) {
                    // Nobody listening is fine.
                    let _ = events.send(Event::AttentionChanged(task.current()));
                }
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

/// The repo and branch the checkout holding `cwd` is on, from its files
/// alone (no `git` process, so it is cheap to call on every refresh). The
/// repo is the main checkout's folder name, also for a linked worktree.
/// `None` outside a repo or on a detached HEAD.
pub fn repo_branch(cwd: &Path) -> Option<(String, String)> {
    let top = cwd.ancestors().find(|d| d.join(".git").exists())?;
    let dot_git = top.join(".git");
    let (git_dir, common) = if dot_git.is_file() {
        let text = fs::read_to_string(&dot_git).ok()?;
        let rel = text.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim();
        let git_dir = top.join(rel);
        // A worktree's git dir names the main `.git` in `commondir`; a
        // submodule's has none, and its own folder is the repo.
        let common = fs::read_to_string(git_dir.join("commondir"))
            .ok()
            .map(|c| git_dir.join(c.trim()));
        (git_dir, common)
    } else {
        (dot_git, None)
    };
    let repo = match common {
        Some(c) => main_repo_name(&c)?,
        None => top.file_name()?.to_str()?.to_string(),
    };
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let branch = head.trim().strip_prefix("ref: refs/heads/")?;
    Some((repo, branch.to_string()))
}

/// The repo name for a main git dir: the folder holding `.git`, or a bare
/// repo's own name less `.git`.
fn main_repo_name(common: &Path) -> Option<String> {
    let common = fs::canonicalize(common).ok()?;
    let name = common.file_name()?.to_str()?;
    if name == ".git" {
        return Some(common.parent()?.file_name()?.to_str()?.to_string());
    }
    Some(name.strip_suffix(".git").unwrap_or(name).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use overseer_proto::{
        Activity, ActivityKind, Approvals, BuildState, ItemKind, Notice, PullRequest, Relay,
        RelayKind, RelayState, Sender, SlotState, WorkItem,
    };
    use std::fs;

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
                now: self.now,
            })
        }

        fn keys(&self) -> Vec<String> {
            self.derive().into_iter().map(|i| i.key).collect()
        }
    }

    #[test]
    fn update_reports_only_changes() {
        let svc = AttentionService {
            view: Mutex::new(AttentionView::default()),
        };
        let one = |since| vec![item(AttentionKind::NeedsYou, 1, since, "needs you")];
        assert!(svc.update(one(10)));
        assert!(!svc.update(one(10)));
        assert!(svc.update(one(20)));
        assert_eq!(svc.current().items, one(20));
        assert!(svc.update(Vec::new()));
        assert!(!svc.update(Vec::new()));
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

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn repo_branch_reads_a_checkout_a_worktree_and_a_subfolder() {
        let t = tempfile::tempdir().unwrap();
        let main = t.path().join("demo-api");
        write(&main.join(".git/HEAD"), "ref: refs/heads/main\n");
        fs::create_dir_all(main.join("src/deep")).unwrap();
        let want = Some(("demo-api".to_string(), "main".to_string()));
        assert_eq!(repo_branch(&main), want);
        assert_eq!(repo_branch(&main.join("src/deep")), want);

        // A linked worktree: `.git` is a file naming its git dir, whose
        // `commondir` leads back to the main `.git`.
        let wt_git = main.join(".git/worktrees/wt");
        write(&wt_git.join("HEAD"), "ref: refs/heads/feature/DEMO-7\n");
        write(&wt_git.join("commondir"), "../..\n");
        let wt = t.path().join("elsewhere/wt");
        write(&wt.join(".git"), &format!("gitdir: {}\n", wt_git.display()));
        fs::create_dir_all(wt.join("crates")).unwrap();
        let want = Some(("demo-api".to_string(), "feature/DEMO-7".to_string()));
        assert_eq!(repo_branch(&wt), want);
        assert_eq!(repo_branch(&wt.join("crates")), want);

        // A relative gitdir works too.
        let wt2 = main.join(".claude/worktrees/wt2");
        let wt2_git = main.join(".git/worktrees/wt2");
        write(&wt2_git.join("HEAD"), "ref: refs/heads/demo-ws\n");
        write(&wt2_git.join("commondir"), "../..\n");
        write(&wt2.join(".git"), "gitdir: ../../../.git/worktrees/wt2\n");
        assert_eq!(
            repo_branch(&wt2),
            Some(("demo-api".to_string(), "demo-ws".to_string()))
        );

        // Detached HEAD, and a folder outside any repo.
        write(
            &wt_git.join("HEAD"),
            "0123456789abcdef0123456789abcdef01234567\n",
        );
        assert_eq!(repo_branch(&wt), None);
        let outside = t.path().join("elsewhere/plain");
        fs::create_dir_all(&outside).unwrap();
        assert_eq!(repo_branch(&outside), None);
    }
}
