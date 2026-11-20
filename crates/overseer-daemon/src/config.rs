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
    #[serde(alias = "notes_dir")]
    pub brain_dir: PathBuf,
    /// The first name used in greetings; empty until resolved by `load`.
    pub user_name: String,
    pub jira_site: String,
    pub jira_email: String,
    /// Empty means unset.
    pub jira_board: String,
    pub jira_cloud_id: String,
    /// True when `brain_dir` is a folder Overseer owns (it holds the
    /// `.overseer-notes` marker, or was defaulted to `~/OverseerNotes`).
    #[serde(skip)]
    pub notes_owned: bool,
    /// Set at daemon start when Overseer would not take the notes folder:
    /// the board banner's text. Never read from `config.toml`.
    #[serde(skip)]
    pub notes_refused: Option<String>,
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
    /// Globs under `brain_dir` for the logs a slot's session writes, read
    /// when the session ends to check their times and `outcome` and
    /// `highlights`; `{yyyy}`, `{mm}`, `{dd}`, `{Month}` and `{slot}` (`s1`)
    /// are filled in. Empty turns the check off.
    pub session_logs: Vec<String>,
    /// The file under `brain_dir` that gets each ended run's real start and
    /// end as a JSON line, while the notes writer is on; the date fields
    /// are the run's start day's. One file a day keeps two machines from
    /// appending to the same file. Empty turns it off.
    pub slot_runs: String,
    /// Sessions that ran fewer minutes than this are not checked.
    pub session_log_min_mins: u64,
    /// Each slot's section is written by Overseer only. Usually outside the
    /// vault; where it is in the vault, or a symlink into it (R7), the
    /// backup commits it like a free file.
    pub current_session: PathBuf,
    /// Minutes between timed backups; 0 turns the backup off.
    pub backup_mins: u64,
    pub push: bool,
    /// When a merge of the notes folder's remote conflicts, files matching
    /// these take this folder's copy (`notes_merge_ours`) or the remote's
    /// (`notes_merge_theirs`); any other conflict stops the merge. An entry
    /// ending in `/` matches the folder, any other the path or file name.
    pub notes_merge_ours: Vec<String>,
    pub notes_merge_theirs: Vec<String>,
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
    /// Repos whose open pull requests the board lists; empty turns the PR source off.
    pub pr_repos: Vec<PathBuf>,
    pub pr_command: Vec<String>,
    pub pr_poll_secs: u64,
    /// Only this author's pull requests; empty shows all authors.
    pub pr_author: String,
    /// Bitbucket accounts whose PR approvals count as AI reviews, by
    /// nickname, display name, account id or uuid. Anyone else is human.
    pub ai_reviewers: Vec<String>,
    /// A Bitbucket workspace whose repos are found and polled; empty turns
    /// this off. Works alongside `pr_repos`.
    pub pr_workspace: String,
    /// Only repos in this Bitbucket project; empty means the whole workspace.
    pub pr_project: String,
    /// Only repos updated in this many days are polled; 0 polls every repo.
    pub pr_active_days: u64,
    /// Lists the workspace's repos; `{workspace}` in any element is
    /// replaced by `pr_workspace`.
    pub pr_repos_command: Vec<String>,
    /// Jira status names for each board stage, matched case-insensitively.
    pub stage_todo: Vec<String>,
    pub stage_blocked: Vec<String>,
    pub stage_next: Vec<String>,
    pub stage_in_progress: Vec<String>,
    pub stage_to_be_tested: Vec<String>,
    pub stage_done: Vec<String>,
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
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
            ticket_pattern: String::new(),
            brain_dir: PathBuf::from("~/Repos/2ndBrain"),
            user_name: String::new(),
            jira_site: String::new(),
            jira_email: String::new(),
            jira_board: String::new(),
            jira_cloud_id: String::new(),
            notes_owned: false,
            notes_refused: None,
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
            session_logs: [
                "Sessions/{yyyy}-{mm}-{dd}-{slot}-*.md",
                "Sessions/{yyyy}/{mm}-{Month}/{yyyy}-{mm}-{dd}-{slot}-*.md",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            slot_runs: "Sessions/Runs/{yyyy}-{mm}-{dd}-runs.jsonl".to_string(),
            session_log_min_mins: 15,
            current_session: PathBuf::from("~/.claude/CURRENT_SESSION.md"),
            backup_mins: 5,
            push: true,
            notes_merge_ours: ["06-Daily/", "Sessions/", "CURRENT_SESSION.md"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            notes_merge_theirs: Vec::new(),
            sync_command: ["05-Meta/Sync-System/sync-run.sh", "--auto"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            bd_command: vec!["bd".to_string()],
            worktree_root: PathBuf::from("~/Repos/.worktrees"),
            sprint_name: String::new(),
            sprint_start: String::new(),
            sprint_days: 10,
            pr_repos: Vec::new(),
            pr_command: strings(&[
                "bb",
                "pullrequest",
                "list",
                "--state",
                "open",
                "--output",
                "json",
            ]),
            pr_poll_secs: 300,
            pr_author: String::new(),
            ai_reviewers: Vec::new(),
            pr_workspace: String::new(),
            pr_project: String::new(),
            pr_active_days: 30,
            pr_repos_command: strings(&[
                "bb",
                "repo",
                "list",
                "--workspace",
                "{workspace}",
                "--output",
                "json",
            ]),
            stage_todo: strings(&["Backlog", "Prioritised Backlog", "To Do"]),
            stage_blocked: strings(&["Blocked", "Waiting", "On Hold"]),
            stage_next: strings(&["Next", "Selected for Development"]),
            stage_in_progress: strings(&["In Progress", "In Review", "Code Review", "Review"]),
            stage_to_be_tested: strings(&["To Be Tested", "Testing", "In Testing", "QA"]),
            stage_done: strings(&["Done", "Closed", "Resolved"]),
        }
    }
}

