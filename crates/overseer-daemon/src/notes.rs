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
/// When git has no identity, the repo gets a local one: `user` and
/// `overseer@localhost`, so every later commit there works too.
pub fn ensure_owned(dir: &Path, user: &str) -> io::Result<()> {
    ensure_owned_with(dir, user, &[])
}

/// `ensure_owned` with extra variables for every git run (tests point
/// `GIT_CONFIG_GLOBAL` away from the developer's own identity).
fn ensure_owned_with(dir: &Path, user: &str, env: &[(&str, &str)]) -> io::Result<()> {
    if foreign(dir) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "{} exists and is not an Overseer notes folder",
                dir.display()
            ),
        ));
    }
    let marker = dir.join(OWNED_MARKER);
    std::fs::create_dir_all(dir.join("Sprints"))?;
    if !marker.exists() {
        std::fs::write(&marker, MARKER_TEXT)?;
    }
    let git = Git { dir, env };
    if !dir.join(".git").exists() {
        git.run(&["init", "-q", "-b", "main"])?;
    }
    git.ensure_identity(user)?;
    // A first commit that failed earlier left `.git` with no commits.
    if !git.ok(&["rev-parse", "--verify", "-q", "HEAD"]) {
        git.run(&["add", OWNED_MARKER])?;
        git.run(&["commit", "-q", "-m", "Start Overseer notes"])?;
    }
    Ok(())
}

/// A folder Overseer will not take: it exists, has other entries and no
/// `.overseer-notes` marker.
pub fn foreign(dir: &Path) -> bool {
    dir.is_dir()
        && !dir.join(OWNED_MARKER).exists()
        && std::fs::read_dir(dir).is_ok_and(|mut d| d.next().is_some())
}

/// Turns the notes writer and the sprint sync off, for a notes folder
/// Overseer could not take.
pub fn disown(config: &mut crate::Config) {
    config.brain_writer = false;
    config.notes_owned = false;
}

/// Readies the owned notes folder at daemon start. When that fails, the
/// writer and sync are turned off and the text for a board notice comes
/// back: which folder, why, and how to fix it.
pub fn prepare(config: &mut crate::Config) -> Option<String> {
    if !config.notes_owned {
        return None;
    }
    let dir = config.brain_dir.clone();
    let err = ensure_owned(&dir, config.user()).err()?;
    tracing::warn!("notes folder not ready, sprint notes are off: {err}");
    disown(config);
    Some(refused_text(&dir, &err))
}

fn refused_text(dir: &Path, err: &io::Error) -> String {
    if err.kind() == io::ErrorKind::AlreadyExists {
        format!(
            "Notes are off: {d} already holds other files and no {OWNED_MARKER} marker, so Overseer will not write there. \
             To fix it, set notes_dir in Settings to another folder, or move {d} aside, then restart the daemon.",
            d = dir.display()
        )
    } else {
        format!(
            "Notes are off: Overseer could not set up {d}: {err}. Fix the folder, or set notes_dir in Settings to another one, then restart the daemon.",
            d = dir.display()
        )
    }
}

struct Git<'a> {
    dir: &'a Path,
    env: &'a [(&'a str, &'a str)],
}

