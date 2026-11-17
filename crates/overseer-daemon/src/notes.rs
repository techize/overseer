//! The notes folder Overseer owns when the user has no vault of their own.

use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::config::OWNED_MARKER;

const MARKER_TEXT: &str = "This folder is kept by Overseer. Delete this file to stop Overseer writing sprint and ticket folders here.\n";

/// Make `dir` an Overseer notes folder: the marker, `Sprints/`, and a git repo
/// with one commit. A folder that has other entries and no marker is left alone.
pub fn ensure_owned(dir: &Path) -> io::Result<()> {
    let marker = dir.join(OWNED_MARKER);
    if dir.exists() && !marker.exists() && std::fs::read_dir(dir)?.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "{} exists and is not an Overseer notes folder",
                dir.display()
            ),
        ));
    }
    std::fs::create_dir_all(dir.join("Sprints"))?;
    if !marker.exists() {
        std::fs::write(&marker, MARKER_TEXT)?;
    }
    if !dir.join(".git").exists() {
        init_repo(dir)?;
    }
    Ok(())
}

fn git(dir: &Path, args: &[&str]) -> io::Result<()> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "git {} failed: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

fn has_identity(dir: &Path) -> bool {
    Command::new("git")
        .args(["config", "user.name"])
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .map(|o| o.status.success() && !o.stdout.iter().all(u8::is_ascii_whitespace))
        .unwrap_or(false)
}

fn init_repo(dir: &Path) -> io::Result<()> {
    git(dir, &["init", "-q", "-b", "main"])?;
    git(dir, &["add", OWNED_MARKER])?;
    let mut args = Vec::new();
    if !has_identity(dir) {
        args.extend([
            "-c",
            "user.name=Overseer",
            "-c",
            "user.email=overseer@localhost",
        ]);
    }
    args.extend(["commit", "-q", "-m", "Start Overseer notes"]);
    git(dir, &args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(dir: &Path, args: &[&str]) -> String {
        let o = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    #[test]
    fn creates_marks_and_inits() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("notes");
        ensure_owned(&dir).unwrap();
        assert!(dir.join(OWNED_MARKER).is_file());
        assert!(dir.join("Sprints").is_dir());
        assert!(dir.join(".git").is_dir());
        assert_eq!(out(&dir, &["log", "--oneline"]).lines().count(), 1);
    }

    #[test]
    fn second_call_changes_nothing() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("notes");
        ensure_owned(&dir).unwrap();
        let head = out(&dir, &["rev-parse", "HEAD"]);
        ensure_owned(&dir).unwrap();
        assert_eq!(out(&dir, &["rev-parse", "HEAD"]), head);
    }

    #[test]
    fn refuses_a_foreign_folder() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("a.md"), "x").unwrap();
        let e = ensure_owned(t.path()).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert!(e.to_string().contains("is not an Overseer notes folder"));
        assert!(!t.path().join(OWNED_MARKER).exists());
    }

    #[test]
    fn adopts_an_empty_folder() {
        let t = tempfile::tempdir().unwrap();
        ensure_owned(t.path()).unwrap();
        assert!(t.path().join(OWNED_MARKER).is_file());
        assert!(t.path().join(".git").is_dir());
    }
}