/// `$HOME`, or `/` when it is unset or empty: the home `~/` expands against.
pub fn home_from_env() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

/// The file that marks a notes folder as one Overseer owns and may sync into.
pub const OWNED_MARKER: &str = ".overseer-notes";

/// The macOS full name (`id -F`), trimmed; `None` on any failure.
pub fn macos_full_name() -> Option<String> {
    let out = std::process::Command::new("id").arg("-F").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// The first word of the full name, else `$USER`, else `you`.
fn default_user_name(full_name: Option<&str>, env_user: Option<&str>) -> String {
    full_name
        .and_then(|n| n.split_whitespace().next())
        .or(env_user.filter(|u| !u.is_empty()))
        .unwrap_or("you")
        .to_string()
}

/// `jira_site` as the bare host: a pasted `https://` or `http://` and any
/// trailing `/` are dropped.
pub fn site_host(site: &str) -> String {
    let s = site.trim();
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);
    s.trim_end_matches('/').to_string()
}

/// The name prompts and messages use: `configured` when it has any text, so
/// an explicit empty `user_name` renders as the default, never as nothing.
fn effective_user_name(
    configured: &str,
    full_name: Option<&str>,
    env_user: Option<&str>,
) -> String {
    match configured.trim() {
        "" => default_user_name(full_name, env_user),
        name => name.to_string(),
    }
}

impl Config {
    /// The name to use in prompts and messages: `user_name`, or `you` when
    /// it is empty (a config that did not come through `load`).
    pub fn user(&self) -> &str {
        match self.user_name.trim() {
            "" => "you",
            name => name,
        }
    }

    /// A missing file gives the defaults and `None`; an unreadable or invalid
    /// file gives the defaults and the error text. `~/` is expanded against `home`.
    pub fn load(path: &Path, home: &Path) -> (Config, Option<String>) {
        Config::load_with(path, home, macos_full_name().as_deref())
    }

    /// `load` with the macOS full name supplied, for the `user_name` default.
    pub fn load_with(
        path: &Path,
        home: &Path,
        full_name: Option<&str>,
    ) -> (Config, Option<String>) {
        let env_user = std::env::var("USER").ok();
        Config::load_inner(path, home, full_name, env_user.as_deref())
    }

