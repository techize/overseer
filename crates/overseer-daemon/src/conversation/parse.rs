//! Turns Claude Code transcript lines into conversation entries. Pure: the
//! caller reads the file and feeds the bytes.

use overseer_proto::msg::{ConvEntry, ConvKind, Question};
use serde_json::Value;

use crate::work::sources::parse_iso_time;

/// User text starting with one of these is Claude Code plumbing, not the user.
const SKIPPED_PREFIXES: [&str; 3] = ["<system-reminder>", "<command-", "<local-command-"];

/// Tool input keys that make a good one-line summary, best first.
const SUMMARY_KEYS: [&str; 5] = ["command", "file_path", "pattern", "description", "url"];

/// The tool Claude Code uses to ask the user a question with options.
pub const ASK_USER_QUESTION: &str = "AskUserQuestion";

/// Longest tool summary, in characters.
const SUMMARY_MAX: usize = 80;

/// Longest entry text, in bytes once escaped for JSON, so a few huge
/// pastes cannot make a full view too big to send.
const TEXT_MAX: usize = 16 * 1024;

/// Ends a text cut to [`TEXT_MAX`].
const CUT_NOTE: &str = "\n\n(cut, see the session)";

/// A main-conversation tool call starting or getting its answer, read from
/// the transcript. Used to tell when a prompt is over, even one that ended
/// with no hook (a denial ends the turn without one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolEvent {
    /// A `tool_use` block of an assistant line. `summary` is the one a
    /// `Tool` entry shows; `questions` are an `AskUserQuestion` call's,
    /// so a question still open is known without its hook.
    Started {
        id: String,
        name: String,
        summary: String,
        questions: Option<Vec<Question>>,
    },
    /// A `tool_result` block: the call was answered, approved or not.
    Finished { id: String },
}

/// User text Claude Code writes when Esc stops a turn, alone or with
/// " for tool use" after it.
pub const INTERRUPTED: &str = "[Request interrupted by user";

/// Where a main-conversation line leaves the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Turn {
    /// The user's prompt: a turn starts.
    Began,
    /// The turn is over: it ended (`end_turn`, or the `turn_duration` line
    /// after it) or was interrupted.
    Ended,
}

/// What some bytes of a transcript held.
#[derive(Debug, Default, PartialEq)]
pub struct Fed {
    pub entries: Vec<ConvEntry>,
    pub tools: Vec<ToolEvent>,
    /// The last line that began or ended a turn, if any did.
    pub turn: Option<Turn>,
    /// The context the last main-conversation assistant line used, in
    /// tokens, if any line said.
    pub context: Option<u64>,
}

/// Follows a transcript file as it grows. Feed it the new bytes; it hands back
/// the entries from every complete line.
#[derive(Debug, Default)]
pub struct Tail {
    offset: u64,
    partial: Vec<u8>,
}

impl Tail {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bytes consumed from the file so far, including any held back as an
    /// unfinished line. Resume reading here.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Takes the next bytes of the file. An unterminated last line waits for
    /// its newline. Lines that are not JSON, or not a shape we know, are skipped.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<ConvEntry> {
        self.read(bytes).entries
    }

    /// [`Tail::feed`], with the tool calls the lines started and finished.
    pub fn read(&mut self, bytes: &[u8]) -> Fed {
        self.offset += bytes.len() as u64;
        self.partial.extend_from_slice(bytes);
        let mut out = Fed::default();
        let mut start = 0;
        while let Some(i) = self.partial[start..].iter().position(|&b| b == b'\n') {
            let mut line = &self.partial[start..start + i];
            start += i + 1;
            if let Some(stripped) = line.strip_suffix(b"\r") {
                line = stripped;
            }
            if let Ok(value) = serde_json::from_slice::<Value>(line) {
                out.entries.extend(entry_of(&value));
                out.tools.extend(tools_of(&value));
                out.turn = turn_of(&value).or(out.turn);
                out.context = context_of(&value).or(out.context);
            }
        }
        self.partial.drain(..start);
        out
    }
}

/// The entries one transcript line carries. Side chains, meta lines, the
/// summary `/compact` leaves, thinking, tool results and every other line
/// type give nothing.
pub fn entry_of(line: &Value) -> Vec<ConvEntry> {
    if flag(line, "isSidechain") || flag(line, "isMeta") || flag(line, "isCompactSummary") {
        return Vec::new();
    }
    let at = line
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_iso_time)
        .unwrap_or(0);
    let content = line.pointer("/message/content");
    let kinds = match line.get("type").and_then(Value::as_str) {
        Some("user") => user_kinds(content),
        Some("assistant") => assistant_kinds(content),
        _ => Vec::new(),
    };
    kinds
        .into_iter()
        .map(|kind| ConvEntry { at, kind })
        .collect()
}

