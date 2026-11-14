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
/// What `{brain_rules}` expands to while the 2ndBrain writer is on.
const BRAIN_RULES_WORKER: &str = include_str!("roles/brain_worker.md");
const BRAIN_RULES_COORDINATOR: &str = include_str!("roles/brain_coordinator.md");
const KICKOFF: &str = include_str!("roles/kickoff.md");
/// The prompt built from a slot's assigned work items.
pub const ASSIGNMENT: &str = include_str!("roles/assignment.md");

/// Stable hashes (FNV-1a 64 over the text with trailing whitespace removed)
/// of role files Overseer shipped before and since replaced. A file on disk
/// that matches one was never edited and is upgraded in place.
const PREVIOUS_COORDINATOR: &[u64] = &[
    0x79c8ca136fcc4cb5,
    0x2053126dcf7ec248,
    0xfb1eff022d50a360,
    0x4baeb879ce73cf13,
    0x7ddaa223f4bdd1f6,
    0xf98858507513ed2d,
    0x551d58f77936ffa0, // phase 4
    0xbbd3513e7aa77734, // phase 5
];
const PREVIOUS_WORKER: &[u64] = &[
    0x215de771dfd84eb4,
    0xbd3ab0020d4d25b1,
    0xe8deae3b8fe65917,
    0x751ce33e657e6c1f, // phase 5
];
const PREVIOUS_KICKOFF: &[u64] = &[
    0x05db44420ef92039, // phase 5
];
const PREVIOUS_ASSIGNMENT: &[u64] = &[
    0x7563493817edf95a, // phase 5
];

fn text_hash(text: &str) -> u64 {
    text.trim_end().bytes().fold(0xcbf29ce484222325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
    })
}

/// Makes `roles/<name>` usable and returns its path. A missing file is
/// created with `default` (create_new, so two racing starts cannot clobber
/// each other). A file equal to `default` or to a `previous` default was not
/// edited: it is replaced by `default` through a temp file and a rename. An
/// edited file is kept and `default` is written beside it as `<name>.new`.
fn ensure_template(
    roles: &Path,
    name: &str,
    default: &str,
    previous: &[u64],
) -> io::Result<PathBuf> {
    std::fs::create_dir_all(roles)?;
    let path = roles.join(name);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut f) => {
            io::Write::write_all(&mut f, default.as_bytes())?;
            return Ok(path);
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let Ok(on_disk) = std::fs::read(&path) else {
        return Ok(path);
    };
    let Ok(on_disk) = String::from_utf8(on_disk) else {
        return Ok(path);
    };
    let current = text_hash(default);
    let have = text_hash(&on_disk);
    if have == current {
        return Ok(path);
    }
    // Best effort from here: the file on disk is usable either way.
    if previous.contains(&have) {
        match replace_file(&path, default) {
            Ok(()) => tracing::info!(
                "upgraded role template {} to the current default",
                path.display()
            ),
            Err(e) => tracing::warn!("cannot upgrade role template {}: {e}", path.display()),
        }
    } else {
        let new = roles.join(format!("{name}.new"));
        let up_to_date = std::fs::read_to_string(&new).is_ok_and(|t| t == default);
        if !up_to_date {
            match replace_file(&new, default) {
                Ok(()) => tracing::warn!(
                    "role template {} was edited; the current default is in {}",
                    path.display(),
                    new.display()
                ),
                Err(e) => tracing::warn!("cannot write {}: {e}", new.display()),
            }
        }
    }
    Ok(path)
}

