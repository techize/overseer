//! Settings read from `config.toml` at daemon start.

use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct Config {
    pub beads_dir: PathBuf,
    /// `{jql}` in any element is replaced by `jql`.
    pub jira_command: Vec<String>,
    pub jql: String,
    pub jira_poll_secs: u64,
    pub beads_poll_secs: u64,
    pub ticket_pattern: String,
    pub brain_dir: PathBuf,
    /// Minutes a lock may go unused before the board flags it as long-held.
    pub lock_idle_mins: u64,
    /// The 2ndBrain writer is opt-in: while false, none of its tools, hook
    /// checks, backup or sync run.
    pub brain_writer: bool,
    /// Globs under `brain_dir` for the shared files; `*` matches one path segment.
    pub shared_files: Vec<String>,
    /// Today's journal under `brain_dir`; `{yyyy}`, `{mm}`, `{Month}` and `{dd}` are filled in.
    pub journal_path: String,
    /// A session's note under `brain_dir`; also takes `{slot}` and `{label}`.
    pub session_note: String,
    /// Each slot's section is written by Overseer only. Usually outside the
    /// vault; where it is in the vault, or a symlink into it (R7), the
    /// backup commits it like a free file.
    pub current_session: PathBuf,
    /// Minutes between timed backups; 0 turns the backup off.
    pub backup_mins: u64,
    pub push: bool,
    /// Run in `brain_dir` after the queue drains; skipped when the file is missing.
    pub sync_command: Vec<String>,
    pub bd_command: Vec<String>,
    /// Where plans put their git worktrees: `<worktree_root>/<repo>-<ticket>`.
    pub worktree_root: PathBuf,
    /// The board header's sprint name; empty means the newest `Sprints/sprint-*` folder.
    pub sprint_name: String,
    /// `YYYY-MM-DD`, the sprint's first day; empty shows the name alone.
    pub sprint_start: String,
    /// Weekdays in the sprint; 0 counts as 10.
    pub sprint_days: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            beads_dir: PathBuf::from("~/Repos/2ndBrain"),
            jira_command: [
                "acli", "jira", "workitem", "search", "--jql", "{jql}", "--json", "--limit", "200",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            jql: "sprint in openSprints() AND assignee = currentUser() ORDER BY priority DESC"
                .to_string(),
            jira_poll_secs: 300,
            beads_poll_secs: 30,
            ticket_pattern: r"PLAT-\d+".to_string(),
            brain_dir: PathBuf::from("~/Repos/2ndBrain"),
            lock_idle_mins: 60,
            brain_writer: false,
            shared_files: [
                "06-Daily/*/*/*-journal.md",
                "Sprints/*/SPRINT-DASHBOARD.md",
                "Sprints/*/sprint-*.md",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            journal_path: "06-Daily/{yyyy}/{mm}-{Month}/{yyyy}-{mm}-{dd}-journal.md".to_string(),
            session_note: "Sessions/{yyyy}/{mm}-{Month}/{yyyy}-{mm}-{dd}-{slot}-{label}.md"
                .to_string(),
            current_session: PathBuf::from("~/.claude/CURRENT_SESSION.md"),
            backup_mins: 5,
            push: true,
            sync_command: ["05-Meta/Sync-System/sync-run.sh", "--auto"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            bd_command: vec!["bd".to_string()],
            worktree_root: PathBuf::from("~/Repos/.worktrees"),
            sprint_name: String::new(),
            sprint_start: String::new(),
            sprint_days: 10,
        }
    }
}

impl Config {
    /// A missing file gives the defaults and `None`; an unreadable or invalid
    /// file gives the defaults and the error text. `~/` is expanded against `home`.
    pub fn load(path: &Path, home: &Path) -> (Config, Option<String>) {
        let (mut config, err) = match std::fs::read_to_string(path) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(c) => (c, None),
                Err(e) => (Config::default(), Some(format!("{}: {e}", path.display()))),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Config::default(), None),
            Err(e) => (Config::default(), Some(format!("{}: {e}", path.display()))),
        };
        config.beads_dir = expand(&config.beads_dir, home);
        config.brain_dir = expand(&config.brain_dir, home);
        config.current_session = expand(&config.current_session, home);
        config.worktree_root = expand(&config.worktree_root, home);
        (config, err)
    }
}