/// The tool calls one transcript line starts or finishes. Side chains are a
/// subagent's, and give nothing.
pub fn tools_of(line: &Value) -> Vec<ToolEvent> {
    if flag(line, "isSidechain") {
        return Vec::new();
    }
    let Some(Value::Array(blocks)) = line.pointer("/message/content") else {
        return Vec::new();
    };
    let text = |b: &Value, k: &str| b.get(k).and_then(Value::as_str).map(String::from);
    let kind = line.get("type").and_then(Value::as_str);
    blocks
        .iter()
        .filter_map(|b| match (kind, b.get("type").and_then(Value::as_str)) {
            (Some("assistant"), Some("tool_use")) => {
                let name = text(b, "name")?;
                let questions = (name == ASK_USER_QUESTION)
                    .then(|| Question::list_of(b.get("input")?))
                    .flatten();
                Some(ToolEvent::Started {
                    id: text(b, "id")?,
                    name,
                    summary: summary_of(b.get("input")),
                    questions,
                })
            }
            (Some("user"), Some("tool_result")) => Some(ToolEvent::Finished {
                id: text(b, "tool_use_id")?,
            }),
            _ => None,
        })
        .collect()
}

/// The context an assistant line's request used: its input tokens, the ones
/// written to the cache and the ones read from it. Side chains are a
/// subagent's own context and give nothing, and so does a line without
/// usage.
pub fn context_of(line: &Value) -> Option<u64> {
    if flag(line, "isSidechain") || line.get("type").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let usage = line.pointer("/message/usage")?.as_object()?;
    let fields = [
        "input_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
    ];
    let found: Vec<u64> = fields
        .iter()
        .filter_map(|k| usage.get(*k).and_then(Value::as_u64))
        .collect();
    (!found.is_empty()).then(|| found.iter().sum())
}

/// Whether one transcript line begins or ends a main-conversation turn. A
/// user line with text is a prompt, unless the text is the interrupt note
/// or Claude Code plumbing; a tool result alone is neither.
pub fn turn_of(line: &Value) -> Option<Turn> {
    if flag(line, "isSidechain") || flag(line, "isMeta") || flag(line, "isCompactSummary") {
        return None;
    }
    match line.get("type").and_then(Value::as_str)? {
        "user" => {
            let texts = user_texts(line.pointer("/message/content"));
            if texts.iter().any(|t| t.starts_with(INTERRUPTED)) {
                Some(Turn::Ended)
            } else {
                texts
                    .iter()
                    .any(|t| !skipped(t) && !t.trim().is_empty())
                    .then_some(Turn::Began)
            }
        }
        "assistant" => (line.pointer("/message/stop_reason").and_then(Value::as_str)
            == Some("end_turn"))
        .then_some(Turn::Ended),
        "system" => (line.get("subtype").and_then(Value::as_str) == Some("turn_duration"))
            .then_some(Turn::Ended),
        _ => None,
    }
}

fn flag(line: &Value, key: &str) -> bool {
    line.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn skipped(text: &str) -> bool {
    SKIPPED_PREFIXES.iter().any(|p| text.starts_with(p))
}

/// The text of a user line's content: the string, or its text blocks.
fn user_texts(content: Option<&Value>) -> Vec<&str> {
    match content {
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect(),
        _ => Vec::new(),
    }
}

fn user_kinds(content: Option<&Value>) -> Vec<ConvKind> {
    let kept: Vec<&str> = user_texts(content)
        .into_iter()
        .filter(|t| !skipped(t) && !t.trim().is_empty())
        .collect();
    if kept.is_empty() {
        return Vec::new();
    }
    vec![ConvKind::User {
        text: capped(kept.join("\n\n")),
    }]
}

/// `text`, cut to [`TEXT_MAX`] bytes as sent (escaped for JSON) on a
/// character boundary, with a note saying so.
fn capped(mut text: String) -> String {
    let mut sent = 0;
    let Some((end, _)) = text.char_indices().find(|&(_, c)| {
        sent += escaped_len(c);
        sent > TEXT_MAX
    }) else {
        return text;
    };
    text.truncate(end);
    text.push_str(CUT_NOTE);
    text
}

/// The bytes `c` takes in a JSON string: a quote, a backslash and the
/// common controls take two, other controls six (`\u0001`).
fn escaped_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
        '\0'..='\u{1f}' => 6,
        c => c.len_utf8(),
    }
}

