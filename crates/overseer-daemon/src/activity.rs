//! What a slot's session is doing, derived from its hook events.

use overseer_proto::{Activity, ActivityKind, HookEvent, HookKind};

/// The activity after `ev` arrives at `now` (unix seconds), given the
/// activity before it. Pure, so the table is testable without a daemon.
///
/// `since` is when the slot entered its current kind, so a change of
/// detail alone (a new tool, a new notification) does not restart it.
///
/// A `Notification` that only says Claude is waiting for input is `Idle`,
/// not `NeedsYou`: Claude sends one about a minute after every turn, and
/// after an Esc interrupt, which fires no `Stop`.
pub fn apply(prev: Option<&Activity>, ev: &HookEvent, now: i64) -> Option<Activity> {
    // These carry a prompt or a tool about to run, not a change of state.
    if matches!(
        ev.event,
        HookKind::PreToolUse | HookKind::PermissionRequest | HookKind::Other
    ) {
        return prev.cloned();
    }
    let idle_wait = ev.event == HookKind::Notification && is_idle_wait(ev);
    let (kind, now_doing, reason) = match ev.event {
        HookKind::Notification if idle_wait => (ActivityKind::Idle, None, None),
        HookKind::UserPromptSubmit => (ActivityKind::Working, None, None),
        HookKind::PostToolUse => {
            let doing = ev.tool.as_ref().map(|tool| match &ev.detail {
                Some(detail) => format!("{tool} {detail}"),
                None => tool.clone(),
            });
            (ActivityKind::Working, doing, None)
        }
        HookKind::Notification => (ActivityKind::NeedsYou, None, ev.notification.clone()),
        HookKind::Stop | HookKind::SessionStart => (ActivityKind::Idle, None, None),
        HookKind::PreToolUse | HookKind::PermissionRequest | HookKind::Other => {
            unreachable!("returned above")
        }
    };
    let since = match prev {
        Some(p) if p.kind == kind => p.since,
        _ => now,
    };
    Some(Activity {
        kind,
        since,
        now_doing,
        reason,
        idle_wait,
    })
}

/// The activity after the slot's transcript records the end of a turn or an
/// interrupt at `now`. A prompt answered No (or dismissed with Esc) fires no
/// hook, so `NeedsYou` would stay until the next prompt; the transcript's
/// interrupt line is the only sign it was answered (#123). Anything else is
/// left to the hooks.
pub fn after_turn(prev: Option<&Activity>, now: i64) -> Option<Activity> {
    match prev {
        Some(p) if p.kind == ActivityKind::NeedsYou => Some(Activity {
            kind: ActivityKind::Idle,
            since: now,
            now_doing: None,
            reason: None,
            idle_wait: false,
        }),
        _ => prev.cloned(),
    }
}