    fn load_inner(
        path: &Path,
        home: &Path,
        full_name: Option<&str>,
        env_user: Option<&str>,
    ) -> (Config, Option<String>) {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let (c, err) = Config::resolve(Some(&text), home, full_name, env_user);
                (c, err.map(|e| format!("{}: {e}", path.display())))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Config::resolve(None, home, full_name, env_user)
            }
            Err(e) => {
                let (c, _) = Config::resolve(None, home, full_name, env_user);
                (c, Some(format!("{}: {e}", path.display())))
            }
        }
    }

    /// The config `text` gives on this machine, as `load` resolves it;
    /// `None` is a missing file. Text that does not parse gives the
    /// defaults and the error.
    pub fn load_text(text: Option<&str>, home: &Path, full_name: Option<&str>) -> Config {
        let env_user = std::env::var("USER").ok();
        Config::resolve(text, home, full_name, env_user.as_deref()).0
    }

    fn resolve(
        text: Option<&str>,
        home: &Path,
        full_name: Option<&str>,
        env_user: Option<&str>,
    ) -> (Config, Option<String>) {
        let (mut config, table, err) = match text {
            Some(text) => match toml::from_str::<Config>(text) {
                Ok(mut c) => {
                    c.fold_legacy_stage_keys(text);
                    let table = text.parse::<toml::Table>().unwrap_or_default();
                    (c, table, None)
                }
                Err(e) => (Config::default(), toml::Table::new(), Some(e.to_string())),
            },
            None => (Config::default(), toml::Table::new(), None),
        };
        let set = |key: &str| table.contains_key(key);
        let mut defaulted_overseer = false;
        if !set("brain_dir") && !set("notes_dir") {
            let vault = home.join("Repos/2ndBrain");
            if vault.is_dir() {
                config.brain_dir = vault;
            } else {
                config.brain_dir = home.join("OverseerNotes");
                defaulted_overseer = true;
            }
        }
        config.brain_dir = expand(&config.brain_dir, home);
        config.notes_owned = defaulted_overseer || config.brain_dir.join(OWNED_MARKER).exists();
        if !set("beads_dir") {
            config.beads_dir = config.brain_dir.clone();
        }
        if config.notes_owned {
            if !set("brain_writer") {
                config.brain_writer = true;
            }
            if !set("push") {
                config.push = false;
            }
            if !set("current_session") {
                config.current_session = config.brain_dir.join("CURRENT_SESSION.md");
            }
            // The default runs a script that lives in the maintainer's vault.
            if !set("sync_command") {
                config.sync_command.clear();
            }
        }
        config.jira_site = site_host(&config.jira_site);
        config.user_name = effective_user_name(&config.user_name, full_name, env_user);
        config.beads_dir = expand(&config.beads_dir, home);
        config.current_session = expand(&config.current_session, home);
        config.worktree_root = expand(&config.worktree_root, home);
        config.pr_repos = config.pr_repos.iter().map(|p| expand(p, home)).collect();
        (config, err)
    }
}

impl Config {
    /// Old config files name `stage_waiting`, `stage_testing` and
    /// `stage_in_review`. The first two stand in for `stage_blocked` and
    /// `stage_to_be_tested` when the new key is absent; the review names are
    /// appended to the In Progress list in effect.
    fn fold_legacy_stage_keys(&mut self, text: &str) {
        let Ok(table) = text.parse::<toml::Table>() else {
            return;
        };
        let list = |key: &str| -> Option<Vec<String>> {
            table.get(key)?.as_array().map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
        };
        if let Some(v) = list("stage_waiting").filter(|_| !table.contains_key("stage_blocked")) {
            self.stage_blocked = v;
        }
        if let Some(v) = list("stage_testing").filter(|_| !table.contains_key("stage_to_be_tested"))
        {
            self.stage_to_be_tested = v;
        }
        if let Some(v) = list("stage_in_review") {
            for name in v {
                if !self.stage_in_progress.contains(&name) {
                    self.stage_in_progress.push(name);
                }
            }
        }
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

    /// A temp home holding `Repos/2ndBrain`, as on the maintainer's machine.
    fn vault_home() -> tempdir::Dir {
        let d = tempdir::Dir::new();
        std::fs::create_dir_all(d.0.join("Repos/2ndBrain")).unwrap();
        d
    }

    fn missing() -> PathBuf {
        PathBuf::from("/nonexistent/config.toml")
    }

    #[test]
    fn missing_file_is_defaults() {
        let h = vault_home();
        let (c, err) = Config::load(&missing(), &h.0);
        assert!(err.is_none());
        assert_eq!(c.jira_poll_secs, 300);
        assert_eq!(c.beads_poll_secs, 30);
        assert_eq!(c.ticket_pattern, "");
        assert_eq!(c.jira_command[0], "acli");
        assert_eq!(c.beads_dir, h.0.join("Repos/2ndBrain"));
        assert_eq!(c.brain_dir, h.0.join("Repos/2ndBrain"));
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
            c.session_logs,
            vec![
                "Sessions/{yyyy}-{mm}-{dd}-{slot}-*.md",
                "Sessions/{yyyy}/{mm}-{Month}/{yyyy}-{mm}-{dd}-{slot}-*.md"
            ]
        );
        assert_eq!(c.slot_runs, "Sessions/Runs/{yyyy}-{mm}-{dd}-runs.jsonl");
        assert_eq!(c.session_log_min_mins, 15);
        assert_eq!(c.current_session, h.0.join(".claude/CURRENT_SESSION.md"));
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
        let h = vault_home();
        let (_d, p) = write("jql = \"project = X\"\nlock_idle_mins = 15\n");
        let (c, err) = Config::load(&p, &h.0);
        assert!(err.is_none());
        assert_eq!(c.jql, "project = X");
        assert_eq!(c.lock_idle_mins, 15);
        assert_eq!(c.jira_poll_secs, 300);
        assert_eq!(c.jira_command.len(), 9);
        assert_eq!(c.ticket_pattern, "");
        assert!(!c.brain_writer);
        assert_eq!(c.backup_mins, 5);

        let (_d, p) =
            write("brain_writer = true\nbackup_mins = 0\ncurrent_session = \"~/cs.md\"\n");
        let (c, err) = Config::load(&p, &h.0);
        assert!(err.is_none());
        assert!(c.brain_writer);
        assert_eq!(c.backup_mins, 0);
        assert_eq!(c.current_session, h.0.join("cs.md"));
        assert_eq!(c.shared_files.len(), 3);
    }