/// Writes `text` to `path` through a temp file in the same folder.
fn replace_file(path: &Path, text: &str) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    tmp.push(format!(".tmp{}-{n}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// The text S0 starts with, from `<state>/roles/kickoff.md`.
pub fn kickoff_prompt(state_dir: &Path) -> io::Result<String> {
    let path = ensure_template(
        &state_dir.join("roles"),
        "kickoff.md",
        KICKOFF,
        PREVIOUS_KICKOFF,
    )?;
    Ok(std::fs::read_to_string(path)?.trim().to_string())
}

/// The assignment template from `<state>/roles/assignment.md`.
pub fn assignment_template(state_dir: &Path) -> io::Result<String> {
    let path = ensure_template(
        &state_dir.join("roles"),
        "assignment.md",
        ASSIGNMENT,
        PREVIOUS_ASSIGNMENT,
    )?;
    std::fs::read_to_string(path)
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
const HOOK_EVENTS: [&str; 6] = [
    "SessionStart",
    "UserPromptSubmit",
    "Notification",
    "Stop",
    "PostToolUse",
    "PreToolUse",
];

/// The tool matcher and timeout (seconds) of each hook. `PreToolUse` is the
/// lock and 2ndBrain write check, which may take up to 1 s, so it gets more
/// room; it runs for Bash and the file-writing tools.
fn hook_shape(event: &str) -> (Option<&'static str>, u64) {
    match event {
        "PostToolUse" => (Some("*"), 2),
        "PreToolUse" => (Some("Bash|Edit|Write|MultiEdit|NotebookEdit"), 5),
        _ => (None, 2),
    }
}

/// The Overseer MCP tools a session may use without being asked.
const ALLOWED_TOOLS: [&str; 14] = [
    "mcp__overseer__claim_target",
    "mcp__overseer__release_target",
    "mcp__overseer__notify_session",
    "mcp__overseer__list_targets",
    "mcp__overseer__commit_2ndbrain",
    "mcp__overseer__append_journal",
    "mcp__overseer__edit_shared_file",
    "mcp__overseer__update_current_session",
    "mcp__overseer__get_board",
    "mcp__overseer__get_work_items",
    "mcp__overseer__propose_plan",
    "mcp__overseer__request_status",
    "mcp__overseer__report_progress",
    "mcp__overseer__set_board_summary",
];

/// Files generated for one slot's start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchFiles {
    pub settings: PathBuf,
    pub role: PathBuf,
    /// The MCP config naming `overseer mcp` for this slot.
    pub mcp: PathBuf,
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
        let (matcher, timeout) = hook_shape(event);
        let mut entry = serde_json::Map::new();
        if let Some(m) = matcher {
            entry.insert("matcher".into(), m.into());
        }
        entry.insert(
            "hooks".into(),
            serde_json::json!([{
                "type": "command",
                "command": format!("{bin} hook {event}"),
                "timeout": timeout,
            }]),
        );
        hooks.insert(event.into(), serde_json::json!([entry]));
    }
    serde_json::to_string_pretty(&serde_json::json!({
        "hooks": hooks,
        "permissions": { "allow": ALLOWED_TOOLS },
    }))
    .expect("settings serialise")
}

/// The slot's MCP config: `overseer mcp` over stdio, told its slot.
fn mcp_json(overseer_bin: &Path, slot: SlotId) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "mcpServers": {
            "overseer": {
                "type": "stdio",
                "command": overseer_bin.to_string_lossy(),
                "args": ["mcp"],
                "env": { "OVERSEER_SLOT": slot_name(slot) },
            }
        }
    }))
    .expect("mcp config serialises")
}

/// The folders a role names, from the daemon's config: `{brain}` is the
/// 2ndBrain folder S0 works in and reads the journal from, `{beads}` where
/// it runs `bd`. The rest is the 2ndBrain writer's: whether it is on (so
/// `{brain_rules}` has content), the session note template and the
/// `CURRENT_SESSION.md` path the rules name.
#[derive(Debug, Clone, PartialEq)]
pub struct RoleDirs {
    pub brain: PathBuf,
    pub beads: PathBuf,
    pub brain_writer: bool,
    pub current_session: PathBuf,
}

impl Default for RoleDirs {
    /// As the config defaults name them, unexpanded.
    fn default() -> Self {
        RoleDirs {
            brain: PathBuf::from("~/Repos/2ndBrain"),
            beads: PathBuf::from("~/Repos/2ndBrain"),
            brain_writer: false,
            current_session: PathBuf::from("~/.claude/CURRENT_SESSION.md"),
        }
    }
}

/// True when the writer is on but the role text has no `{brain_rules}`: an
/// edited file from before the placeholder existed.
fn lacks_brain_rules(template: &str, writer_on: bool) -> bool {
    writer_on && !template.contains("{brain_rules}")
}

