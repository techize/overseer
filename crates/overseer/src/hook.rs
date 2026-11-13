//! `overseer hook <Event>`: relays one Claude Code hook event to the daemon.
//!
//! Claude runs this on every hook, so it must be fast and silent. Stdout of
//! some hook events is fed back into the conversation, so nothing is
//! printed, except the deny decision of `PreToolUse`. Every failure exits 0
//! so a hook problem never disturbs a session.

use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use overseer_proto::{
    paths, BashCheck, Client, HookEvent, HookKind, Request, Response, SlotId, WriteCheck, CHECK_ENV,
};

/// Total budget for connect plus the round trip. A hook that waits longer
/// would slow Claude down, and a missed event is cheaper than a slow session.
const DELIVER_WITHIN: Duration = Duration::from_millis(50);
/// Budget for the `PreToolUse` lock check, connect included. Past it the
/// command is allowed: a slow daemon never stops work.
const CHECK_WITHIN: Duration = Duration::from_secs(1);
const DETAIL_MAX: usize = 80;
/// Slots are S0 to S6; the daemon's manager owns the same bound.
const SLOT_COUNT: SlotId = 7;

fn kind(name: &str) -> Option<HookKind> {
    Some(match name {
        "SessionStart" => HookKind::SessionStart,
        "UserPromptSubmit" => HookKind::UserPromptSubmit,
        "Notification" => HookKind::Notification,
        "Stop" => HookKind::Stop,
        "PostToolUse" => HookKind::PostToolUse,
        _ => return None,
    })
}

/// `OVERSEER_SLOT` is `S<n>`; anything else is not one of our slots.
pub(crate) fn slot(env: &str) -> Option<SlotId> {
    let n: SlotId = env.strip_prefix('S')?.parse().ok()?;
    (n < SLOT_COUNT).then_some(n)
}

/// First line of the tool input's command, file path or pattern, cut to
/// `DETAIL_MAX` chars (never mid-character).
fn detail(input: &serde_json::Value) -> Option<String> {
    let text = ["command", "file_path", "pattern"]
        .iter()
        .find_map(|k| input.get(*k)?.as_str())?;
    let line = text.lines().next().unwrap_or("");
    Some(line.chars().take(DETAIL_MAX).collect())
}

/// Empty stdin is a valid event (only slot and kind are needed); non-empty
/// stdin that is not a JSON object is not.
pub fn parse_hook(event: &str, slot_env: &str, stdin: &[u8]) -> Option<HookEvent> {
    let event = kind(event)?;
    let slot = slot(slot_env)?;
    let json: serde_json::Value = if stdin.iter().all(u8::is_ascii_whitespace) {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(stdin).ok()?
    };
    let text = |k: &str| json.get(k)?.as_str().map(String::from);
    Some(HookEvent {
        slot,
        event,
        session_id: text("session_id"),
        tool: text("tool_name"),
        detail: json.get("tool_input").and_then(detail),
        notification: text("message"),
        tool_use_id: text("tool_use_id"),
        notification_type: text("notification_type"),
    })
}

/// The check for a `PreToolUse` event, or `None` when the daemon need not
/// be asked: a Bash command is a `CheckBash`; the file tools are a
/// `CheckWrite` of their `file_path` (`notebook_path` for `NotebookEdit`).
/// This is the one place that decides which tools are checked.
pub fn parse_pre_tool_use(
    slot_env: &str,
    stdin: &[u8],
    env: impl Fn(&str) -> Option<String>,
) -> Option<Request> {
    let slot = slot(slot_env)?;
    let json: serde_json::Value = serde_json::from_slice(stdin).ok()?;
    let text = |k: &str| json.get(k)?.as_str().map(String::from);
    let tool = text("tool_name")?;
    let input = json.get("tool_input")?;
    let path_key = match tool.as_str() {
        "Bash" => {
            let command = input.get("command")?.as_str()?.to_owned();
            return Some(Request::CheckBash(BashCheck {
                slot,
                session_id: text("session_id"),
                tool_use_id: text("tool_use_id"),
                command,
                cwd: text("cwd").map(PathBuf::from),
                env: CHECK_ENV
                    .iter()
                    .filter_map(|k| Some((k.to_string(), env(k)?)))
                    .collect(),
            }));
        }
        "Edit" | "Write" | "MultiEdit" => "file_path",
        "NotebookEdit" => "notebook_path",
        _ => return None,
    };
    let path = input.get(path_key)?.as_str().filter(|p| !p.is_empty())?;
    Some(Request::CheckWrite(WriteCheck {
        slot,
        tool,
        paths: vec![PathBuf::from(path)],
        cwd: text("cwd").map(PathBuf::from),
    }))
}

