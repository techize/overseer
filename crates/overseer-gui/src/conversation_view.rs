//! What the board knows of each slot's conversation: the entries and open
//! prompt the daemon sent, the reply being typed, and the answers being
//! picked. This is the pure model; the app keeps one `Conv` per slot and
//! sends the requests.

use overseer_proto::msg::{
    Answer, ConvEntry, ConvKind, ConversationView, PromptInfo, PromptShown, QuestionAnswer,
};
use overseer_proto::{ActivityKind, AttentionKind, AttentionView, Request, SlotId, SlotInfo};

use crate::attention_view::oldest_first;
use crate::start_dialog::TextField;

/// How many slots the board has, S0 included.
pub const SLOT_COUNT: usize = 7;

/// One slot's conversation as the app holds it.
#[derive(Debug, Clone)]
pub struct Conv {
    pub view: ConversationView,
    /// The reply being typed.
    pub draft: TextField,
    /// The answers picked so far for the open prompt's questions, one per
    /// question. Empty for a permission prompt or no prompt.
    pub picks: Vec<QuestionAnswer>,
    /// The last thing that went wrong with a send, shown until the next one.
    pub status: Option<String>,
}

impl Conv {
    pub fn new(slot: SlotId) -> Self {
        Conv {
            view: ConversationView {
                slot,
                session: String::new(),
                entries_from: 0,
                entries: Vec::new(),
                prompt: None,
            },
            draft: TextField::default(),
            picks: Vec::new(),
            status: None,
        }
    }

    /// Takes in what the daemon sent. True when it fitted; false when the
    /// full view should be asked for again (see `apply`). The prompt is
    /// always the daemon's latest, and a different prompt starts with fresh
    /// picks and no status.
    pub fn apply(&mut self, ev: ConversationView) -> bool {
        let before = self.view.prompt.as_ref().map(|p| p.id.clone());
        let fitted = apply(&mut self.view, ev);
        let after = self.view.prompt.as_ref().map(|p| &p.id);
        if before.as_ref() != after {
            self.picks = blank_picks(&self.view);
            self.status = None;
        }
        fitted
    }

    /// Forgets what the daemon said, as when the connection is lost. The
    /// draft stays: it is the user's own typing.
    pub fn clear(&mut self) {
        let draft = std::mem::take(&mut self.draft);
        *self = Conv::new(self.view.slot);
        self.draft = draft;
    }

    /// The request that sends the draft to the session this view shows, tied
    /// to the open prompt if there is one. Nothing for a blank draft; a slot
    /// with no session yet says so in `status` instead.
    pub fn reply_request(&mut self) -> Option<Request> {
        let text = self.draft.text.trim();
        if text.is_empty() {
            return None;
        }
        if self.view.session.is_empty() {
            self.status = Some(format!("{} has no session yet", slot_label(self.view.slot)));
            return None;
        }
        self.status = None;
        Some(Request::Reply {
            slot: self.view.slot,
            session: self.view.session.clone(),
            prompt: self.view.prompt.as_ref().map(|p| p.id.clone()),
            text: self.draft.text.clone(),
        })
    }

    /// The request that answers the open prompt, if there is one.
    pub fn answer_request(&self, answer: Answer) -> Option<Request> {
        let prompt = self.view.prompt.as_ref()?;
        Some(Request::Answer {
            slot: self.view.slot,
            prompt: prompt.id.clone(),
            answer,
        })
    }
}

fn slot_label(slot: SlotId) -> String {
    overseer_proto::slot_name(slot)
}

/// One empty pick per question of the open prompt.
fn blank_picks(view: &ConversationView) -> Vec<QuestionAnswer> {
    match view.prompt.as_ref().map(|p| &p.info) {
        Some(PromptShown::Known(PromptInfo::Question { questions })) => questions
            .iter()
            .map(|_| QuestionAnswer::Options { picked: Vec::new() })
            .collect(),
        _ => Vec::new(),
    }
}

