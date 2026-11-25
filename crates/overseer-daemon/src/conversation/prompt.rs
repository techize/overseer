//! Which prompt a slot's session is waiting on, followed from its hook
//! events.

use overseer_proto::msg::{HookEvent, HookKind, Prompt, PromptInfo, PromptShown};

/// The tool Claude Code uses to ask the user a question with options.
const ASK_USER_QUESTION: &str = "AskUserQuestion";

/// The notification Claude Code sends when it draws a permission dialog.
const PERMISSION_PROMPT: &str = "permission_prompt";

/// The prompt a slot is waiting on, if any.
#[derive(Debug, Default)]
pub struct PromptState {
    open: Option<Prompt>,
    /// Numbers the ids of prompts that came with no `tool_use_id`.
    counter: u64,
}

impl PromptState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies one hook event. True when the open prompt changed.
    pub fn on_hook(&mut self, ev: &HookEvent) -> bool {
        let main = ev.agent_id.is_none();
        let next = match ev.event {
            HookKind::PreToolUse if main && ev.tool.as_deref() == Some(ASK_USER_QUESTION) => {
                Some(Some(self.prompt(ev, false)))
            }
            HookKind::PermissionRequest if ev.tool.as_deref() != Some(ASK_USER_QUESTION) => {
                Some(Some(self.prompt(ev, !main)))
            }
            HookKind::Notification
                if main
                    && self.open.is_none()
                    && ev.notification_type.as_deref() == Some(PERMISSION_PROMPT) =>
            {
                let id = self.next_id(None);
                Some(Some(Prompt {
                    id,
                    subagent: false,
                    info: PromptShown::Unknown,
                }))
            }
            // Matched by the tool call, not the tool name, since parallel
            // calls can finish while a dialog is open. The id is unique, so
            // a subagent's own call clears the subagent prompt it raised.
            HookKind::PostToolUse if self.is_open_call(ev) => Some(None),
            // A session start is a new process (a resume, say): any dialog
            // of the one before is gone.
            HookKind::UserPromptSubmit | HookKind::Stop | HookKind::SessionStart if main => {
                Some(None)
            }
            _ => None,
        };
        match next {
            Some(next) if next != self.open => {
                self.open = next;
                true
            }
            _ => false,
        }
    }

    /// Forgets the open prompt, for a new session. Ids keep counting, so a
    /// new session's prompt never reuses an earlier one's id.
    pub fn clear(&mut self) {
        self.open = None;
    }

    /// The prompt the session is waiting on.
    pub fn open(&self) -> Option<&Prompt> {
        self.open.as_ref()
    }

    fn is_open_call(&self, ev: &HookEvent) -> bool {
        match (&self.open, &ev.tool_use_id) {
            (Some(open), Some(id)) => open.id == *id,
            _ => false,
        }
    }

    /// The prompt an event opens. A repeat of the open prompt's event keeps
    /// its id, so it is no change.
    fn prompt(&mut self, ev: &HookEvent, subagent: bool) -> Prompt {
        let info = match &ev.prompt {
            Some(PromptInfo::Other) | None => PromptShown::Unknown,
            Some(info) => PromptShown::Known(info.clone()),
        };
        Prompt {
            id: self.next_id(ev.tool_use_id.as_deref()),
            subagent,
            info,
        }
    }

    /// The `tool_use_id`, or the next `n<counter>`.
    fn next_id(&mut self, tool_use_id: Option<&str>) -> String {
        match tool_use_id {
            Some(id) => id.to_string(),
            None => {
                self.counter += 1;
                format!("n{}", self.counter)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use overseer_proto::msg::Question;

    fn ev(event: HookKind) -> HookEvent {
        HookEvent {
            slot: 2,
            event,
            session_id: Some("demo-session".into()),
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

    fn question(id: &str) -> HookEvent {
        HookEvent {
            tool: Some(ASK_USER_QUESTION.into()),
            tool_use_id: Some(id.into()),
            prompt: Some(PromptInfo::Question {
                questions: vec![Question {
                    header: "Deploy".into(),
                    question: "Deploy demo-api now?".into(),
                    options: vec!["Yes".into(), "No".into()],
                    multi: false,
                }],
            }),
            ..ev(HookKind::PreToolUse)
        }
    }

    fn permission(tool: &str, id: &str) -> HookEvent {
        HookEvent {
            tool: Some(tool.into()),
            tool_use_id: Some(id.into()),
            prompt: Some(PromptInfo::Permission {
                tool: tool.into(),
                summary: "rm -rf /tmp/demo/target".into(),
                plan: None,
            }),
            ..ev(HookKind::PermissionRequest)
        }
    }

    fn post(id: &str) -> HookEvent {
        HookEvent {
            tool: Some("Bash".into()),
            tool_use_id: Some(id.into()),
            ..ev(HookKind::PostToolUse)
        }
    }

    #[test]
    fn question_opens_and_its_post_tool_use_clears() {
        let mut s = PromptState::new();
        assert!(s.on_hook(&question("tu-1")));
        let open = s.open().unwrap();
        assert_eq!(open.id, "tu-1");
        assert!(!open.subagent);
        assert!(matches!(
            open.info,
            PromptShown::Known(PromptInfo::Question { .. })
        ));
        // The same event again is no change.
        assert!(!s.on_hook(&question("tu-1")));
        assert!(s.on_hook(&post("tu-1")));
        assert!(s.open().is_none());
    }

    #[test]
    fn another_tools_post_tool_use_keeps_the_prompt() {
        let mut s = PromptState::new();
        assert!(s.on_hook(&permission("Bash", "tu-1")));
        assert!(!s.on_hook(&post("tu-2")));
        assert_eq!(s.open().unwrap().id, "tu-1");
        // A PostToolUse with no id clears nothing either.
        assert!(!s.on_hook(&ev(HookKind::PostToolUse)));
        assert_eq!(s.open().unwrap().id, "tu-1");
    }

    #[test]
    fn permission_for_ask_user_question_is_ignored() {
        let mut s = PromptState::new();
        assert!(s.on_hook(&question("tu-1")));
        assert!(!s.on_hook(&permission(ASK_USER_QUESTION, "tu-1")));
        assert!(matches!(
            s.open().unwrap().info,
            PromptShown::Known(PromptInfo::Question { .. })
        ));
    }

    #[test]
    fn subagent_permission_is_shown_as_subagent() {
        let mut s = PromptState::new();
        let sub = HookEvent {
            agent_id: Some("ag-1".into()),
            ..permission("Bash", "tu-7")
        };
        assert!(s.on_hook(&sub));
        let open = s.open().unwrap();
        assert!(open.subagent);
        assert_eq!(open.id, "tu-7");
        // Other subagent events are not followed: its question is not shown,
        // and its Stop does not clear the main terminal's prompt.
        let mut other = PromptState::new();
        let sub_question = HookEvent {
            agent_id: Some("ag-1".into()),
            ..question("tu-8")
        };
        assert!(!other.on_hook(&sub_question));
        assert!(other.open().is_none());
        let sub_stop = HookEvent {
            agent_id: Some("ag-1".into()),
            ..ev(HookKind::Stop)
        };
        assert!(!s.on_hook(&sub_stop));
        // Its own tool call finishing clears it.
        let sub_post = HookEvent {
            agent_id: Some("ag-1".into()),
            ..post("tu-7")
        };
        assert!(s.on_hook(&sub_post));
        assert!(s.open().is_none());
    }

    #[test]
    fn permission_notification_with_nothing_known_is_unknown() {
        let mut s = PromptState::new();
        let note = HookEvent {
            notification_type: Some(PERMISSION_PROMPT.into()),
            ..ev(HookKind::Notification)
        };
        assert!(s.on_hook(&note));
        let open = s.open().unwrap();
        assert_eq!(open.info, PromptShown::Unknown);
        assert_eq!(open.id, "n1");
        // While a prompt is open the notification changes nothing.
        assert!(!s.on_hook(&note));
        let mut known = PromptState::new();
        assert!(known.on_hook(&permission("Bash", "tu-1")));
        assert!(!known.on_hook(&note));
        assert_eq!(known.open().unwrap().id, "tu-1");
        // Another notification type opens nothing.
        let idle = HookEvent {
            notification_type: Some("idle_prompt".into()),
            ..ev(HookKind::Notification)
        };
        let mut none = PromptState::new();
        assert!(!none.on_hook(&idle));
    }

    #[test]
    fn user_prompt_and_stop_clear() {
        let mut s = PromptState::new();
        assert!(s.on_hook(&permission("Bash", "tu-1")));
        assert!(s.on_hook(&ev(HookKind::UserPromptSubmit)));
        assert!(s.open().is_none());
        assert!(s.on_hook(&question("tu-2")));
        assert!(s.on_hook(&ev(HookKind::Stop)));
        assert!(s.open().is_none());
        // Clearing with nothing open is no change.
        assert!(!s.on_hook(&ev(HookKind::Stop)));
        assert!(s.on_hook(&permission("Bash", "tu-3")));
        assert!(s.on_hook(&ev(HookKind::SessionStart)));
        assert!(s.open().is_none());
    }

    #[test]
    fn a_new_prompt_replaces_the_old_and_ids_count_up() {
        let mut s = PromptState::new();
        assert!(s.on_hook(&permission("Bash", "tu-1")));
        assert!(s.on_hook(&permission("Edit", "tu-2")));
        assert_eq!(s.open().unwrap().id, "tu-2");
        let no_id = HookEvent {
            tool_use_id: None,
            ..permission("Bash", "unused")
        };
        assert!(s.on_hook(&no_id));
        assert_eq!(s.open().unwrap().id, "n1");
        // With no id, a repeat is a new prompt.
        assert!(s.on_hook(&no_id));
        assert_eq!(s.open().unwrap().id, "n2");
    }
}
