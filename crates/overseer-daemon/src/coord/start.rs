//! Starting one ticket on a free slot by dropping it there on the board
//! (#87). `PrepareStart` answers with a draft the GUI shows on a confirm
//! card; `StartTicket` runs it as a one-slot plan through the same steps an
//! approved plan takes: assign, worktree, start with the brief.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use overseer_proto::msg::{
    slot_name, Plan, PlanSlot, PlanWorktree, Response, SlotId, SlotState, StartDraft, WorkItem,
};

use super::plan::{kickoff_for, validate, PlanCtx};
use super::worktree::{is_git_repo, worktree_path};
use super::CoordService;
use crate::manager::valid_id;

/// The longest branch name before a `-2` style suffix, in characters.
const MAX_SLUG: usize = 50;
/// Suffixes tried after the bare name: `-2` up to this.
const MAX_SUFFIX: u32 = 99;

fn error(message: impl Into<String>) -> Response {
    Response::Error {
        message: message.into(),
    }
}

/// `demo-12-short-title` from `DEMO-12` and its title: lowercase ASCII
/// letters and digits joined by single dashes, at most `MAX_SLUG` characters,
/// cut at a word boundary when the key leaves room for one.
pub fn branch_slug(key: &str, title: &str) -> String {
    fn words(s: &str) -> Vec<String> {
        s.split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(str::to_ascii_lowercase)
            .collect()
    }
    let mut slug = words(key).join("-");
    if slug.len() > MAX_SLUG {
        slug.truncate(MAX_SLUG);
        slug = slug.trim_end_matches('-').to_string();
    }
    for w in words(title) {
        let extra = usize::from(!slug.is_empty()) + w.len();
        if slug.len() + extra > MAX_SLUG {
            if slug.is_empty() {
                slug = w[..MAX_SLUG].to_string();
            }
            break;
        }
        if !slug.is_empty() {
            slug.push('-');
        }
        slug.push_str(&w);
    }
    if slug.is_empty() {
        "work".into()
    } else {
        slug
    }
}

/// `base`, else `base-2`, `base-3` and so on: the first name `taken` says is
/// free. `None` when every suffix up to `MAX_SUFFIX` is taken.
pub fn unique_branch(base: &str, taken: impl Fn(&str) -> bool) -> Option<String> {
    std::iter::once(base.to_string())
        .chain((2..=MAX_SUFFIX).map(|n| format!("{base}-{n}")))
        .find(|b| !taken(b))
}