/// Puts `ev` into `conv`. From index 0 it replaces everything. Otherwise it
/// is the daemon resending from `entries_from`, so anything we hold from
/// there on is dropped and replaced. A different session, or a start past
/// what we hold, cannot be joined: it replaces with what came and returns
/// false, so the caller asks for the whole view again.
pub fn apply(conv: &mut ConversationView, ev: ConversationView) -> bool {
    let from = ev.entries_from;
    let joins = from == 0 || (conv.session == ev.session && from <= conv.entries.len());
    if joins && from > 0 {
        conv.entries.truncate(from);
        conv.entries.extend(ev.entries);
    } else {
        conv.entries = ev.entries;
    }
    conv.session = ev.session;
    conv.entries_from = 0;
    conv.prompt = ev.prompt;
    joins
}

/// The slot to jump to next from `current`: one that is `NeedsYou` or has an
/// open prompt. Those the attention list shows come first, oldest first as
/// it orders them; any other waiting slot follows by id. Goes to the one
/// after `current` in that order, wrapping, or to the first when `current`
/// is not waiting. `None` when no slot other than `current` is waiting.
pub fn next_waiting(
    attention: &AttentionView,
    slots: &[SlotInfo],
    convs: &[Conv],
    current: SlotId,
) -> Option<SlotId> {
    let waiting = |s: SlotId| {
        let needs = slots.get(s as usize).is_some_and(|i| {
            i.activity
                .as_ref()
                .is_some_and(|a| a.kind == ActivityKind::NeedsYou && !a.idle_wait)
        });
        let prompt = convs
            .get(s as usize)
            .is_some_and(|c| c.view.prompt.is_some());
        needs || prompt
    };
    let mut order: Vec<SlotId> = Vec::new();
    for item in oldest_first(attention) {
        if let (AttentionKind::NeedsYou, Some(s)) = (item.kind, item.slot) {
            if waiting(s) && !order.contains(&s) {
                order.push(s);
            }
        }
    }
    for s in 0..slots.len().max(convs.len()) {
        let s = s as SlotId;
        if waiting(s) && !order.contains(&s) {
            order.push(s);
        }
    }
    match order.iter().position(|&s| s == current) {
        Some(at) => order[at + 1..].iter().chain(&order[..at]).copied().next(),
        None => order.first().copied(),
    }
}

