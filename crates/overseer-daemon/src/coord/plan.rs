//! Coordinator plans: S0 proposes, Jonathan approves, edits or rejects, and
//! an approved plan runs slot by slot (Rulings 1, 4, 5 and 7).
//!
//! `validate` is a pure check against the board as it is at that moment. It
//! runs at propose time and again at approval, so a plan whose board has
//! moved on is refused with the current reason and stays pending.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use overseer_proto::msg::{
    owned_by_message, slot_name, LockView, Plan, PlanDecision, PlanSlot, PlanState, Response,
    SlotId, SlotInfo, SlotState, WorkItem, WorkView,
};

use super::worktree::{ensure_worktree, is_git_repo, worktree_path, Worktree};
use super::CoordService;
use crate::brain::BrainService;
use crate::env::BaseEnv;
use crate::ledger::decide_plan_on;
use crate::locks::targets::valid_target;
use crate::locks::{owned_items, LockService};
use crate::manager::{valid_id, SlotManager};
use crate::relay::MAX_TEXT as RELAY_MAX;
use crate::work::ownership::{decide, Decision};
use crate::work::store::{fill_kickoff, KickoffFacts};
use crate::work::WorkStore;

/// The longest kickoff a plan may carry for one slot, in characters.
const MAX_KICKOFF: usize = 2000;

/// The board as validation sees it.
pub struct Facts<'a> {
    pub slots: &'a [SlotInfo],
    pub work: &'a WorkView,
    pub locks: &'a LockView,
    pub owners: &'a HashMap<String, SlotId>,
    /// Where worktrees go, for the path a warning names.
    pub worktree_root: &'a Path,
}

/// `Facts` with its own copies, so it can be built and then borrowed.
pub struct OwnedFacts {
    slots: Vec<SlotInfo>,
    work: WorkView,
    locks: LockView,
    owners: HashMap<String, SlotId>,
    worktree_root: PathBuf,
}

impl OwnedFacts {
    pub fn facts(&self) -> Facts<'_> {
        Facts {
            slots: &self.slots,
            work: &self.work,
            locks: &self.locks,
            owners: &self.owners,
            worktree_root: &self.worktree_root,
        }
    }
}

/// What approving a plan works with: the services it drives and the paths
/// its kickoffs mention.
pub struct PlanCtx {
    pub manager: Arc<Mutex<SlotManager>>,
    pub store: Arc<WorkStore>,
    pub locks: Arc<LockService>,
    pub brain: Option<Arc<BrainService>>,
    pub env: BaseEnv,
    /// `config.worktree_root`.
    pub worktree_root: PathBuf,
    /// `config.brain_dir`.
    pub brain_dir: PathBuf,
    /// `config.current_session`.
    pub current_session: PathBuf,
    /// Holds `roles/assignment.md`, the kickoff template.
    pub state_dir: PathBuf,
}

