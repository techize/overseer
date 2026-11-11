//! What it takes to start `claude` the way Overseer wants: generated
//! per-slot settings (the hooks), a role prompt and a session id.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use overseer_proto::msg::{slot_name, SlotId};

/// Compiled-in defaults, written to `<state>/roles/` on first use so
/// Jonathan can edit the copies. S0 uses the coordinator role, S1 to S6 the
/// worker role, and kickoff.md is the first prompt S0 starts with.
const WORKER_ROLE: &str = include_str!("roles/worker.md");
const COORDINATOR_ROLE: &str = include_str!("roles/coordinator.md");
const KICKOFF: &str = include_str!("roles/kickoff.md");

/// Writes `default` to `roles/<name>` if the file is missing and returns its
/// path. create_new: never clobber an edited template, even when two starts
/// race.
fn ensure_template(roles: &Path, name: &str, default: &str) -> io::Result<PathBuf> {
    std::fs::create_dir_all(roles)?;
    let path = roles.join(name);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut f) => io::Write::write_all(&mut f, default.as_bytes())?,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    Ok(path)
}

/// The text S0 starts with, from `<state>/roles/kickoff.md`.
pub fn kickoff_prompt(state_dir: &Path) -> io::Result<String> {
    let path = ensure_template(&state_dir.join("roles"), "kickoff.md", KICKOFF)?;
    Ok(std::fs::read_to_string(path)?.trim().to_string())
}

/// The first prompt for a start. An explicit prompt wins; otherwise a fresh
/// S0 gets the kickoff. Resumes get none (the session already has its
/// history), and neither do other slots. An empty or whitespace-only prompt
/// (`--prompt ""`, an emptied kickoff.md) counts as none: claude would
/// otherwise be started with an empty positional prompt.
pub fn first_prompt(
    state_dir: &Path,
    slot: SlotId,
    start: &Start,
    explicit: Option<String>,
) -> io::Result<Option<String>> {
    let explicit = explicit.filter(|p| !p.trim().is_empty());
    if explicit.is_some() || slot != 0 || !matches!(start, Start::New(_)) {
        return Ok(explicit);
    }
    Ok(Some(kickoff_prompt(state_dir)?).filter(|p| !p.is_empty()))
}

/// The hook events Overseer listens to.
const HOOK_EVENTS: [&str; 5] = [
    "SessionStart",
    "UserPromptSubmit",
    "Notification",
    "Stop",
    "PostToolUse",
];

/// Files generated for one slot's start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchFiles {
    pub settings: PathBuf,
    pub role: PathBuf,
}

/// How the session id is given to claude.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Start {
    /// A fresh session with this (uuid) id.
    New(String),
    /// Continue the session with this id.
    Resume(String),
}

/// Claude's folder of saved conversations: `$CLAUDE_CONFIG_DIR/projects`,
/// else `$HOME/.claude/projects`, read from the slots' environment.
pub fn claude_projects_dir(env: &[(OsString, OsString)]) -> Option<PathBuf> {
    let var = |name: &str| {
        env.iter()
            .find(|(k, v)| k == name && !v.is_empty())
            .map(|(_, v)| PathBuf::from(v))
    };
    var("CLAUDE_CONFIG_DIR")
        .or_else(|| var("HOME").map(|h| h.join(".claude")))
        .map(|d| d.join("projects"))
}

/// True when a project folder under `projects` holds `<session_id>.jsonl`.
/// Claude writes it only once the session has a message, so a slot started
/// and never used has none, and `--resume` on it exits at once with "No
/// conversation found". The folder name is Claude's encoding of the working
/// directory, so every folder is checked rather than guessing the encoding.
pub fn has_transcript(projects: &Path, session_id: &str) -> bool {
    let file = format!("{session_id}.jsonl");
    std::fs::read_dir(projects)
        .is_ok_and(|dirs| dirs.flatten().any(|d| d.path().join(&file).is_file()))
}

/// True when the command is Claude Code (basename of `argv[0]` is `claude`).
/// Anything else is run as given, with no flags and no files.
pub fn is_claude(argv: &[String]) -> bool {
    argv.first()
        .and_then(|a| Path::new(a).file_name())
        .is_some_and(|n| n == "claude")
}