/// A `Notification` that only says the session is waiting for input, not a
/// permission or question prompt: its type is `idle_prompt`, or, with no
/// type, its message says it is waiting for your input.
fn is_idle_wait(ev: &HookEvent) -> bool {
    match ev.notification_type.as_deref() {
        Some(kind) => kind == "idle_prompt",
        None => ev
            .notification
            .as_deref()
            .is_some_and(|m| m.contains("waiting for your input")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: HookKind) -> HookEvent {
        HookEvent {
            slot: 1,
            event: kind,
            session_id: None,
            tool: None,
            detail: None,
            notification: None,
            tool_use_id: None,
            notification_type: None,
            run: None,
            agent_id: None,
            transcript_path: None,
            prompt: None,
        }
    }

    fn act(kind: ActivityKind, since: i64) -> Activity {
        Activity {
            kind,
            since,
            now_doing: None,
            reason: None,
            idle_wait: false,
        }
    }

    #[test]
    fn new_hook_kinds_leave_activity_alone() {
        let prev = act(ActivityKind::Working, 5);
        for kind in [
            HookKind::PreToolUse,
            HookKind::PermissionRequest,
            HookKind::Other,
        ] {
            assert_eq!(apply(Some(&prev), &ev(kind), 9), Some(prev.clone()));
            assert_eq!(apply(None, &ev(kind), 9), None);
        }
    }

    #[test]
    fn user_prompt_submit_starts_working_and_clears_now_doing() {
        let mut prev = act(ActivityKind::Working, 5);
        prev.now_doing = Some("Bash ls".into());
        let a = apply(Some(&prev), &ev(HookKind::UserPromptSubmit), 9).unwrap();
        assert_eq!(a.kind, ActivityKind::Working);
        assert_eq!(a.now_doing, None);
        assert_eq!(a.since, 5);
        let a = apply(None, &ev(HookKind::UserPromptSubmit), 9).unwrap();
        assert_eq!((a.kind, a.since), (ActivityKind::Working, 9));
    }

    #[test]
    fn post_tool_use_sets_now_doing_with_detail() {
        let mut e = ev(HookKind::PostToolUse);
        e.tool = Some("Edit".into());
        e.detail = Some("src/main.rs".into());
        let a = apply(None, &e, 7).unwrap();
        assert_eq!(a.kind, ActivityKind::Working);
        assert_eq!(a.now_doing.as_deref(), Some("Edit src/main.rs"));
        assert_eq!(a.since, 7);
    }

    #[test]
    fn post_tool_use_without_detail_is_just_the_tool() {
        let mut e = ev(HookKind::PostToolUse);
        e.tool = Some("Read".into());
        let a = apply(None, &e, 7).unwrap();
        assert_eq!(a.now_doing.as_deref(), Some("Read"));
    }

    #[test]
    fn post_tool_use_keeps_working_since() {
        let prev = act(ActivityKind::Working, 5);
        let mut e = ev(HookKind::PostToolUse);
        e.tool = Some("Bash".into());
        let a = apply(Some(&prev), &e, 50).unwrap();
        assert_eq!(a.since, 5);
        // From any other kind it is a fresh start.
        let prev = act(ActivityKind::NeedsYou, 5);
        let a = apply(Some(&prev), &e, 50).unwrap();
        assert_eq!((a.kind, a.since), (ActivityKind::Working, 50));
        assert_eq!(a.reason, None);
    }

    #[test]
    fn notification_needs_you_with_the_message() {
        let mut e = ev(HookKind::Notification);
        e.notification = Some("Claude needs your permission".into());
        let a = apply(Some(&act(ActivityKind::Working, 5)), &e, 8).unwrap();
        assert_eq!(a.kind, ActivityKind::NeedsYou);
        assert_eq!(a.since, 8);
        assert_eq!(a.reason.as_deref(), Some("Claude needs your permission"));
        assert_eq!(a.now_doing, None);
        let a = apply(None, &ev(HookKind::Notification), 8).unwrap();
        assert_eq!(a.reason, None);
    }

    #[test]
    fn idle_prompt_notification_is_an_idle_wait() {
        let mut e = ev(HookKind::Notification);
        e.notification_type = Some("idle_prompt".into());
        assert!(apply(None, &e, 1).unwrap().idle_wait);
        e.notification_type = Some("permission_prompt".into());
        e.notification = Some("Claude is waiting for your input".into());
        assert!(!apply(None, &e, 1).unwrap().idle_wait, "the type wins");
    }

    #[test]
    fn untyped_notification_is_an_idle_wait_by_its_message() {
        let mut e = ev(HookKind::Notification);
        e.notification = Some("Claude is waiting for your input".into());
        assert!(apply(None, &e, 1).unwrap().idle_wait);
        e.notification = Some("Claude needs your permission to use Bash".into());
        assert!(!apply(None, &e, 1).unwrap().idle_wait);
        e.notification = None;
        assert!(!apply(None, &e, 1).unwrap().idle_wait);
    }

    #[test]
    fn an_idle_wait_is_idle_not_needs_you() {
        let mut e = ev(HookKind::Notification);
        e.notification_type = Some("idle_prompt".into());
        e.notification = Some("Claude is waiting for your input".into());
        // After a turn that ended with Stop: still idle since the Stop.
        let a = apply(Some(&act(ActivityKind::Idle, 5)), &e, 65).unwrap();
        assert_eq!((a.kind, a.since, a.reason), (ActivityKind::Idle, 5, None));
        assert!(a.idle_wait);
        // After an Esc interrupt, which sends no Stop: Working ends here.
        let a = apply(Some(&act(ActivityKind::Working, 5)), &e, 65).unwrap();
        assert_eq!((a.kind, a.since), (ActivityKind::Idle, 65));
    }

    #[test]
    fn any_other_event_clears_the_idle_wait() {
        let mut prev = act(ActivityKind::NeedsYou, 5);
        prev.idle_wait = true;
        for kind in [
            HookKind::Stop,
            HookKind::SessionStart,
            HookKind::UserPromptSubmit,
            HookKind::PostToolUse,
        ] {
            let mut e = ev(kind);
            // Even carrying an idle type, only a Notification counts.
            e.notification_type = Some("idle_prompt".into());
            assert!(!apply(Some(&prev), &e, 9).unwrap().idle_wait, "{kind:?}");
        }
    }

    #[test]
    fn stop_and_session_start_go_idle() {
        let mut prev = act(ActivityKind::Working, 5);
        prev.now_doing = Some("Bash ls".into());
        for kind in [HookKind::Stop, HookKind::SessionStart] {
            let a = apply(Some(&prev), &ev(kind), 12).unwrap();
            assert_eq!(a, act(ActivityKind::Idle, 12), "{kind:?}");
        }
    }

    #[test]
    fn a_turn_end_answers_a_prompt_and_leaves_the_rest() {
        let mut asked = act(ActivityKind::NeedsYou, 5);
        asked.reason = Some("Claude needs your permission".into());
        assert_eq!(
            after_turn(Some(&asked), 30),
            Some(act(ActivityKind::Idle, 30))
        );
        for kind in [ActivityKind::Working, ActivityKind::Idle] {
            let prev = act(kind, 5);
            assert_eq!(after_turn(Some(&prev), 30), Some(prev), "{kind:?}");
        }
        assert_eq!(after_turn(None, 30), None);
    }

    #[test]
    fn repeating_an_event_changes_nothing() {
        let prev = act(ActivityKind::Idle, 5);
        assert_eq!(apply(Some(&prev), &ev(HookKind::Stop), 99), Some(prev));
    }
}