    #[test]
    fn bad_toml_falls_back_with_an_error() {
        let h = vault_home();
        let (_d, p) = write("jql = [not toml");
        let (c, err) = Config::load(&p, &h.0);
        assert!(err.is_some_and(|e| !e.is_empty()));
        assert_eq!(c.jira_poll_secs, 300);
        assert_eq!(c.beads_dir, h.0.join("Repos/2ndBrain"));
    }

    #[test]
    fn tilde_paths_expand() {
        let h = vault_home();
        let (_d, p) = write("beads_dir = \"~/work/beads\"\nbrain_dir = \"/abs/brain\"\n");
        let (c, err) = Config::load(&p, &h.0);
        assert!(err.is_none());
        assert_eq!(c.beads_dir, h.0.join("work/beads"));
        assert_eq!(c.brain_dir, PathBuf::from("/abs/brain"));
    }

    #[test]
    fn worktree_root_defaults_and_expands() {
        let h = vault_home();
        let (c, _) = Config::load(&missing(), &h.0);
        assert_eq!(c.worktree_root, h.0.join("Repos/.worktrees"));
        let (_d, p) = write("worktree_root = \"~/wt\"\n");
        let (c, err) = Config::load(&p, &h.0);
        assert!(err.is_none());
        assert_eq!(c.worktree_root, h.0.join("wt"));
        let (_d, p) = write("worktree_root = \"/abs/wt\"\n");
        let (c, _) = Config::load(&p, &h.0);
        assert_eq!(c.worktree_root, PathBuf::from("/abs/wt"));
    }

    #[test]
    fn sprint_keys_default_and_load() {
        let h = vault_home();
        let (c, _) = Config::load(&missing(), &h.0);
        assert_eq!(c.sprint_name, "");
        assert_eq!(c.sprint_start, "");
        assert_eq!(c.sprint_days, 10);
        let (_d, p) =
            write("sprint_name = \"Demo\"\nsprint_start = \"2026-11-09\"\nsprint_days = 5\n");
        let (c, err) = Config::load(&p, &h.0);
        assert!(err.is_none());
        assert_eq!(c.sprint_name, "Demo");
        assert_eq!(c.sprint_start, "2026-11-09");
        assert_eq!(c.sprint_days, 5);
    }