/// Whether `ticket` can be part of a folder and branch name:
/// `^[A-Za-z0-9._-]{1,64}$`.
fn valid_ticket(ticket: &str) -> bool {
    (1..=64).contains(&ticket.len())
        && ticket
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Checks `plan` against `facts`. `Ok` carries the warnings (approval counts
/// as confirming them); `Err` is the first refusal, in Ruling 4's order.
pub fn validate(plan: &Plan, facts: &Facts) -> Result<Vec<String>, String> {
    if plan.slots.is_empty() {
        return Err("the plan has no slots".into());
    }
    for s in &plan.slots {
        if s.slot == 0 {
            return Err("S0 is the coordinator; a plan assigns S1 to S6".into());
        }
        if !valid_id(s.slot) {
            return Err(format!("invalid slot id {}", s.slot));
        }
    }
    for (at, s) in plan.slots.iter().enumerate() {
        if plan.slots[..at].iter().any(|p| p.slot == s.slot) {
            return Err(format!("{} appears twice in the plan", slot_name(s.slot)));
        }
    }
    let info = |slot: SlotId| facts.slots.iter().find(|i| i.id == slot);
    for s in &plan.slots {
        if matches!(
            info(s.slot).map(|i| &i.state),
            Some(SlotState::Resumable { .. })
        ) {
            return Err(format!(
                "{} has a session to resume; resume or stop it before planning for it",
                slot_name(s.slot)
            ));
        }
    }
    // Every item's decision, in plan order; the first unknown item refuses.
    let mut decisions: Vec<(SlotId, &str, Decision)> = Vec::new();
    for s in &plan.slots {
        for key in &s.items {
            let d = decide(&facts.work.items, facts.owners, key, s.slot, false, false)?;
            decisions.push((s.slot, key, d));
        }
    }
    let mut seen: Vec<(&str, SlotId)> = Vec::new();
    for (slot, key, _) in &decisions {
        match seen.iter().find(|(k, _)| k == key) {
            Some((_, first)) if first != slot => {
                return Err(format!(
                    "{key} appears in {} and {}",
                    slot_name(*first),
                    slot_name(*slot)
                ));
            }
            Some(_) => {}
            None => seen.push((key, *slot)),
        }
    }
    for (_, key, d) in &decisions {
        match d {
            Decision::Refuse { owner } | Decision::Move { from: owner } => {
                return Err(owned_by_message(key, *owner));
            }
            _ => {}
        }
    }
    for s in &plan.slots {
        for t in &s.targets {
            if !valid_target(&t.target) {
                return Err(format!(
                    "{} is not a target (repo:, argo:, deploy:, tf:, kube:, custom:)",
                    t.target
                ));
            }
        }
    }
    for s in &plan.slots {
        if let Some(w) = &s.worktree {
            if !is_git_repo(&w.repo) {
                return Err(format!("{} is not a git repository", w.repo.display()));
            }
        }
    }
    for s in &plan.slots {
        if let Some(w) = &s.worktree {
            if !valid_ticket(&w.ticket) {
                return Err(format!("{} cannot be a worktree name", w.ticket));
            }
        }
    }
    for s in &plan.slots {
        if let Some(cwd) = &s.cwd {
            if !cwd.is_dir() {
                return Err(format!("{} is not a folder", cwd.display()));
            }
        }
    }
    for s in &plan.slots {
        let idle = matches!(
            info(s.slot).map(|i| &i.state),
            Some(SlotState::Empty | SlotState::Exited { .. })
        );
        if idle && s.cwd.is_none() && s.worktree.is_none() {
            return Err(format!(
                "{} is empty and the plan gives it no cwd or worktree",
                slot_name(s.slot)
            ));
        }
    }
    for s in &plan.slots {
        if s.kickoff.chars().count() > MAX_KICKOFF {
            return Err(format!(
                "the kickoff for {} is longer than {MAX_KICKOFF} characters",
                slot_name(s.slot)
            ));
        }
    }

    let mut warnings = Vec::new();
    for s in &plan.slots {
        for (slot, _, d) in &decisions {
            if let (true, Decision::Warn { message, .. }) = (*slot == s.slot, d) {
                warnings.push(message.clone());
            }
        }
        for t in &s.targets {
            if let Some(c) = facts
                .locks
                .claims
                .iter()
                .find(|c| c.target == t.target && c.slot != s.slot)
            {
                warnings.push(format!(
                    "{} is held by {}; {} will queue for it",
                    t.target,
                    slot_name(c.slot),
                    slot_name(s.slot)
                ));
            }
        }
        if let Some(w) = &s.worktree {
            if matches!(
                info(s.slot).map(|i| &i.state),
                Some(SlotState::Running { .. })
            ) {
                warnings.push(format!(
                    "{} is running; it will be told to work in {} but not moved",
                    slot_name(s.slot),
                    worktree_path(facts.worktree_root, &w.repo, &w.ticket).display()
                ));
            }
        }
    }
    Ok(warnings)
}

/// The kickoff for one slot of a plan (Ruling 7): the assignment template
/// filled in, then a blank line and the plan's own kickoff text when it has
/// one. `items` is the whole work view; the slot's own are picked from it.
pub fn kickoff_for(
    template: &str,
    slot: &PlanSlot,
    items: &[WorkItem],
    held: &[String],
    worktree: Option<&Path>,
    brain_dir: &Path,
    current_session: &Path,
) -> String {
    let mine: Vec<&WorkItem> = items
        .iter()
        .filter(|i| i.owner == Some(slot.slot))
        .collect();
    let facts = KickoffFacts {
        held: held.to_vec(),
        worktree: worktree.map(Path::to_path_buf),
        rules: slot.rules.clone(),
        current_session: current_session.to_path_buf(),
    };
    let base = fill_kickoff(template, slot.slot, &mine, items, brain_dir, &facts);
    let own = slot.kickoff.trim();
    if own.is_empty() {
        base
    } else {
        format!("{base}\n\n{own}")
    }
}

fn error(message: impl Into<String>) -> Response {
    Response::Error {
        message: message.into(),
    }
}

impl PlanCtx {
    fn state_of(&self, slot: SlotId) -> SlotState {
        self.manager.lock().unwrap().list()[slot as usize]
            .state
            .clone()
    }

    fn live_s0(&self) -> bool {
        matches!(self.state_of(0), SlotState::Running { .. })
    }
}

impl CoordService {
    /// A copy of the board for `validate`.
    pub fn gather(&self, ctx: &PlanCtx) -> OwnedFacts {
        let owners = self
            .ledger
            .assignments()
            .map(|a| a.into_iter().collect())
            .unwrap_or_else(|e| {
                tracing::warn!("assignments unreadable: {e:#}");
                HashMap::new()
            });
        OwnedFacts {
            slots: ctx.manager.lock().unwrap().list(),
            work: ctx.store.view(),
            locks: ctx.locks.view(),
            owners,
            worktree_root: ctx.worktree_root.clone(),
        }
    }

    /// `propose_plan`: validates and records `plan` as the pending one.
    pub fn propose(&self, from: Option<SlotId>, plan: Plan, facts: &Facts, now: i64) -> Response {
        if let Err(message) = super::coordinator_only(from, "propose a plan") {
            return error(message);
        }
        let warnings = match validate(&plan, facts) {
            Ok(w) => w,
            Err(message) => return error(message),
        };
        let (id, replaced) = match self.ledger.add_plan(&plan, &warnings, now) {
            Ok(r) => r,
            Err(e) => return error(format!("ledger: {e:#}")),
        };
        self.announce();
        let mut text = format!("Plan {id} is waiting for Jonathan.");
        if let Some(old) = replaced {
            text.push_str(&format!(" It replaces plan {old}."));
        }
        Response::Text { text }
    }

    /// Jonathan's answer to plan `id`. Approving re-validates `plan` against
    /// the board now, marks the plan approved in one ledger transaction
    /// (a second approval finds it no longer pending), then runs the steps.
    pub async fn decide(
        self: &Arc<Self>,
        id: i64,
        from: Option<SlotId>,
        decision: PlanDecision,
        ctx: &PlanCtx,
        now: i64,
    ) -> Response {
        if from.is_some() {
            return error("only Jonathan can approve or reject a plan");
        }
        let record = match self.ledger.plan(id) {
            Ok(Some(r)) => r,
            Ok(None) => return error(format!("no plan {id}")),
            Err(e) => return error(format!("ledger: {e:#}")),
        };
        let not_pending = || error(format!("plan {id} is no longer pending"));
        if record.state != PlanState::Pending {
            return not_pending();
        }
        match decision {
            PlanDecision::Reject { reason } => {
                let settled = self.ledger.transaction(|conn| {
                    decide_plan_on(conn, id, PlanState::Rejected, &record.plan, false, now)
                });
                match settled {
                    Ok(true) => {}
                    Ok(false) => return not_pending(),
                    Err(e) => return error(format!("ledger: {e:#}")),
                }
                let reason = reason.trim();
                let reason = if reason.is_empty() {
                    "no reason given"
                } else {
                    reason
                };
                self.announce();
                if ctx.live_s0() {
                    let text = format!("Jonathan rejected plan {id}: {reason}");
                    if let Err(e) = self.tell(&ctx.locks, 0, &text, now) {
                        tracing::warn!("could not tell S0 about plan {id}: {e:#}");
                    }
                }
                Response::Ok
            }
            PlanDecision::Approve { plan } => self.approve(id, record.plan, plan, ctx, now).await,
        }
    }

    async fn approve(
        self: &Arc<Self>,
        id: i64,
        proposed: Plan,
        plan: Plan,
        ctx: &PlanCtx,
        now: i64,
    ) -> Response {
        let owned = self.gather(ctx);
        if let Err(message) = validate(&plan, &owned.facts()) {
            return error(message);
        }
        let edited = plan != proposed;
        let settled = self
            .ledger
            .transaction(|conn| decide_plan_on(conn, id, PlanState::Approved, &plan, edited, now));
        match settled {
            Ok(true) => {}
            Ok(false) => return error(format!("plan {id} is no longer pending")),
            Err(e) => return error(format!("ledger: {e:#}")),
        }
        // From here the plan is approved for good: a crash or a failing step
        // never runs it twice.
        self.announce();
        let template = match crate::launch::assignment_template(&ctx.state_dir) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("assignment template unreadable, using the default: {e}");
                crate::launch::ASSIGNMENT.to_string()
            }
        };
        let mut slots: Vec<&PlanSlot> = plan.slots.iter().collect();
        slots.sort_by_key(|s| s.slot);
        let mut outcome = Vec::new();
        for s in slots {
            self.run_slot(s, &template, ctx, now, &mut outcome).await;
        }
        if let Err(e) = self.ledger.set_plan_outcome(id, &outcome) {
            tracing::warn!("outcome of plan {id} not stored: {e:#}");
        }
        self.announce();
        let with = if edited { " with edits" } else { "" };
        if ctx.live_s0() {
            let text = format!("Jonathan approved plan {id}{with}.\n{}", outcome.join("\n"));
            let text: String = text.trim_end().chars().take(RELAY_MAX).collect();
            if let Err(e) = self.tell(&ctx.locks, 0, &text, now) {
                tracing::warn!("could not tell S0 about plan {id}: {e:#}");
            }
        }
        Response::Text {
            text: if outcome.is_empty() {
                format!("Plan {id} approved. There was nothing to run.")
            } else {
                outcome.join("\n")
            },
        }
    }

    /// One slot's steps (Ruling 5). A failing step ends this slot's part
    /// with one line saying so; the next slot goes on.
    async fn run_slot(
        &self,
        ps: &PlanSlot,
        template: &str,
        ctx: &PlanCtx,
        now: i64,
        out: &mut Vec<String>,
    ) {
        let slot = ps.slot;
        let name = slot_name(slot);
        let skipped = |step: &str, err: &str| {
            format!("{name}: {step} failed: {err}; the rest of {name}'s plan was skipped")
        };
        let state = ctx.state_of(slot);
        if matches!(state, SlotState::Resumable { .. }) {
            out.push(skipped("start", "it has a session to resume"));
            return;
        }
        let running = matches!(state, SlotState::Running { .. });
        // A fresh session replaces the last one: its waits, unused grants and
        // pending relays are not the new session's. Done before the claims
        // below, which belong to the new one.
        if !running {
            ctx.locks.on_session_ended(slot, now);
        }

        // 1. Items.
        let mut assigned = Vec::new();
        for key in &ps.items {
            match ctx.store.assign(key, slot, true, false, now) {
                Response::Ok => assigned.push(key.as_str()),
                Response::Error { message } => {
                    if !assigned.is_empty() {
                        out.push(format!("{name}: assigned {}", assigned.join(", ")));
                    }
                    out.push(skipped(&format!("assign {key}"), &message));
                    return;
                }
                other => {
                    out.push(skipped(
                        &format!("assign {key}"),
                        &format!("unexpected reply {other:?}"),
                    ));
                    return;
                }
            }
        }
        if !assigned.is_empty() {
            out.push(format!("{name}: assigned {}", assigned.join(", ")));
        }

        // 2. Worktree.
        let mut worktree: Option<PathBuf> = None;
        if let Some(w) = &ps.worktree {
            match ensure_worktree(&ctx.worktree_root, &w.repo, &w.ticket).await {
                Ok(Worktree::Created(p)) => {
                    out.push(format!("{name}: worktree {} created", p.display()));
                    worktree = Some(p);
                }
                Ok(Worktree::Reused(p)) => {
                    out.push(format!("{name}: worktree {} reused", p.display()));
                    worktree = Some(p);
                }
                Err(e) => {
                    let path = worktree_path(&ctx.worktree_root, &w.repo, &w.ticket);
                    out.push(skipped(&format!("worktree {}", path.display()), &e));
                    return;
                }
            }
        }

        // 3. Targets.
        let mut held = Vec::new();
        let owned = owned_items(&ctx.store.view(), slot);
        for t in &ps.targets {
            match ctx
                .locks
                .claim(slot, &t.target, t.reason.as_deref(), None, &owned, now)
            {
                Response::Text { .. } => {
                    out.push(format!("{name}: holds {}", t.target));
                    held.push(t.target.clone());
                }
                Response::Queued { .. } => {
                    let behind = ctx
                        .locks
                        .view()
                        .claims
                        .iter()
                        .find(|c| c.target == t.target)
                        .map_or_else(String::new, |c| format!(" behind {}", slot_name(c.slot)));
                    out.push(format!("{name}: queued for {}{behind}", t.target));
                }
                Response::Error { message } => {
                    out.push(skipped(&format!("claim {}", t.target), &message));
                    return;
                }
                other => {
                    out.push(skipped(
                        &format!("claim {}", t.target),
                        &format!("unexpected reply {other:?}"),
                    ));
                    return;
                }
            }
        }

        // 4. Kickoff: the first prompt of an empty slot, a relay to a
        // running one.
        let items = ctx.store.view().items;
        let kickoff = kickoff_for(
            template,
            ps,
            &items,
            &held,
            worktree.as_deref(),
            &ctx.brain_dir,
            &ctx.current_session,
        );
        if running {
            let n = kickoff.chars().count();
            if n > RELAY_MAX {
                out.push(skipped(
                    "kickoff",
                    &format!("kickoff is {n} characters; the limit is {RELAY_MAX}"),
                ));
                return;
            }
            match self.tell(&ctx.locks, slot, &kickoff, now) {
                Ok(()) => out.push(format!("{name}: kickoff queued")),
                Err(e) => out.push(skipped("kickoff", &format!("ledger: {e:#}"))),
            }
            return;
        }
        let Some(dir) = worktree.or_else(|| ps.cwd.clone()) else {
            out.push(skipped("start", "no folder to start in"));
            return;
        };
        let label = if !ps.label.trim().is_empty() {
            ps.label.trim().to_string()
        } else {
            ps.items.first().cloned().unwrap_or_else(|| "work".into())
        };
        // The same two steps as a start from the GUI: the login shell's
        // environment, and a fresh CURRENT_SESSION section.
        let mut env = ctx.env.clone();
        env.wait().await;
        if let Some(b) = &ctx.brain {
            b.clear_current_session(slot).await;
        }
        let started = {
            let mut m = ctx.manager.lock().unwrap();
            m.start(slot, label, &dir, None, Some(kickoff))
        };
        match started {
            Ok(()) => out.push(format!("{name}: started in {}", dir.display())),
            Err(e) => out.push(skipped("start", &e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use overseer_proto::msg::{Claim, ClaimKind, ItemKind, PlanTarget, PlanWorktree, SlotState};

    fn empty_slot(id: SlotId) -> SlotInfo {
        SlotInfo {
            id,
            label: String::new(),
            cwd: None,
            command: None,
            state: SlotState::Empty,
            cols: 80,
            rows: 24,
            activity: None,
            session_note: None,
        }
    }

    fn running_slot(id: SlotId) -> SlotInfo {
        SlotInfo {
            state: SlotState::Running { pid: 1, since: 0 },
            ..empty_slot(id)
        }
    }

    fn item(key: &str, kind: ItemKind, parent: Option<&str>, owner: Option<SlotId>) -> WorkItem {
        WorkItem {
            key: key.into(),
            kind,
            title: format!("Title of {key}"),
            status: "open".into(),
            priority: None,
            parent: parent.map(String::from),
            owner,
            in_list: true,
            folder: None,
        }
    }

    struct Board {
        slots: Vec<SlotInfo>,
        work: WorkView,
        locks: LockView,
        owners: HashMap<String, SlotId>,
        root: PathBuf,
    }

    impl Board {
        fn new() -> Board {
            Board {
                slots: (0..7).map(empty_slot).collect(),
                work: WorkView {
                    items: vec![
                        item("DEMO-12", ItemKind::Ticket, None, None),
                        item("DEMO-13", ItemKind::Ticket, None, None),
                        item("ov-3", ItemKind::Bead, Some("DEMO-12"), None),
                    ],
                    sources: vec![],
                    notices: vec![],
                },
                locks: LockView {
                    claims: vec![],
                    waits: vec![],
                    relays: vec![],
                    flag_after_secs: 0,
                },
                owners: HashMap::new(),
                root: PathBuf::from("/wt"),
            }
        }

        fn own(&mut self, key: &str, slot: SlotId) {
            self.owners.insert(key.into(), slot);
            self.work
                .items
                .iter_mut()
                .find(|i| i.key == key)
                .unwrap()
                .owner = Some(slot);
        }

        fn check(&self, plan: &Plan) -> Result<Vec<String>, String> {
            validate(
                plan,
                &Facts {
                    slots: &self.slots,
                    work: &self.work,
                    locks: &self.locks,
                    owners: &self.owners,
                    worktree_root: &self.root,
                },
            )
        }
    }

    fn slot(n: SlotId) -> PlanSlot {
        PlanSlot {
            slot: n,
            label: String::new(),
            items: vec![],
            targets: vec![],
            worktree: None,
            cwd: Some(std::env::temp_dir()),
            kickoff: String::new(),
            rules: vec![],
        }
    }

    fn plan(slots: Vec<PlanSlot>) -> Plan {
        Plan {
            summary: String::new(),
            slots,
        }
    }

    fn refusal(board: &Board, p: &Plan) -> String {
        board.check(p).unwrap_err()
    }

    #[test]
    fn valid_plan_has_no_warnings() {
        let b = Board::new();
        let mut s = slot(3);
        s.items = vec!["DEMO-12".into()];
        s.targets = vec![PlanTarget {
            target: "repo:demo-api:main".into(),
            reason: None,
        }];
        assert_eq!(b.check(&plan(vec![s])), Ok(vec![]));
    }

    #[test]
    fn refuses_a_plan_without_slots() {
        assert_eq!(
            refusal(&Board::new(), &plan(vec![])),
            "the plan has no slots"
        );
    }

    #[test]
    fn refuses_the_coordinator_and_bad_slot_ids() {
        let b = Board::new();
        assert_eq!(
            refusal(&b, &plan(vec![slot(0)])),
            "S0 is the coordinator; a plan assigns S1 to S6"
        );
        assert_eq!(refusal(&b, &plan(vec![slot(9)])), "invalid slot id 9");
    }

    #[test]
    fn refuses_a_slot_twice() {
        assert_eq!(
            refusal(&Board::new(), &plan(vec![slot(3), slot(3)])),
            "S3 appears twice in the plan"
        );
    }

    #[test]
    fn refuses_a_resumable_slot() {
        let mut b = Board::new();
        b.slots[3].state = SlotState::Resumable {
            session_id: "x".into(),
            at: 0,
        };
        assert_eq!(
            refusal(&b, &plan(vec![slot(3)])),
            "S3 has a session to resume; resume or stop it before planning for it"
        );
    }

    #[test]
    fn refuses_an_unknown_item() {
        let mut s = slot(3);
        s.items = vec!["DEMO-99".into()];
        assert_eq!(
            refusal(&Board::new(), &plan(vec![s])),
            "no work item DEMO-99"
        );
    }

    #[test]
    fn refuses_an_item_in_two_slots() {
        let (mut a, mut c) = (slot(2), slot(3));
        a.items = vec!["DEMO-12".into()];
        c.items = vec!["DEMO-12".into()];
        assert_eq!(
            refusal(&Board::new(), &plan(vec![a, c])),
            "DEMO-12 appears in S2 and S3"
        );
    }

    #[test]
    fn refuses_an_item_owned_by_another_slot() {
        let mut b = Board::new();
        b.own("DEMO-12", 2);
        let mut s = slot(3);
        s.items = vec!["DEMO-12".into()];
        assert_eq!(refusal(&b, &plan(vec![s])), "DEMO-12 is owned by S2");
    }

    #[test]
    fn refuses_a_bad_target() {
        let mut s = slot(3);
        s.targets = vec![PlanTarget {
            target: "main".into(),
            reason: None,
        }];
        assert_eq!(
            refusal(&Board::new(), &plan(vec![s])),
            "main is not a target (repo:, argo:, deploy:, tf:, kube:, custom:)"
        );
    }

    #[test]
    fn refuses_a_worktree_repo_that_is_not_git() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = slot(3);
        s.worktree = Some(PlanWorktree {
            repo: dir.path().to_path_buf(),
            ticket: "DEMO-12".into(),
        });
        assert_eq!(
            refusal(&Board::new(), &plan(vec![s])),
            format!("{} is not a git repository", dir.path().display())
        );
    }

    #[test]
    fn refuses_a_ticket_that_cannot_name_a_worktree() {
        // A repo is needed to get past the repo check.
        let dir = tempfile::tempdir().unwrap();
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success();
        assert!(ok);
        for bad in ["a b", "a/b", "", &"x".repeat(65)] {
            let mut s = slot(3);
            s.worktree = Some(PlanWorktree {
                repo: dir.path().to_path_buf(),
                ticket: bad.into(),
            });
            assert_eq!(
                refusal(&Board::new(), &plan(vec![s])),
                format!("{bad} cannot be a worktree name")
            );
        }
    }

    #[test]
    fn refuses_a_cwd_that_is_not_a_folder() {
        let mut s = slot(3);
        s.cwd = Some(PathBuf::from("/no/such/folder"));
        assert_eq!(
            refusal(&Board::new(), &plan(vec![s])),
            "/no/such/folder is not a folder"
        );
    }

    #[test]
    fn refuses_an_empty_slot_with_nowhere_to_start() {
        let mut s = slot(4);
        s.cwd = None;
        assert_eq!(
            refusal(&Board::new(), &plan(vec![s.clone()])),
            "S4 is empty and the plan gives it no cwd or worktree"
        );
        let mut b = Board::new();
        b.slots[4].state = SlotState::Exited {
            code: Some(0),
            at: 0,
        };
        assert_eq!(
            refusal(&b, &plan(vec![s.clone()])),
            "S4 is empty and the plan gives it no cwd or worktree"
        );
        // A running slot needs none.
        b.slots[4] = running_slot(4);
        assert_eq!(b.check(&plan(vec![s])), Ok(vec![]));
    }

    #[test]
    fn refuses_a_long_kickoff() {
        let mut s = slot(3);
        s.kickoff = "x".repeat(2001);
        assert_eq!(
            refusal(&Board::new(), &plan(vec![s])),
            "the kickoff for S3 is longer than 2000 characters"
        );
        let mut s = slot(3);
        s.kickoff = "x".repeat(2000);
        assert!(Board::new().check(&plan(vec![s])).is_ok());
    }

    #[test]
    fn refusals_come_in_ruling_order() {
        // An unknown item beats a bad target and a bad folder.
        let mut s = slot(3);
        s.items = vec!["DEMO-99".into()];
        s.cwd = Some(PathBuf::from("/no/such/folder"));
        s.targets = vec![PlanTarget {
            target: "main".into(),
            reason: None,
        }];
        assert_eq!(
            refusal(&Board::new(), &plan(vec![s])),
            "no work item DEMO-99"
        );
    }

    #[test]
    fn warns_about_a_bead_under_another_slots_ticket() {
        let mut b = Board::new();
        b.own("DEMO-12", 2);
        let mut s = slot(3);
        s.items = vec!["ov-3".into()];
        let warnings = b.check(&plan(vec![s])).unwrap();
        assert_eq!(
            warnings,
            vec!["This bead is under DEMO-12, owned by S2. Assign to S3 anyway?".to_string()]
        );
    }

    #[test]
    fn warns_about_a_target_held_by_another_slot() {
        let mut b = Board::new();
        b.locks.claims.push(Claim {
            target: "argo:uat/root".into(),
            slot: 1,
            kind: ClaimKind::Claim,
            reason: None,
            item: None,
            since: 0,
            last_used: 0,
            pin_revision: None,
        });
        let mut s = slot(3);
        s.targets = vec![PlanTarget {
            target: "argo:uat/root".into(),
            reason: None,
        }];
        assert_eq!(
            b.check(&plan(vec![s])).unwrap(),
            vec!["argo:uat/root is held by S1; S3 will queue for it".to_string()]
        );
    }

    #[test]
    fn warns_about_a_worktree_for_a_running_slot() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("demo-api");
        std::fs::create_dir(&repo).unwrap();
        assert!(std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success());
        let mut b = Board::new();
        b.slots[3] = running_slot(3);
        let mut s = slot(3);
        s.worktree = Some(PlanWorktree {
            repo,
            ticket: "DEMO-12".into(),
        });
        assert_eq!(
            b.check(&plan(vec![s])).unwrap(),
            vec![
                "S3 is running; it will be told to work in /wt/demo-api-DEMO-12 but not moved"
                    .to_string()
            ]
        );
    }

    fn template() -> &'static str {
        crate::launch::ASSIGNMENT
    }

    #[test]
    fn kickoff_lists_items_targets_worktree_rules_and_plan_text() {
        let mut s = slot(4);
        s.items = vec!["DEMO-12".into()];
        s.rules = vec![
            "Do not deploy to UAT before S2 merges".into(),
            "Ask S1 before touching the schema".into(),
        ];
        s.kickoff = "Start with the parser.".into();
        let mut ticket = item("DEMO-12", ItemKind::Ticket, None, Some(4));
        ticket.folder = Some(PathBuf::from("/b/Sprints/s1/DEMO-12-widget"));
        let text = kickoff_for(
            template(),
            &s,
            &[ticket],
            &["repo:demo-api:main".to_string()],
            Some(Path::new("/wt/demo-api-DEMO-12")),
            Path::new("/b"),
            Path::new("/home/x/CURRENT_SESSION.md"),
        );
        assert!(text.starts_with("You are S4. Overseer has assigned you this work:"));
        assert!(
            text.contains("- DEMO-12 (open): Title of DEMO-12"),
            "{text}"
        );
        assert!(
            text.contains("You hold these targets: repo:demo-api:main\n"),
            "{text}"
        );
        assert!(text.contains("Work in /wt/demo-api-DEMO-12.\n"), "{text}");
        assert!(
            text.contains(
                "Keep your section of /home/x/CURRENT_SESSION.md current with update_current_session.\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "Rules for this work:\n- Do not deploy to UAT before S2 merges\n- Ask S1 before touching the schema\n"
            ),
            "{text}"
        );
        assert!(text.ends_with("\n\nStart with the parser."), "{text}");
        assert!(!text.contains("{"), "{text}");
    }

    #[test]
    fn kickoff_drops_empty_lines() {
        let s = slot(4);
        let text = kickoff_for(
            template(),
            &s,
            &[item("DEMO-12", ItemKind::Ticket, None, Some(4))],
            &[],
            None,
            Path::new("/b"),
            Path::new("/home/x/CURRENT_SESSION.md"),
        );
        assert!(!text.contains("You hold these targets"), "{text}");
        assert!(!text.contains("Work in"), "{text}");
        assert!(!text.contains("Rules for this work"), "{text}");
        assert!(text.contains("update_current_session"), "{text}");
        assert!(!text.contains("\n\n\n"), "{text}");
        // No plan text: it ends with the template.
        assert!(text
            .trim_end()
            .ends_with("Report back here when you finish a step."));
    }

    #[test]
    fn tickets_are_checked_like_the_ruling() {
        assert!(valid_ticket("DEMO-12"));
        assert!(valid_ticket("a.b_c-1"));
        assert!(!valid_ticket(""));
        assert!(!valid_ticket("a b"));
        assert!(!valid_ticket(&"x".repeat(65)));
        assert!(valid_ticket(&"x".repeat(64)));
    }
}
