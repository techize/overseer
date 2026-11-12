//! `overseer hook <Event>`: relays one Claude Code hook event to the daemon.
//!
//! Claude runs this on every hook, so it must be fast and silent. Stdout of
//! some hook events is fed back into the conversation, so nothing is ever
//! printed. Every failure exits 0 so a hook problem never disturbs a session.

use std::io::{IsTerminal, Read};
use std::time::Duration;

use overseer_proto::{paths, Client, HookEvent, HookKind, Request, SlotId};

/// Total budget for connect plus the round trip. A hook that waits longer
/// would slow Claude down, and a missed event is cheaper than a slow session.
const DELIVER_WITHIN: Duration = Duration::from_millis(50);
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
fn slot(env: &str) -> Option<SlotId> {
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

/// Runs the hook. Prints nothing and always returns, so the caller exits 0.
pub fn run(event: &str) {
    let Ok(slot_env) = std::env::var("OVERSEER_SLOT") else {
        return;
    };
    let mut stdin = Vec::new();
    // A terminal would block forever waiting for EOF.
    if !std::io::stdin().is_terminal() {
        let _ = std::io::stdin().read_to_end(&mut stdin);
    }
    let Some(ev) = parse_hook(event, &slot_env, &stdin) else {
        return;
    };
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
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
