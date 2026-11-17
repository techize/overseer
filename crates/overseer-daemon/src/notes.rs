//! The notes folder Overseer owns when the user has no vault of their own.

use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::brain::local_stamp;
use crate::config::OWNED_MARKER;
use crate::work::sources::Ticket;
use crate::work::sprint::slug;
use crate::work::store::{folder_for, list_folders};

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

/// The longest title slug in a ticket folder name.
const TITLE_SLUG_MAX: usize = 50;

const UPDATED_PREFIX: &str = "- Updated: ";

/// `KEY-<slug of title>`, the slug at most 50 characters; `KEY` when the
/// title leaves no slug.
pub fn ticket_folder_name(key: &str, title: &str) -> String {
    let mut s = slug(title);
    // Every character is ASCII, so this cuts on a character boundary.
    s.truncate(TITLE_SLUG_MAX);
    let s = s.trim_end_matches('-');
    if s.is_empty() {
        key.to_owned()
    } else {
        format!("{key}-{s}")
    }
}

/// A key that is safe as the start of a folder name.
fn safe_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn ticket_md(t: &Ticket, site: &str, stamp: &str) -> String {
    let jira = if site.is_empty() {
        String::new()
    } else {
        format!("- Jira: https://{site}/browse/{}\n", t.key)
    };
    format!(
        "# {key}: {title}\n\n- Status: {status}\n- Priority: {priority}\n{jira}{UPDATED_PREFIX}{stamp} by Overseer\n\nOverseer rewrites this file from Jira. Put your own notes in other files in this folder.\n",
        key = t.key,
        title = t.title,
        status = t.status,
        priority = t.priority.as_deref().unwrap_or("none"),
    )
}

