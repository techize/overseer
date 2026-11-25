//! What the board types into a slot: the keys that answer a prompt, and
//! whether a reply still goes to the conversation the pane showed.
//!
//! Pure: the daemon's handlers (`server.rs`) take the slot's key lock, check
//! the state again and type what these return.

use overseer_proto::msg::{slot_name, Answer, Prompt, PromptInfo, PromptShown, QuestionAnswer};
use overseer_proto::SlotId;

/// Moves to the next question's tab.
const RIGHT: &[u8] = b"\x1b[C";
/// Submits a free-text answer, and the review screen after the last question.
const RETURN: &[u8] = b"\r";
/// The permission choices the board offers: Yes, Yes and don't ask again, No.
const PERMISSION_CHOICES: usize = 3;
/// No, in every permission dialog whatever its option count.
const ESC: &[u8] = b"\x1b";
/// Options are picked by one digit, so the Other row must be 9 or below.
const MAX_DIGIT: usize = 9;

/// The refusal when the slot's session or prompt is not the one shown.
pub fn moved_on(slot: SlotId) -> String {
    format!("{} moved on, not sent", slot_name(slot))
}

/// The refusal at a prompt the board cannot show.
pub fn unknown_prompt(slot: SlotId) -> String {
    format!(
        "{} is at a prompt Overseer can't show, not sent",
        slot_name(slot)
    )
}

/// The key that picks the row numbered `row` (from 1).
fn digit(row: usize) -> Result<Vec<u8>, String> {
    if (1..=MAX_DIGIT).contains(&row) {
        Ok(row.to_string().into_bytes())
    } else {
        Err(format!("option {row} cannot be picked with one key"))
    }
}

/// The key chunks that answer `prompt` with `answer`, to be typed in order.
/// Yes and Yes-and-don't-ask-again are their digits; No is Esc, which
/// is No in every permission dialog, so a dialog with fewer options cannot
/// turn it into something else. A plan approval takes no choice: its
/// options differ by mode, so it is answered in the session (or by a
/// reply, which types Esc first). Each question takes its digit (a
/// multi-select question each picked digit, then Right to the next tab),
/// or for free text the Other row's digit, the text and Return; a Return
/// after the last question submits the review screen.
///
/// These follow Claude Code 2.1.x and are kept in this one function.
pub fn answer_keys(prompt: &PromptInfo, answer: &Answer) -> Result<Vec<Vec<u8>>, String> {
    match (prompt, answer) {
        (PromptInfo::Permission { plan: Some(_), .. }, Answer::Permission { .. }) => {
            Err("a plan is approved in the session, not by a choice".into())
        }
        (PromptInfo::Permission { .. }, Answer::Permission { choice }) => {
            if *choice >= PERMISSION_CHOICES {
                return Err(format!("permission choice {choice} is out of range"));
            }
            if *choice == PERMISSION_CHOICES - 1 {
                return Ok(vec![ESC.to_vec()]);
            }
            Ok(vec![digit(choice + 1)?])
        }
        (PromptInfo::Question { questions }, Answer::Questions { answers }) => {
            if answers.len() != questions.len() {
                return Err(format!(
                    "{} answers for {} questions",
                    answers.len(),
                    questions.len()
                ));
            }
            let mut keys = Vec::new();
            for (n, (q, a)) in questions.iter().zip(answers).enumerate() {
                let which = n + 1;
                match a {
                    QuestionAnswer::Options { picked } => {
                        if picked.is_empty() {
                            return Err(format!("question {which}: nothing picked"));
                        }
                        if !q.multi && picked.len() > 1 {
                            return Err(format!("question {which}: only one option may be picked"));
                        }
                        for (i, &p) in picked.iter().enumerate() {
                            if p >= q.options.len() {
                                return Err(format!("question {which}: no option {p}"));
                            }
                            if picked[..i].contains(&p) {
                                return Err(format!("question {which}: option {p} picked twice"));
                            }
                            keys.push(digit(p + 1)?);
                        }
                        if q.multi {
                            keys.push(RIGHT.to_vec());
                        }
                    }
                    QuestionAnswer::Other { text } => {
                        if text.trim().is_empty() {
                            return Err(format!("question {which}: the answer is empty"));
                        }
                        if text.chars().any(char::is_control) {
                            return Err(format!("question {which}: the answer must be one line"));
                        }
                        keys.push(digit(q.options.len() + 1)?);
                        keys.push(text.as_bytes().to_vec());
                        keys.push(RETURN.to_vec());
                    }
                    QuestionAnswer::Unrecognised => {
                        return Err(format!("question {which}: answer kind not known"));
                    }
                }
            }
            keys.push(RETURN.to_vec());
            Ok(keys)
        }
        (_, Answer::Other) => Err("answer kind not known".into()),
        (PromptInfo::Other, _) => Err("prompt kind not known".into()),
        _ => Err("the answer does not fit the prompt".into()),
    }
}