/// Whether `repo` has a local branch `name`.
fn has_branch(repo: &Path, name: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/heads/{name}"))
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The main checkout of the repo `dir` belongs to, for a slot working in a
/// worktree: the parent of the git common dir when that is a `.git` folder.
fn main_checkout(dir: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let common = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    if common.file_name()? != ".git" {
        return None;
    }
    common.parent()?.canonicalize().ok()
}

/// What a slot is busy with, for the refusal: "S2 is running DEMO-7".
fn busy_reason(slot: SlotId, state: &SlotState, mine: &[&WorkItem]) -> Option<String> {
    let name = slot_name(slot);
    let what = mine
        .iter()
        .map(|i| i.key.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    match state {
        SlotState::Empty | SlotState::Exited { .. } => None,
        SlotState::Running { .. } if what.is_empty() => Some(format!("{name} is running")),
        SlotState::Running { .. } => Some(format!("{name} is running {what}")),
        SlotState::Resumable { .. } if what.is_empty() => {
            Some(format!("{name} has a session to resume"))
        }
        SlotState::Resumable { .. } => Some(format!("{name} has a session to resume for {what}")),
    }
}

/// The one-slot plan a drop runs.
fn plan_for(slot: SlotId, key: &str, repo: &Path, branch: &str) -> Plan {
    Plan {
        slots: vec![PlanSlot {
            slot,
            label: key.to_string(),
            items: vec![key.to_string()],
            worktree: Some(PlanWorktree {
                repo: repo.to_path_buf(),
                ticket: branch.to_string(),
            }),
            moves: Vec::new(),
            targets: Vec::new(),
            cwd: None,
            kickoff: String::new(),
            rules: Vec::new(),
        }],
        summary: String::new(),
    }
}

impl CoordService {
    /// The repos a drop can start in: `pr_repos`, then the main checkouts of
    /// the folders slots work in, without repeats or folders that are not
    /// git repos.
    fn start_repos(ctx: &PlanCtx) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        let mut seen: Vec<PathBuf> = Vec::new();
        let slot_dirs: Vec<PathBuf> = ctx
            .manager
            .lock()
            .unwrap()
            .list()
            .into_iter()
            .filter_map(|s| s.cwd)
            .collect();
        let from_slots = slot_dirs.iter().filter_map(|d| main_checkout(d));
        for repo in ctx.repos.iter().cloned().chain(from_slots) {
            let key = repo.canonicalize().unwrap_or_else(|_| repo.clone());
            if seen.contains(&key) || !is_git_repo(&repo) {
                continue;
            }
            seen.push(key);
            out.push(repo);
        }
        out
    }

    /// The checks both requests share: a worker slot, free, and a ticket on
    /// the board. `Ok` is the ticket.
    fn check_drop(&self, slot: SlotId, key: &str, ctx: &PlanCtx) -> Result<WorkItem, String> {
        if slot == 0 {
            return Err("S0 is the coordinator; drop a ticket on S1 to S6".into());
        }
        if !valid_id(slot) {
            return Err(format!("invalid slot id {slot}"));
        }
        let items = ctx.store.view().items;
        let mine: Vec<&WorkItem> = items.iter().filter(|i| i.owner == Some(slot)).collect();
        if let Some(reason) = busy_reason(slot, &ctx.state_of(slot), &mine) {
            return Err(reason);
        }
        items
            .iter()
            .find(|i| i.key == key)
            .cloned()
            .ok_or_else(|| format!("{key} is not on the board"))
    }

    /// The draft for dropping `key` on `slot`: the repos to pick from, the
    /// one picked (`repo` when given, else the last one used), and the
    /// branch, worktree and brief that start would use.
    pub fn prepare_start(
        &self,
        slot: SlotId,
        key: &str,
        repo: Option<PathBuf>,
        ctx: &PlanCtx,
    ) -> Response {
        let ticket = match self.check_drop(slot, key, ctx) {
            Ok(t) => t,
            Err(message) => return error(message),
        };
        let repos = Self::start_repos(ctx);
        let last = self.last_repo.lock().unwrap().clone();
        let picked = repo
            .filter(|r| repos.contains(r))
            .or_else(|| last.filter(|r| repos.contains(r)))
            .or_else(|| repos.first().cloned());
        let Some(picked) = picked else {
            return error("there is no repo to start in; add one to PR repos in Settings");
        };
        let base = branch_slug(&ticket.key, &ticket.title);
        let taken = |b: &str| {
            has_branch(&picked, b) || worktree_path(&ctx.worktree_root, &picked, b).exists()
        };
        let Some(branch) = unique_branch(&base, taken) else {
            return error(format!(
                "every branch name from {base} to {base}-{MAX_SUFFIX} is taken"
            ));
        };
        let plan = plan_for(slot, key, &picked, &branch);
        let owned = self.gather(ctx);
        if let Err(message) = validate(&plan, &owned.facts()) {
            return error(message);
        }
        let worktree = worktree_path(&ctx.worktree_root, &picked, &branch);
        // The brief as it will read once the ticket is this slot's.
        let mut items = ctx.store.view().items;
        for i in items.iter_mut().filter(|i| i.key == key) {
            i.owner = Some(slot);
        }
        let brief = kickoff_for(
            &start_template(ctx),
            &plan.slots[0],
            &items,
            &[],
            Some(&worktree),
            &ctx.brain_dir,
            &ctx.current_session,
        );
        Response::StartDraft(StartDraft {
            slot,
            key: ticket.key,
            title: ticket.title,
            repos,
            repo: picked,
            branch,
            worktree,
            brief,
        })
    }

    /// Starts `key` on `slot` in a new worktree of `repo` on `branch`, the
    /// draft the user confirmed. Any step that fails refuses with what
    /// happened; the steps before it stay done, as with a plan.
    pub async fn start_ticket(
        &self,
        slot: SlotId,
        key: &str,
        repo: PathBuf,
        branch: &str,
        ctx: &PlanCtx,
        now: i64,
    ) -> Response {
        let _one_at_a_time = self.starting.lock().await;
        if let Err(message) = self.check_drop(slot, key, ctx) {
            return error(message);
        }
        if !Self::start_repos(ctx).contains(&repo) {
            return error(format!(
                "{} is not one of the repos to start in",
                repo.display()
            ));
        }
        let plan = plan_for(slot, key, &repo, branch);
        let owned = self.gather(ctx);
        if let Err(message) = validate(&plan, &owned.facts()) {
            return error(message);
        }
        *self.last_repo.lock().unwrap() = Some(repo);
        let mut outcome = Vec::new();
        self.run_slot(
            0,
            &plan.slots[0],
            &start_template(ctx),
            ctx,
            now,
            &mut outcome,
        )
        .await;
        let text = outcome.join("\n");
        if outcome.iter().any(|l| l.ends_with("was skipped")) {
            error(text)
        } else {
            Response::Text { text }
        }
    }
}

fn start_template(ctx: &PlanCtx) -> String {
    crate::launch::start_template(&ctx.state_dir).unwrap_or_else(|e| {
        tracing::warn!("start template unreadable, using the default: {e}");
        crate::launch::START.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_slug_is_the_key_and_title_in_lowercase_words() {
        assert_eq!(branch_slug("DEMO-12", "Short title"), "demo-12-short-title");
        assert_eq!(
            branch_slug("DEMO-3", "Fix the API's /v2 path (again)!"),
            "demo-3-fix-the-api-s-v2-path-again"
        );
        assert_eq!(branch_slug("DEMO-4", "  "), "demo-4");
        assert_eq!(branch_slug("", "Ünïcode only"), "n-code-only");
        assert_eq!(branch_slug("", ""), "work");
    }

    #[test]
    fn branch_slug_stops_at_a_word_within_fifty_characters() {
        let title = "a very long ticket title that goes on and on well past the limit";
        let slug = branch_slug("DEMO-12", title);
        assert_eq!(slug, "demo-12-a-very-long-ticket-title-that-goes-on-and");
        assert!(slug.len() <= MAX_SLUG);
        // One word longer than the limit is cut rather than dropped.
        let slug = branch_slug("", &"x".repeat(80));
        assert_eq!(slug.len(), MAX_SLUG);
        // A slug always passes the worktree name check.
        for s in [
            branch_slug("DEMO-12", title),
            branch_slug(&"K".repeat(70), "t"),
        ] {
            assert!(s.len() <= 64);
            assert!(!s.starts_with('-') && !s.ends_with('-'));
            assert!(s
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'));
        }
    }

    #[test]
    fn unique_branch_adds_the_first_free_suffix() {
        assert_eq!(
            unique_branch("demo-1", |_| false).as_deref(),
            Some("demo-1")
        );
        let taken = ["demo-1", "demo-1-2"];
        assert_eq!(
            unique_branch("demo-1", |b| taken.contains(&b)).as_deref(),
            Some("demo-1-3")
        );
        assert_eq!(unique_branch("demo-1", |_| true), None);
    }

    #[test]
    fn a_busy_slot_says_what_it_is_running() {
        let item = |key: &str| WorkItem {
            key: key.into(),
            kind: overseer_proto::msg::ItemKind::Ticket,
            title: String::new(),
            status: "open".into(),
            priority: None,
            parent: None,
            owner: Some(2),
            in_list: true,
            folder: None,
            stage: None,
            stage_since: None,
        };
        let running = SlotState::Running { pid: 1, since: 0 };
        let a = item("DEMO-7");
        assert_eq!(
            busy_reason(2, &running, &[&a]).as_deref(),
            Some("S2 is running DEMO-7")
        );
        assert_eq!(
            busy_reason(2, &running, &[]).as_deref(),
            Some("S2 is running")
        );
        let resumable = SlotState::Resumable {
            session_id: "s".into(),
            at: 0,
        };
        assert_eq!(
            busy_reason(2, &resumable, &[]).as_deref(),
            Some("S2 has a session to resume")
        );
        assert_eq!(busy_reason(2, &SlotState::Empty, &[&a]), None);
        assert_eq!(
            busy_reason(
                2,
                &SlotState::Exited {
                    code: Some(0),
                    at: 0
                },
                &[]
            ),
            None
        );
    }

    #[test]
    fn main_checkout_of_a_worktree_is_its_repo() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("demo-api");
        std::fs::create_dir(&repo).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&repo, &["init", "-q", "-b", "main"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=Demo",
                "-c",
                "user.email=demo@example.test",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "first",
            ],
        );
        let wt = d.path().join("demo-api-demo-1");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "demo-1",
                wt.to_str().unwrap(),
            ],
        );
        let canon = repo.canonicalize().unwrap();
        assert_eq!(main_checkout(&wt), Some(canon.clone()));
        assert_eq!(main_checkout(&repo), Some(canon));
        assert!(has_branch(&repo, "demo-1"));
        assert!(!has_branch(&repo, "demo-2"));
        assert_eq!(main_checkout(d.path()), None);
    }
}