/// Writes `slots/S<n>/settings.json`, `mcp.json` and `role.md` under
/// `state_dir`, replacing them on every start. The role is copied from
/// `roles/coordinator.md` for S0 and `roles/worker.md` otherwise (created from the compiled-in default if missing;
/// an unedited older default is upgraded, an edited file is kept) with `{slot}`, `{label}`, `{overseer}`,
/// `{brain}`, `{beads}`, `{session_note}` (`note`, vault-relative) and `{current_session}` filled in,
/// and `{brain_rules}` expanded when the 2ndBrain writer is on.
pub fn write_slot_files(
    state_dir: &Path,
    slot: SlotId,
    label: &str,
    overseer_bin: &Path,
    dirs: &RoleDirs,
    note: Option<&str>,
) -> io::Result<LaunchFiles> {
    let roles = state_dir.join("roles");
    // All defaults are written so Jonathan finds every file to edit.
    let worker = ensure_template(&roles, "worker.md", WORKER_ROLE, PREVIOUS_WORKER)?;
    let coordinator = ensure_template(
        &roles,
        "coordinator.md",
        COORDINATOR_ROLE,
        PREVIOUS_COORDINATOR,
    )?;
    ensure_template(&roles, "kickoff.md", KICKOFF, PREVIOUS_KICKOFF)?;
    ensure_template(&roles, "assignment.md", ASSIGNMENT, PREVIOUS_ASSIGNMENT)?;
    let template_path = if slot == 0 { coordinator } else { worker };
    let template = std::fs::read_to_string(&template_path)?;
    if lacks_brain_rules(&template, dirs.brain_writer) {
        let which = if slot == 0 {
            "coordinator.md"
        } else {
            "worker.md"
        };
        tracing::warn!(
            "role {which} is edited and has no {{brain_rules}}: add it, see {which}.new"
        );
    }
    // `{brain_rules}` goes first: its text holds more placeholders. Off, it
    // leaves nothing behind, not even its line break.
    let template = if dirs.brain_writer {
        let rules = if slot == 0 {
            BRAIN_RULES_COORDINATOR
        } else {
            BRAIN_RULES_WORKER
        };
        template.replace("{brain_rules}", &format!("\n{}", rules.trim_end()))
    } else if template.contains("\n{brain_rules}") {
        template.replace("\n{brain_rules}", "")
    } else {
        template.replace("{brain_rules}", "")
    };
    // The session note is the ledger's, vault-relative. With none, its line
    // is left out rather than naming a path the hook would not treat as the
    // slot's note.
    let template = match note {
        Some(rel) => template.replace(
            "{session_note}",
            &Path::new(&dirs.brain).join(rel).to_string_lossy(),
        ),
        None => template
            .split_inclusive('\n')
            .filter(|l| !l.contains("{session_note}"))
            .collect(),
    };
    // The label goes in last so braces inside it are not expanded again.
    let role_text = template
        .replace("{current_session}", &dirs.current_session.to_string_lossy())
        .replace("{slot}", &slot_name(slot))
        .replace("{overseer}", &overseer_bin.to_string_lossy())
        .replace("{brain}", &dirs.brain.to_string_lossy())
        .replace("{beads}", &dirs.beads.to_string_lossy())
        .replace("{label}", label);

    let dir = state_dir.join("slots").join(slot_name(slot));
    std::fs::create_dir_all(&dir)?;
    let files = LaunchFiles {
        settings: dir.join("settings.json"),
        role: dir.join("role.md"),
        mcp: dir.join("mcp.json"),
    };
    std::fs::write(&files.settings, settings_json(overseer_bin))?;
    std::fs::write(&files.mcp, mcp_json(overseer_bin, slot))?;
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
        "--mcp-config".into(),
        files.mcp.to_string_lossy().into_owned(),
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
            mcp: "/st/slots/S2/mcp.json".into(),
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
                "--mcp-config",
                "/st/slots/S2/mcp.json",
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

    fn json(path: &Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn settings_file_has_six_hooks() {
        let dir = temp("settings");
        let bin = Path::new("/Applications/My App/overseer");
        let f = write_slot_files(&dir, 3, "api", bin, &RoleDirs::default(), None).unwrap();
        let v = json(&f.settings);
        let hooks = v["hooks"].as_object().unwrap();
        assert_eq!(hooks.len(), 6);
        for ev in HOOK_EVENTS {
            let entry = &hooks[ev][0];
            let h = &entry["hooks"][0];
            assert_eq!(h["type"], "command");
            assert_eq!(
                h["command"],
                format!("'/Applications/My App/overseer' hook {ev}")
            );
            match ev {
                "PostToolUse" => {
                    assert_eq!(entry["matcher"], "*");
                    assert_eq!(h["timeout"], 2);
                }
                "PreToolUse" => {
                    assert_eq!(entry["matcher"], "Bash|Edit|Write|MultiEdit|NotebookEdit");
                    assert_eq!(h["timeout"], 5);
                }
                _ => {
                    assert!(entry.get("matcher").is_none(), "{ev}");
                    assert_eq!(h["timeout"], 2);
                }
            }
        }
        let role = std::fs::read_to_string(&f.role).unwrap();
        assert!(role.contains("You are S3 of an Overseer team"), "{role}");
        assert!(role.contains("working on api."), "{role}");
        assert!(f.settings.ends_with("slots/S3/settings.json"));
    }

    #[test]
    fn settings_allow_the_overseer_tools() {
        let dir = temp("allow");
        let f =
            write_slot_files(&dir, 2, "api", Path::new("/o"), &RoleDirs::default(), None).unwrap();
        let v = json(&f.settings);
        assert_eq!(
            v["permissions"]["allow"],
            serde_json::json!([
                "mcp__overseer__claim_target",
                "mcp__overseer__release_target",
                "mcp__overseer__notify_session",
                "mcp__overseer__list_targets",
                "mcp__overseer__commit_2ndbrain",
                "mcp__overseer__append_journal",
                "mcp__overseer__edit_shared_file",
                "mcp__overseer__update_current_session",
                "mcp__overseer__get_board",
                "mcp__overseer__get_work_items",
                "mcp__overseer__propose_plan",
                "mcp__overseer__request_status",
                "mcp__overseer__report_progress",
                "mcp__overseer__set_board_summary",
            ])
        );
    }

    #[test]
    fn mcp_file_names_the_slot() {
        let dir = temp("mcp");
        let bin = Path::new("/Applications/My App/overseer");
        let f = write_slot_files(&dir, 4, "api", bin, &RoleDirs::default(), None).unwrap();
        assert!(f.mcp.ends_with("slots/S4/mcp.json"));
        assert_eq!(
            json(&f.mcp),
            serde_json::json!({"mcpServers": {"overseer": {
                "type": "stdio",
                "command": "/Applications/My App/overseer",
                "args": ["mcp"],
                "env": {"OVERSEER_SLOT": "S4"},
            }}})
        );
    }

    #[test]
    fn argv_passes_mcp_config() {
        let argv = claude_argv(
            &s(&["claude"]),
            2,
            "api",
            &files(),
            Start::Resume("u".into()),
            None,
        );
        let i = argv.iter().position(|a| a == "--mcp-config").unwrap();
        assert_eq!(argv[i + 1], "/st/slots/S2/mcp.json");
        assert_eq!(argv[i - 2], "--settings");
        assert_eq!(argv.iter().filter(|a| *a == "--mcp-config").count(), 1);
    }

    #[test]
    fn edited_role_template_is_kept() {
        let dir = temp("template");
        write_slot_files(&dir, 1, "a", Path::new("/o"), &RoleDirs::default(), None).unwrap();
        std::fs::write(
            dir.join("roles/worker.md"),
            "mine {slot} {label} {overseer}",
        )
        .unwrap();
        let f = write_slot_files(
            &dir,
            1,
            "{slot}",
            Path::new("/o"),
            &RoleDirs::default(),
            None,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(f.role).unwrap(),
            "mine S1 {slot} /o"
        );
    }

    #[test]
    fn old_shipped_default_is_upgraded() {
        let dir = temp("upgrade");
        let roles = dir.join("roles");
        std::fs::create_dir_all(&roles).unwrap();
        let old = "old shipped text\n";
        std::fs::write(roles.join("x.md"), format!("{old}\n")).unwrap();
        let prev = [text_hash(old)];
        let p = ensure_template(&roles, "x.md", "new default\n", &prev).unwrap();
        assert_eq!(std::fs::read_to_string(p).unwrap(), "new default\n");
        assert!(!roles.join("x.md.new").exists());
    }

    #[test]
    fn edited_template_is_kept_and_new_default_written_beside_it() {
        let dir = temp("edited");
        let roles = dir.join("roles");
        std::fs::create_dir_all(&roles).unwrap();
        std::fs::write(roles.join("x.md"), "mine").unwrap();
        std::fs::write(roles.join("x.md.new"), "stale").unwrap();
        let p = ensure_template(&roles, "x.md", "new default\n", &[1]).unwrap();
        assert_eq!(std::fs::read_to_string(p).unwrap(), "mine");
        assert_eq!(
            std::fs::read_to_string(roles.join("x.md.new")).unwrap(),
            "new default\n"
        );
    }

    #[test]
    fn read_only_roles_dir_with_edited_file_still_works() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp("ro");
        let roles = dir.join("roles");
        std::fs::create_dir_all(&roles).unwrap();
        std::fs::write(roles.join("x.md"), "mine").unwrap();
        std::fs::write(roles.join("y.md"), "old").unwrap();
        std::fs::set_permissions(&roles, std::fs::Permissions::from_mode(0o500)).unwrap();
        let edited = ensure_template(&roles, "x.md", "new\n", &[1]);
        let old = ensure_template(&roles, "y.md", "new\n", &[text_hash("old")]);
        std::fs::set_permissions(&roles, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(std::fs::read_to_string(edited.unwrap()).unwrap(), "mine");
        assert_eq!(std::fs::read_to_string(old.unwrap()).unwrap(), "old");
    }

    #[test]
    fn temp_names_are_unique() {
        let dir = temp("tmpnames");
        let p = dir.join("f");
        // Many concurrent writers of one file never trip over a shared temp.
        std::thread::scope(|sc| {
            for _ in 0..8 {
                sc.spawn(|| {
                    for _ in 0..50 {
                        replace_file(&p, "x").unwrap();
                    }
                });
            }
        });
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "x");
    }

    #[test]
    fn current_default_is_left_alone() {
        let dir = temp("current");
        let roles = dir.join("roles");
        std::fs::create_dir_all(&roles).unwrap();
        std::fs::write(roles.join("x.md"), "cur").unwrap();
        ensure_template(&roles, "x.md", "cur\n", &[]).unwrap();
        assert_eq!(std::fs::read_to_string(roles.join("x.md")).unwrap(), "cur");
        assert!(!roles.join("x.md.new").exists());
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
            Some("Run the start-of-day round: call get_board, read today's journal, and propose a plan with propose_plan.")
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
        write_slot_files(&dir, 0, "a", Path::new("/o"), &RoleDirs::default(), None).unwrap();
        std::fs::write(dir.join("roles/coordinator.md"), "my coordinator").unwrap();
        std::fs::write(dir.join("roles/kickoff.md"), "my kickoff").unwrap();
        let f =
            write_slot_files(&dir, 0, "a", Path::new("/o"), &RoleDirs::default(), None).unwrap();
        assert_eq!(std::fs::read_to_string(f.role).unwrap(), "my coordinator");
        assert_eq!(kickoff_prompt(&dir).unwrap(), "my kickoff");
    }

    #[test]
    fn slot_zero_uses_the_coordinator_and_others_the_worker() {
        let dir = temp("which");
        let o = Path::new("/Apps/overseer");
        let c = std::fs::read_to_string(
            write_slot_files(&dir, 0, "", o, &RoleDirs::default(), None)
                .unwrap()
                .role,
        )
        .unwrap();
        let w = std::fs::read_to_string(
            write_slot_files(&dir, 4, "x", o, &RoleDirs::default(), None)
                .unwrap()
                .role,
        )
        .unwrap();
        assert!(c.contains("bd ready --json"), "{c}");
        assert!(!c.contains("You are S0 of an Overseer team"), "{c}");
        assert!(w.contains("You are S4 of an Overseer team"), "{w}");
    }

    #[test]
    fn overseer_path_with_a_space_is_quoted() {
        let dir = temp("space");
        let o = Path::new("/Applications/My App/overseer");
        let f = write_slot_files(&dir, 1, "", o, &RoleDirs::default(), None).unwrap();
        let role = std::fs::read_to_string(f.role).unwrap();
        assert!(
            role.contains("\"/Applications/My App/overseer\" ctl items"),
            "{role}"
        );
    }

    #[test]
    fn placeholders_are_filled() {
        let dir = temp("fill");
        let f = write_slot_files(
            &dir,
            1,
            "lbl",
            Path::new("/Apps/overseer"),
            &RoleDirs::default(),
            None,
        )
        .unwrap();
        let role = std::fs::read_to_string(f.role).unwrap();
        assert!(role.contains("\"/Apps/overseer\" ctl items"), "{role}");
        assert!(
            !role.contains("{overseer}") && !role.contains("{slot}"),
            "{role}"
        );
        assert!(!role.contains('\u{2014}'), "no em dashes");
        // The folders come from the config, never a fixed path.
        let dirs = RoleDirs {
            brain: PathBuf::from("/tmp/demo-brain"),
            beads: PathBuf::from("/tmp/demo-beads"),
            ..RoleDirs::default()
        };
        let f = write_slot_files(&dir, 0, "lbl", Path::new("/Apps/overseer"), &dirs, None).unwrap();
        let role = std::fs::read_to_string(f.role).unwrap();
        assert!(
            role.contains("Your working folder is /tmp/demo-brain."),
            "{role}"
        );
        assert!(
            role.contains("`bd ready --json` in /tmp/demo-beads"),
            "{role}"
        );
        assert!(
            !role.contains("2ndBrain") && !role.contains("{brain}"),
            "{role}"
        );
        assert!(role.contains("Call `notify_session`"), "{role}");
    }

    const PHASE_3_WORKER: &str = include_str!("../tests/fixtures/roles-phase3/worker.md");
    const PHASE_3_COORDINATOR: &str = include_str!("../tests/fixtures/roles-phase3/coordinator.md");

    #[test]
    fn phase_3_roles_upgrade() {
        assert_eq!(text_hash(PHASE_3_WORKER), 0xbd3ab0020d4d25b1);
        assert_eq!(text_hash(PHASE_3_COORDINATOR), 0x7ddaa223f4bdd1f6);
        assert!(PREVIOUS_WORKER.contains(&text_hash(PHASE_3_WORKER)));
        assert!(PREVIOUS_COORDINATOR.contains(&text_hash(PHASE_3_COORDINATOR)));
        let dir = temp("phase3");
        let roles = dir.join("roles");
        std::fs::create_dir_all(&roles).unwrap();
        std::fs::write(roles.join("worker.md"), PHASE_3_WORKER).unwrap();
        std::fs::write(roles.join("coordinator.md"), PHASE_3_COORDINATOR).unwrap();
        write_slot_files(
            &dir,
            1,
            "lbl",
            Path::new("/Apps/overseer"),
            &RoleDirs::default(),
            None,
        )
        .unwrap();
        write_slot_files(
            &dir,
            0,
            "lbl",
            Path::new("/Apps/overseer"),
            &RoleDirs::default(),
            None,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(roles.join("worker.md")).unwrap(),
            WORKER_ROLE
        );
        assert_eq!(
            std::fs::read_to_string(roles.join("coordinator.md")).unwrap(),
            COORDINATOR_ROLE
        );
        assert!(!roles.join("worker.md.new").exists());
        assert!(!roles.join("coordinator.md.new").exists());
    }

    #[test]
    fn worker_role_names_the_tools() {
        let dir = temp("workertools");
        let f = write_slot_files(
            &dir,
            2,
            "lbl",
            Path::new("/Apps/overseer"),
            &RoleDirs::default(),
            None,
        )
        .unwrap();
        let role = std::fs::read_to_string(f.role).unwrap();
        for t in [
            "claim_target",
            "release_target",
            "notify_session",
            "list_targets",
        ] {
            assert!(role.contains(&format!("`{t}`")), "{t}: {role}");
        }
        assert!(
            role.contains("Overseer locks shared write targets"),
            "{role}"
        );
        assert!(!role.contains('\u{2014}'), "no em dashes");
    }

    #[test]
    fn coordinator_role_reads_the_journal() {
        assert!(COORDINATOR_ROLE.contains("Before you propose work, read today's journal."));
        assert!(COORDINATOR_ROLE.contains("date '+%Y/%m-%B/%Y-%m-%d'"));
        assert!(COORDINATOR_ROLE.contains("06-Daily/"));
    }

    const PHASE_5_ASSIGNMENT: &str = include_str!("../tests/fixtures/roles-phase5/assignment.md");

    #[test]
    fn previous_assignment_hash_is_listed() {
        assert!(PREVIOUS_ASSIGNMENT.contains(&text_hash(PHASE_5_ASSIGNMENT)));
        // An unedited phase 5 file is upgraded in place.
        let dir = temp("phase5assign");
        let roles = dir.join("roles");
        std::fs::create_dir_all(&roles).unwrap();
        std::fs::write(roles.join("assignment.md"), PHASE_5_ASSIGNMENT).unwrap();
        assert_eq!(
            assignment_template(&dir).unwrap().trim_end(),
            ASSIGNMENT.trim_end()
        );
        assert!(!roles.join("assignment.md.new").exists());
    }

    const PHASE_5_COORDINATOR: &str = include_str!("../tests/fixtures/roles-phase5/coordinator.md");
    const PHASE_5_WORKER: &str = include_str!("../tests/fixtures/roles-phase5/worker.md");
    const PHASE_5_KICKOFF: &str = include_str!("../tests/fixtures/roles-phase5/kickoff.md");

    #[test]
    fn phase5_roles_are_previous_defaults() {
        assert!(PREVIOUS_COORDINATOR.contains(&text_hash(PHASE_5_COORDINATOR)));
        assert!(PREVIOUS_WORKER.contains(&text_hash(PHASE_5_WORKER)));
        assert!(PREVIOUS_KICKOFF.contains(&text_hash(PHASE_5_KICKOFF)));
        // Unedited phase 5 files are upgraded in place.
        let dir = temp("phase5roles");
        let roles = dir.join("roles");
        std::fs::create_dir_all(&roles).unwrap();
        for (name, old, current, previous) in [
            ("worker.md", PHASE_5_WORKER, WORKER_ROLE, PREVIOUS_WORKER),
            (
                "coordinator.md",
                PHASE_5_COORDINATOR,
                COORDINATOR_ROLE,
                PREVIOUS_COORDINATOR,
            ),
            ("kickoff.md", PHASE_5_KICKOFF, KICKOFF, PREVIOUS_KICKOFF),
        ] {
            std::fs::write(roles.join(name), old).unwrap();
            let path = ensure_template(&roles, name, current, previous).unwrap();
            assert_eq!(
                std::fs::read_to_string(path).unwrap().trim_end(),
                current.trim_end()
            );
            assert!(!roles.join(format!("{name}.new")).exists());
        }
    }

    #[test]
    fn coordinator_role_names_the_board_tools() {
        for tool in [
            "get_board",
            "get_work_items",
            "propose_plan",
            "request_status",
            "set_board_summary",
            "notify_session",
        ] {
            assert!(COORDINATOR_ROLE.contains(tool), "missing {tool}");
        }
        for gone in [
            "ctl assign",
            "ctl start",
            "Never claim targets",
            "To give work to a running slot",
            "Only propose idle slots",
        ] {
            assert!(!COORDINATOR_ROLE.contains(gone), "still has {gone}");
        }
    }

    #[test]
    fn worker_role_mentions_report_progress() {
        assert!(WORKER_ROLE.contains("report_progress"));
    }

    #[test]
    fn coordinator_role_keeps_brain_rules() {
        assert!(COORDINATOR_ROLE.contains("{brain_rules}"));
    }

    #[test]
    fn roles_have_no_em_dashes() {
        for text in [COORDINATOR_ROLE, WORKER_ROLE, KICKOFF] {
            assert!(!text.contains('\u{2014}'));
        }
    }

    const PHASE_4_WORKER: &str = include_str!("../tests/fixtures/roles-phase4/worker.md");
    const PHASE_4_COORDINATOR: &str = include_str!("../tests/fixtures/roles-phase4/coordinator.md");

    #[test]
    fn phase_4_roles_upgrade() {
        assert!(PREVIOUS_WORKER.contains(&text_hash(PHASE_4_WORKER)));
        assert!(PREVIOUS_COORDINATOR.contains(&text_hash(PHASE_4_COORDINATOR)));
        let dir = temp("phase4");
        let roles = dir.join("roles");
        std::fs::create_dir_all(&roles).unwrap();
        std::fs::write(roles.join("worker.md"), PHASE_4_WORKER).unwrap();
        std::fs::write(roles.join("coordinator.md"), PHASE_4_COORDINATOR).unwrap();
        for slot in [1, 0] {
            write_slot_files(
                &dir,
                slot,
                "lbl",
                Path::new("/o"),
                &RoleDirs::default(),
                None,
            )
            .unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(roles.join("worker.md")).unwrap(),
            WORKER_ROLE
        );
        assert_eq!(
            std::fs::read_to_string(roles.join("coordinator.md")).unwrap(),
            COORDINATOR_ROLE
        );
        assert!(!roles.join("worker.md.new").exists());
        // An edited phase 4 file is kept and the new default lands beside it.
        let edited = format!("{PHASE_4_WORKER}\nMy own rule.\n");
        std::fs::write(roles.join("worker.md"), &edited).unwrap();
        write_slot_files(&dir, 1, "lbl", Path::new("/o"), &RoleDirs::default(), None).unwrap();
        assert_eq!(
            std::fs::read_to_string(roles.join("worker.md")).unwrap(),
            edited
        );
        assert_eq!(
            std::fs::read_to_string(roles.join("worker.md.new")).unwrap(),
            WORKER_ROLE
        );
    }

    const NOTE: &str = "Sessions/2026/11-November/2026-11-13-s3-demo-lane.md";

    fn brain_dirs(on: bool) -> RoleDirs {
        RoleDirs {
            brain: PathBuf::from("/tmp/demo-brain"),
            beads: PathBuf::from("/tmp/demo-brain"),
            brain_writer: on,
            current_session: PathBuf::from("/tmp/demo-home/CURRENT_SESSION.md"),
        }
    }

    #[test]
    fn worker_role_names_the_brain_tools() {
        let tools = [
            "commit_2ndbrain",
            "append_journal",
            "edit_shared_file",
            "update_current_session",
        ];
        let dir = temp("brainon");
        let f = write_slot_files(
            &dir,
            3,
            "Demo Lane",
            Path::new("/o"),
            &brain_dirs(true),
            Some(NOTE),
        )
        .unwrap();
        let role = std::fs::read_to_string(f.role).unwrap();
        for t in tools {
            assert!(role.contains(&format!("`{t}`")), "{t}: {role}");
        }
        let note = role
            .lines()
            .find_map(|l| l.split("session note is ").nth(1))
            .expect("session note line");
        assert!(
            note.starts_with(&format!("/tmp/demo-brain/{NOTE}.")),
            "{note}"
        );
        assert!(role.contains("`${`"), "{role}");
        assert!(role.contains("/tmp/demo-home/CURRENT_SESSION.md"), "{role}");
        assert!(role.contains("Never `git commit`"), "{role}");
        assert!(!role.contains("{brain_rules}") && !role.contains("{session_note}"));
        assert!(!role.contains("{current_session}"), "{role}");
        assert!(!role.contains('\u{2014}'), "no em dashes");
        // Off: none of it, and no blank lines left behind.
        let f = write_slot_files(
            &dir,
            3,
            "Demo Lane",
            Path::new("/o"),
            &brain_dirs(false),
            None,
        )
        .unwrap();
        let off = std::fs::read_to_string(f.role).unwrap();
        for t in tools {
            assert!(!off.contains(t), "{t}: {off}");
        }
        assert!(
            !off.contains("{brain_rules}") && !off.contains("Sessions/"),
            "{off}"
        );
        assert!(off.ends_with("step.\n"), "{off:?}");
        assert!(!off.contains("\n\n\n"), "{off:?}");
    }

    #[test]
    fn no_note_leaves_the_session_note_line_out() {
        let dir = temp("nonote");
        let f = write_slot_files(&dir, 3, "x", Path::new("/o"), &brain_dirs(true), None).unwrap();
        let role = std::fs::read_to_string(f.role).unwrap();
        assert!(
            !role.contains("Your session note") && !role.contains("{session_note}"),
            "{role}"
        );
        assert!(role.contains("`commit_2ndbrain`"), "{role}");
    }

    #[test]
    fn missing_brain_rules_is_noticed_only_when_on() {
        assert!(lacks_brain_rules("my role", true));
        assert!(!lacks_brain_rules("my role", false));
        assert!(!lacks_brain_rules("my role\n{brain_rules}\n", true));
    }

    #[test]
    fn coordinator_role_gets_the_shared_file_rules_only() {
        let dir = temp("coordbrain");
        let f =
            write_slot_files(&dir, 0, "x", Path::new("/o"), &brain_dirs(true), Some(NOTE)).unwrap();
        let role = std::fs::read_to_string(f.role).unwrap();
        for t in ["commit_2ndbrain", "append_journal", "edit_shared_file"] {
            assert!(role.contains(&format!("`{t}`")), "{t}: {role}");
        }
        assert!(!role.contains("update_current_session"), "{role}");
        assert!(!role.contains("session note"), "{role}");
        assert!(!role.contains("{brain_rules}"), "{role}");
        assert!(!role.contains('\u{2014}'), "no em dashes");
        let f = write_slot_files(&dir, 0, "x", Path::new("/o"), &brain_dirs(false), None).unwrap();
        let off = std::fs::read_to_string(f.role).unwrap();
        assert!(
            !off.contains("append_journal") && !off.contains("{brain_rules}"),
            "{off}"
        );
        assert!(off.ends_with("into this session.\n"), "{off:?}");
    }
}