fn expand(p: &Path, home: &Path) -> PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => p.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(body: &str) -> (tempdir::Dir, PathBuf) {
        let d = tempdir::Dir::new();
        let p = d.0.join("config.toml");
        std::fs::write(&p, body).unwrap();
        (d, p)
    }

    mod tempdir {
        use std::path::PathBuf;
        pub struct Dir(pub PathBuf);
        impl Dir {
            pub fn new() -> Dir {
                let p = std::env::temp_dir().join(format!("ovs-cfg-{}", uuid::Uuid::new_v4()));
                std::fs::create_dir_all(&p).unwrap();
                Dir(p)
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    fn home() -> PathBuf {
        PathBuf::from("/Users/me")
    }

    #[test]
    fn missing_file_is_defaults() {
        let (c, err) = Config::load(Path::new("/nonexistent/config.toml"), &home());
        assert!(err.is_none());
        assert_eq!(c.jira_poll_secs, 300);
        assert_eq!(c.beads_poll_secs, 30);
        assert_eq!(c.ticket_pattern, r"PLAT-\d+");
        assert_eq!(c.jira_command[0], "acli");
        assert_eq!(c.beads_dir, PathBuf::from("/Users/me/Repos/2ndBrain"));
        assert_eq!(c.brain_dir, PathBuf::from("/Users/me/Repos/2ndBrain"));
        assert_eq!(c.lock_idle_mins, 60);
        assert!(!c.brain_writer);
        assert_eq!(
            c.shared_files,
            vec![
                "06-Daily/*/*/*-journal.md",
                "Sprints/*/SPRINT-DASHBOARD.md",
                "Sprints/*/sprint-*.md"
            ]
        );
        assert_eq!(
            c.journal_path,
            "06-Daily/{yyyy}/{mm}-{Month}/{yyyy}-{mm}-{dd}-journal.md"
        );
        assert_eq!(
            c.session_note,
            "Sessions/{yyyy}/{mm}-{Month}/{yyyy}-{mm}-{dd}-{slot}-{label}.md"
        );
        assert_eq!(
            c.current_session,
            PathBuf::from("/Users/me/.claude/CURRENT_SESSION.md")
        );
        assert_eq!(c.backup_mins, 5);
        assert!(c.push);
        assert_eq!(
            c.sync_command,
            vec!["05-Meta/Sync-System/sync-run.sh", "--auto"]
        );
        assert_eq!(c.bd_command, vec!["bd"]);
    }

    #[test]
    fn partial_file_overrides_only_its_keys() {
        let (_d, p) = write("jql = \"project = X\"\nlock_idle_mins = 15\n");
        let (c, err) = Config::load(&p, &home());
        assert!(err.is_none());
        assert_eq!(c.jql, "project = X");
        assert_eq!(c.lock_idle_mins, 15);
        assert_eq!(c.jira_poll_secs, 300);
        assert_eq!(c.jira_command.len(), 9);
        assert_eq!(c.ticket_pattern, r"PLAT-\d+");
        assert!(!c.brain_writer);
        assert_eq!(c.backup_mins, 5);

        let (_d, p) =
            write("brain_writer = true\nbackup_mins = 0\ncurrent_session = \"~/cs.md\"\n");
        let (c, err) = Config::load(&p, &home());
        assert!(err.is_none());
        assert!(c.brain_writer);
        assert_eq!(c.backup_mins, 0);
        assert_eq!(c.current_session, PathBuf::from("/Users/me/cs.md"));
        assert_eq!(c.shared_files.len(), 3);
    }

    #[test]
    fn bad_toml_falls_back_with_an_error() {
        let (_d, p) = write("jql = [not toml");
        let (c, err) = Config::load(&p, &home());
        assert!(err.is_some_and(|e| !e.is_empty()));
        assert_eq!(c.jira_poll_secs, 300);
        assert_eq!(c.beads_dir, PathBuf::from("/Users/me/Repos/2ndBrain"));
    }

    #[test]
    fn tilde_paths_expand() {
        let (_d, p) = write("beads_dir = \"~/work/beads\"\nbrain_dir = \"/abs/brain\"\n");
        let (c, err) = Config::load(&p, &home());
        assert!(err.is_none());
        assert_eq!(c.beads_dir, PathBuf::from("/Users/me/work/beads"));
        assert_eq!(c.brain_dir, PathBuf::from("/abs/brain"));
    }

    #[test]
    fn worktree_root_defaults_and_expands() {
        let (c, _) = Config::load(Path::new("/nonexistent/config.toml"), &home());
        assert_eq!(c.worktree_root, PathBuf::from("/Users/me/Repos/.worktrees"));
        let (_d, p) = write("worktree_root = \"~/wt\"\n");
        let (c, err) = Config::load(&p, &home());
        assert!(err.is_none());
        assert_eq!(c.worktree_root, PathBuf::from("/Users/me/wt"));
        let (_d, p) = write("worktree_root = \"/abs/wt\"\n");
        let (c, _) = Config::load(&p, &home());
        assert_eq!(c.worktree_root, PathBuf::from("/abs/wt"));
    }

    #[test]
    fn sprint_keys_default_and_load() {
        let (c, _) = Config::load(Path::new("/nonexistent/config.toml"), &home());
        assert_eq!(c.sprint_name, "");
        assert_eq!(c.sprint_start, "");
        assert_eq!(c.sprint_days, 10);
        let (_d, p) =
            write("sprint_name = \"Demo\"\nsprint_start = \"2026-11-09\"\nsprint_days = 5\n");
        let (c, err) = Config::load(&p, &home());
        assert!(err.is_none());
        assert_eq!(c.sprint_name, "Demo");
        assert_eq!(c.sprint_start, "2026-11-09");
        assert_eq!(c.sprint_days, 5);
    }
}
