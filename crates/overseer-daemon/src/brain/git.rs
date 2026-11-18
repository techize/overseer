//! The vault's git runner: one async mutex, pathspec commits, push with a
//! guarded merge on rejection, and `index.lock` handling (Rulings 5, 6 and 7).

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use tokio::process::Command;
use tokio::sync::{Mutex, MutexGuard};

/// How long a missing-then-present `index.lock` is waited for.
const LOCK_WAIT: Duration = Duration::from_secs(5);
/// A lock older than this after the wait is treated as stale and removed.
const STALE_AFTER: Duration = Duration::from_secs(60);
/// Timeouts per child: network commands, then everything else.
const NET_TIMEOUT: Duration = Duration::from_secs(120);
const LOCAL_TIMEOUT: Duration = Duration::from_secs(30);
const LOCK_POLL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitError {
    /// `.git/index.lock` is held and is not yet stale.
    IndexLocked,
    /// A merge would conflict (aborted; nothing changed) or its autostash
    /// did not re-apply. Holds what to tell the user.
    Conflict(String),
    /// A merge made these files much shorter; it is committed here but not
    /// pushed until someone checks it. `before` is HEAD before the merge.
    Held {
        before: String,
        shrunk: Vec<Shrink>,
    },
    Failed(String),
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitError::IndexLocked => write!(f, "index.lock held, will retry"),
            GitError::Conflict(m) => write!(f, "merge conflict: {m}"),
            GitError::Held { before, shrunk } => write!(
                f,
                "push held: {} in the merge; the copy from before it is in {}",
                list_shrinks(shrunk),
                short(before)
            ),
            GitError::Failed(m) => write!(f, "{m}"),
        }
    }
}

/// A file a merge made much shorter: more than 20 lines and more than 30%
/// gone. A deleted file counts as 0 lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shrink {
    pub path: String,
    pub before: u32,
    pub after: u32,
}

impl fmt::Display for Shrink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} went from {} to {} lines",
            self.path, self.before, self.after
        )
    }
}

/// A commit hash cut to the 12 characters people read.
pub fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

pub fn list_shrinks(s: &[Shrink]) -> String {
    s.iter()
        .map(Shrink::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn shrank(before: u32, after: u32) -> bool {
    before.saturating_sub(after) > 20 && u64::from(after) * 10 < u64::from(before) * 7
}

/// Which side a conflicted path takes in a merge. An entry ending in `/`
/// matches every path under it; any other entry matches the whole path or
/// the file name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergePolicy {
    pub ours: Vec<String>,
    pub theirs: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Ours,
    Theirs,
}

fn policy_matches(entry: &str, path: &str) -> bool {
    if entry.is_empty() {
        return false;
    }
    if entry.ends_with('/') {
        return path.starts_with(entry);
    }
    path == entry || path.rsplit('/').next() == Some(entry)
}

impl MergePolicy {
    fn side(&self, path: &str) -> Option<Side> {
        if self.ours.iter().any(|e| policy_matches(e, path)) {
            Some(Side::Ours)
        } else if self.theirs.iter().any(|e| policy_matches(e, path)) {
            Some(Side::Theirs)
        } else {
            None
        }
    }
}

/// What a merge of the upstream did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeOutcome {
    /// HEAD before the merge: where the old copies are.
    pub before: String,
    /// Every file that differs from before the merge.
    pub merged: Vec<String>,
    /// Conflicted files that took this folder's copy, then the remote's.
    pub ours: Vec<String>,
    pub theirs: Vec<String>,
    pub shrunk: Vec<Shrink>,
}

/// What `sync` found and did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Commits ahead of and behind the upstream after the fetch.
    pub ahead: u32,
    pub behind: u32,
    /// The merges made, the sync's own first and any a rejected push needed.
    pub merges: Vec<MergeOutcome>,
    pub pushed: bool,
    /// Files the merge shrank; the push waits for `confirm`.
    pub held: Vec<Shrink>,
}

impl std::error::Error for GitError {}

/// Proof that the vault mutex is held. Callers keep it across a commit and its push.
pub struct VaultGuard<'a> {
    _g: MutexGuard<'a, ()>,
}

pub struct VaultGit {
    root: PathBuf,
    lock: Mutex<()>,
    push: bool,
    lock_wait: Duration,
    timeout: Option<Duration>,
    git_bin: PathBuf,
    removed_stale_lock: AtomicBool,
    policy: MergePolicy,
}

struct Out {
    ok: bool,
    stdout: String,
    stderr: String,
}

fn first_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("git failed")
        .to_string()
}

impl VaultGit {
    pub fn new(root: PathBuf, push: bool) -> Self {
        Self {
            root,
            lock: Mutex::new(()),
            push,
            lock_wait: LOCK_WAIT,
            timeout: None,
            git_bin: PathBuf::from("git"),
            removed_stale_lock: AtomicBool::new(false),
            policy: MergePolicy::default(),
        }
    }

    /// Which conflicted files a merge may settle by taking one side.
    pub fn with_policy(mut self, policy: MergePolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Shorten the `index.lock` wait (tests).
    pub fn with_lock_wait(mut self, wait: Duration) -> Self {
        self.lock_wait = wait;
        self
    }

    /// Override the per-command timeout (tests).
    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = Some(t);
        self
    }

    /// Use another git executable (tests).
    pub fn with_git_bin(mut self, bin: PathBuf) -> Self {
        self.git_bin = bin;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub async fn lock(&self) -> VaultGuard<'_> {
        VaultGuard {
            _g: self.lock.lock().await,
        }
    }

    /// True once if a stale `index.lock` was removed since the last call, so the
    /// caller can raise a notice. Cleared by reading.
    pub fn take_removed_stale_lock(&self, _g: &VaultGuard<'_>) -> bool {
        self.removed_stale_lock.swap(false, Ordering::SeqCst)
    }

