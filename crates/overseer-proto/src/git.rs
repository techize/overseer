//! What a checkout is on, read from its files alone.

use std::fs;
use std::path::Path;

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
