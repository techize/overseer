//! Git worktrees for planned tickets (Ruling 6): one folder per repo and
//! ticket under the worktree root, created when missing and reused when it
//! already belongs to the repo. Nothing is forced and nothing is removed.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Worktree {
    Created(PathBuf),
    Reused(PathBuf),
}

/// `<root>/<repo folder name>-<ticket>`.
pub fn worktree_path(root: &Path, repo: &Path, ticket: &str) -> PathBuf {
    let name = repo
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string());
    root.join(format!("{name}-{ticket}"))
}

/// Whether `path` is inside a git repository or worktree.
pub fn is_git_repo(path: &Path) -> bool {
    path.is_dir()
        && std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "--git-dir"])
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
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

async fn git(dir: &Path, args: &[&str]) -> Result<Out, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        // Untranslated messages, like the vault's git runner.
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("could not run git: {e}"))?;
    Ok(Out {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// The canonical git common dir of the repo or worktree at `dir`.
async fn common_dir(dir: &Path) -> Option<PathBuf> {
    let out = git(dir, &["rev-parse", "--git-common-dir"]).await.ok()?;
    if !out.ok {
        return None;
    }
    let raw = PathBuf::from(out.stdout.trim());
    // Relative output is relative to `dir`.
    dir.join(raw).canonicalize().ok()
}

/// Creates the worktree for `ticket` of `repo` under `root`, or reuses the
/// one already there. A folder at the path that is not a worktree of `repo`
/// is an error and is left alone.
pub async fn ensure_worktree(root: &Path, repo: &Path, ticket: &str) -> Result<Worktree, String> {
    std::fs::create_dir_all(root)
        .map_err(|e| format!("could not create {}: {e}", root.display()))?;
    let path = worktree_path(root, repo, ticket);
    if path.exists() {
        let mine = common_dir(repo).await;
        let theirs = common_dir(&path).await;
        return match (mine, theirs) {
            (Some(a), Some(b)) if a == b => Ok(Worktree::Reused(path)),
            _ => Err(format!(
                "{} exists and is not a worktree of {}",
                path.display(),
                repo.display()
            )),
        };
    }
    let path_arg = path.to_string_lossy().into_owned();
    let branch_ref = format!("refs/heads/{ticket}");
    let branch_exists = git(repo, &["show-ref", "--verify", "--quiet", &branch_ref])
        .await?
        .ok;
    let out = if branch_exists {
        git(repo, &["worktree", "add", "--", &path_arg, ticket]).await?
    } else {
        git(repo, &["worktree", "add", "-b", ticket, "--", &path_arg]).await?
    };
    if out.ok {
        Ok(Worktree::Created(path))
    } else {
        Err(first_line(&out.stderr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) -> String {
        let o = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    /// A repo named `demo-api` with one commit, and an empty worktree root.
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("demo-api");
        std::fs::create_dir_all(&repo).unwrap();
        sh(&repo, &["init", "-b", "main"]);
        sh(&repo, &["config", "user.name", "Test"]);
        sh(&repo, &["config", "user.email", "test@example.com"]);
        sh(&repo, &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("a.txt"), "a").unwrap();
        sh(&repo, &["add", "a.txt"]);
        sh(&repo, &["commit", "-m", "first"]);
        let root = d.path().join("worktrees");
        (d, repo, root)
    }

    #[test]
    fn path_is_root_repo_name_ticket() {
        assert_eq!(
            worktree_path(Path::new("/w"), Path::new("/r/demo-api"), "DEMO-12"),
            PathBuf::from("/w/demo-api-DEMO-12")
        );
    }

    #[tokio::test]
    async fn creates_a_worktree_on_a_new_branch() {
        let (_d, repo, root) = fixture();
        let w = ensure_worktree(&root, &repo, "DEMO-12").await.unwrap();
        let Worktree::Created(p) = w else {
            panic!("expected Created, got {w:?}")
        };
        assert_eq!(p, root.join("demo-api-DEMO-12"));
        assert_eq!(sh(&p, &["branch", "--show-current"]), "DEMO-12");
    }

    #[tokio::test]
    async fn reuses_its_own_worktree() {
        let (_d, repo, root) = fixture();
        ensure_worktree(&root, &repo, "DEMO-12").await.unwrap();
        let again = ensure_worktree(&root, &repo, "DEMO-12").await.unwrap();
        assert_eq!(again, Worktree::Reused(root.join("demo-api-DEMO-12")));
    }

    #[tokio::test]
    async fn uses_an_existing_branch() {
        let (_d, repo, root) = fixture();
        sh(&repo, &["branch", "DEMO-13"]);
        let w = ensure_worktree(&root, &repo, "DEMO-13").await.unwrap();
        let Worktree::Created(p) = w else {
            panic!("expected Created, got {w:?}")
        };
        assert_eq!(sh(&p, &["branch", "--show-current"]), "DEMO-13");
    }

    #[tokio::test]
    async fn refuses_a_folder_that_is_not_its_worktree() {
        let (_d, repo, root) = fixture();
        let path = root.join("demo-api-DEMO-12");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("keep.txt"), "mine").unwrap();
        let err = ensure_worktree(&root, &repo, "DEMO-12").await.unwrap_err();
        assert_eq!(
            err,
            format!(
                "{} exists and is not a worktree of {}",
                path.display(),
                repo.display()
            )
        );
        assert_eq!(
            std::fs::read_to_string(path.join("keep.txt")).unwrap(),
            "mine"
        );
    }

    #[tokio::test]
    async fn refuses_a_worktree_of_another_repo() {
        let (d, repo, root) = fixture();
        let other = d.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        sh(&other, &["init", "-b", "main"]);
        sh(&other, &["config", "user.name", "Test"]);
        sh(&other, &["config", "user.email", "test@example.com"]);
        sh(&other, &["commit", "--allow-empty", "-m", "first"]);
        ensure_worktree(&root, &other, "DEMO-12").await.unwrap();
        // Same folder name rule: make the other repo's worktree sit at the demo-api path.
        std::fs::rename(root.join("other-DEMO-12"), root.join("demo-api-DEMO-12")).unwrap();
        let err = ensure_worktree(&root, &repo, "DEMO-12").await;
        assert!(err.unwrap_err().contains("is not a worktree of"));
    }

    #[test]
    fn not_a_repo() {
        let (d, repo, _root) = fixture();
        let plain = d.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        // The temp dir itself is not inside a repo, so a plain folder is not one.
        assert!(!is_git_repo(&plain));
        assert!(is_git_repo(&repo));
        assert!(!is_git_repo(&d.path().join("missing")));
    }
}