impl Git<'_> {
    fn output(&self, args: &[&str]) -> io::Result<std::process::Output> {
        Command::new("git")
            .args(args)
            .current_dir(self.dir)
            .envs(self.env.iter().copied())
            .stdin(Stdio::null())
            .output()
    }

    fn run(&self, args: &[&str]) -> io::Result<()> {
        let out = self.output(args)?;
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

    fn ok(&self, args: &[&str]) -> bool {
        self.output(args).is_ok_and(|o| o.status.success())
    }

    /// Whether git has a non-blank value for `key` from any config file.
    fn has(&self, key: &str) -> bool {
        self.output(&["config", key])
            .map(|o| o.status.success() && !o.stdout.iter().all(u8::is_ascii_whitespace))
            .unwrap_or(false)
    }

    /// Writes a local `user.name` and `user.email` where git has none.
    fn ensure_identity(&self, user: &str) -> io::Result<()> {
        if !self.has("user.name") {
            let name = match user.trim() {
                "" => "Overseer",
                n => n,
            };
            self.run(&["config", "--local", "user.name", name])?;
        }
        if !self.has("user.email") {
            self.run(&["config", "--local", "user.email", "overseer@localhost"])?;
        }
        Ok(())
    }
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
/// changes. The sprint folder is made even with no tickets to write.
/// Returns how many files were written. Folders of tickets that left the
/// sprint stay as they are.
pub fn sync_sprint(
    dir: &Path,
    sprint: &str,
    tickets: &[Ticket],
    site: &str,
    now: i64,
) -> io::Result<usize> {
    // Checked on every sync, so deleting the marker stops writes at once.
    if !dir.join(OWNED_MARKER).exists() {
        return Ok(0);
    }
    let folders = list_folders(dir);
    let sprint_dir = dir.join("Sprints").join(sprint);
    std::fs::create_dir_all(&sprint_dir)?;
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
        ensure_owned(&dir, "Demo").unwrap();
        assert!(dir.join(OWNED_MARKER).is_file());
        assert!(dir.join("Sprints").is_dir());
        assert!(dir.join(".git").is_dir());
        assert_eq!(out(&dir, &["log", "--oneline"]).lines().count(), 1);
    }

    #[test]
    fn second_call_changes_nothing() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("notes");
        ensure_owned(&dir, "Demo").unwrap();
        let head = out(&dir, &["rev-parse", "HEAD"]);
        ensure_owned(&dir, "Demo").unwrap();
        assert_eq!(out(&dir, &["rev-parse", "HEAD"]), head);
    }

    #[test]
    fn refuses_a_foreign_folder() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("a.md"), "x").unwrap();
        let e = ensure_owned(t.path(), "Demo").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert!(e.to_string().contains("is not an Overseer notes folder"));
        assert!(!t.path().join(OWNED_MARKER).exists());
    }

    #[test]
    fn adopts_an_empty_folder() {
        let t = tempfile::tempdir().unwrap();
        ensure_owned(t.path(), "Demo").unwrap();
        assert!(t.path().join(OWNED_MARKER).is_file());
        assert!(t.path().join(".git").is_dir());
    }

    /// Git variables for a machine with no identity anywhere.
    fn no_identity(home: &Path) -> Vec<(&'static str, String)> {
        vec![
            ("GIT_CONFIG_GLOBAL", "/dev/null".into()),
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
            ("HOME", home.display().to_string()),
            ("XDG_CONFIG_HOME", home.display().to_string()),
        ]
    }

    #[test]
    fn no_git_identity_gets_a_local_one_and_later_commits_work() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("notes");
        let vars = no_identity(t.path());
        let env: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (*k, v.as_str())).collect();
        ensure_owned_with(&dir, "Demo", &env).unwrap();
        let git = |args: &[&str]| {
            let o = Command::new("git")
                .args(args)
                .current_dir(&dir)
                .envs(env.iter().copied())
                .output()
                .unwrap();
            (
                o.status.success(),
                String::from_utf8_lossy(&o.stdout).trim().to_string(),
            )
        };
        assert_eq!(git(&["config", "--local", "user.name"]).1, "Demo");
        assert_eq!(
            git(&["config", "--local", "user.email"]).1,
            "overseer@localhost"
        );
        assert_eq!(
            git(&["log", "--format=%an <%ae>"]).1,
            "Demo <overseer@localhost>"
        );
        // A later commit, as the writer makes, needs no identity of its own.
        std::fs::write(dir.join("a.md"), "x").unwrap();
        assert!(git(&["add", "a.md"]).0);
        assert!(git(&["commit", "-q", "-m", "a"]).0);
        assert_eq!(git(&["log", "--oneline"]).1.lines().count(), 2);
    }

    #[test]
    fn a_repo_with_no_commits_gets_the_marker_commit() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("notes");
        let vars = no_identity(t.path());
        let env: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (*k, v.as_str())).collect();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(OWNED_MARKER), MARKER_TEXT).unwrap();
        Git {
            dir: &dir,
            env: &env,
        }
        .run(&["init", "-q", "-b", "main"])
        .unwrap();
        ensure_owned_with(&dir, "Demo", &env).unwrap();
        assert_eq!(out(&dir, &["log", "--oneline"]).lines().count(), 1);
    }

    #[test]
    fn prepare_turns_notes_off_with_a_notice_for_a_foreign_folder() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("a.md"), "x").unwrap();
        let mut c = crate::Config {
            brain_dir: t.path().to_path_buf(),
            notes_owned: true,
            brain_writer: true,
            ..crate::Config::default()
        };
        let text = prepare(&mut c).unwrap();
        assert!(!c.notes_owned && !c.brain_writer);
        assert!(text.contains(&t.path().display().to_string()), "{text}");
        assert!(text.contains("no .overseer-notes marker"), "{text}");
        assert!(text.contains("set notes_dir"), "{text}");
        assert!(!t.path().join(OWNED_MARKER).exists());

        let mut c = crate::Config {
            notes_owned: false,
            ..crate::Config::default()
        };
        assert_eq!(prepare(&mut c), None);
    }

    #[test]
    fn deleting_the_marker_stops_the_sync() {
        let t = owned();
        std::fs::remove_file(t.path().join(OWNED_MARKER)).unwrap();
        let ts = [ticket("DEMO-1", "Fix", "To Do")];
        assert_eq!(
            sync_sprint(t.path(), "sprint-demo-1", &ts, SITE, T0).unwrap(),
            0
        );
        assert!(!t.path().join("Sprints/sprint-demo-1").exists());
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
        ensure_owned(t.path(), "Demo").unwrap();
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
    fn the_sprint_folder_is_made_with_no_tickets() {
        let t = owned();
        assert_eq!(
            sync_sprint(t.path(), "sprint-empty", &[], SITE, T0).unwrap(),
            0
        );
        assert!(t.path().join("Sprints/sprint-empty").is_dir());
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
        // The sprint folder is made, but the ticket stays where it was.
        let sprint = t.path().join("Sprints/sprint-demo-1");
        assert!(sprint.is_dir());
        assert_eq!(std::fs::read_dir(&sprint).unwrap().count(), 0);
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
