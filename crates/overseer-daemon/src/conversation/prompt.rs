//! Which prompt a slot's session is waiting on, followed from its hook
//! events and the tool calls its transcript records.

use std::collections::VecDeque;

use overseer_proto::msg::{HookEvent, HookKind, Prompt, PromptInfo, PromptShown};

use super::parse::ToolEvent;

/// The tool Claude Code uses to ask the user a question with options.
const ASK_USER_QUESTION: &str = "AskUserQuestion";

/// The notification Claude Code sends when it draws a permission dialog.
const PERMISSION_PROMPT: &str = "permission_prompt";

/// Unanswered tool calls remembered per slot; a call that never gets an
/// answer is forgotten once this many newer ones started.
const STARTED_MAX: usize = 32;

/// The prompt a slot is waiting on, if any.
#[derive(Debug, Default)]
pub struct PromptState {
    open: Option<Prompt>,
    /// What ends the open prompt.
    ends: Ends,
    /// Main-conversation tool calls the transcript started and has not
    /// answered yet, oldest first.
    started: VecDeque<Started>,
    /// Numbers the ids of prompts that came with no `tool_use_id`.
    counter: u64,
}

/// A tool call the transcript started.
#[derive(Debug, Clone)]
struct Started {
    id: String,
    name: String,
    summary: String,
}