/// Claude Code's decision JSON denying a tool call, with `reason` shown to
/// the session.
pub fn deny_json(reason: &str) -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    })
    .to_string()
}

/// Claude Code's decision JSON allowing a tool call with its Bash command
/// replaced by `command`. `input` is the original `tool_input`: it is
/// copied and only `command` replaced, so `timeout`, `description` and
/// `run_in_background` survive whether Claude Code merges or replaces it.
pub fn rewrite_json(command: &str, input: Option<&serde_json::Value>) -> String {
    let mut updated = match input {
        Some(serde_json::Value::Object(o)) => o.clone(),
        _ => serde_json::Map::new(),
    };
    updated.insert("command".into(), command.into());
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow",
            "updatedInput": updated,
        }
    })
    .to_string()
}

/// What the hook prints for the daemon's answer: a deny wins over a
/// rewrite; an allow prints nothing.
pub fn decision_json(answer: &Response, input: Option<&serde_json::Value>) -> Option<String> {
    match answer {
        Response::Verdict {
            deny: Some(reason), ..
        } => Some(deny_json(reason)),
        Response::Verdict {
            rewrite: Some(command),
            ..
        } => Some(rewrite_json(command, input)),
        _ => None,
    }
}

fn read_stdin() -> Vec<u8> {
    let mut stdin = Vec::new();
    // A terminal would block forever waiting for EOF.
    if !std::io::stdin().is_terminal() {
        let _ = std::io::stdin().read_to_end(&mut stdin);
    }
    stdin
}

fn runtime() -> Option<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()
}

/// Asks the daemon about a Bash command or file write and prints its deny
/// or rewrite decision if it gives one. A missing, slow or confused daemon means allow.
fn pre_tool_use(slot_env: &str) {
    let stdin = read_stdin();
    let Some(check) = parse_pre_tool_use(slot_env, &stdin, |k| std::env::var(k).ok()) else {
        return;
    };
    let input = serde_json::from_slice::<serde_json::Value>(&stdin)
        .ok()
        .and_then(|j| j.get("tool_input").cloned());
    let Some(rt) = runtime() else { return };
    let answer = rt.block_on(async {
        tokio::time::timeout(CHECK_WITHIN, async {
            let client = Client::connect(paths::socket_path()).await.ok()?;
            client.request(check).await.ok()
        })
        .await
        .ok()
        .flatten()
    });
    if let Some(out) = answer
        .as_ref()
        .and_then(|a| decision_json(a, input.as_ref()))
    {
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "{out}");
        let _ = stdout.flush();
    }
}

