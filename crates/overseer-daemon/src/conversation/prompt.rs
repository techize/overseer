//! Which prompt a slot's session is waiting on, followed from its hook
//! events.

use std::collections::HashMap;

use overseer_proto::msg::{HookEvent, HookKind, Prompt, PromptInfo, PromptShown};

/// The tool Claude Code uses to ask the user a question with options.
const ASK_USER_QUESTION: &str = "AskUserQuestion";

/// The notification Claude Code sends when it draws a permission dialog.
const PERMISSION_PROMPT: &str = "permission_prompt";

/// The prompt a slot is waiting on, if any.
#[derive(Debug, Default)]
pub struct PromptState {
    open: Option<Prompt>,
    /// What ends the open prompt.
    ends: Ends,
    /// The latest `PreToolUse` of each conversation, keyed by `agent_id`
    /// (`None` for the main one).
    last_call: HashMap<Option<String>, Call>,
    /// Numbers the ids of prompts that came with no `tool_use_id`.
    counter: u64,
}

/// One tool call as a hook saw it.
#[derive(Debug, Clone, PartialEq)]
struct Call {
    id: Option<String>,
    tool: Option<String>,
    detail: Option<String>,
}

impl Call {
    fn of(ev: &HookEvent) -> Call {
        Call {
            id: ev.tool_use_id.clone(),
            tool: ev.tool.clone(),
            detail: ev.detail.clone(),
        }
    }

    /// The same tool with the same input summary.
    fn same_call(&self, other: &Call) -> bool {
        self.tool == other.tool && self.detail == other.detail
    }
}

/// How the open prompt ends, beyond the turn ending or a new prompt.
#[derive(Debug, Default)]
struct Ends {
    /// The conversation it was raised in.
    agent: Option<String>,
    /// Its tool call: the PostToolUse with this id ends it.
    id: Option<String>,
    /// A permission with no tool call id: the PostToolUse of the same tool
    /// and summary in its conversation ends it.
    call: Option<Call>,
    /// A permission whose request carried no id: any later PreToolUse of
    /// its conversation that is not its own call ends it (the session moved
    /// on, after a denial say).
    moved_on: bool,
}