    #[test]
    fn pr_and_stage_keys_default_and_load() {
        let h = vault_home();
        let (c, err) = Config::load(&missing(), &h.0);
        assert!(err.is_none());
        assert!(c.pr_repos.is_empty());
        assert_eq!(
            c.pr_command,
            [
                "bb",
                "pullrequest",
                "list",
                "--state",
                "open",
                "--output",
                "json"
            ]
        );
        assert_eq!(c.pr_poll_secs, 300);
        assert_eq!(c.pr_author, "");
        assert_eq!(c.stage_todo, ["Backlog", "Prioritised Backlog", "To Do"]);
        assert_eq!(c.stage_blocked, ["Blocked", "Waiting", "On Hold"]);
        assert_eq!(c.stage_next, ["Next", "Selected for Development"]);
        assert_eq!(
            c.stage_in_progress,
            ["In Progress", "In Review", "Code Review", "Review"]
        );
        assert_eq!(
            c.stage_to_be_tested,
            ["To Be Tested", "Testing", "In Testing", "QA"]
        );
        assert_eq!(c.stage_done, ["Done", "Closed", "Resolved"]);

        let (_d, p) = write("");
        let (c, _) = Config::load(&p, &h.0);
        assert!(c.pr_repos.is_empty());
        assert_eq!(c.pr_poll_secs, 300);

        let (_d, p) =
            write("pr_repos = [\"~/Repos/demo-api\", \"/abs/demo-web\"]\npr_author = \"demo\"\n");
        let (c, err) = Config::load(&p, &h.0);
        assert!(err.is_none());
        assert_eq!(
            c.pr_repos,
            [h.0.join("Repos/demo-api"), PathBuf::from("/abs/demo-web")]
        );
        assert_eq!(c.pr_author, "demo");
    }

    #[test]
    fn legacy_stage_keys_load() {
        let h = vault_home();
        let (_d, p) = write("stage_waiting = [\"Parked\"]\nstage_in_review = [\"Peer Check\"]\n");
        let (c, err) = Config::load(&p, &h.0);
        assert!(err.is_none());
        assert_eq!(c.stage_blocked, ["Parked"]);
        assert_eq!(
            c.stage_in_progress,
            [
                "In Progress",
                "In Review",
                "Code Review",
                "Review",
                "Peer Check"
            ]
        );

        let (_d, p) = write("stage_testing = [\"UAT\"]\n");
        let (c, _) = Config::load(&p, &h.0);
        assert_eq!(c.stage_to_be_tested, ["UAT"]);

        let (_d, p) = write("stage_waiting = [\"A\"]\nstage_blocked = [\"B\"]\n");
        let (c, _) = Config::load(&p, &h.0);
        assert_eq!(c.stage_blocked, ["B"]);
    }

    #[test]
    fn pr_workspace_keys_default_and_load() {
        let h = vault_home();
        let (c, _) = Config::load(&missing(), &h.0);
        assert_eq!(c.pr_workspace, "");
        assert_eq!(c.pr_project, "");
        assert_eq!(c.pr_active_days, 30);
        assert_eq!(
            c.pr_repos_command,
            [
                "bb",
                "repo",
                "list",
                "--workspace",
                "{workspace}",
                "--output",
                "json"
            ]
        );
        let (_d, p) = write(
            "pr_workspace = \"demo-ws\"\npr_project = \"DEMO\"\npr_active_days = 0\npr_repos_command = [\"fake-bb\", \"{workspace}\"]\n",
        );
        let (c, err) = Config::load(&p, &h.0);
        assert!(err.is_none());
        assert_eq!(c.pr_workspace, "demo-ws");
        assert_eq!(c.pr_project, "DEMO");
        assert_eq!(c.pr_active_days, 0);
        assert_eq!(c.pr_repos_command, ["fake-bb", "{workspace}"]);
    }

    fn fresh_home() -> tempdir::Dir {
        tempdir::Dir::new()
    }

    #[test]
    fn no_file_with_2ndbrain_is_todays_setup() {
        let h = vault_home();
        let (c, err) = Config::load_with(&missing(), &h.0, Some("Demo Person"));
        assert!(err.is_none());
        assert_eq!(c.brain_dir, h.0.join("Repos/2ndBrain"));
        assert_eq!(c.beads_dir, h.0.join("Repos/2ndBrain"));
        assert!(!c.notes_owned);
        assert!(!c.brain_writer);
        assert!(c.push);
        assert_eq!(c.current_session, h.0.join(".claude/CURRENT_SESSION.md"));
        assert_eq!(c.jira_command[0], "acli");
        assert_eq!(c.ticket_pattern, "");
        assert_eq!(c.user_name, "Demo");
        assert_eq!(c.sync_command[0], "05-Meta/Sync-System/sync-run.sh");
    }