/// Whether a reply the pane made in `shown_session`, at `shown_prompt` (the
/// open prompt's id, or none), still goes to that conversation: the slot
/// runs that session now and its open prompt is the same one (or still
/// none). A prompt Overseer cannot show is refused even when it is the one
/// shown.
pub fn check_reply(
    slot: SlotId,
    shown_session: &str,
    shown_prompt: Option<&str>,
    now_session: Option<&str>,
    now_prompt: Option<&Prompt>,
) -> Result<(), String> {
    if now_session != Some(shown_session) {
        return Err(moved_on(slot));
    }
    match (shown_prompt, now_prompt) {
        (None, None) => Ok(()),
        (Some(shown), Some(now)) if shown == now.id => match now.info {
            PromptShown::Known(_) => Ok(()),
            PromptShown::Unknown => Err(unknown_prompt(slot)),
        },
        _ => Err(moved_on(slot)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use overseer_proto::msg::Question;

    fn permission() -> PromptInfo {
        PromptInfo::Permission {
            tool: "Bash".into(),
            summary: "kubectl apply -f demo-api.yaml".into(),
            plan: None,
        }
    }

    fn question(options: &[&str], multi: bool) -> Question {
        Question {
            header: "Pick".into(),
            question: "Which one?".into(),
            options: options.iter().map(|s| s.to_string()).collect(),
            multi,
        }
    }

    fn questions(qs: Vec<Question>) -> PromptInfo {
        PromptInfo::Question { questions: qs }
    }

    fn picked(p: &[usize]) -> QuestionAnswer {
        QuestionAnswer::Options { picked: p.to_vec() }
    }

    fn keys(chunks: &[&[u8]]) -> Vec<Vec<u8>> {
        chunks.iter().map(|c| c.to_vec()).collect()
    }

    #[test]
    fn permission_yes_is_its_digit_and_no_is_esc() {
        for (choice, key) in [(0, &b"1"[..]), (1, b"2"), (2, b"\x1b")] {
            assert_eq!(
                answer_keys(&permission(), &Answer::Permission { choice }),
                Ok(keys(&[key]))
            );
        }
        assert!(answer_keys(&permission(), &Answer::Permission { choice: 3 }).is_err());
    }

    #[test]
    fn a_plan_takes_no_choice() {
        let plan = PromptInfo::Permission {
            tool: "ExitPlanMode".into(),
            summary: String::new(),
            plan: Some("1. Fix demo-api".into()),
        };
        for choice in 0..3 {
            assert!(answer_keys(&plan, &Answer::Permission { choice }).is_err());
        }
    }

    #[test]
    fn question_answers_walk_the_tabs_and_submit() {
        let p = questions(vec![
            question(&["demo-api", "demo-ws"], false),
            question(&["DEMO-1", "DEMO-2"], false),
        ]);
        let a = Answer::Questions {
            answers: vec![picked(&[0]), picked(&[1])],
        };
        assert_eq!(answer_keys(&p, &a), Ok(keys(&[b"1", b"2", b"\r"])));
        // Single-select takes one option, in range.
        let two = Answer::Questions {
            answers: vec![picked(&[0, 1]), picked(&[1])],
        };
        assert!(answer_keys(&p, &two).is_err());
        let out = Answer::Questions {
            answers: vec![picked(&[2]), picked(&[1])],
        };
        assert!(answer_keys(&p, &out).is_err());
    }

    #[test]
    fn multi_select_toggles_then_moves_on() {
        let p = questions(vec![
            question(&["lint", "test", "build"], true),
            question(&["yes", "no"], false),
        ]);
        let a = Answer::Questions {
            answers: vec![picked(&[0, 2]), picked(&[0])],
        };
        assert_eq!(
            answer_keys(&p, &a),
            Ok(keys(&[b"1", b"3", b"\x1b[C", b"1", b"\r"]))
        );
        let twice = Answer::Questions {
            answers: vec![picked(&[0, 0]), picked(&[0])],
        };
        assert!(answer_keys(&p, &twice).is_err());
    }

    #[test]
    fn other_types_the_text() {
        let p = questions(vec![question(&["demo-api", "demo-ws"], false)]);
        let a = Answer::Questions {
            answers: vec![QuestionAnswer::Other {
                text: "both, demo-api first".into(),
            }],
        };
        assert_eq!(
            answer_keys(&p, &a),
            Ok(keys(&[b"3", b"both, demo-api first", b"\r", b"\r"]))
        );
        // Text never carries a key of its own.
        for text in ["", "  ", "two\nlines", "esc\x1b"] {
            let a = Answer::Questions {
                answers: vec![QuestionAnswer::Other { text: text.into() }],
            };
            assert!(answer_keys(&p, &a).is_err(), "{text:?}");
        }
    }

    #[test]
    fn answer_count_must_match() {
        let p = questions(vec![
            question(&["a", "b"], false),
            question(&["c", "d"], false),
        ]);
        let one = Answer::Questions {
            answers: vec![picked(&[0])],
        };
        assert!(answer_keys(&p, &one).is_err());
        let three = Answer::Questions {
            answers: vec![picked(&[0]), picked(&[0]), picked(&[0])],
        };
        assert!(answer_keys(&p, &three).is_err());
    }

    #[test]
    fn unknown_kinds_and_mismatches_are_never_typed() {
        let p = questions(vec![question(&["a", "b"], false)]);
        assert!(answer_keys(&p, &Answer::Other).is_err());
        assert!(answer_keys(&p, &Answer::Permission { choice: 0 }).is_err());
        let unknown = Answer::Questions {
            answers: vec![QuestionAnswer::Unrecognised],
        };
        assert!(answer_keys(&p, &unknown).is_err());
        assert!(answer_keys(&permission(), &Answer::Questions { answers: vec![] }).is_err());
        assert!(answer_keys(&PromptInfo::Other, &Answer::Permission { choice: 0 }).is_err());
    }

    fn prompt(id: &str, info: PromptShown) -> Prompt {
        Prompt {
            id: id.into(),
            subagent: false,
            info,
        }
    }

    fn known(id: &str) -> Prompt {
        prompt(id, PromptShown::Known(permission()))
    }

    #[test]
    fn reply_is_refused_when_a_prompt_opened_since() {
        let now = known("p1");
        assert_eq!(
            check_reply(2, "demo-s", None, Some("demo-s"), Some(&now)),
            Err("S2 moved on, not sent".into())
        );
        // Or a different prompt from the one shown, or none any more.
        assert_eq!(
            check_reply(2, "demo-s", Some("p0"), Some("demo-s"), Some(&now)),
            Err("S2 moved on, not sent".into())
        );
        assert_eq!(
            check_reply(2, "demo-s", Some("p0"), Some("demo-s"), None),
            Err("S2 moved on, not sent".into())
        );
    }

    #[test]
    fn reply_is_refused_in_another_session() {
        assert_eq!(
            check_reply(3, "demo-s", None, Some("demo-t"), None),
            Err("S3 moved on, not sent".into())
        );
        assert_eq!(
            check_reply(3, "demo-s", None, None, None),
            Err("S3 moved on, not sent".into())
        );
    }

    #[test]
    fn reply_at_an_unknown_prompt_is_refused() {
        let now = prompt("p1", PromptShown::Unknown);
        assert_eq!(
            check_reply(2, "demo-s", Some("p1"), Some("demo-s"), Some(&now)),
            Err("S2 is at a prompt Overseer can't show, not sent".into())
        );
    }

    #[test]
    fn reply_matching_the_shown_prompt_is_allowed() {
        let now = known("p1");
        assert_eq!(
            check_reply(2, "demo-s", Some("p1"), Some("demo-s"), Some(&now)),
            Ok(())
        );
        assert_eq!(check_reply(2, "demo-s", None, Some("demo-s"), None), Ok(()));
    }
}