impl PromptState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies one hook event. True when the open prompt changed.
    pub fn on_hook(&mut self, ev: &HookEvent) -> bool {
        let main = ev.agent_id.is_none();
        let next = match ev.event {
            HookKind::PreToolUse => {
                let moved_on = self.open.is_some()
                    && self.ends.moved_on
                    && self.ends.agent == ev.agent_id
                    && self.ends.id != ev.tool_use_id;
                self.last_call.insert(ev.agent_id.clone(), Call::of(ev));
                if main && ev.tool.as_deref() == Some(ASK_USER_QUESTION) {
                    let id = self.next_id(ev.tool_use_id.as_deref());
                    let ends = Ends {
                        id: Some(id.clone()),
                        ..Ends::default()
                    };
                    Some((Some(self.prompt(ev, id, false)), ends))
                } else if moved_on {
                    Some((None, Ends::default()))
                } else {
                    None
                }
            }
            HookKind::PermissionRequest if ev.tool.as_deref() != Some(ASK_USER_QUESTION) => {
                Some(self.permission(ev))
            }
            HookKind::Notification
                if main
                    && self.open.is_none()
                    && ev.notification_type.as_deref() == Some(PERMISSION_PROMPT) =>
            {
                let prompt = Prompt {
                    id: self.next_id(None),
                    subagent: false,
                    info: PromptShown::Unknown,
                };
                Some((Some(prompt), Ends::default()))
            }
            HookKind::PostToolUse if self.ended_by(ev) => Some((None, Ends::default())),
            // A session start is a new process (a resume, say): any dialog
            // of the one before is gone.
            HookKind::UserPromptSubmit | HookKind::Stop | HookKind::SessionStart if main => {
                Some((None, Ends::default()))
            }
            _ => None,
        };
        let Some((next, ends)) = next else {
            return false;
        };
        self.ends = ends;
        if next == self.open {
            return false;
        }
        self.open = next;
        true
    }

    /// Forgets the open prompt, for a new session. Ids keep counting, so a
    /// new session's prompt never reuses an earlier one's id.
    pub fn clear(&mut self) {
        self.open = None;
        self.ends = Ends::default();
        self.last_call.clear();
    }

    /// The prompt the session is waiting on.
    pub fn open(&self) -> Option<&Prompt> {
        self.open.as_ref()
    }

    /// The prompt a `PermissionRequest` opens and what ends it. Claude Code
    /// sends no `tool_use_id` with it, so the call is the conversation's
    /// latest PreToolUse when that was the same tool and input; that hook
    /// only covers some tools, so failing that the call is matched by tool
    /// and summary when it finishes.
    fn permission(&mut self, ev: &HookEvent) -> (Option<Prompt>, Ends) {
        let agent = ev.agent_id.clone();
        let subagent = agent.is_some();
        if let Some(id) = &ev.tool_use_id {
            let ends = Ends {
                agent,
                id: Some(id.clone()),
                ..Ends::default()
            };
            return (Some(self.prompt(ev, id.clone(), subagent)), ends);
        }
        let this = Call::of(ev);
        let adopted = self
            .last_call
            .get(&agent)
            .filter(|pre| pre.same_call(&this))
            .and_then(|pre| pre.id.clone());
        let ends = Ends {
            agent,
            id: adopted.clone(),
            call: adopted.is_none().then_some(this),
            moved_on: true,
        };
        let id = self.next_id(adopted.as_deref());
        (Some(self.prompt(ev, id, subagent)), ends)
    }

    /// True when a PostToolUse finishes the open prompt's call. Matched by
    /// the call's id where known, not the tool name, since parallel calls
    /// can finish while a dialog is open. The id is unique, so a subagent's
    /// own call clears the subagent prompt it raised.
    fn ended_by(&self, ev: &HookEvent) -> bool {
        if self.open.is_none() {
            return false;
        }
        if self.ends.id.is_some() && self.ends.id == ev.tool_use_id {
            return true;
        }
        self.ends
            .call
            .as_ref()
            .is_some_and(|call| self.ends.agent == ev.agent_id && call.same_call(&Call::of(ev)))
    }

    fn prompt(&self, ev: &HookEvent, id: String, subagent: bool) -> Prompt {
        let info = match &ev.prompt {
            Some(PromptInfo::Other) | None => PromptShown::Unknown,
            Some(info) => PromptShown::Known(info.clone()),
        };
        Prompt { id, subagent, info }
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

    fn sub(agent: &str, ev: HookEvent) -> HookEvent {
        HookEvent {
            agent_id: Some(agent.into()),
            ..ev
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

    /// A permission request as Claude Code sends it: no `tool_use_id`.
    fn permission(tool: &str, detail: &str) -> HookEvent {
        HookEvent {
            tool: Some(tool.into()),
            detail: Some(detail.into()),
            prompt: Some(PromptInfo::Permission {
                tool: tool.into(),
                summary: detail.into(),
                plan: None,
            }),
            ..ev(HookKind::PermissionRequest)
        }
    }

    fn call(kind: HookKind, tool: &str, detail: &str, id: &str) -> HookEvent {
        HookEvent {
            tool: Some(tool.into()),
            detail: Some(detail.into()),
            tool_use_id: Some(id.into()),
            ..ev(kind)
        }
    }

    fn pre(tool: &str, detail: &str, id: &str) -> HookEvent {
        call(HookKind::PreToolUse, tool, detail, id)
    }

    fn post(tool: &str, detail: &str, id: &str) -> HookEvent {
        call(HookKind::PostToolUse, tool, detail, id)
    }

    const RM: &str = "rm -rf /tmp/demo/target";

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
        assert!(s.on_hook(&post(ASK_USER_QUESTION, "", "tu-1")));
        assert!(s.open().is_none());
    }

    #[test]
    fn another_tools_post_tool_use_keeps_the_prompt() {
        let mut s = PromptState::new();
        assert!(!s.on_hook(&pre("Bash", RM, "tu-1")));
        assert!(s.on_hook(&permission("Bash", RM)));
        assert!(!s.on_hook(&post("Bash", "ls", "tu-2")));
        assert_eq!(s.open().unwrap().id, "tu-1");
        // A PostToolUse with no id clears nothing either.
        assert!(!s.on_hook(&ev(HookKind::PostToolUse)));
        assert_eq!(s.open().unwrap().id, "tu-1");
    }

    #[test]
    fn permission_for_ask_user_question_is_ignored() {
        let mut s = PromptState::new();
        assert!(s.on_hook(&question("tu-1")));
        assert!(!s.on_hook(&permission(ASK_USER_QUESTION, "")));
        assert!(matches!(
            s.open().unwrap().info,
            PromptShown::Known(PromptInfo::Question { .. })
        ));
    }

    #[test]
    fn subagent_permission_is_shown_as_subagent() {
        let mut s = PromptState::new();
        assert!(!s.on_hook(&sub("ag-1", pre("Bash", RM, "tu-7"))));
        assert!(s.on_hook(&sub("ag-1", permission("Bash", RM))));
        let open = s.open().unwrap();
        assert!(open.subagent);
        assert_eq!(open.id, "tu-7");
        // Other subagent events are not followed: its question is not shown,
        // and its Stop does not clear the main terminal's prompt.
        let mut other = PromptState::new();
        assert!(!other.on_hook(&sub("ag-1", question("tu-8"))));
        assert!(other.open().is_none());
        assert!(!s.on_hook(&sub("ag-1", ev(HookKind::Stop))));
        // The main conversation's next call is not the subagent moving on.
        assert!(!s.on_hook(&pre("Edit", "/tmp/demo/a.rs", "tu-9")));
        // Its own tool call finishing clears it.
        assert!(s.on_hook(&sub("ag-1", post("Bash", RM, "tu-7"))));
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
        assert!(!known.on_hook(&pre("Bash", RM, "tu-1")));
        assert!(known.on_hook(&permission("Bash", RM)));
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
        assert!(s.on_hook(&permission("Bash", RM)));
        assert!(s.on_hook(&ev(HookKind::UserPromptSubmit)));
        assert!(s.open().is_none());
        assert!(s.on_hook(&question("tu-2")));
        assert!(s.on_hook(&ev(HookKind::Stop)));
        assert!(s.open().is_none());
        // Clearing with nothing open is no change.
        assert!(!s.on_hook(&ev(HookKind::Stop)));
        assert!(s.on_hook(&permission("Bash", RM)));
        assert!(s.on_hook(&ev(HookKind::SessionStart)));
        assert!(s.open().is_none());
    }

    #[test]
    fn a_new_prompt_replaces_the_old_and_ids_count_up() {
        let mut s = PromptState::new();
        assert!(s.on_hook(&permission("WebFetch", "https://demo.example.test")));
        assert_eq!(s.open().unwrap().id, "n1");
        assert!(s.on_hook(&permission("Edit", "/tmp/demo/a.rs")));
        assert_eq!(s.open().unwrap().id, "n2");
        // With no id to go by, a repeat is a new prompt.
        assert!(s.on_hook(&permission("Edit", "/tmp/demo/a.rs")));
        assert_eq!(s.open().unwrap().id, "n3");
        // A request that does carry an id is keyed by it.
        let with_id = HookEvent {
            tool_use_id: Some("tu-4".into()),
            ..permission("Bash", RM)
        };
        assert!(s.on_hook(&with_id));
        assert_eq!(s.open().unwrap().id, "tu-4");
        assert!(s.on_hook(&post("Bash", RM, "tu-4")));
    }

    #[test]
    fn permission_without_id_takes_the_pre_tool_use_id() {
        let mut s = PromptState::new();
        assert!(!s.on_hook(&pre("Bash", "cargo test", "tu-5")));
        assert!(s.on_hook(&permission("Bash", "cargo test")));
        assert_eq!(s.open().unwrap().id, "tu-5");
        // A repeat of the request is the same prompt.
        assert!(!s.on_hook(&permission("Bash", "cargo test")));
        assert!(s.on_hook(&post("Bash", "cargo test", "tu-5")));
        assert!(s.open().is_none());
        // A PreToolUse of another call is not taken.
        assert!(!s.on_hook(&pre("Bash", "ls", "tu-6")));
        assert!(s.on_hook(&permission("Bash", RM)));
        assert_eq!(s.open().unwrap().id, "n1");
    }

    #[test]
    fn permission_without_id_clears_on_matching_post_tool_use() {
        let mut s = PromptState::new();
        let url = "https://demo.example.test/a";
        assert!(s.on_hook(&permission("WebFetch", url)));
        assert_eq!(s.open().unwrap().id, "n1");
        // Another call of the tool, or the same call in a subagent, is not it.
        assert!(!s.on_hook(&post("WebFetch", "https://demo.example.test/b", "tu-1")));
        assert!(!s.on_hook(&sub("ag-1", post("WebFetch", url, "tu-2"))));
        assert!(s.open().is_some());
        assert!(s.on_hook(&post("WebFetch", url, "tu-3")));
        assert!(s.open().is_none());
    }

    #[test]
    fn a_later_pre_tool_use_clears_a_denied_permission() {
        let mut s = PromptState::new();
        assert!(!s.on_hook(&pre("Bash", RM, "tu-1")));
        assert!(s.on_hook(&permission("Bash", RM)));
        // Its own call again, or a subagent's call, is no move.
        assert!(!s.on_hook(&pre("Bash", RM, "tu-1")));
        assert!(!s.on_hook(&sub("ag-1", pre("Bash", "ls", "tu-2"))));
        assert_eq!(s.open().unwrap().id, "tu-1");
        // Denied, the session goes on to another call.
        assert!(s.on_hook(&pre("Edit", "/tmp/demo/a.rs", "tu-3")));
        assert!(s.open().is_none());
        // The same for one matched by tool and summary.
        assert!(s.on_hook(&permission("WebFetch", "https://demo.example.test")));
        assert!(s.on_hook(&pre("Bash", "ls", "tu-4")));
        assert!(s.open().is_none());
        // A question is not cleared by another call starting.
        assert!(s.on_hook(&question("tu-5")));
        assert!(!s.on_hook(&pre("Bash", "ls", "tu-6")));
        assert_eq!(s.open().unwrap().id, "tu-5");
    }
}