/// Quotes `s` for a POSIX shell, since the hook command is run through one
/// and the binary path can contain spaces (Application Support).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn settings_json(overseer_bin: &Path) -> String {
    let bin = shell_quote(&overseer_bin.to_string_lossy());
    let mut hooks = serde_json::Map::new();
    for event in HOOK_EVENTS {
        let mut entry = serde_json::Map::new();
        if event == "PostToolUse" {
            entry.insert("matcher".into(), "*".into());
        }
        entry.insert(
            "hooks".into(),
            serde_json::json!([{
                "type": "command",
                "command": format!("{bin} hook {event}"),
                "timeout": 2,
            }]),
        );
        hooks.insert(event.into(), serde_json::json!([entry]));
    }
    serde_json::to_string_pretty(&serde_json::json!({ "hooks": hooks }))
        .expect("settings serialise")
}

/// Writes `slots/S<n>/settings.json` and `role.md` under `state_dir`,
/// replacing both on every start. The role is copied from
/// `roles/coordinator.md` for S0 and `roles/worker.md` otherwise (created from the compiled-in default if missing, never
/// overwritten) with `{slot}`, `{label}` and `{overseer}` filled in.
pub fn write_slot_files(
    state_dir: &Path,
    slot: SlotId,
    label: &str,
    overseer_bin: &Path,
) -> io::Result<LaunchFiles> {
    let roles = state_dir.join("roles");
    // All defaults are written so Jonathan finds every file to edit.
    let worker = ensure_template(&roles, "worker.md", WORKER_ROLE)?;
    let coordinator = ensure_template(&roles, "coordinator.md", COORDINATOR_ROLE)?;
    ensure_template(&roles, "kickoff.md", KICKOFF)?;
    let template_path = if slot == 0 { coordinator } else { worker };
    let template = std::fs::read_to_string(&template_path)?;
    // The label goes in last so braces inside it are not expanded again.
    let role_text = template
        .replace("{slot}", &slot_name(slot))
        .replace("{overseer}", &overseer_bin.to_string_lossy())
        .replace("{label}", label);

    let dir = state_dir.join("slots").join(slot_name(slot));
    std::fs::create_dir_all(&dir)?;
    let files = LaunchFiles {
        settings: dir.join("settings.json"),
        role: dir.join("role.md"),
    };
    std::fs::write(&files.settings, settings_json(overseer_bin))?;
    std::fs::write(&files.role, role_text)?;
    Ok(files)
}