/// How the open prompt ends, beyond the turn ending or a new prompt.
#[derive(Debug, Default)]
struct Ends {
    /// The conversation it was raised in.
    agent: Option<String>,
    /// Its tool call: that call's PostToolUse or transcript answer ends it.
    key: Option<String>,
    /// A permission with no tool call id, by tool and summary: until its
    /// call is found in the transcript, the PostToolUse of the same tool
    /// and summary in its conversation ends it.
    call: Option<(Option<String>, Option<String>)>,
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
                let id = self.next_id(ev.tool_use_id.as_deref());
                let ends = Ends {
                    key: Some(id.clone()),
                    ..Ends::default()
                };
                Some((Some(prompt(ev, id, false)), ends))
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
        if self.awaits_call() {
            self.ends.key = self.best_started(false);
        }
        if next == self.open {
            return false;
        }
        self.open = next;
        true
    }

    /// Applies the tool calls a transcript read found. An answered call
    /// ends the prompt it belongs to; a permission prompt with no call id
    /// takes the call it is for from the unanswered ones. True when the
    /// open prompt changed.
    pub fn on_tools(&mut self, tools: &[ToolEvent]) -> bool {
        let was = self.open.is_some();
        for tool in tools {
            match tool {
                ToolEvent::Started { id, name, summary } => {
                    if self.started.iter().any(|s| s.id == *id) {
                        continue;
                    }
                    self.started.push_back(Started {
                        id: id.clone(),
                        name: name.clone(),
                        summary: summary.clone(),
                    });
                    if self.started.len() > STARTED_MAX {
                        self.started.pop_front();
                    }
                }
                ToolEvent::Finished { id } => {
                    // Started and answered within one read: the call is
                    // found before it is forgotten, but only by a summary
                    // that agrees, since a wrong guess would end the prompt.
                    if self.awaits_call() {
                        self.ends.key = self.best_started(true);
                    }
                    if self.open.is_some()
                        && (self.ends.key.as_ref() == Some(id) || self.unplaced())
                    {
                        self.open = None;
                        self.ends = Ends::default();
                    }
                    self.started.retain(|s| s.id != *id);
                }
            }
        }
        if self.awaits_call() {
            self.ends.key = self.best_started(false);
        }
        was && self.open.is_none()
    }

    /// Forgets the open prompt and the unanswered calls, for a new
    /// session. Ids keep counting, so a new session's prompt never reuses
    /// an earlier one's id.
    pub fn clear(&mut self) {
        self.open = None;
        self.ends = Ends::default();
        self.started.clear();
    }

    /// The prompt the session is waiting on.
    pub fn open(&self) -> Option<&Prompt> {
        self.open.as_ref()
    }

    /// Drops prompt `id` if it is the open one, as when the screen shows
    /// its dialog has gone. True when it was open.
    pub fn forget(&mut self, id: &str) -> bool {
        if self.open.as_ref().is_none_or(|p| p.id != id) {
            return false;
        }
        self.open = None;
        self.ends = Ends::default();
        true
    }

    /// An open prompt with nothing known of its tool call: a permission
    /// notification with no request. Any main-conversation call finishing
    /// ends it, since the notification may come after the call it was for.
    fn unplaced(&self) -> bool {
        self.open.is_some() && self.ends.key.is_none() && self.ends.call.is_none()
    }

    /// The prompt a `PermissionRequest` opens and what ends it. Claude Code
    /// sends no `tool_use_id` with it, so the call is found later in the
    /// transcript, or matched by tool and summary when it finishes.
    fn permission(&mut self, ev: &HookEvent) -> (Option<Prompt>, Ends) {
        let subagent = ev.agent_id.is_some();
        let ends = Ends {
            agent: ev.agent_id.clone(),
            key: ev.tool_use_id.clone(),
            call: ev
                .tool_use_id
                .is_none()
                .then(|| (ev.tool.clone(), ev.detail.clone())),
        };
        let id = self.next_id(ev.tool_use_id.as_deref());
        (Some(prompt(ev, id, subagent)), ends)
    }

    /// An open main-conversation permission whose call is not known yet.
    /// A subagent's calls are in its own transcript, which is not read.
    fn awaits_call(&self) -> bool {
        self.open.as_ref().is_some_and(|p| !p.subagent)
            && self.ends.key.is_none()
            && self.ends.call.is_some()
    }

    /// The unanswered call the open permission is most likely for: the
    /// latest of its tool whose summary agrees with the prompt's, else
    /// (unless `strict`) the latest of its tool.
    fn best_started(&self, strict: bool) -> Option<String> {
        let (tool, detail) = self.ends.call.as_ref()?;
        let of_tool = || {
            self.started
                .iter()
                .rev()
                .filter(|s| Some(&s.name) == tool.as_ref())
        };
        of_tool()
            .find(|s| detail.as_deref().is_some_and(|d| same_start(d, &s.summary)))
            .or_else(|| of_tool().next().filter(|_| !strict))
            .map(|s| s.id.clone())
    }

    /// True when a PostToolUse finishes the open prompt's call: by id where
    /// known (the id is unique, so a subagent's own call clears the
    /// subagent prompt it raised), else by tool and summary in the same
    /// conversation. A prompt with no call at all ends with any main call.
    fn ended_by(&self, ev: &HookEvent) -> bool {
        if self.open.is_none() {
            return false;
        }
        if let Some(key) = &self.ends.key {
            return ev.tool_use_id.as_ref() == Some(key);
        }
        if self.unplaced() {
            return ev.agent_id.is_none();
        }
        self.ends.call.as_ref().is_some_and(|(tool, detail)| {
            self.ends.agent == ev.agent_id && *tool == ev.tool && *detail == ev.detail
        })
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

fn prompt(ev: &HookEvent, id: String, subagent: bool) -> Prompt {
    let info = match &ev.prompt {
        Some(PromptInfo::Other) | None => PromptShown::Unknown,
        Some(info) => PromptShown::Known(info.clone()),
    };
    Prompt { id, subagent, info }
}

/// Two summaries of one input agree: both non-empty and equal over the
/// shorter one, since the hook and the transcript cut them differently.
fn same_start(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim(), b.trim());
    let n = a.chars().count().min(b.chars().count());
    n > 0 && a.chars().take(n).eq(b.chars().take(n))
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

    fn post(tool: &str, detail: &str, id: &str) -> HookEvent {
        HookEvent {
            tool: Some(tool.into()),
            detail: Some(detail.into()),
            tool_use_id: Some(id.into()),
            ..ev(HookKind::PostToolUse)
        }
    }

    fn started(id: &str, name: &str, summary: &str) -> ToolEvent {
        ToolEvent::Started {
            id: id.into(),
            name: name.into(),
            summary: summary.into(),
        }
    }

    fn finished(id: &str) -> ToolEvent {
        ToolEvent::Finished { id: id.into() }
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
        assert!(s.on_hook(&permission("Bash", RM)));
        assert!(!s.on_hook(&post("Bash", "ls", "tu-2")));
        assert!(!s.on_hook(&post("Edit", RM, "tu-3")));
        assert_eq!(s.open().unwrap().id, "n1");
        // A PostToolUse with no id clears nothing either.
        assert!(!s.on_hook(&ev(HookKind::PostToolUse)));
        assert!(s.open().is_some());
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
        assert!(s.on_hook(&sub("ag-1", permission("Bash", RM))));
        let open = s.open().unwrap();
        assert!(open.subagent);
        // Other subagent events are not followed: its question is not shown,
        // and its Stop does not clear the main terminal's prompt.
        let mut other = PromptState::new();
        assert!(!other.on_hook(&sub("ag-1", question("tu-8"))));
        assert!(other.open().is_none());
        assert!(!s.on_hook(&sub("ag-1", ev(HookKind::Stop))));
        // A main call of the same tool is not the subagent's.
        assert!(!s.on_tools(&[started("tu-9", "Bash", RM)]));
        assert!(!s.on_tools(&[finished("tu-9")]));
        assert!(!s.on_hook(&post("Bash", RM, "tu-9")));
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
        assert!(known.on_hook(&permission("Bash", RM)));
        assert!(!known.on_hook(&note));
        assert!(matches!(known.open().unwrap().info, PromptShown::Known(_)));
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
    fn permission_without_id_clears_on_matching_post_tool_use() {
        let mut s = PromptState::new();
        let url = "https://demo.example.test/a";
        assert!(s.on_hook(&permission("WebFetch", url)));
        // Another call of the tool, or the same call in a subagent, is not it.
        assert!(!s.on_hook(&post("WebFetch", "https://demo.example.test/b", "tu-1")));
        assert!(!s.on_hook(&sub("ag-1", post("WebFetch", url, "tu-2"))));
        assert!(s.open().is_some());
        assert!(s.on_hook(&post("WebFetch", url, "tu-3")));
        assert!(s.open().is_none());
    }

    #[test]
    fn a_denied_permission_clears_from_the_transcript() {
        let mut s = PromptState::new();
        assert!(s.on_hook(&permission("Bash", RM)));
        let id = s.open().unwrap().id.clone();
        // Another call starting and finishing is not this one.
        assert!(!s.on_tools(&[started("tu-1", "Read", "/tmp/demo/a.rs")]));
        assert!(!s.on_tools(&[finished("tu-1")]));
        assert!(!s.on_tools(&[started("tu-2", "Bash", RM)]));
        // Found, but the id the pane saw stays.
        assert_eq!(s.open().unwrap().id, id);
        // The denial is the call's tool_result.
        assert!(s.on_tools(&[finished("tu-2")]));
        assert!(s.open().is_none());
    }

    #[test]
    fn an_approved_permission_clears_when_its_result_is_written() {
        let mut s = PromptState::new();
        assert!(s.on_hook(&permission("Bash", RM)));
        // Started and answered within one read.
        assert!(s.on_tools(&[started("tu-1", "Bash", RM), finished("tu-1")]));
        assert!(s.open().is_none());
        // Of two of its tool, the one whose summary agrees is taken; the
        // transcript's summary may be cut shorter than the hook's.
        let long = format!("{RM} && echo {}", "x".repeat(80));
        assert!(s.on_hook(&permission("Bash", &long)));
        let cut: String = long.chars().take(40).collect();
        let tools = [started("tu-2", "Bash", &cut), started("tu-3", "Bash", "ls")];
        assert!(!s.on_tools(&tools));
        assert!(!s.on_tools(&[finished("tu-3")]));
        assert!(s.on_tools(&[finished("tu-2")]));
        // In one read, another call of the tool finishing is not taken for
        // it by name alone.
        assert!(s.on_hook(&permission("Bash", RM)));
        assert!(!s.on_tools(&[started("tu-4", "Bash", "ls"), finished("tu-4")]));
        assert!(s.open().is_some());
    }

    #[test]
    fn a_tool_started_before_the_hook_is_still_adopted() {
        let mut s = PromptState::new();
        assert!(!s.on_tools(&[started("tu-1", "Bash", RM)]));
        assert!(s.on_hook(&permission("Bash", RM)));
        assert!(s.on_tools(&[finished("tu-1")]));
        assert!(s.open().is_none());
        // An answered call is not taken by a later prompt.
        assert!(s.on_hook(&permission("Bash", RM)));
        assert!(!s.on_tools(&[finished("tu-1")]));
        assert!(s.open().is_some());
    }

    #[test]
    fn a_question_clears_from_its_transcript_result() {
        let mut s = PromptState::new();
        assert!(s.on_hook(&question("tu-1")));
        assert!(!s.on_tools(&[started("tu-1", ASK_USER_QUESTION, "")]));
        assert!(!s.on_tools(&[finished("tu-2")]));
        assert!(s.on_tools(&[finished("tu-1")]));
        assert!(s.open().is_none());
    }

    #[test]
    fn unanswered_calls_are_bounded_and_cleared() {
        let mut s = PromptState::new();
        let many: Vec<ToolEvent> = (0..STARTED_MAX + 5)
            .map(|n| started(&format!("tu-{n}"), "Bash", RM))
            .collect();
        s.on_tools(&many);
        assert_eq!(s.started.len(), STARTED_MAX);
        assert_eq!(s.started[0].id, "tu-5");
        s.clear();
        assert!(s.started.is_empty());
    }

    #[test]
    fn a_known_call_is_ended_by_its_own_id_only() {
        let mut s = PromptState::new();
        assert!(s.on_hook(&permission("Bash", RM)));
        assert!(!s.on_tools(&[started("tu-1", "Bash", RM)]));
        // A parallel call of the same tool and summary is not this one.
        assert!(!s.on_hook(&post("Bash", RM, "tu-2")));
        assert!(s.open().is_some());
        assert!(s.on_hook(&post("Bash", RM, "tu-1")));
        assert!(s.open().is_none());
    }

    #[test]
    fn a_prompt_with_no_call_ends_with_the_next_main_call() {
        let note = HookEvent {
            notification_type: Some(PERMISSION_PROMPT.into()),
            ..ev(HookKind::Notification)
        };
        let mut s = PromptState::new();
        assert!(s.on_hook(&note));
        // A subagent's call is not the main conversation's.
        assert!(!s.on_hook(&sub("ag-1", post("Bash", RM, "tu-1"))));
        assert!(s.on_hook(&post("Bash", RM, "tu-2")));
        assert!(s.open().is_none());
        // Or its answer in the transcript.
        assert!(s.on_hook(&note));
        assert!(s.on_tools(&[finished("tu-3")]));
        assert!(s.open().is_none());
    }

    #[test]
    fn forget_drops_only_the_named_prompt() {
        let mut s = PromptState::new();
        assert!(s.on_hook(&permission("Bash", RM)));
        assert!(!s.forget("n9"));
        assert!(s.open().is_some());
        assert!(s.forget("n1"));
        assert!(s.open().is_none());
        assert!(!s.forget("n1"));
    }
}