    #[test]
    fn no_file_without_vault_owns_overseer_notes_folder() {
        let h = fresh_home();
        let (c, _) = Config::load_with(&missing(), &h.0, Some("Demo Person"));
        assert_eq!(c.brain_dir, h.0.join("OverseerNotes"));
        assert_eq!(c.beads_dir, h.0.join("OverseerNotes"));
        assert!(c.notes_owned);
        assert!(c.brain_writer);
        assert!(!c.push);
        assert_eq!(
            c.current_session,
            h.0.join("OverseerNotes/CURRENT_SESSION.md")
        );
        assert!(c.sync_command.is_empty());
    }

    #[test]
    fn owned_folder_keeps_a_set_sync_command() {
        let h = fresh_home();
        let (_d, p) = write("sync_command = [\"demo-sync\"]\n");
        let (c, err) = Config::load_with(&p, &h.0, None);
        assert!(err.is_none());
        assert!(c.notes_owned);
        assert_eq!(c.sync_command, vec!["demo-sync".to_string()]);
    }

    #[test]
    fn notes_dir_alias_and_marker() {
        let h = fresh_home();
        std::fs::create_dir_all(h.0.join("n")).unwrap();
        std::fs::write(h.0.join("n").join(OWNED_MARKER), "").unwrap();
        let (_d, p) = write("notes_dir = \"~/n\"\n");
        let (c, err) = Config::load_with(&p, &h.0, None);
        assert!(err.is_none());
        assert_eq!(c.brain_dir, h.0.join("n"));
        assert!(c.notes_owned);
        assert!(c.brain_writer);

        let h = fresh_home();
        std::fs::create_dir_all(h.0.join("n")).unwrap();
        let (c, _) = Config::load_with(&p, &h.0, None);
        assert_eq!(c.brain_dir, h.0.join("n"));
        assert!(!c.notes_owned);
        assert!(!c.brain_writer);
    }

    #[test]
    fn jira_site_is_the_bare_host() {
        for raw in [
            "demo.atlassian.net",
            "https://demo.atlassian.net",
            "http://demo.atlassian.net/",
            " https://demo.atlassian.net// ",
        ] {
            assert_eq!(site_host(raw), "demo.atlassian.net", "{raw}");
        }
        let h = fresh_home();
        let (_d, p) = write("jira_site = \"https://demo.atlassian.net/\"\n");
        let (c, err) = Config::load_with(&p, &h.0, None);
        assert!(err.is_none());
        assert_eq!(c.jira_site, "demo.atlassian.net");
    }

    #[test]
    fn explicit_keys_win() {
        let h = fresh_home();
        let (_d, p) =
            write("brain_writer = false\npush = true\nticket_pattern = \"DEMO-\\\\d+\"\n");
        let (c, err) = Config::load_with(&p, &h.0, None);
        assert!(err.is_none());
        assert!(c.notes_owned);
        assert!(!c.brain_writer);
        assert!(c.push);
        assert_eq!(c.ticket_pattern, r"DEMO-\d+");
    }

    #[test]
    fn user_name_fallbacks() {
        let h = fresh_home();
        let load = |body: &str, full: Option<&str>, env: Option<&str>| {
            let (_d, p) = write(body);
            Config::load_inner(&p, &h.0, full, env).0.user_name
        };
        assert_eq!(load("", Some("Demo Person"), Some("dp")), "Demo");
        assert_eq!(load("", None, Some("dp")), "dp");
        assert_eq!(load("", None, None), "you");
        assert_eq!(load("", Some("  "), Some("dp")), "dp");
        assert_eq!(
            load("user_name = \"Sam\"\n", Some("Demo Person"), Some("dp")),
            "Sam"
        );
        // An explicit empty name renders as the default, never as nothing.
        assert_eq!(
            load("user_name = \"\"\n", Some("Demo Person"), Some("dp")),
            "Demo"
        );
        assert_eq!(load("user_name = \"  \"\n", None, None), "you");
    }

    #[test]
    fn user_is_never_empty() {
        let mut c = Config::default();
        assert_eq!(c.user(), "you");
        c.user_name = "Demo".into();
        assert_eq!(c.user(), "Demo");
    }
}