    async fn exec(&self, args: &[&str]) -> Result<Out, GitError> {
        let limit = self.timeout.unwrap_or(match args.first() {
            Some(&"push") | Some(&"pull") | Some(&"fetch") => NET_TIMEOUT,
            _ => LOCAL_TIMEOUT,
        });
        let mut cmd = Command::new(&self.git_bin);
        cmd.args(args)
            .current_dir(&self.root)
            .env("OVERSEER_COMMIT", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            // Paths are file names, never patterns.
            .env("GIT_LITERAL_PATHSPECS", "1")
            // Untranslated messages: push rejection is detected from stderr text.
            .env("LC_ALL", "C")
            .env("LANG", "C")
            .stdin(Stdio::null())
            .kill_on_drop(true);
        if std::env::var_os("GIT_SSH_COMMAND").is_none() {
            cmd.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
        }
        let out = match tokio::time::timeout(limit, cmd.output()).await {
            Ok(r) => r.map_err(|e| GitError::Failed(format!("could not run git: {e}")))?,
            Err(_) => {
                return Err(GitError::Failed(format!(
                    "git {} timed out",
                    args.first().copied().unwrap_or("")
                )))
            }
        };
        Ok(Out {
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    /// Run git with the vault as cwd and `OVERSEER_COMMIT=1`; returns stdout.
    pub async fn run(&self, _g: &VaultGuard<'_>, args: &[&str]) -> Result<String, GitError> {
        self.run_inner(args).await
    }

    /// Whether the checked-out branch has an upstream.
    pub async fn has_upstream(&self, _g: &VaultGuard<'_>) -> bool {
        self.run_inner(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"])
            .await
            .is_ok()
    }

    /// `git fetch --quiet` of the upstream's remote.
    pub async fn fetch(&self, _g: &VaultGuard<'_>) -> Result<(), GitError> {
        self.run_inner(&["fetch", "--quiet"]).await.map(|_| ())
    }

    /// Commits ahead of and behind the upstream, from the refs as they are
    /// (no fetch).
    pub async fn ahead_behind(&self, _g: &VaultGuard<'_>) -> Result<(u32, u32), GitError> {
        let out = self
            .run_inner(&["rev-list", "--left-right", "--count", "HEAD...@{u}"])
            .await?;
        parse_counts(&out)
            .ok_or_else(|| GitError::Failed(format!("unexpected rev-list output: {}", out.trim())))
    }

    async fn run_inner(&self, args: &[&str]) -> Result<String, GitError> {
        let o = self.exec(args).await?;
        if o.ok {
            Ok(o.stdout)
        } else if o.stderr.contains("index.lock") {
            Err(GitError::IndexLocked)
        } else {
            Err(GitError::Failed(first_line(&o.stderr)))
        }
    }

    fn index_lock(&self) -> PathBuf {
        self.root.join(".git").join("index.lock")
    }

    /// Ruling 7: wait for `index.lock` to go; remove it if it is still there and
    /// older than 60 s; else `IndexLocked`.
    async fn clear_index_lock(&self) -> Result<(), GitError> {
        let lock = self.index_lock();
        let deadline = tokio::time::Instant::now() + self.lock_wait;
        while lock.exists() {
            if tokio::time::Instant::now() >= deadline {
                let age = std::fs::metadata(&lock)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| SystemTime::now().duration_since(t).ok());
                return match age {
                    Some(a) if a > STALE_AFTER => {
                        match std::fs::remove_file(&lock) {
                            Ok(()) => {}
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                            Err(e) => {
                                return Err(GitError::Failed(format!(
                                    "could not remove stale index.lock: {e}"
                                )))
                            }
                        }
                        self.removed_stale_lock.store(true, Ordering::SeqCst);
                        Ok(())
                    }
                    _ => Err(GitError::IndexLocked),
                };
            }
            tokio::time::sleep(LOCK_POLL).await;
        }
        Ok(())
    }

    /// Stage and commit only `paths`. Returns the new commit's hash, or `None`
    /// when those paths have nothing to commit.
    pub async fn commit_paths(
        &self,
        _g: &VaultGuard<'_>,
        paths: &[String],
        message: &str,
    ) -> Result<Option<String>, GitError> {
        if paths.is_empty() {
            return Ok(None);
        }
        self.clear_index_lock().await?;
        // The paths go through a NUL-separated file: a first backup of a
        // large vault would not fit on the command line.
        let list = self.root.join(".git").join("overseer-pathspec");
        let mut body = Vec::new();
        for p in paths {
            body.extend_from_slice(p.as_bytes());
            body.push(0);
        }
        std::fs::write(&list, body)
            .map_err(|e| GitError::Failed(format!("cannot write the path list: {e}")))?;
        let from = format!("--pathspec-from-file={}", list.display());
        let result = self.commit_listed(&from, paths, message).await;
        let _ = std::fs::remove_file(&list);
        result
    }

    async fn commit_listed(
        &self,
        from: &str,
        paths: &[String],
        message: &str,
    ) -> Result<Option<String>, GitError> {
        self.run_inner(&["add", "-A", from, "--pathspec-file-nul"])
            .await?;
        // `git diff` takes no path file: list everything staged and match.
        let staged = self
            .run_inner(&["diff", "--cached", "--name-only", "--no-renames", "-z"])
            .await?;
        let ours = staged.split('\0').any(|s| {
            !s.is_empty()
                && paths.iter().any(|p| {
                    s == p
                        || s.strip_prefix(p.as_str())
                            .is_some_and(|rest| rest.starts_with('/'))
                })
        });
        if !ours {
            return Ok(None);
        }
        self.run_inner(&["commit", "-m", message, from, "--pathspec-file-nul"])
            .await?;
        let head = self.run_inner(&["rev-parse", "HEAD"]).await?;
        Ok(Some(head.trim().to_string()))
    }

    fn rebase_in_progress(&self) -> bool {
        let git = self.root.join(".git");
        git.join("rebase-merge").exists() || git.join("rebase-apply").exists()
    }

    fn merge_in_progress(&self) -> bool {
        self.root.join(".git").join("MERGE_HEAD").exists()
    }

    /// `git push`; when rejected, fetch, merge the upstream (`merge_upstream`)
    /// and push again, twice at most. A merge that would conflict is aborted
    /// and reported as `Conflict`; one that shrank a file is `Held`. A no-op
    /// when `push` is off.
    pub async fn push(&self, _g: &VaultGuard<'_>) -> Result<(), GitError> {
        if !self.push {
            return Ok(());
        }
        self.push_merging().await.map(|_| ())
    }

    async fn push_merging(&self) -> Result<Vec<MergeOutcome>, GitError> {
        let mut merges = Vec::new();
        for attempt in 0..3 {
            let o = self.exec(&["push"]).await?;
            if o.ok {
                return Ok(merges);
            }
            let rejected = o.stderr.contains("rejected")
                || o.stderr.contains("non-fast-forward")
                || o.stderr.contains("fetch first");
            if !rejected || attempt == 2 {
                return Err(GitError::Failed(first_line(&o.stderr)));
            }
            self.run_inner(&["fetch", "--quiet"]).await?;
            let m = self.merge_upstream().await?;
            if !m.shrunk.is_empty() {
                return Err(GitError::Held {
                    before: m.before,
                    shrunk: m.shrunk,
                });
            }
            merges.push(m);
        }
        unreachable!("the last attempt returns")
    }

    /// Fetch, merge the upstream when behind and push when ahead. Refuses
    /// while a merge or rebase is under way or files are unmerged. A merge
    /// that shrank a file is kept here and not pushed unless `confirm`.
    pub async fn sync(&self, _g: &VaultGuard<'_>, confirm: bool) -> Result<SyncReport, GitError> {
        if self.merge_in_progress() || self.rebase_in_progress() {
            return Err(GitError::Failed(
                "a merge or rebase is under way in the notes folder; finish or abort it first"
                    .into(),
            ));
        }
        let unmerged = self.unmerged_inner().await?;
        if !unmerged.is_empty() {
            return Err(GitError::Failed(format!(
                "unresolved conflicts in {}; resolve them first (git status)",
                unmerged.join(", ")
            )));
        }
        if self
            .run_inner(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"])
            .await
            .is_err()
        {
            return Err(GitError::Failed(
                "the notes folder's branch has no upstream to sync with".into(),
            ));
        }
        self.run_inner(&["fetch", "--quiet"]).await?;
        let counts = self
            .run_inner(&["rev-list", "--left-right", "--count", "HEAD...@{u}"])
            .await?;
        let (ahead, behind) = parse_counts(&counts).ok_or_else(|| {
            GitError::Failed(format!("unexpected rev-list output: {}", counts.trim()))
        })?;
        let mut report = SyncReport {
            ahead,
            behind,
            ..SyncReport::default()
        };
        if behind > 0 {
            let m = self.merge_upstream().await?;
            let shrunk = m.shrunk.clone();
            report.merges.push(m);
            if !shrunk.is_empty() && !confirm {
                report.held = shrunk;
                return Ok(report);
            }
        }
        if !self.push {
            return Ok(report);
        }
        let now = self
            .run_inner(&["rev-list", "--count", "@{u}..HEAD"])
            .await?;
        if now.trim() != "0" {
            report.merges.extend(self.push_merging().await?);
            report.pushed = true;
        }
        Ok(report)
    }

    /// Merges `@{u}` (already fetched) into HEAD. Skip-worktree files are
    /// set aside first and put back after, whatever happened. A conflict in
    /// a file the policy names takes that side; any other conflict aborts
    /// the merge, leaving HEAD and the files as they were.
    async fn merge_upstream(&self) -> Result<MergeOutcome, GitError> {
        let pre = self.run_inner(&["rev-parse", "HEAD"]).await?;
        let pre = pre.trim().to_string();
        let skipped = self.unskip().await?;
        let merged = self.merge_inner().await;
        let back = self.reskip(&skipped).await;
        let (ours, theirs) = merged?;
        back?;
        let names = self
            .run_inner(&["diff", "--name-only", "-z", "--no-renames", &pre, "HEAD"])
            .await?;
        let shrunk = self.shrunk_since(&pre).await?;
        Ok(MergeOutcome {
            before: pre,
            merged: names
                .split('\0')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            ours,
            theirs,
            shrunk,
        })
    }

    /// Skip-worktree paths with their contents (`None` when the file is
    /// absent), after clearing the flag and checking each out: a merge
    /// refuses to touch them otherwise.
    async fn unskip(&self) -> Result<Vec<(String, Option<Vec<u8>>)>, GitError> {
        let out = self.run_inner(&["ls-files", "-v", "-z"]).await?;
        let paths: Vec<String> = out
            .split('\0')
            .filter_map(|e| e.strip_prefix("S "))
            .map(str::to_string)
            .collect();
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let saved: Vec<(String, Option<Vec<u8>>)> = paths
            .iter()
            .map(|p| (p.clone(), std::fs::read(self.root.join(p)).ok()))
            .collect();
        let mut args = vec!["update-index", "--no-skip-worktree", "--"];
        args.extend(paths.iter().map(String::as_str));
        if let Err(e) = self.run_inner(&args).await {
            // Leave nothing half done: set the flag on every path again.
            let _ = self.reskip(&saved).await;
            return Err(e);
        }
        let mut args = vec!["checkout", "--"];
        args.extend(paths.iter().map(String::as_str));
        if let Err(e) = self.run_inner(&args).await {
            let _ = self.reskip(&saved).await;
            return Err(e);
        }
        Ok(saved)
    }

    /// Puts back what `unskip` set aside and sets the flag on each path
    /// still tracked.
    async fn reskip(&self, saved: &[(String, Option<Vec<u8>>)]) -> Result<(), GitError> {
        if saved.is_empty() {
            return Ok(());
        }
        let mut failed = None;
        for (p, body) in saved {
            let file = self.root.join(p);
            let r = match body {
                Some(b) => file
                    .parent()
                    .map_or(Ok(()), std::fs::create_dir_all)
                    .and_then(|()| std::fs::write(&file, b)),
                None => match std::fs::remove_file(&file) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                    _ => Ok(()),
                },
            };
            if let Err(e) = r {
                failed.get_or_insert(format!("cannot put back {p}: {e}"));
            }
        }
        let mut args = vec!["ls-files", "-z", "--"];
        args.extend(saved.iter().map(|(p, _)| p.as_str()));
        let tracked = self.run_inner(&args).await?;
        let tracked: Vec<&str> = tracked.split('\0').filter(|s| !s.is_empty()).collect();
        if !tracked.is_empty() {
            let mut args = vec!["update-index", "--skip-worktree", "--"];
            args.extend(tracked);
            self.run_inner(&args).await?;
        }
        match failed {
            Some(m) => Err(GitError::Failed(m)),
            None => Ok(()),
        }
    }

    /// `merge --no-edit --autostash @{u}`, settling conflicts by policy.
    /// Returns the paths that took ours, then theirs.
    async fn merge_inner(&self) -> Result<(Vec<String>, Vec<String>), GitError> {
        let m = self
            .exec(&["merge", "--no-edit", "--autostash", "@{u}"])
            .await?;
        let mut said = format!("{}{}", m.stdout, m.stderr);
        let mut ours = Vec::new();
        let mut theirs = Vec::new();
        if !m.ok {
            if !self.merge_in_progress() {
                let msg = if m.stderr.trim().is_empty() {
                    first_line(&m.stdout)
                } else {
                    first_line(&m.stderr)
                };
                return Err(GitError::Failed(msg));
            }
            let unmerged = self.unmerged_inner().await?;
            let mut unlisted = Vec::new();
            for p in unmerged {
                match self.policy.side(&p) {
                    Some(Side::Ours) => ours.push(p),
                    Some(Side::Theirs) => theirs.push(p),
                    None => unlisted.push(p),
                }
            }
            if !unlisted.is_empty() || (ours.is_empty() && theirs.is_empty()) {
                self.abort_merge().await?;
                let what = if unlisted.is_empty() {
                    first_line(&m.stderr)
                } else {
                    unlisted.join(", ")
                };
                return Err(GitError::Conflict(format!(
                    "merge would conflict in {what}; nothing was changed"
                )));
            }
            for (side, paths) in [("--ours", &ours), ("--theirs", &theirs)] {
                for p in paths.iter() {
                    if let Err(e) = self.take_side(side, p).await {
                        self.abort_merge().await?;
                        return Err(e);
                    }
                }
            }
            let c = self.exec(&["commit", "--no-edit"]).await?;
            said.push_str(&c.stdout);
            said.push_str(&c.stderr);
            if !c.ok {
                let why = first_line(&c.stderr);
                self.abort_merge().await?;
                return Err(GitError::Failed(format!("the merge commit failed: {why}")));
            }
        }
        // The autostash can fail to re-apply while git still exits 0. The
        // stash stack is shared, so the stash is named by its hash.
        let unmerged = self.unmerged_inner().await?;
        if !unmerged.is_empty() || said.contains("Applying autostash resulted in conflicts") {
            let stash = match self.autostash_sha(&said).await {
                Some(sha) => format!("stash {sha}"),
                None => "the stash (git stash list)".to_string(),
            };
            return Err(GitError::Conflict(format!(
                "autostash did not re-apply; your changes are safe in {stash}"
            )));
        }
        Ok((ours, theirs))
    }

    /// Resolves one conflicted path to `side`; when that side deleted it,
    /// removes it.
    async fn take_side(&self, side: &str, p: &str) -> Result<(), GitError> {
        if self.run_inner(&["checkout", side, "--", p]).await.is_ok() {
            self.run_inner(&["add", "--", p]).await.map(|_| ())
        } else {
            self.run_inner(&["rm", "-q", "--", p]).await.map(|_| ())
        }
    }

    async fn abort_merge(&self) -> Result<(), GitError> {
        let abort = self.exec(&["merge", "--abort"]).await;
        if matches!(&abort, Ok(o) if o.ok) && !self.merge_in_progress() {
            return Ok(());
        }
        let why = match abort {
            Ok(o) => first_line(&o.stderr),
            Err(e) => e.to_string(),
        };
        Err(GitError::Failed(format!("merge --abort failed: {why}")))
    }

    /// Files HEAD has much shorter than `pre` had them.
    async fn shrunk_since(&self, pre: &str) -> Result<Vec<Shrink>, GitError> {
        let out = self
            .run_inner(&["diff", "--numstat", "-z", "--no-renames", pre, "HEAD"])
            .await?;
        let mut found = Vec::new();
        for entry in out.split('\0').filter(|s| !s.is_empty()) {
            // `<added>\t<deleted>\t<path>`; binary files show `-`.
            let mut it = entry.splitn(3, '\t');
            let (Some(Ok(added)), Some(Ok(deleted)), Some(path)) = (
                it.next().map(str::parse::<u32>),
                it.next().map(str::parse::<u32>),
                it.next(),
            ) else {
                continue;
            };
            if deleted <= added.saturating_add(20) {
                continue;
            }
            let before = self.line_count(pre, path).await;
            let after = self.line_count("HEAD", path).await;
            if shrank(before, after) {
                found.push(Shrink {
                    path: path.to_string(),
                    before,
                    after,
                });
            }
        }
        Ok(found)
    }

    async fn line_count(&self, rev: &str, path: &str) -> u32 {
        let spec = format!("{rev}:{path}");
        match self.run_inner(&["show", &spec]).await {
            Ok(body) => u32::try_from(body.lines().count()).unwrap_or(u32::MAX),
            Err(_) => 0,
        }
    }

    /// Writes `path` as it was at `rev` (a commit), in the folder and the
    /// index. The caller commits it.
    pub async fn restore_file(
        &self,
        _g: &VaultGuard<'_>,
        path: &str,
        rev: &str,
    ) -> Result<String, GitError> {
        if rev.is_empty() || rev.starts_with('-') {
            return Err(GitError::Failed(format!("not a revision: {rev}")));
        }
        let spec = format!("{rev}^{{commit}}");
        let sha = self
            .run_inner(&["rev-parse", "-q", "--verify", &spec])
            .await
            .map_err(|_| GitError::Failed(format!("no commit {rev} in the notes folder")))?;
        let sha = sha.trim().to_string();
        let blob = format!("{sha}:{path}");
        if self.run_inner(&["cat-file", "-e", &blob]).await.is_err() {
            return Err(GitError::Failed(format!("{path} is not in {rev}")));
        }
        self.clear_index_lock().await?;
        self.run_inner(&["checkout", &sha, "--", path]).await?;
        Ok(sha)
    }

    /// The full hash of the stash `merge --autostash` left behind: from its
    /// `Created autostash: <sha>` line, else the newest stash entry git
    /// stored as `autostash`.
    async fn autostash_sha(&self, said: &str) -> Option<String> {
        let short = said
            .lines()
            .find_map(|l| l.trim().strip_prefix("Created autostash: "))
            .map(str::trim)
            .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit()));
        if let Some(short) = short {
            let spec = format!("{short}^{{commit}}");
            if let Ok(full) = self
                .run_inner(&["rev-parse", "-q", "--verify", &spec])
                .await
            {
                let full = full.trim();
                if !full.is_empty() {
                    return Some(full.to_string());
                }
            }
        }
        let list = self
            .run_inner(&["log", "-g", "--format=%H %gs", "refs/stash", "--"])
            .await
            .ok()?;
        list.lines().find_map(|l| {
            let (sha, subject) = l.split_once(' ')?;
            (subject.trim() == "autostash").then(|| sha.to_string())
        })
    }

    /// Paths with unmerged index entries (`git ls-files -u -z`), each once.
    /// While any exist, nothing the daemon commits may include them: a
    /// conflicted file holds conflict markers.
    pub async fn unmerged(&self, _g: &VaultGuard<'_>) -> Result<Vec<String>, GitError> {
        self.unmerged_inner().await
    }

    async fn unmerged_inner(&self) -> Result<Vec<String>, GitError> {
        let out = self.run_inner(&["ls-files", "-u", "-z"]).await?;
        let mut paths: Vec<String> = Vec::new();
        for entry in out.split('\0').filter(|s| !s.is_empty()) {
            // `<mode> <sha> <stage>\t<path>`
            if let Some((_, path)) = entry.split_once('\t') {
                if !paths.iter().any(|p| p == path) {
                    paths.push(path.to_string());
                }
            }
        }
        Ok(paths)
    }

    /// Every path `git status` reports as changed; renames list both paths.
    pub async fn changed_paths(&self, _g: &VaultGuard<'_>) -> Result<Vec<String>, GitError> {
        let out = self
            .run_inner(&["status", "--porcelain", "-z", "--untracked-files=all"])
            .await?;
        let mut parts = out.split('\0').filter(|s| !s.is_empty());
        let mut paths = Vec::new();
        while let Some(entry) = parts.next() {
            if entry.len() < 4 {
                continue;
            }
            let (xy, path) = entry.split_at(3);
            paths.push(path.to_string());
            if xy.starts_with('R') || xy.starts_with('C') || xy[1..2].eq("R") || xy[1..2].eq("C") {
                if let Some(orig) = parts.next() {
                    paths.push(orig.to_string());
                }
            }
        }
        Ok(paths)
    }
}

/// `rev-list --left-right --count` output: `<ahead>\t<behind>`.
fn parse_counts(out: &str) -> Option<(u32, u32)> {
    let mut it = out.split_whitespace().map(str::parse::<u32>);
    match (it.next(), it.next(), it.next()) {
        (Some(Ok(a)), Some(Ok(b)), None) => Some((a, b)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn sh(dir: &Path, args: &[&str]) -> String {
        let o = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    fn configure(dir: &Path) {
        sh(dir, &["config", "user.name", "Test"]);
        sh(dir, &["config", "user.email", "test@example.com"]);
        sh(dir, &["config", "commit.gpgsign", "false"]);
        sh(dir, &["config", "pull.rebase", "false"]);
    }

    struct TestVault {
        base: PathBuf,
        vault: PathBuf,
        remote: PathBuf,
        git: VaultGit,
    }

    impl TestVault {
        fn new(tag: &str) -> Self {
            let base =
                std::env::temp_dir().join(format!("ov-brain-git-{tag}-{}", uuid::Uuid::new_v4()));
            let remote = base.join("remote.git");
            let vault = base.join("vault");
            std::fs::create_dir_all(&remote).unwrap();
            std::fs::create_dir_all(&vault).unwrap();
            sh(&remote, &["init", "--bare", "-b", "main"]);
            sh(&vault, &["init", "-b", "main"]);
            configure(&vault);
            sh(
                &vault,
                &["remote", "add", "origin", remote.to_str().unwrap()],
            );
            std::fs::write(vault.join("old.md"), "old\n").unwrap();
            std::fs::write(vault.join("jonathan.md"), "line one\n").unwrap();
            std::fs::write(vault.join("a.md"), "a\n").unwrap();
            sh(&vault, &["add", "-A"]);
            sh(&vault, &["commit", "-m", "initial"]);
            sh(&vault, &["push", "-u", "origin", "main"]);
            let git = VaultGit::new(vault.clone(), true).with_lock_wait(Duration::from_millis(300));
            Self {
                base,
                vault,
                remote,
                git,
            }
        }

        fn clone_other(&self) -> PathBuf {
            let other = self.base.join("other");
            sh(
                &self.base,
                &["clone", self.remote.to_str().unwrap(), "other"],
            );
            configure(&other);
            other
        }
    }

    impl Drop for TestVault {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    #[tokio::test]
    async fn commit_takes_only_its_paths() {
        let v = TestVault::new("only");
        std::fs::write(v.vault.join("a.md"), "a2\n").unwrap();
        std::fs::write(v.vault.join("b.md"), "b\n").unwrap();
        std::fs::remove_file(v.vault.join("old.md")).unwrap();
        std::fs::write(v.vault.join("jonathan.md"), "line one\nmine\n").unwrap();
        let g = v.git.lock().await;
        let sha = v
            .git
            .commit_paths(&g, &["a.md".into(), "old.md".into()], "S1: notes")
            .await
            .unwrap();
        assert!(sha.is_some());
        let shown = sh(&v.vault, &["show", "--name-status", "--format=", "HEAD"]);
        let mut lines: Vec<&str> = shown.lines().collect();
        lines.sort();
        assert_eq!(lines, vec!["D\told.md", "M\ta.md"]);
        let changed = v.git.changed_paths(&g).await.unwrap();
        assert!(changed.contains(&"b.md".to_string()), "{changed:?}");
        assert!(changed.contains(&"jonathan.md".to_string()), "{changed:?}");
        assert_eq!(changed.len(), 2, "{changed:?}");
    }

    #[tokio::test]
    async fn two_concurrent_commits_do_not_mix() {
        let v = TestVault::new("conc");
        let git = Arc::new(VaultGit::new(v.vault.clone(), false));
        std::fs::write(v.vault.join("s1.md"), "1\n").unwrap();
        std::fs::write(v.vault.join("s2.md"), "2\n").unwrap();
        let mut tasks = Vec::new();
        for f in ["s1.md", "s2.md"] {
            let git = git.clone();
            tasks.push(tokio::spawn(async move {
                let g = git.lock().await;
                git.commit_paths(&g, &[f.to_string()], &format!("commit {f}"))
                    .await
                    .unwrap()
            }));
        }
        for t in tasks {
            assert!(t.await.unwrap().is_some());
        }
        let log = sh(&v.vault, &["log", "--format=%s", "-3"]);
        assert_eq!(log.lines().count(), 3);
        for f in ["s1.md", "s2.md"] {
            let subject = format!("commit {f}");
            let sha = sh(&v.vault, &["log", "--format=%H", "--grep", &subject, "-1"]);
            let files = sh(&v.vault, &["show", "--name-only", "--format=", sha.trim()]);
            assert_eq!(files.trim(), f);
        }
    }

    #[tokio::test]
    async fn unchanged_paths_are_nothing_to_commit() {
        let v = TestVault::new("nothing");
        let g = v.git.lock().await;
        let r = v
            .git
            .commit_paths(&g, &["a.md".into()], "S1: nothing")
            .await;
        assert_eq!(r, Ok(None));
    }

    #[tokio::test]
    async fn ahead_and_behind_are_counted_after_a_fetch() {
        let v = TestVault::new("counts");
        let g = v.git.lock().await;
        assert!(v.git.has_upstream(&g).await);
        assert_eq!(v.git.ahead_behind(&g).await.unwrap(), (0, 0));
        let other = v.clone_other();
        std::fs::write(other.join("other.md"), "o\n").unwrap();
        sh(&other, &["add", "-A"]);
        sh(&other, &["commit", "-m", "other"]);
        sh(&other, &["push"]);
        std::fs::write(v.vault.join("mine.md"), "m\n").unwrap();
        v.git
            .commit_paths(&g, &["mine.md".into()], "S1: mine")
            .await
            .unwrap();
        // Before a fetch the remote's commit is not known here.
        assert_eq!(v.git.ahead_behind(&g).await.unwrap(), (1, 0));
        v.git.fetch(&g).await.unwrap();
        assert_eq!(v.git.ahead_behind(&g).await.unwrap(), (1, 1));
    }

    #[test]
    fn rev_list_counts_parse() {
        assert_eq!(parse_counts("65\t4\n"), Some((65, 4)));
        assert_eq!(parse_counts("1"), None);
        assert_eq!(parse_counts("a\tb"), None);
    }

    #[tokio::test]
    async fn rejected_push_merges_and_pushes() {
        let v = TestVault::new("reject");
        let other = v.clone_other();
        std::fs::write(other.join("other.md"), "o\n").unwrap();
        sh(&other, &["add", "-A"]);
        sh(&other, &["commit", "-m", "other"]);
        sh(&other, &["push"]);

        std::fs::write(v.vault.join("mine.md"), "m\n").unwrap();
        let g = v.git.lock().await;
        v.git
            .commit_paths(&g, &["mine.md".into()], "S1: mine")
            .await
            .unwrap();
        v.git.push(&g).await.unwrap();
        let log = sh(&v.remote, &["log", "--format=%s", "main"]);
        let subjects: Vec<&str> = log.lines().collect();
        assert!(
            subjects.contains(&"other") && subjects.contains(&"S1: mine"),
            "{subjects:?}"
        );
    }

    #[tokio::test]
    async fn conflicting_push_keeps_the_local_commit() {
        let v = TestVault::new("conflict");
        let other = v.clone_other();
        std::fs::write(other.join("jonathan.md"), "theirs\n").unwrap();
        sh(&other, &["add", "-A"]);
        sh(&other, &["commit", "-m", "theirs"]);
        sh(&other, &["push"]);

        std::fs::write(v.vault.join("jonathan.md"), "ours\n").unwrap();
        let g = v.git.lock().await;
        v.git
            .commit_paths(&g, &["jonathan.md".into()], "S1: ours")
            .await
            .unwrap();
        let head = sh(&v.vault, &["rev-parse", "HEAD"]);
        std::fs::write(v.vault.join("a.md"), "dirty\n").unwrap();

        let err = v.git.push(&g).await.unwrap_err();
        match &err {
            GitError::Conflict(m) => assert_eq!(
                m, "merge would conflict in jonathan.md; nothing was changed",
                "{err:?}"
            ),
            e => panic!("{e:?}"),
        }
        assert!(!v.vault.join(".git/MERGE_HEAD").exists());
        assert!(v.git.unmerged(&g).await.unwrap().is_empty());
        assert_eq!(sh(&v.vault, &["rev-parse", "HEAD"]), head);
        assert_eq!(
            std::fs::read_to_string(v.vault.join("a.md")).unwrap(),
            "dirty\n"
        );
        assert_eq!(
            std::fs::read_to_string(v.vault.join("jonathan.md")).unwrap(),
            "ours\n"
        );
    }

    #[tokio::test]
    async fn fresh_index_lock_fails_then_stale_is_removed() {
        let v = TestVault::new("lock");
        let lock = v.vault.join(".git/index.lock");
        std::fs::write(&lock, "").unwrap();
        std::fs::write(v.vault.join("a.md"), "a2\n").unwrap();
        let git = VaultGit::new(v.vault.clone(), false); // real 5 s wait
        let g = git.lock().await;
        let t = std::time::Instant::now();
        let r = git.commit_paths(&g, &["a.md".into()], "S1: a").await;
        assert_eq!(r, Err(GitError::IndexLocked));
        let waited = t.elapsed();
        assert!(
            waited >= Duration::from_secs(4) && waited < Duration::from_secs(8),
            "{waited:?}"
        );
        assert!(lock.exists());
        assert!(!git.take_removed_stale_lock(&g));

        let old = SystemTime::now() - Duration::from_secs(120);
        std::fs::File::options()
            .write(true)
            .open(&lock)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let r = v.git.commit_paths(&g, &["a.md".into()], "S1: a").await;
        assert!(r.unwrap().is_some());
        assert!(!lock.exists());
        assert!(v.git.take_removed_stale_lock(&g));
        assert!(!v.git.take_removed_stale_lock(&g));
    }

    #[tokio::test]
    async fn post_commit_hook_sees_overseer_commit() {
        let v = TestVault::new("hook");
        let hook = v.vault.join(".git/hooks/post-commit");
        let out = v.base.join("hook-out.txt");
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\nprintf '%s' \"$OVERSEER_COMMIT\" > '{}'\n",
                out.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(v.vault.join("a.md"), "a2\n").unwrap();
        let g = v.git.lock().await;
        v.git
            .commit_paths(&g, &["a.md".into()], "S1: a")
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "1");
    }

    #[tokio::test]
    async fn autostash_conflict_is_reported_and_text_kept() {
        let v = TestVault::new("autostash");
        let other = v.clone_other();
        std::fs::write(other.join("jonathan.md"), "theirs\n").unwrap();
        sh(&other, &["add", "-A"]);
        sh(&other, &["commit", "-m", "theirs"]);
        sh(&other, &["push"]);
        std::fs::write(v.vault.join("jonathan.md"), "line one\nmine\n").unwrap();
        std::fs::write(v.vault.join("a.md"), "a2\n").unwrap();
        let g = v.git.lock().await;
        v.git
            .commit_paths(&g, &["a.md".into()], "S1: a")
            .await
            .unwrap();
        let err = v.git.push(&g).await.unwrap_err();
        match &err {
            GitError::Conflict(m) => assert!(m.contains("safe in stash "), "{m}"),
            e => panic!("{e:?}"),
        }
        let sha = match &err {
            GitError::Conflict(m) => m.rsplit(' ').next().unwrap().to_string(),
            _ => unreachable!(),
        };
        assert_eq!(sha.len(), 40, "named by its full hash: {err:?}");
        let stash = sh(&v.vault, &["stash", "show", "-p", &sha]);
        assert!(stash.contains("mine"), "{stash}");
        assert_eq!(
            v.git.unmerged(&g).await.unwrap(),
            vec!["jonathan.md".to_string()]
        );
        let remote = sh(&v.remote, &["log", "--format=%s", "main"]);
        assert!(!remote.contains("S1: a"));
    }

    #[tokio::test]
    async fn long_path_lists_fit_the_command_line() {
        let v = TestVault::new("many");
        // About 1.3 MB of path names: more than ARG_MAX on macOS.
        let dir = v.vault.join("many");
        std::fs::create_dir_all(&dir).unwrap();
        let stem = "n".repeat(200);
        let paths: Vec<String> = (0..6000)
            .map(|i| {
                let name = format!("{stem}-{i}.md");
                std::fs::write(dir.join(&name), "x\n").unwrap();
                format!("many/{name}")
            })
            .collect();
        std::fs::write(v.vault.join("b.md"), "b\n").unwrap();
        let g = v.git.lock().await;
        let sha = v
            .git
            .commit_paths(&g, &paths, "vault backup")
            .await
            .unwrap();
        assert!(sha.is_some());
        let shown = sh(&v.vault, &["show", "--name-only", "--format=", "HEAD"]);
        assert_eq!(shown.lines().count(), 6000);
        assert!(!shown.contains("b.md"));
        let again = v.git.commit_paths(&g, &paths, "vault backup").await;
        assert_eq!(again, Ok(None));
    }

    #[tokio::test]
    async fn pathspecs_are_literal() {
        let v = TestVault::new("literal");
        std::fs::write(v.vault.join("n[1].md"), "x\n").unwrap();
        std::fs::write(v.vault.join("n1.md"), "y\n").unwrap();
        let g = v.git.lock().await;
        v.git
            .commit_paths(&g, &["n[1].md".into()], "S1: n")
            .await
            .unwrap();
        let shown = sh(&v.vault, &["show", "--name-only", "--format=", "HEAD"]);
        assert_eq!(shown.trim(), "n[1].md");
        assert!(v
            .git
            .changed_paths(&g)
            .await
            .unwrap()
            .contains(&"n1.md".to_string()));
        assert!(sh(&v.vault, &["ls-files", "--others"]).contains("n1.md"));
    }

    #[tokio::test]
    async fn failed_abort_is_failed_not_conflict() {
        let v = TestVault::new("abort");
        let other = v.clone_other();
        std::fs::write(other.join("jonathan.md"), "theirs\n").unwrap();
        sh(&other, &["add", "-A"]);
        sh(&other, &["commit", "-m", "theirs"]);
        sh(&other, &["push"]);
        std::fs::write(v.vault.join("jonathan.md"), "ours\n").unwrap();
        let g = v.git.lock().await;
        v.git
            .commit_paths(&g, &["jonathan.md".into()], "S1: ours")
            .await
            .unwrap();
        let wrapper = v.base.join("git-wrap.sh");
        std::fs::write(
            &wrapper,
            "#!/bin/sh\nif [ \"$1\" = merge ] && [ \"$2\" = --abort ]; then echo 'abort refused' >&2; exit 1; fi\nexec git \"$@\"\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let git = VaultGit::new(v.vault.clone(), true).with_git_bin(wrapper);
        drop(g);
        let g = git.lock().await;
        let r = git.push(&g).await;
        match r {
            Err(GitError::Failed(m)) => assert!(m.contains("merge --abort failed"), "{m}"),
            e => panic!("{e:?}"),
        }
    }

    /// Commits `files` in a fresh clone and pushes them.
    fn push_from_other(v: &TestVault, files: &[(&str, &str)]) -> PathBuf {
        let other = v.base.join("other");
        if !other.exists() {
            v.clone_other();
        } else {
            sh(&other, &["pull", "--no-rebase", "-q"]);
        }
        for (f, body) in files {
            let path = other.join(f);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        sh(&other, &["add", "-A"]);
        sh(&other, &["commit", "-qm", "theirs"]);
        sh(&other, &["push", "-q"]);
        other
    }

    fn lines(n: usize) -> String {
        (0..n).map(|i| format!("line {i}\n")).collect()
    }

    #[test]
    fn policy_entries_match_folders_paths_and_names() {
        let p = MergePolicy {
            ours: vec!["06-Daily/".into(), "CURRENT_SESSION.md".into()],
            theirs: vec!["Sprints/board.md".into()],
        };
        assert_eq!(p.side("06-Daily/2026/a.md"), Some(Side::Ours));
        assert_eq!(p.side("06-Daily"), None);
        assert_eq!(p.side("CURRENT_SESSION.md"), Some(Side::Ours));
        assert_eq!(p.side("x/CURRENT_SESSION.md"), Some(Side::Ours));
        assert_eq!(p.side("Sprints/board.md"), Some(Side::Theirs));
        assert_eq!(p.side("board.md"), None);
        assert_eq!(p.side("notes.md"), None);
        assert!(shrank(40, 3));
        assert!(!shrank(40, 30), "under 30% gone");
        assert!(shrank(25, 0), "a deleted file counts as 0 lines");
        assert!(!shrank(20, 0), "20 lines or fewer gone");
    }

    #[tokio::test]
    async fn conflicts_the_policy_names_take_their_side() {
        let v = TestVault::new("policy");
        push_from_other(
            &v,
            &[("06-Daily/today.md", "theirs\n"), ("board.md", "theirs\n")],
        );
        std::fs::create_dir_all(v.vault.join("06-Daily")).unwrap();
        std::fs::write(v.vault.join("06-Daily/today.md"), "ours\n").unwrap();
        std::fs::write(v.vault.join("board.md"), "ours\n").unwrap();
        let git = VaultGit::new(v.vault.clone(), true).with_policy(MergePolicy {
            ours: vec!["06-Daily/".into()],
            theirs: vec!["board.md".into()],
        });
        let g = git.lock().await;
        git.commit_paths(
            &g,
            &["06-Daily/today.md".into(), "board.md".into()],
            "S1: ours",
        )
        .await
        .unwrap();
        let report = git.sync(&g, false).await.unwrap();
        assert_eq!((report.ahead, report.behind), (1, 1));
        assert!(report.pushed);
        assert_eq!(report.merges.len(), 1);
        assert_eq!(report.merges[0].ours, vec!["06-Daily/today.md".to_string()]);
        assert_eq!(report.merges[0].theirs, vec!["board.md".to_string()]);
        let day = sh(&v.remote, &["show", "main:06-Daily/today.md"]);
        assert_eq!(day, "ours\n");
        assert_eq!(sh(&v.remote, &["show", "main:board.md"]), "theirs\n");
        assert!(!v.vault.join(".git/MERGE_HEAD").exists());
    }

    #[tokio::test]
    async fn skip_worktree_files_keep_their_local_copy() {
        let v = TestVault::new("skip");
        let rel = ".obsidian/data.json";
        std::fs::create_dir_all(v.vault.join(".obsidian")).unwrap();
        std::fs::write(v.vault.join(rel), "{}\n").unwrap();
        sh(&v.vault, &["add", "-A"]);
        sh(&v.vault, &["commit", "-qm", "plugin"]);
        sh(&v.vault, &["push", "-q"]);
        sh(&v.vault, &["update-index", "--skip-worktree", rel]);
        std::fs::write(v.vault.join(rel), "{\"local\":1}\n").unwrap();
        push_from_other(&v, &[(rel, "{\"remote\":1}\n"), ("other.md", "o\n")]);
        std::fs::write(v.vault.join("mine.md"), "m\n").unwrap();
        let g = v.git.lock().await;
        v.git
            .commit_paths(&g, &["mine.md".into()], "S1: mine")
            .await
            .unwrap();
        v.git.push(&g).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(v.vault.join(rel)).unwrap(),
            "{\"local\":1}\n"
        );
        let flags = sh(&v.vault, &["ls-files", "-v", rel]);
        assert!(flags.starts_with("S "), "{flags}");
        assert_eq!(
            sh(&v.vault, &["show", &format!("HEAD:{rel}")]),
            "{\"remote\":1}\n"
        );
        let log = sh(&v.remote, &["log", "--format=%s", "main"]);
        assert!(log.contains("S1: mine"), "{log}");
    }

    #[tokio::test]
    async fn a_merge_that_shrinks_a_file_is_held_until_confirmed() {
        let v = TestVault::new("shrink");
        std::fs::write(v.vault.join("notes.md"), lines(40)).unwrap();
        sh(&v.vault, &["add", "-A"]);
        sh(&v.vault, &["commit", "-qm", "long notes"]);
        sh(&v.vault, &["push", "-q"]);
        push_from_other(&v, &[("notes.md", &lines(3))]);
        std::fs::write(v.vault.join("mine.md"), "m\n").unwrap();
        let g = v.git.lock().await;
        v.git
            .commit_paths(&g, &["mine.md".into()], "S1: mine")
            .await
            .unwrap();
        let before = sh(&v.vault, &["rev-parse", "HEAD"]);
        let err = v.git.push(&g).await.unwrap_err();
        let want = vec![Shrink {
            path: "notes.md".into(),
            before: 40,
            after: 3,
        }];
        let before = before.trim().to_string();
        assert_eq!(
            err,
            GitError::Held {
                before: before.clone(),
                shrunk: want
            }
        );
        assert_eq!(
            err.to_string(),
            format!(
                "push held: notes.md went from 40 to 3 lines in the merge; the copy from before it is in {}",
                &before[..12]
            )
        );
        assert!(!sh(&v.remote, &["log", "--format=%s", "main"]).contains("S1: mine"));
        // The merge stays here; the old copy is one commit back.
        assert_ne!(sh(&v.vault, &["rev-parse", "HEAD"]).trim(), before);

        let report = v.git.sync(&g, false).await.unwrap();
        assert_eq!(report.behind, 0);
        assert!(report.pushed, "the merge is already made: {report:?}");
        assert!(sh(&v.remote, &["log", "--format=%s", "main"]).contains("S1: mine"));
    }

    #[tokio::test]
    async fn sync_holds_a_shrinking_merge_unless_confirmed() {
        let v = TestVault::new("synchold");
        std::fs::write(v.vault.join("notes.md"), lines(40)).unwrap();
        sh(&v.vault, &["add", "-A"]);
        sh(&v.vault, &["commit", "-qm", "long notes"]);
        sh(&v.vault, &["push", "-q"]);
        push_from_other(&v, &[("notes.md", &lines(3))]);
        std::fs::write(v.vault.join("mine.md"), "m\n").unwrap();
        let g = v.git.lock().await;
        v.git
            .commit_paths(&g, &["mine.md".into()], "S1: mine")
            .await
            .unwrap();
        let report = v.git.sync(&g, false).await.unwrap();
        assert_eq!((report.ahead, report.behind), (1, 1));
        assert!(!report.pushed);
        assert_eq!(report.held.len(), 1);
        assert_eq!(report.merges[0].merged, vec!["notes.md".to_string()]);
        assert!(!sh(&v.remote, &["log", "--format=%s", "main"]).contains("S1: mine"));

        // The old copy comes back from the commit before the merge.
        let sha = v.git.restore_file(&g, "notes.md", "HEAD^1").await.unwrap();
        assert_eq!(sha.len(), 40);
        assert_eq!(
            std::fs::read_to_string(v.vault.join("notes.md")).unwrap(),
            lines(40)
        );
        v.git
            .commit_paths(&g, &["notes.md".into()], "restore notes.md")
            .await
            .unwrap();
        let report = v.git.sync(&g, true).await.unwrap();
        assert!(report.pushed);
        assert_eq!(sh(&v.remote, &["show", "main:notes.md"]), lines(40));
    }

    #[tokio::test]
    async fn sync_refuses_without_an_upstream_or_with_conflicts() {
        let v = TestVault::new("syncno");
        let g = v.git.lock().await;
        let ok = v.git.sync(&g, false).await.unwrap();
        assert_eq!(ok, SyncReport::default());
        std::fs::write(v.vault.join(".git/MERGE_HEAD"), "x\n").unwrap();
        let r = v.git.sync(&g, false).await;
        assert!(
            matches!(&r, Err(GitError::Failed(m)) if m.contains("under way")),
            "{r:?}"
        );
        std::fs::remove_file(v.vault.join(".git/MERGE_HEAD")).unwrap();
        sh(&v.vault, &["branch", "--unset-upstream"]);
        let r = v.git.sync(&g, false).await;
        assert!(
            matches!(&r, Err(GitError::Failed(m)) if m.contains("no upstream")),
            "{r:?}"
        );
    }

    #[tokio::test]
    async fn restore_refuses_options_and_missing_files() {
        let v = TestVault::new("restore");
        let g = v.git.lock().await;
        for rev in ["", "--orphan", "nosuchrev"] {
            assert!(v.git.restore_file(&g, "a.md", rev).await.is_err(), "{rev}");
        }
        let r = v.git.restore_file(&g, "missing.md", "HEAD").await;
        assert_eq!(r, Err(GitError::Failed("missing.md is not in HEAD".into())));
    }

    #[tokio::test]
    async fn hung_push_times_out() {
        let v = TestVault::new("timeout");
        let hook = v.vault.join(".git/hooks/pre-push");
        std::fs::write(&hook, "#!/bin/sh\nexec sleep 10\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        let git = VaultGit::new(v.vault.clone(), true).with_timeout(Duration::from_secs(1));
        let g = git.lock().await;
        let t = std::time::Instant::now();
        let r = git.push(&g).await;
        assert_eq!(r, Err(GitError::Failed("git push timed out".into())));
        assert!(t.elapsed() < Duration::from_secs(5));
    }
}
