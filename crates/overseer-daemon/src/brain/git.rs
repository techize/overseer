//! The vault's git runner: one async mutex, pathspec commits, push with
//! rebase on rejection, and `index.lock` handling (Rulings 5, 6 and 7).

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
const LOCK_POLL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitError {
    /// `.git/index.lock` is held and is not yet stale.
    IndexLocked,
    /// A rebase stopped on a conflict (aborted; local commit kept). Holds the first error line.
    Conflict(String),
    Failed(String),
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitError::IndexLocked => write!(f, "index.lock held, will retry"),
            GitError::Conflict(m) => write!(f, "rebase conflict: {m}"),
            GitError::Failed(m) => write!(f, "{m}"),
        }
    }
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
    removed_stale_lock: AtomicBool,
}

struct Out {
    ok: bool,
    code: Option<i32>,
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
            removed_stale_lock: AtomicBool::new(false),
        }
    }

    /// Shorten the `index.lock` wait (tests).
    pub fn with_lock_wait(mut self, wait: Duration) -> Self {
        self.lock_wait = wait;
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
        let out = Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .env("OVERSEER_COMMIT", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|e| GitError::Failed(format!("could not run git: {e}")))?;
        Ok(Out {
            ok: out.status.success(),
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    /// Run git with the vault as cwd and `OVERSEER_COMMIT=1`; returns stdout.
    pub async fn run(&self, _g: &VaultGuard<'_>, args: &[&str]) -> Result<String, GitError> {
        self.run_inner(args).await
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
        let mut add = vec!["add", "-A", "--"];
        add.extend(paths.iter().map(String::as_str));
        self.run_inner(&add).await?;

        let mut diff = vec!["diff", "--cached", "--quiet", "--"];
        diff.extend(paths.iter().map(String::as_str));
        let d = self.exec(&diff).await?;
        match d.code {
            Some(0) => return Ok(None),
            Some(1) => {}
            _ => return Err(GitError::Failed(first_line(&d.stderr))),
        }

        let mut commit = vec!["commit", "-m", message, "--"];
        commit.extend(paths.iter().map(String::as_str));
        self.run_inner(&commit).await?;
        let head = self.run_inner(&["rev-parse", "HEAD"]).await?;
        Ok(Some(head.trim().to_string()))
    }

    fn rebase_in_progress(&self) -> bool {
        let git = self.root.join(".git");
        git.join("rebase-merge").exists() || git.join("rebase-apply").exists()
    }

    /// `git push`; when rejected, `git pull --rebase --autostash` and push once
    /// more. A rebase conflict is aborted and reported as `Conflict`; the local
    /// commit is kept. A no-op when `push` is off.
    pub async fn push(&self, _g: &VaultGuard<'_>) -> Result<(), GitError> {
        if !self.push {
            return Ok(());
        }
        let first = self.exec(&["push"]).await?;
        if first.ok {
            return Ok(());
        }
        let rejected = first.stderr.contains("rejected")
            || first.stderr.contains("non-fast-forward")
            || first.stderr.contains("fetch first");
        if !rejected {
            return Err(GitError::Failed(first_line(&first.stderr)));
        }
        let pull = self.exec(&["pull", "--rebase", "--autostash"]).await?;
        if !pull.ok {
            let msg = if pull.stderr.trim().is_empty() {
                first_line(&pull.stdout)
            } else {
                first_line(&pull.stderr)
            };
            if self.rebase_in_progress() {
                let _ = self.exec(&["rebase", "--abort"]).await;
                // Prefer the conflict line git printed on stdout when stderr is generic.
                let line = pull
                    .stdout
                    .lines()
                    .chain(pull.stderr.lines())
                    .map(str::trim)
                    .find(|l| l.starts_with("CONFLICT") || l.starts_with("error:"))
                    .map(str::to_string)
                    .unwrap_or(msg);
                return Err(GitError::Conflict(line));
            }
            return Err(GitError::Failed(msg));
        }
        let second = self.exec(&["push"]).await?;
        if second.ok {
            Ok(())
        } else {
            Err(GitError::Failed(first_line(&second.stderr)))
        }
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
    async fn rejected_push_rebases_and_pushes() {
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
        assert!(matches!(err, GitError::Conflict(_)), "{err:?}");
        assert!(!v.vault.join(".git/rebase-merge").exists());
        assert!(!v.vault.join(".git/rebase-apply").exists());
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
}