/// The line an entry shows: what was said, or a tool as `Bash: cargo test`.
/// `None` for an entry kind this build does not know.
pub fn entry_text(e: &ConvEntry) -> Option<String> {
    match &e.kind {
        ConvKind::User { text } | ConvKind::Assistant { text } => Some(text.clone()),
        ConvKind::Tool { name, summary } if summary.is_empty() => Some(name.clone()),
        ConvKind::Tool { name, summary } => Some(format!("{name}: {summary}")),
        ConvKind::Other => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use overseer_proto::msg::{Prompt, Question};
    use overseer_proto::{Activity, AttentionItem, SlotState};

    fn entry(text: &str) -> ConvEntry {
        ConvEntry {
            at: 1,
            kind: ConvKind::User { text: text.into() },
        }
    }

    fn view(session: &str, from: usize, texts: &[&str]) -> ConversationView {
        ConversationView {
            slot: 1,
            session: session.into(),
            entries_from: from,
            entries: texts.iter().map(|t| entry(t)).collect(),
            prompt: None,
        }
    }

    fn texts(v: &ConversationView) -> Vec<String> {
        v.entries.iter().filter_map(entry_text).collect()
    }

    fn question_prompt(id: &str, n: usize) -> Prompt {
        Prompt {
            id: id.into(),
            subagent: false,
            info: PromptShown::Known(PromptInfo::Question {
                questions: (0..n)
                    .map(|i| Question {
                        header: format!("Q{i}"),
                        question: "Which?".into(),
                        options: vec!["a".into(), "b".into()],
                        multi: false,
                    })
                    .collect(),
            }),
        }
    }

    #[test]
    fn appends_from_entries_from() {
        let mut v = view("s1", 0, &["a", "b"]);
        assert!(apply(&mut v, view("s1", 2, &["c"])));
        assert_eq!(texts(&v), ["a", "b", "c"]);
        // A resend from earlier replaces the tail.
        assert!(apply(&mut v, view("s1", 1, &["B", "c"])));
        assert_eq!(texts(&v), ["a", "B", "c"]);
    }

    #[test]
    fn a_new_session_replaces() {
        let mut v = view("s1", 0, &["a", "b"]);
        assert!(apply(&mut v, view("s2", 0, &["x"])));
        assert_eq!((v.session.as_str(), texts(&v)), ("s2", vec!["x".into()]));
        // A tail of another session cannot be joined.
        assert!(!apply(&mut v, view("s3", 1, &["y"])));
        assert_eq!((v.session.as_str(), texts(&v)), ("s3", vec!["y".into()]));
    }

    #[test]
    fn a_gap_asks_again() {
        let mut v = view("s1", 0, &["a"]);
        assert!(!apply(&mut v, view("s1", 5, &["f"])));
        assert_eq!(texts(&v), ["f"]);
    }

    #[test]
    fn a_new_prompt_resets_picks_and_status() {
        let mut c = Conv::new(1);
        let mut ev = view("s1", 0, &[]);
        ev.prompt = Some(question_prompt("p1", 2));
        assert!(c.apply(ev.clone()));
        assert_eq!(c.picks.len(), 2);
        c.picks[0] = QuestionAnswer::Options { picked: vec![1] };
        c.status = Some("S1 moved on, not sent".into());
        // The same prompt again keeps what was picked.
        assert!(c.apply(ev.clone()));
        assert_eq!(c.picks[0], QuestionAnswer::Options { picked: vec![1] });
        assert!(c.status.is_some());
        ev.prompt = Some(question_prompt("p2", 1));
        c.apply(ev.clone());
        assert_eq!(c.picks, [QuestionAnswer::Options { picked: vec![] }]);
        assert_eq!(c.status, None);
        ev.prompt = None;
        c.apply(ev);
        assert!(c.picks.is_empty());
    }

    #[test]
    fn replies_carry_the_shown_session_and_prompt() {
        let mut c = Conv::new(2);
        c.draft.set("  go ahead ");
        assert_eq!(c.reply_request(), None);
        assert_eq!(c.status.as_deref(), Some("S2 has no session yet"));
        let mut ev = view("s9", 0, &[]);
        ev.slot = 2;
        ev.prompt = Some(question_prompt("p1", 1));
        c.apply(ev);
        assert_eq!(
            c.reply_request(),
            Some(Request::Reply {
                slot: 2,
                session: "s9".into(),
                prompt: Some("p1".into()),
                text: "  go ahead ".into(),
            })
        );
        assert_eq!(c.status, None);
        c.draft.set("   ");
        assert_eq!(c.reply_request(), None);
    }

    #[test]
    fn answers_need_an_open_prompt() {
        let mut c = Conv::new(1);
        let a = Answer::Permission { choice: 0 };
        assert_eq!(c.answer_request(a.clone()), None);
        let mut ev = view("s1", 0, &[]);
        ev.prompt = Some(question_prompt("p1", 1));
        c.apply(ev);
        assert_eq!(
            c.answer_request(a.clone()),
            Some(Request::Answer {
                slot: 1,
                prompt: "p1".into(),
                answer: a
            })
        );
    }

    #[test]
    fn clearing_keeps_the_draft() {
        let mut c = Conv::new(1);
        c.apply(view("s1", 0, &["a"]));
        c.draft.set("typing");
        c.clear();
        assert!(c.view.entries.is_empty() && c.view.session.is_empty());
        assert_eq!(c.draft.text, "typing");
        assert_eq!(c.view.slot, 1);
    }

    fn slot(id: SlotId, needs: bool) -> SlotInfo {
        SlotInfo {
            id,
            label: String::new(),
            cwd: None,
            command: None,
            state: SlotState::Empty,
            cols: 80,
            rows: 24,
            activity: needs.then_some(Activity {
                kind: ActivityKind::NeedsYou,
                since: 0,
                now_doing: None,
                reason: None,
                idle_wait: false,
            }),
            session_note: None,
            held: false,
        }
    }

    fn needs_item(slot: SlotId, since: i64) -> AttentionItem {
        AttentionItem {
            key: format!("needs_you:{slot}"),
            kind: AttentionKind::NeedsYou,
            slot: Some(slot),
            pr: None,
            url: None,
            since,
            text: "needs you".into(),
        }
    }

    fn convs() -> Vec<Conv> {
        (0..SLOT_COUNT as SlotId).map(Conv::new).collect()
    }

    #[test]
    fn next_waiting_follows_the_attention_order_and_wraps() {
        // S5 has waited longest, then S2; S4 only has a prompt (no item).
        let slots: Vec<_> = (0..7).map(|i| slot(i, i == 2 || i == 5)).collect();
        let attention = AttentionView {
            items: vec![needs_item(2, 200), needs_item(5, 100)],
        };
        let mut cs = convs();
        let mut ev = view("s", 0, &[]);
        ev.slot = 4;
        ev.prompt = Some(question_prompt("p", 1));
        cs[4].apply(ev);
        // Order is S5, S2, S4.
        assert_eq!(next_waiting(&attention, &slots, &cs, 5), Some(2));
        assert_eq!(next_waiting(&attention, &slots, &cs, 2), Some(4));
        assert_eq!(next_waiting(&attention, &slots, &cs, 4), Some(5));
        // From a slot that is not waiting, the first.
        assert_eq!(next_waiting(&attention, &slots, &cs, 1), Some(5));
    }

    #[test]
    fn next_waiting_with_none_is_none() {
        let slots: Vec<_> = (0..7).map(|i| slot(i, false)).collect();
        let cs = convs();
        assert_eq!(
            next_waiting(&AttentionView::default(), &slots, &cs, 1),
            None
        );
        // Only the current slot waiting is not somewhere to go.
        let slots: Vec<_> = (0..7).map(|i| slot(i, i == 3)).collect();
        assert_eq!(
            next_waiting(&AttentionView::default(), &slots, &cs, 3),
            None
        );
    }

    #[test]
    fn drafts_are_kept_per_slot() {
        let mut cs = crate::app::empty_convs();
        assert_eq!(cs.len(), crate::app::empty_slots().len());
        cs[1].draft.set("half a thought");
        // Selecting S2 and back only asks for views; S1's draft is its own.
        let mut ev = view("s2", 0, &["hello"]);
        ev.slot = 2;
        cs[2].apply(ev);
        assert_eq!(cs[2].draft.text, "");
        cs[1].apply(view("s1", 0, &["hi"]));
        assert_eq!(cs[1].draft.text, "half a thought");
        assert_eq!(cs[2].draft.text, "");
    }

    #[test]
    fn tool_entries_read_as_one_line() {
        let tool = |name: &str, summary: &str| ConvEntry {
            at: 0,
            kind: ConvKind::Tool {
                name: name.into(),
                summary: summary.into(),
            },
        };
        assert_eq!(
            entry_text(&tool("Bash", "cargo test")).as_deref(),
            Some("Bash: cargo test")
        );
        assert_eq!(entry_text(&tool("Task", "")).as_deref(), Some("Task"));
        let other = ConvEntry {
            at: 0,
            kind: ConvKind::Other,
        };
        assert_eq!(entry_text(&other), None);
    }
}
