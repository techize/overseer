//! What it takes to start `claude` the way Overseer wants: generated
//! per-slot settings (the hooks), a role prompt and a session id.

use std::io;
use std::path::{Path, PathBuf};

use overseer_proto::msg::{slot_name, SlotId};

/// Compiled-in default for the worker role, written to
/// `<state>/roles/worker.md` on first use so Jonathan can edit the copy.
const WORKER_ROLE: &str = include_str!("roles/worker.md");

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
/// `roles/worker.md` (created from the compiled-in default if missing, never
/// overwritten) with `{slot}`, `{label}` and `{overseer}` filled in.
pub fn write_slot_files(
    state_dir: &Path,
    slot: SlotId,
    label: &str,
    overseer_bin: &Path,
) -> io::Result<LaunchFiles> {
    let roles = state_dir.join("roles");
    std::fs::create_dir_all(&roles)?;
    let template_path = roles.join("worker.md");
    // create_new: never clobber an edited template, even when two starts race.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&template_path)
    {
        Ok(mut f) => io::Write::write_all(&mut f, WORKER_ROLE.as_bytes())?,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
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
/// with spaces needs no quoting.
pub fn claude_argv(
    base: &[String],
    slot: SlotId,
    label: &str,
    files: &LaunchFiles,
    start: Start,
) -> Vec<String> {
    let mut argv = base.to_vec();
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
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let argv = claude_argv(&base, 2, "api", &files(), Start::New("uuid-1".into()));
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
        );
        let i = argv.iter().position(|a| a == "--name").unwrap();
        assert_eq!(argv[i + 1], "S1 fix the login bug");
    }
}