/// Runs the hook. Prints nothing (but a `PreToolUse` deny) and always
/// returns, so the caller exits 0.
pub fn run(event: &str) {
    let Ok(slot_env) = std::env::var("OVERSEER_SLOT") else {
        return;
    };
    if event == "PreToolUse" {
        pre_tool_use(&slot_env);
        return;
    }
    let stdin = read_stdin();
    let Some(ev) = parse_hook(event, &slot_env, &stdin) else {
        return;
    };
    let Some(rt) = runtime() else {
        return;
    };
    // Plain connect: a hook must never start a daemon.
    // The timeout is built inside the async block: it needs the runtime's
    // timer, which only exists once `block_on` is running.
    rt.block_on(async {
        let _ = tokio::time::timeout(DELIVER_WITHIN, async {
            let client = Client::connect(paths::socket_path()).await.ok()?;
            client.request(Request::Hook(ev)).await.ok()
        })
        .await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use overseer_proto::HookKind;

    #[test]
    fn parses_post_tool_use_bash() {
        let long = format!("echo {}\nsecond line", "é".repeat(100));
        let json = serde_json::json!({
            "session_id": "sid-1",
            "tool_name": "Bash",
            "tool_input": {"command": long}
        })
        .to_string();
        let ev = parse_hook("PostToolUse", "S2", json.as_bytes()).unwrap();
        assert_eq!(ev.slot, 2);
        assert_eq!(ev.event, HookKind::PostToolUse);
        assert_eq!(ev.session_id.as_deref(), Some("sid-1"));
        assert_eq!(ev.tool.as_deref(), Some("Bash"));
        let d = ev.detail.unwrap();
        assert_eq!(d.chars().count(), 80);
        assert!(d.starts_with("echo é"));
        assert!(!d.contains('\n'));
    }

    #[test]
    fn detail_falls_back_to_file_path_then_pattern() {
        let f = br#"{"tool_name":"Edit","tool_input":{"file_path":"/a/b.rs"}}"#;
        let ev = parse_hook("PostToolUse", "S0", f).unwrap();
        assert_eq!(ev.detail.as_deref(), Some("/a/b.rs"));
        let g = br#"{"tool_name":"Grep","tool_input":{"pattern":"foo"}}"#;
        let ev = parse_hook("PostToolUse", "S0", g).unwrap();
        assert_eq!(ev.detail.as_deref(), Some("foo"));
        let n = br#"{"tool_name":"Task","tool_input":{}}"#;
        assert_eq!(parse_hook("PostToolUse", "S0", n).unwrap().detail, None);
    }

    #[test]
    fn parses_notification_message() {
        let json = br#"{"session_id":"s","message":"Claude needs your permission","notification_type":"permission_prompt"}"#;
        let ev = parse_hook("Notification", "S6", json).unwrap();
        assert_eq!(ev.event, HookKind::Notification);
        assert_eq!(
            ev.notification.as_deref(),
            Some("Claude needs your permission")
        );
    }

    #[test]
    fn parse_hook_fills_tool_use_id_and_notification_type() {
        let post = br#"{"tool_name":"Bash","tool_use_id":"tu-9","tool_input":{"command":"ls"}}"#;
        let ev = parse_hook("PostToolUse", "S1", post).unwrap();
        assert_eq!(ev.tool_use_id.as_deref(), Some("tu-9"));
        assert_eq!(ev.notification_type, None);
        let note = br#"{"message":"waiting","notification_type":"idle_prompt"}"#;
        let ev = parse_hook("Notification", "S1", note).unwrap();
        assert_eq!(ev.notification_type.as_deref(), Some("idle_prompt"));
        assert_eq!(ev.tool_use_id, None);
    }

    #[test]
    fn pre_tool_use_reads_bash_and_the_file_tools() {
        let env = |k: &str| (k == "KUBECONFIG").then(|| "/k".to_string());
        let bash = br#"{"session_id":"s","tool_name":"Bash","tool_use_id":"tu-1","cwd":"/w","tool_input":{"command":"git push"}}"#;
        let Some(Request::CheckBash(c)) = parse_pre_tool_use("S3", bash, env) else {
            panic!("not a bash check");
        };
        assert_eq!(c.slot, 3);
        assert_eq!(c.command, "git push");
        assert_eq!(c.tool_use_id.as_deref(), Some("tu-1"));
        assert_eq!(c.session_id.as_deref(), Some("s"));
        assert_eq!(c.cwd, Some(PathBuf::from("/w")));
        assert_eq!(c.env, [("KUBECONFIG".to_string(), "/k".to_string())]);
        let grep = br#"{"tool_name":"Grep","tool_input":{"pattern":"x"}}"#;
        assert!(parse_pre_tool_use("S3", grep, env).is_none());
        let no_command = br#"{"tool_name":"Bash","tool_input":{}}"#;
        assert!(parse_pre_tool_use("S3", no_command, env).is_none());
        assert!(parse_pre_tool_use("S9", bash, env).is_none());
        assert!(parse_pre_tool_use("S3", b"junk", env).is_none());
    }

    #[test]
    fn edit_of_a_file_tool_becomes_check_write() {
        let env = |_: &str| None;
        for tool in ["Edit", "Write", "MultiEdit"] {
            let json = format!(
                r#"{{"tool_name":"{tool}","cwd":"/w","tool_input":{{"file_path":"/v/a.md","old_string":"x"}}}}"#
            );
            let Some(Request::CheckWrite(c)) = parse_pre_tool_use("S2", json.as_bytes(), env)
            else {
                panic!("{tool}: not a write check");
            };
            assert_eq!(c.slot, 2);
            assert_eq!(c.tool, tool);
            assert_eq!(c.paths, [PathBuf::from("/v/a.md")]);
            assert_eq!(c.cwd, Some(PathBuf::from("/w")));
        }
        let nb = br#"{"tool_name":"NotebookEdit","tool_input":{"notebook_path":"/v/n.ipynb"}}"#;
        let Some(Request::CheckWrite(c)) = parse_pre_tool_use("S2", nb, env) else {
            panic!("not a write check");
        };
        assert_eq!(c.tool, "NotebookEdit");
        assert_eq!(c.paths, [PathBuf::from("/v/n.ipynb")]);
        let none = br#"{"tool_name":"Edit","tool_input":{}}"#;
        assert!(parse_pre_tool_use("S2", none, env).is_none());
    }

    #[test]
    fn rewrite_prints_updated_input() {
        let v: serde_json::Value =
            serde_json::from_str(&rewrite_json("overseer ctl bd -- close ov-3", None)).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "allow",
                "updatedInput": {"command": "overseer ctl bd -- close ov-3"},
            }})
        );
        assert_eq!(
            decision_json(
                &Response::Verdict {
                    deny: None,
                    rewrite: Some("x".into())
                },
                None
            )
            .as_deref(),
            Some(rewrite_json("x", None).as_str())
        );
        assert_eq!(
            decision_json(
                &Response::Verdict {
                    deny: Some("no".into()),
                    rewrite: Some("x".into())
                },
                None
            )
            .as_deref(),
            Some(deny_json("no").as_str())
        );
        assert_eq!(
            decision_json(
                &Response::Verdict {
                    deny: None,
                    rewrite: None
                },
                None
            ),
            None
        );
    }

    #[test]
    fn rewrite_keeps_the_other_tool_input_fields() {
        let input = serde_json::json!({
            "command": "bd close ov-3",
            "description": "close it",
            "timeout": 5000,
            "run_in_background": true
        });
        let v: serde_json::Value =
            serde_json::from_str(&rewrite_json("overseer ctl bd -- close ov-3", Some(&input)))
                .unwrap();
        assert_eq!(
            v["hookSpecificOutput"]["updatedInput"],
            serde_json::json!({
                "command": "overseer ctl bd -- close ov-3",
                "description": "close it",
                "timeout": 5000,
                "run_in_background": true
            })
        );
    }

    #[test]
    fn deny_json_is_claude_codes_shape() {
        let v: serde_json::Value = serde_json::from_str(&deny_json("held \"x\"")).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(
            v["hookSpecificOutput"]["permissionDecisionReason"],
            "held \"x\""
        );
    }

    #[test]
    fn no_slot_env_gives_none() {
        assert!(parse_hook("Stop", "", b"{}").is_none());
        assert!(parse_hook("Stop", "7", b"{}").is_none());
        assert!(parse_hook("Stop", "S7", b"{}").is_none());
        assert!(parse_hook("Stop", "Sx", b"{}").is_none());
    }

    #[test]
    fn bad_json_gives_none() {
        assert!(parse_hook("Stop", "S1", b"not json").is_none());
        assert!(parse_hook("Bogus", "S1", b"{}").is_none());
    }

    #[test]
    fn empty_stdin_is_still_an_event() {
        let ev = parse_hook("Stop", "S1", b"").unwrap();
        assert_eq!(ev.event, HookKind::Stop);
        assert_eq!(ev.session_id, None);
    }
}