fn assistant_kinds(content: Option<&Value>) -> Vec<ConvKind> {
    let Some(Value::Array(blocks)) = content else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter_map(|b| match b.get("type").and_then(Value::as_str)? {
            "text" => Some(ConvKind::Assistant {
                text: capped(b.get("text").and_then(Value::as_str)?.to_string()),
            }),
            "tool_use" => Some(ConvKind::Tool {
                name: b.get("name").and_then(Value::as_str)?.to_string(),
                summary: summary_of(b.get("input")),
            }),
            _ => None,
        })
        .collect()
}

fn summary_of(input: Option<&Value>) -> String {
    let Some(input) = input else {
        return String::new();
    };
    SUMMARY_KEYS
        .iter()
        .find_map(|k| input.get(k).and_then(Value::as_str))
        .map(|s| {
            let first = s.lines().next().unwrap_or("").trim();
            first.chars().take(SUMMARY_MAX).collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod context_tests {
    use super::*;

    fn line(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn context_is_the_input_and_cache_tokens_of_an_assistant_line() {
        let l = line(
            r#"{"type":"assistant","message":{"usage":{"input_tokens":12,"cache_creation_input_tokens":300,"cache_read_input_tokens":41000,"output_tokens":900}}}"#,
        );
        assert_eq!(context_of(&l), Some(41312));
        let some = line(r#"{"type":"assistant","message":{"usage":{"input_tokens":5}}}"#);
        assert_eq!(context_of(&some), Some(5));
        for none in [
            r#"{"type":"assistant","message":{"usage":{"output_tokens":9}}}"#,
            r#"{"type":"assistant","message":{"content":[]}}"#,
            r#"{"type":"assistant","isSidechain":true,"message":{"usage":{"input_tokens":5}}}"#,
            r#"{"type":"user","message":{"usage":{"input_tokens":5}}}"#,
        ] {
            assert_eq!(context_of(&line(none)), None, "{none}");
        }
    }

    #[test]
    fn a_read_keeps_the_last_context_seen() {
        let mut t = Tail::new();
        let text = concat!(
            r#"{"type":"assistant","message":{"usage":{"input_tokens":10}}}"#,
            "\n",
            r#"{"type":"assistant","isSidechain":true,"message":{"usage":{"input_tokens":99999}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"usage":{"input_tokens":20,"cache_read_input_tokens":5}}}"#,
            "\n",
            r#"{"type":"user","message":{"content":"next"}}"#,
            "\n",
        );
        assert_eq!(t.read(text.as_bytes()).context, Some(25));
        assert_eq!(t.read(b"{\"type\":\"user\"}\n").context, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = concat!(
        r#"{"type":"user","timestamp":"2026-11-25T10:00:00Z","message":{"role":"user","content":"fix DEMO-1 in demo-api"}}"#,
        "\n",
        r#"{"type":"assistant","timestamp":"2026-11-25T10:00:05Z","message":{"role":"assistant","content":[{"type":"text","text":"On it."},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test\n--quiet"}}]}}"#,
        "\n",
    );

    fn kinds(entries: &[ConvEntry]) -> Vec<ConvKind> {
        entries.iter().map(|e| e.kind.clone()).collect()
    }

    #[test]
    fn user_and_assistant_text_become_entries() {
        let mut tail = Tail::new();
        let got = tail.feed(FIXTURE.as_bytes());
        assert_eq!(got[0].at, 1_795_600_800);
        assert_eq!(got[1].at, 1_795_600_805);
        assert_eq!(
            kinds(&got)[..2],
            [
                ConvKind::User {
                    text: "fix DEMO-1 in demo-api".into()
                },
                ConvKind::Assistant {
                    text: "On it.".into()
                },
            ]
        );
        assert_eq!(tail.offset(), FIXTURE.len() as u64);
    }

    #[test]
    fn tool_calls_fold_to_one_line() {
        let got = Tail::new().feed(FIXTURE.as_bytes());
        assert_eq!(
            got[2].kind,
            ConvKind::Tool {
                name: "Bash".into(),
                summary: "cargo test".into()
            }
        );
        let long = "x".repeat(200);
        let v = serde_json::json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","name":"Read","input":{"file_path":"/tmp/demo/a.rs"}},
            {"type":"tool_use","name":"Task","input":{"prompt":"p"}},
            {"type":"tool_use","name":"Bash","input":{"command":long}}]}});
        let got = entry_of(&v);
        assert_eq!(got[0].at, 0);
        assert_eq!(
            got[0].kind,
            ConvKind::Tool {
                name: "Read".into(),
                summary: "/tmp/demo/a.rs".into()
            }
        );
        assert_eq!(
            got[1].kind,
            ConvKind::Tool {
                name: "Task".into(),
                summary: String::new()
            }
        );
        let ConvKind::Tool { summary, .. } = &got[2].kind else {
            panic!("not a tool");
        };
        assert_eq!(summary.chars().count(), 80);
    }

    #[test]
    fn sidechain_meta_thinking_and_reminders_are_left_out() {
        let side = r#"{"type":"user","isSidechain":true,"message":{"content":"hi"}}"#;
        let meta = r#"{"type":"user","isMeta":true,"message":{"content":"hi"}}"#;
        let think =
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hmm"}]}}"#;
        let remind =
            r#"{"type":"user","message":{"content":"<system-reminder>x</system-reminder>"}}"#;
        let cmd = r#"{"type":"user","message":{"content":[{"type":"text","text":"<command-name>/demo</command-name>"},{"type":"text","text":"<local-command-stdout>ok"}]}}"#;
        let other = r#"{"type":"summary","summary":"demo-ws"}"#;
        let mixed = r#"{"type":"user","message":{"content":[{"type":"text","text":"<system-reminder>x"},{"type":"text","text":"real"},{"type":"text","text":"more"}]}}"#;
        let input = [side, meta, think, remind, cmd, other, mixed].join("\n") + "\n";
        let got = Tail::new().feed(input.as_bytes());
        assert_eq!(
            kinds(&got),
            [ConvKind::User {
                text: "real\n\nmore".into()
            }]
        );
    }

    #[test]
    fn tool_results_are_left_out() {
        let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#;
        assert!(entry_of(&serde_json::from_str(line).unwrap()).is_empty());
    }

    #[test]
    fn tool_calls_start_and_finish() {
        let result = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"The user doesn't want to proceed with this tool use."}]}}"#;
        let input = format!("{FIXTURE}{result}\n");
        let fed = Tail::new().read(input.as_bytes());
        assert_eq!(fed.entries.len(), 3);
        assert_eq!(
            fed.tools,
            [
                ToolEvent::Started {
                    id: "t1".into(),
                    name: "Bash".into(),
                    summary: "cargo test".into(),
                    questions: None,
                },
                ToolEvent::Finished { id: "t1".into() },
            ]
        );
    }

    #[test]
    fn a_question_call_carries_its_questions() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"q1","name":"AskUserQuestion","input":{"questions":[{"header":"Deploy","question":"Deploy demo-api now?","multiSelect":false,"options":[{"label":"Yes","description":"ship it"},{"label":"No"}]}]}}]}}"#;
        assert_eq!(
            tools_of(&serde_json::from_str(line).unwrap()),
            [ToolEvent::Started {
                id: "q1".into(),
                name: ASK_USER_QUESTION.into(),
                summary: String::new(),
                questions: Some(vec![Question {
                    header: "Deploy".into(),
                    question: "Deploy demo-api now?".into(),
                    options: vec!["Yes".into(), "No".into()],
                    multi: false,
                }]),
            }]
        );
    }

    #[test]
    fn side_chain_tools_and_text_give_no_tool_events() {
        let side = r#"{"type":"assistant","isSidechain":true,"message":{"content":[{"type":"tool_use","id":"s1","name":"Bash","input":{}}]}}"#;
        let side_result = r#"{"type":"user","isSidechain":true,"message":{"content":[{"type":"tool_result","tool_use_id":"s1"}]}}"#;
        let text = r#"{"type":"user","message":{"content":"run demo-api"}}"#;
        let no_id = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{}}]}}"#;
        for line in [side, side_result, text, no_id] {
            assert!(
                tools_of(&serde_json::from_str(line).unwrap()).is_empty(),
                "{line}"
            );
        }
    }

    #[test]
    fn prompts_begin_turns_and_ends_or_interrupts_close_them() {
        let lines = [
            (
                r#"{"type":"user","message":{"content":"write a long poem"}}"#,
                Some(Turn::Began),
            ),
            (
                r#"{"type":"user","message":{"content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#,
                Some(Turn::Ended),
            ),
            (
                r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1"},{"type":"text","text":"[Request interrupted by user for tool use]"}]}}"#,
                Some(Turn::Ended),
            ),
            (
                r#"{"type":"assistant","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"Done."}]}}"#,
                Some(Turn::Ended),
            ),
            (
                r#"{"type":"system","subtype":"turn_duration","isSidechain":false}"#,
                Some(Turn::Ended),
            ),
            (
                r#"{"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{}}]}}"#,
                None,
            ),
            (
                r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1"}]}}"#,
                None,
            ),
            (
                r#"{"type":"user","isSidechain":true,"message":{"content":"hi"}}"#,
                None,
            ),
            (
                r#"{"type":"user","isMeta":true,"message":{"content":"hi"}}"#,
                None,
            ),
            (
                r#"{"type":"user","message":{"content":"<system-reminder>x"}}"#,
                None,
            ),
            (r#"{"type":"system","subtype":"stop_hook_summary"}"#, None),
        ];
        for (line, want) in lines {
            assert_eq!(
                turn_of(&serde_json::from_str(line).unwrap()),
                want,
                "{line}"
            );
        }
        // The last mark of a read is the one kept.
        let input = format!("{}\n{}\n", lines[0].0, lines[1].0);
        assert_eq!(Tail::new().read(input.as_bytes()).turn, Some(Turn::Ended));
        let input = format!("{}\n{}\n{}\n", lines[1].0, lines[0].0, lines[5].0);
        assert_eq!(Tail::new().read(input.as_bytes()).turn, Some(Turn::Began));
    }

    #[test]
    fn a_partial_last_line_waits_for_its_newline() {
        let line = FIXTURE.lines().next().unwrap();
        let (a, b) = line.split_at(30);
        let mut tail = Tail::new();
        assert!(tail.feed(a.as_bytes()).is_empty());
        assert_eq!(tail.offset(), 30);
        let got = tail.feed(format!("{b}\r\n").as_bytes());
        assert_eq!(got.len(), 1);
        assert_eq!(tail.offset(), line.len() as u64 + 2);
    }

    #[test]
    fn a_bad_line_is_skipped() {
        let input = format!("not json\n{{\"type\":5}}\n[1]\n{}", FIXTURE);
        let got = Tail::new().feed(input.as_bytes());
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn compact_summaries_and_empty_user_lines_are_left_out() {
        let compact = r#"{"type":"user","isCompactSummary":true,"message":{"content":"This session is being continued from a previous conversation."}}"#;
        let empty = r#"{"type":"user","message":{"content":""}}"#;
        let blank = r#"{"type":"user","message":{"content":[{"type":"text","text":"  \n"}]}}"#;
        for line in [compact, empty, blank] {
            assert!(
                entry_of(&serde_json::from_str(line).unwrap()).is_empty(),
                "{line}"
            );
        }
    }

    #[test]
    fn huge_texts_are_cut_with_a_note() {
        // Multi-byte, so the cut must find a character boundary.
        let big = "\u{e9}".repeat(TEXT_MAX);
        let user = serde_json::json!({"type":"user","message":{"content":big}});
        let said = serde_json::json!({"type":"assistant","message":{"content":[
            {"type":"text","text":big}]}});
        for line in [user, said] {
            let got = entry_of(&line);
            let (ConvKind::User { text } | ConvKind::Assistant { text }) = &got[0].kind else {
                panic!("not text");
            };
            assert!(text.len() <= TEXT_MAX + CUT_NOTE.len());
            assert!(text.ends_with("(cut, see the session)"));
        }
        // Control characters grow six times when sent: the cut counts
        // what is sent, not the raw bytes.
        let noisy = "\u{1}".repeat(TEXT_MAX / 2);
        let user = serde_json::json!({"type":"user","message":{"content":noisy}});
        let got = entry_of(&user);
        let ConvKind::User { text } = &got[0].kind else {
            panic!("not text");
        };
        let sent = serde_json::to_string(text).unwrap().len();
        let note = serde_json::to_string(CUT_NOTE).unwrap().len();
        assert!(sent <= TEXT_MAX + note, "{sent}");
        assert!(text.ends_with("(cut, see the session)"));
        // Escaped text that fits is kept whole.
        let quoted = "\"\\\t".repeat(TEXT_MAX / 8);
        let said = serde_json::json!({"type":"assistant","message":{"content":[
            {"type":"text","text":quoted}]}});
        assert_eq!(
            entry_of(&said)[0].kind,
            ConvKind::Assistant { text: quoted }
        );
        let small = serde_json::json!({"type":"user","message":{"content":"fix DEMO-1"}});
        assert_eq!(
            entry_of(&small)[0].kind,
            ConvKind::User {
                text: "fix DEMO-1".into()
            }
        );
    }
}