fn without_updated(text: &str) -> String {
    text.lines()
        .filter(|l| !l.starts_with(UPDATED_PREFIX))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Keeps a `ticket.md` for each ticket under `Sprints/<sprint>/`, in the
/// folder the key already has anywhere under `Sprints/`, else a new one.
/// A file is written only when its content, apart from the `Updated:` line,
/// changes. Returns how many files were written. Folders of tickets that
/// left the sprint stay as they are.
pub fn sync_sprint(
    dir: &Path,
    sprint: &str,
    tickets: &[Ticket],
    site: &str,
    now: i64,
) -> io::Result<usize> {
    let folders = list_folders(dir);
    let sprint_dir = dir.join("Sprints").join(sprint);
    let stamp = local_stamp(now);
    let mut written = 0;
    for t in tickets.iter().filter(|t| safe_key(&t.key)) {
        let folder = folder_for(&folders, &t.key)
            .unwrap_or_else(|| sprint_dir.join(ticket_folder_name(&t.key, &t.title)));
        let file = folder.join("ticket.md");
        let want = ticket_md(t, site, &stamp);
        if let Ok(have) = std::fs::read_to_string(&file) {
            if without_updated(&have) == without_updated(&want) {
                continue;
            }
        }
        std::fs::create_dir_all(&folder)?;
        std::fs::write(&file, want)?;
        written += 1;
    }
    Ok(written)
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

    fn ticket(key: &str, title: &str, status: &str) -> Ticket {
        Ticket {
            key: key.into(),
            title: title.into(),
            status: status.into(),
            priority: Some("High".into()),
        }
    }

    fn owned() -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        ensure_owned(t.path()).unwrap();
        t
    }

    const SITE: &str = "demo.atlassian.net";
    const T0: i64 = 1_800_000_000;

    #[test]
    fn creates_sprint_and_ticket_folders() {
        let t = owned();
        let ts = [
            ticket("DEMO-1", "Fix the login", "In Progress"),
            ticket("DEMO-2", "Add search", "To Do"),
        ];
        assert_eq!(
            sync_sprint(t.path(), "sprint-demo-1", &ts, SITE, T0).unwrap(),
            2
        );
        let f = t
            .path()
            .join("Sprints/sprint-demo-1/DEMO-1-fix-the-login/ticket.md");
        let want = format!(
            "# DEMO-1: Fix the login\n\n- Status: In Progress\n- Priority: High\n- Jira: https://demo.atlassian.net/browse/DEMO-1\n- Updated: {} by Overseer\n\nOverseer rewrites this file from Jira. Put your own notes in other files in this folder.\n",
            local_stamp(T0)
        );
        assert_eq!(std::fs::read_to_string(f).unwrap(), want);
        assert!(t
            .path()
            .join("Sprints/sprint-demo-1/DEMO-2-add-search/ticket.md")
            .is_file());
    }

    #[test]
    fn no_site_leaves_out_the_jira_line() {
        let t = owned();
        let mut tk = ticket("DEMO-1", "A", "To Do");
        tk.priority = None;
        sync_sprint(t.path(), "sprint-x", &[tk], "", T0).unwrap();
        let s =
            std::fs::read_to_string(t.path().join("Sprints/sprint-x/DEMO-1-a/ticket.md")).unwrap();
        assert!(!s.contains("Jira:"));
        assert!(s.contains("- Priority: none"));
    }

    #[test]
    fn second_sync_writes_nothing() {
        let t = owned();
        let ts = [ticket("DEMO-1", "Fix", "To Do")];
        sync_sprint(t.path(), "sprint-demo-1", &ts, SITE, T0).unwrap();
        let f = t.path().join("Sprints/sprint-demo-1/DEMO-1-fix/ticket.md");
        let before = std::fs::read(&f).unwrap();
        assert_eq!(
            sync_sprint(t.path(), "sprint-demo-1", &ts, SITE, T0 + 60).unwrap(),
            0
        );
        assert_eq!(std::fs::read(&f).unwrap(), before);
    }

    #[test]
    fn status_change_rewrites_only_ticket_md() {
        let t = owned();
        sync_sprint(
            t.path(),
            "sprint-demo-1",
            &[ticket("DEMO-1", "Fix", "To Do")],
            SITE,
            T0,
        )
        .unwrap();
        let d = t.path().join("Sprints/sprint-demo-1/DEMO-1-fix");
        std::fs::write(d.join("notes.md"), "mine\n").unwrap();
        let n = sync_sprint(
            t.path(),
            "sprint-demo-1",
            &[ticket("DEMO-1", "Fix", "Review")],
            SITE,
            T0 + 60,
        )
        .unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            std::fs::read_to_string(d.join("notes.md")).unwrap(),
            "mine\n"
        );
        assert!(std::fs::read_to_string(d.join("ticket.md"))
            .unwrap()
            .contains("- Status: Review"));
    }

    #[test]
    fn existing_folder_for_key_is_reused() {
        let t = owned();
        let old = t.path().join("Sprints/sprint-demo-0/DEMO-1-old");
        std::fs::create_dir_all(&old).unwrap();
        sync_sprint(
            t.path(),
            "sprint-demo-1",
            &[ticket("DEMO-1", "New title", "To Do")],
            SITE,
            T0,
        )
        .unwrap();
        assert!(old.join("ticket.md").is_file());
        assert!(!t.path().join("Sprints/sprint-demo-1").exists());
    }

    #[test]
    fn ticket_leaving_sprint_keeps_folder() {
        let t = owned();
        let both = [
            ticket("DEMO-1", "A", "To Do"),
            ticket("DEMO-2", "B", "To Do"),
        ];
        sync_sprint(t.path(), "sprint-demo-1", &both, SITE, T0).unwrap();
        sync_sprint(t.path(), "sprint-demo-1", &both[..1], SITE, T0 + 60).unwrap();
        assert!(t
            .path()
            .join("Sprints/sprint-demo-1/DEMO-2-b/ticket.md")
            .is_file());
    }

    #[test]
    fn hostile_title_stays_inside() {
        let t = owned();
        sync_sprint(
            t.path(),
            "sprint-demo-1",
            &[ticket("DEMO-1", "../../x", "To Do")],
            SITE,
            T0,
        )
        .unwrap();
        assert!(t
            .path()
            .join("Sprints/sprint-demo-1/DEMO-1-x/ticket.md")
            .is_file());
        assert_eq!(ticket_folder_name("DEMO-2", "../.."), "DEMO-2");
        let long = ticket_folder_name("DEMO-3", &"ab ".repeat(100));
        assert!(long.len() <= "DEMO-3-".len() + 50 && !long.ends_with('-'));
    }
}