/// `base` plus Overseer's flags. Each value is its own argument, so a label
/// with spaces needs no quoting. A first prompt goes last, as the positional
/// argument, on a fresh start only: it is passed at launch because text typed
/// before Claude's input box is ready can be lost.
pub fn claude_argv(
    base: &[String],
    slot: SlotId,
    label: &str,
    files: &LaunchFiles,
    start: Start,
    prompt: Option<&str>,
) -> Vec<String> {
    let mut argv = base.to_vec();
    let fresh = matches!(start, Start::New(_));
    match start {
        Start::New(id) => argv.extend(["--session-id".into(), id]),
        Start::Resume(id) => argv.extend(["--resume".into(), id]),
    }
    argv.extend([
        "--name".into(),
        format!("{} {label}", slot_name(slot))
            .trim_end()
            .to_string(),
        "--settings".into(),
        files.settings.to_string_lossy().into_owned(),
        "--append-system-prompt-file".into(),
        files.role.to_string_lossy().into_owned(),
    ]);
    if let (true, Some(p)) = (fresh, prompt) {
        // "--" so a prompt starting with "-" is not parsed as a flag.
        argv.push("--".into());
        argv.push(p.to_string());
    }
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_dir_prefers_claude_config_dir() {
        let home = vec![(OsString::from("HOME"), OsString::from("/Users/j"))];
        assert_eq!(
            claude_projects_dir(&home),
            Some(PathBuf::from("/Users/j/.claude/projects"))
        );
        let mut both = home.clone();
        both.push(("CLAUDE_CONFIG_DIR".into(), "/cfg".into()));
        assert_eq!(
            claude_projects_dir(&both),
            Some(PathBuf::from("/cfg/projects"))
        );
        assert_eq!(claude_projects_dir(&[]), None);
    }

    #[test]
    fn transcript_is_found_in_any_project_folder() {
        let projects = temp("transcript").join("projects");
        assert!(!has_transcript(&projects, "abc"), "missing folder");
        std::fs::create_dir_all(projects.join("-Users-j-repo")).unwrap();
        assert!(!has_transcript(&projects, "abc"));
        std::fs::write(projects.join("-Users-j-repo").join("abc.jsonl"), "{}").unwrap();
        assert!(has_transcript(&projects, "abc"));
        assert!(!has_transcript(&projects, "ab"));
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn files() -> LaunchFiles {
        LaunchFiles {
            settings: "/st/slots/S2/settings.json".into(),
            role: "/st/slots/S2/role.md".into(),
        }
    }

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ovl-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn only_claude_gets_flags() {
        assert!(!is_claude(&s(&["/bin/sh"])));
        assert!(is_claude(&s(&["/opt/homebrew/bin/claude", "--model", "x"])));
        assert!(is_claude(&s(&["claude"])));
        assert!(!is_claude(&[]));
        let base = s(&["/opt/homebrew/bin/claude", "--model", "x"]);
        let argv = claude_argv(&base, 2, "api", &files(), Start::New("uuid-1".into()), None);
        assert_eq!(&argv[..3], &base[..]);
        assert_eq!(
            &argv[3..],
            &s(&[
                "--session-id",
                "uuid-1",
                "--name",
                "S2 api",
                "--settings",
                "/st/slots/S2/settings.json",
                "--append-system-prompt-file",
                "/st/slots/S2/role.md",
            ])[..]
        );
    }

    #[test]
    fn resume_uses_resume_not_session_id() {
        let argv = claude_argv(
            &s(&["claude"]),
            2,
            "api",
            &files(),
            Start::Resume("uuid-9".into()),
            None,
        );
        assert!(!argv.contains(&"--session-id".to_string()));
        let i = argv.iter().position(|a| a == "--resume").unwrap();
        assert_eq!(argv[i + 1], "uuid-9");
    }

    #[test]
    fn settings_file_has_five_hooks_pointing_at_the_binary() {
        let dir = temp("settings");
        let bin = Path::new("/Applications/My App/overseer");
        let f = write_slot_files(&dir, 3, "api", bin).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&f.settings).unwrap()).unwrap();
        let hooks = v["hooks"].as_object().unwrap();
        assert_eq!(hooks.len(), 5);
        for ev in HOOK_EVENTS {
            let entry = &hooks[ev][0];
            let h = &entry["hooks"][0];
            assert_eq!(h["type"], "command");
            assert_eq!(h["timeout"], 2);
            assert_eq!(
                h["command"],
                format!("'/Applications/My App/overseer' hook {ev}")
            );
            if ev == "PostToolUse" {
                assert_eq!(entry["matcher"], "*");
            } else {
                assert!(entry.get("matcher").is_none(), "{ev}");
            }
        }
        let role = std::fs::read_to_string(&f.role).unwrap();
        assert!(role.contains("You are S3 of an Overseer team"), "{role}");
        assert!(role.contains("working on api."), "{role}");
        assert!(f.settings.ends_with("slots/S3/settings.json"));
    }

    #[test]
    fn edited_role_template_is_kept() {
        let dir = temp("template");
        write_slot_files(&dir, 1, "a", Path::new("/o")).unwrap();
        std::fs::write(
            dir.join("roles/worker.md"),
            "mine {slot} {label} {overseer}",
        )
        .unwrap();
        let f = write_slot_files(&dir, 1, "{slot}", Path::new("/o")).unwrap();
        assert_eq!(
            std::fs::read_to_string(f.role).unwrap(),
            "mine S1 {slot} /o"
        );
    }

    #[test]
    fn label_with_spaces_stays_one_argument() {
        let argv = claude_argv(
            &s(&["claude"]),
            1,
            "fix the login bug",
            &files(),
            Start::New("u".into()),
            None,
        );
        let i = argv.iter().position(|a| a == "--name").unwrap();
        assert_eq!(argv[i + 1], "S1 fix the login bug");
    }

    #[test]
    fn prompt_is_last_positional_on_new() {
        let argv = claude_argv(
            &s(&["claude"]),
            2,
            "api",
            &files(),
            Start::New("u".into()),
            Some("do the thing"),
        );
        assert_eq!(argv.last().unwrap(), "do the thing");
        assert_eq!(argv[argv.len() - 2], "--");
        assert_eq!(argv[argv.len() - 3], "/st/slots/S2/role.md");
    }

    #[test]
    fn dash_prompt_follows_double_dash() {
        for p in ["--help me", "- fix login"] {
            let argv = claude_argv(
                &s(&["claude"]),
                2,
                "api",
                &files(),
                Start::New("u".into()),
                Some(p),
            );
            assert_eq!(argv.last().unwrap(), p);
            assert_eq!(argv[argv.len() - 2], "--");
        }
        let argv = claude_argv(
            &s(&["claude"]),
            2,
            "api",
            &files(),
            Start::New("u".into()),
            None,
        );
        assert!(!argv.contains(&"--".to_string()));
    }

    #[test]
    fn prompt_is_dropped_on_resume() {
        let argv = claude_argv(
            &s(&["claude"]),
            2,
            "api",
            &files(),
            Start::Resume("u".into()),
            Some("do the thing"),
        );
        assert!(!argv.contains(&"do the thing".to_string()));
    }

    #[test]
    fn s0_fresh_gets_the_kickoff() {
        let dir = temp("kick");
        let p = first_prompt(&dir, 0, &Start::New("u".into()), None).unwrap();
        assert_eq!(
            p.as_deref(),
            Some("Run the start-of-day round: check beads and Jira, check the board, and propose who works on what.")
        );
        // An explicit prompt wins; other slots get none.
        let p = first_prompt(&dir, 0, &Start::New("u".into()), Some("mine".into())).unwrap();
        assert_eq!(p.as_deref(), Some("mine"));
        assert_eq!(
            first_prompt(&dir, 2, &Start::New("u".into()), None).unwrap(),
            None
        );
    }

    #[test]
    fn empty_prompts_are_none() {
        let dir = temp("empty");
        let fresh = Start::New("u".into());
        for p in ["", "  \n\t "] {
            assert_eq!(
                first_prompt(&dir, 2, &fresh, Some(p.into())).unwrap(),
                None,
                "{p:?}"
            );
            // On S0 an empty prompt counts as none given, so the kickoff runs.
            assert_eq!(
                first_prompt(&dir, 0, &fresh, Some(p.into())).unwrap(),
                Some(kickoff_prompt(&dir).unwrap()),
                "{p:?}"
            );
        }
        // An emptied kickoff.md gives no prompt rather than `-- ""`.
        std::fs::create_dir_all(dir.join("roles")).unwrap();
        std::fs::write(dir.join("roles/kickoff.md"), " \n").unwrap();
        assert_eq!(first_prompt(&dir, 0, &fresh, None).unwrap(), None);
    }

    #[test]
    fn s0_resumed_gets_no_kickoff() {
        let dir = temp("nokick");
        let p = first_prompt(&dir, 0, &Start::Resume("u".into()), None).unwrap();
        assert_eq!(p, None);
    }

    #[test]
    fn edited_template_is_not_overwritten() {
        let dir = temp("keep");
        write_slot_files(&dir, 0, "a", Path::new("/o")).unwrap();
        std::fs::write(dir.join("roles/coordinator.md"), "my coordinator").unwrap();
        std::fs::write(dir.join("roles/kickoff.md"), "my kickoff").unwrap();
        let f = write_slot_files(&dir, 0, "a", Path::new("/o")).unwrap();
        assert_eq!(std::fs::read_to_string(f.role).unwrap(), "my coordinator");
        assert_eq!(kickoff_prompt(&dir).unwrap(), "my kickoff");
    }

    #[test]
    fn slot_zero_uses_the_coordinator_and_others_the_worker() {
        let dir = temp("which");
        let o = Path::new("/Apps/overseer");
        let c = std::fs::read_to_string(write_slot_files(&dir, 0, "", o).unwrap().role).unwrap();
        let w = std::fs::read_to_string(write_slot_files(&dir, 4, "x", o).unwrap().role).unwrap();
        assert!(c.contains("bd ready --json"), "{c}");
        assert!(!c.contains("You are S0 of an Overseer team"), "{c}");
        assert!(w.contains("You are S4 of an Overseer team"), "{w}");
    }

    #[test]
    fn overseer_path_with_a_space_is_quoted() {
        let dir = temp("space");
        let o = Path::new("/Applications/My App/overseer");
        let f = write_slot_files(&dir, 0, "", o).unwrap();
        let role = std::fs::read_to_string(f.role).unwrap();
        assert!(
            role.contains("\"/Applications/My App/overseer\" ctl status"),
            "{role}"
        );
    }

    #[test]
    fn placeholders_are_filled() {
        let dir = temp("fill");
        let f = write_slot_files(&dir, 0, "lbl", Path::new("/Apps/overseer")).unwrap();
        let role = std::fs::read_to_string(f.role).unwrap();
        assert!(role.contains("\"/Apps/overseer\" ctl start"), "{role}");
        assert!(
            !role.contains("{overseer}") && !role.contains("{slot}"),
            "{role}"
        );
        assert!(!role.contains('\u{2014}'), "no em dashes");
        // Bead and Jira text must not be shell-expanded.
        assert!(
            role.contains("ctl start <n> --cwd <dir> --label '<item key>' --prompt '<text>'"),
            "{role}"
        );
        assert!(role.contains("ctl send <n> '<text>'"), "{role}");
        assert!(role.contains("'\\''"), "{role}");
        assert!(!role.contains("--prompt \""), "{role}");
    }
}
