//! Typing into a slot from the board: answering its prompt, replying and
//! stopping it. Every send checks the slot's state, takes its key lock,
//! checks again and only then types; afterwards it checks the session took
//! what was typed.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use overseer_proto::msg::{slot_name, Answer, Prompt, PromptShown};
use overseer_proto::{ActivityKind, Request, Response, SlotId, SlotState};
use overseer_term::{encode_paste, TermModes};

use super::keys::{answer_keys, check_reply, moved_on, unknown_prompt};
use super::service::ConversationService;
use crate::manager::{valid_id, KeyLock, SlotManager};
use crate::relay::courier::{
    paste_then_return, strip_controls, Keyboard, Pasted, NO_BRACKETS, RETURN_DELAY,
};
use crate::relay::screen::{input_box, shows_working, InputBox};

/// Between the key chunks of an answer.
const ANSWER_STEP: Duration = Duration::from_millis(150);
/// Between looks at the screen while waiting on it.
const SCREEN_POLL: Duration = Duration::from_millis(200);
/// How long a session has to stop after Esc before a reply.
const STOP_WITHIN: Duration = Duration::from_secs(5);
/// How long a session has to take a reply or an answer once it is typed.
const TAKE_WITHIN: Duration = Duration::from_secs(2);

/// How long the board's typing waits, and between what.
#[derive(Clone, Copy)]
struct Timing {
    step: Duration,
    poll: Duration,
    stop_within: Duration,
    take_within: Duration,
    return_delay: Duration,
}

const TIMING: Timing = Timing {
    step: ANSWER_STEP,
    poll: SCREEN_POLL,
    stop_within: STOP_WITHIN,
    take_within: TAKE_WITHIN,
    return_delay: RETURN_DELAY,
};

/// A slot as the board's typing sees it at one moment.
struct SlotNow {
    /// Still the process the keys go to, and running.
    running: bool,
    /// The session it runs, when its conversation is from that session.
    session: Option<String>,
    /// The prompt its conversation is waiting on.
    prompt: Option<Prompt>,
    /// A hook said it is working.
    working: bool,
}

/// One slot the board types into. Every send checks the slot's state, takes
/// its key lock, checks again and only then types.
pub struct Typist {
    slot: SlotId,
    keys: Arc<dyn Keyboard>,
    lock: KeyLock,
    now: Box<dyn Fn() -> SlotNow + Send + Sync>,
    timing: Timing,
}

/// The typist for `slot`'s running session, which sees the slot through
/// `manager` and its conversation through `conversation`.
pub fn typist(
    manager: &Arc<Mutex<SlotManager>>,
    conversation: &Arc<ConversationService>,
    slot: SlotId,
) -> Result<Typist, String> {
    if !valid_id(slot) {
        return Err(format!("invalid slot id {slot}"));
    }
    let (keys, lock) = {
        let m = manager.lock().unwrap();
        let keys = m.slot(slot).ok_or_else(|| not_running(slot))?;
        (keys, m.key_lock(slot))
    };
    let (manager, conversation, ours) = (manager.clone(), conversation.clone(), keys.clone());
    let now = Box::new(move || {
        let (info, same, running_session) = {
            let m = manager.lock().unwrap();
            let same = m.slot(slot).is_some_and(|s| Arc::ptr_eq(&s, &ours));
            (
                m.list().swap_remove(slot as usize),
                same,
                m.running_session(slot),
            )
        };
        let running = same && matches!(info.state, SlotState::Running { .. });
        SlotNow {
            running,
            session: running_session
                .filter(|s| running && conversation.session(slot).as_ref() == Some(s)),
            prompt: conversation.open_prompt(slot),
            working: info
                .activity
                .is_some_and(|a| a.kind == ActivityKind::Working),
        }
    });
    Ok(Typist {
        slot,
        keys,
        lock,
        now,
        timing: TIMING,
    })
}

fn not_running(slot: SlotId) -> String {
    format!("{} is not running, not sent", slot_name(slot))
}

impl Typist {
    fn refusal(&self, why: &str) -> String {
        format!("{} {why}, not sent", slot_name(self.slot))
    }

    /// Answers `Answer`, `Reply` or `Interrupt`.
    pub async fn handle(&self, req: Request) -> Response {
        let done = match req {
            Request::Answer { prompt, answer, .. } => self.answer(&prompt, &answer).await,
            Request::Reply {
                session,
                prompt,
                text,
                ..
            } => self.reply(&session, prompt.as_deref(), &text).await,
            Request::Interrupt { .. } => self.interrupt().await,
            other => Err(format!("not a typing request: {other:?}")),
        };
        match done {
            Ok(()) => Response::Ok,
            Err(message) => Response::Error { message },
        }
    }

    /// The keys that answer the open prompt `id`, if the slot still runs
    /// a session (`session`, once known) and waits on that prompt.
    fn answer_check(
        &self,
        id: &str,
        answer: &Answer,
        session: Option<&str>,
    ) -> Result<(String, Vec<Vec<u8>>), String> {
        let now = (self.now)();
        if !now.running {
            return Err(not_running(self.slot));
        }
        let Some(running) = now.session.filter(|s| session.is_none_or(|was| was == s)) else {
            return Err(moved_on(self.slot));
        };
        match now.prompt {
            Some(p) if p.id == id => match p.info {
                PromptShown::Known(info) => Ok((running, answer_keys(&info, answer)?)),
                PromptShown::Unknown => Err(unknown_prompt(self.slot)),
            },
            _ => Err(moved_on(self.slot)),
        }
    }

    /// Types the keys that answer prompt `id`, `step` apart.
    async fn answer(&self, id: &str, answer: &Answer) -> Result<(), String> {
        let (session, _) = self.answer_check(id, answer, None)?;
        let _typing = self.lock.lock().await;
        let (_, keys) = self.answer_check(id, answer, Some(&session))?;
        for (i, chunk) in keys.iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(self.timing.step).await;
            }
            if let Err(e) = self.keys.write(chunk) {
                return Err(format!(
                    "{} answer not typed ({i} of {} keys went): {e}",
                    slot_name(self.slot),
                    keys.len()
                ));
            }
        }
        if self.answered(id).await {
            Ok(())
        } else {
            Err(format!("{} did not take the answer", slot_name(self.slot)))
        }
    }

    /// Whether, within `take_within`, prompt `id` is no longer open or the
    /// screen shows the empty input box.
    async fn answered(&self, id: &str) -> bool {
        let deadline = tokio::time::Instant::now() + self.timing.take_within;
        loop {
            tokio::time::sleep(self.timing.poll).await;
            let gone = (self.now)().prompt.is_none_or(|p| p.id != id);
            let empty = self
                .keys
                .screen()
                .is_some_and(|(snap, _)| input_box(&snap) == InputBox::Empty);
            if gone || empty {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
        }
    }

    /// The slot's state, if a reply made in `session` at `prompt` still
    /// goes to that conversation.
    fn reply_check(&self, session: &str, prompt: Option<&str>) -> Result<SlotNow, String> {
        let now = (self.now)();
        if !now.running {
            return Err(not_running(self.slot));
        }
        check_reply(
            self.slot,
            session,
            prompt,
            now.session.as_deref(),
            now.prompt.as_ref(),
        )?;
        Ok(now)
    }

    /// Types `text` as the user's next message. A session that is working
    /// or at a prompt is stopped with Esc first (at a permission prompt that
    /// answers No), and the text goes once the screen shows the empty box.
    async fn reply(&self, session: &str, prompt: Option<&str>, text: &str) -> Result<(), String> {
        let text = strip_controls(text);
        if text.trim().is_empty() {
            return Err(self.refusal("has nothing to send"));
        }
        self.reply_check(session, prompt)?;
        let _typing = self.lock.lock().await;
        let now = self.reply_check(session, prompt)?;
        let screen = self.keys.screen();
        let stop = now.working
            || now.prompt.is_some()
            || screen.as_ref().is_some_and(|(s, _)| shows_working(s));
        let Some((snap, modes)) = screen else {
            return Err(self.refusal(if stop {
                "did not stop"
            } else {
                "has a screen that cannot be read yet"
            }));
        };
        let brackets_needed = text.contains('\n');
        if brackets_needed && !modes.bracketed_paste {
            return Err(self.refusal(&format!("cannot take this: {NO_BRACKETS}")));
        }
        let modes = if stop {
            self.keys
                .write(b"\x1b")
                .map_err(|e| format!("{} Esc not typed: {e}", slot_name(self.slot)))?;
            let modes = self.stopped().await?;
            // The Esc answered the prompt, so only the session must hold.
            let now = (self.now)();
            if !now.running {
                return Err(not_running(self.slot));
            }
            if now.session.as_deref() != Some(session) {
                return Err(moved_on(self.slot));
            }
            if brackets_needed && !modes.bracketed_paste {
                return Err(self.refusal(&format!("cannot take this: {NO_BRACKETS}")));
            }
            modes
        } else {
            match input_box(&snap) {
                InputBox::Empty => {}
                InputBox::Typed => return Err(self.refusal("has text in its input box")),
                InputBox::NotFound => return Err(self.refusal("shows no input box")),
            }
            modes
        };
        let paste = encode_paste(&text, &modes);
        match paste_then_return(self.keys.clone(), paste, self.timing.return_delay).await {
            Ok(Pasted::Whole) => {}
            Ok(Pasted::NoReturn(e)) | Err(e) => {
                return Err(format!("{} reply not typed: {e}", slot_name(self.slot)));
            }
        }
        if self.taken().await {
            Ok(())
        } else {
            Err(format!("{} did not take the reply", slot_name(self.slot)))
        }
    }

    /// Waits for the screen to show the empty input box with no working
    /// line, up to `stop_within`. The screen's modes when it does; else the
    /// refusal, which names text left in the box.
    async fn stopped(&self) -> Result<TermModes, String> {
        let deadline = tokio::time::Instant::now() + self.timing.stop_within;
        loop {
            tokio::time::sleep(self.timing.poll).await;
            let screen = self.keys.screen();
            if let Some((snap, modes)) = &screen {
                if input_box(snap) == InputBox::Empty && !shows_working(snap) {
                    return Ok(*modes);
                }
            }
            if tokio::time::Instant::now() >= deadline {
                let typed = screen.is_some_and(|(snap, _)| input_box(&snap) == InputBox::Typed);
                return Err(self.refusal(if typed {
                    "has text in its input box"
                } else {
                    "did not stop"
                }));
            }
        }
    }

    /// Whether the input box lets go of the text within `take_within`.
    async fn taken(&self) -> bool {
        let deadline = tokio::time::Instant::now() + self.timing.take_within;
        loop {
            tokio::time::sleep(self.timing.poll).await;
            if self
                .keys
                .screen()
                .is_some_and(|(snap, _)| input_box(&snap) != InputBox::Typed)
            {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
        }
    }

    /// Types Esc, which stops a working session or answers a prompt No.
    async fn interrupt(&self) -> Result<(), String> {
        if !(self.now)().running {
            return Err(not_running(self.slot));
        }
        let _typing = self.lock.lock().await;
        if !(self.now)().running {
            return Err(not_running(self.slot));
        }
        self.keys
            .write(b"\x1b")
            .map_err(|e| format!("{} Esc not typed: {e}", slot_name(self.slot)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::screen::screen_of;
    use overseer_proto::msg::{PromptInfo, QuestionAnswer};
    use overseer_term::Snapshot;

    const COLS: u16 = 60;

    /// Claude Code's screen: the input box holding `typed` (else empty),
    /// under the working line when `working`, with bracketed paste on when
    /// `brackets`.
    fn screen(typed: Option<&str>, working: bool, brackets: bool) -> (Snapshot, TermModes) {
        let rule = "\u{2500}".repeat(COLS as usize);
        let mut out = String::from("\x1b[2J\x1b[H");
        if brackets {
            out.push_str("\x1b[?2004h");
        }
        out.push_str(if working {
            "\u{273b} Churning\u{2026} (4s \u{b7} 323 tokens)\r\n\r\n"
        } else {
            "Working on DEMO-12 in demo-api\r\n\r\n"
        });
        out.push_str(&format!("{rule}\r\n"));
        match typed {
            Some(t) => out.push_str(&format!("\u{276f} {t}\r\n")),
            None => out
                .push_str("\u{276f} \x1b[7mT\x1b[0m\x1b[2mry \"fix the failing test\"\x1b[0m\r\n"),
        }
        out.push_str(&format!("{rule}\r\n  ? for shortcuts"));
        screen_of(out.as_bytes(), COLS, 12)
    }

    fn idle() -> Option<(Snapshot, TermModes)> {
        Some(screen(None, false, true))
    }

    type Screen = Option<(Snapshot, TermModes)>;

    /// A slot's keyboard that records what is typed, shows a set screen,
    /// and switches screens when it is sent a given key.
    #[derive(Default)]
    struct FakeSlot {
        typed: Mutex<Vec<Vec<u8>>>,
        screen: Mutex<Screen>,
        on_key: Mutex<Vec<(Vec<u8>, Screen)>>,
    }

    impl FakeSlot {
        fn showing(screen: Option<(Snapshot, TermModes)>) -> Arc<Self> {
            let fake = FakeSlot::default();
            *fake.screen.lock().unwrap() = screen;
            Arc::new(fake)
        }

        fn when(&self, key: &[u8], screen: Option<(Snapshot, TermModes)>) {
            self.on_key.lock().unwrap().push((key.to_vec(), screen));
        }

        fn typed(&self) -> Vec<Vec<u8>> {
            self.typed.lock().unwrap().clone()
        }
    }

    impl Keyboard for FakeSlot {
        fn write(&self, data: &[u8]) -> std::io::Result<()> {
            self.typed.lock().unwrap().push(data.to_vec());
            if let Some((_, s)) = self.on_key.lock().unwrap().iter().find(|(k, _)| k == data) {
                *self.screen.lock().unwrap() = s.clone();
            }
            Ok(())
        }

        fn screen(&self) -> Option<(Snapshot, TermModes)> {
            self.screen.lock().unwrap().clone()
        }
    }

    /// What the fake slot's state reads as; tests change it as they go.
    #[derive(Clone)]
    struct State {
        running: bool,
        session: Option<String>,
        prompt: Option<Prompt>,
        working: bool,
    }

    fn at_rest() -> State {
        State {
            running: true,
            session: Some("demo-s".into()),
            prompt: None,
            working: false,
        }
    }

    const FAST: Timing = Timing {
        step: Duration::from_millis(1),
        poll: Duration::from_millis(2),
        stop_within: Duration::from_millis(100),
        take_within: Duration::from_millis(50),
        return_delay: Duration::from_millis(20),
    };

    fn fake_typist(fake: &Arc<FakeSlot>, state: &Arc<Mutex<State>>) -> Typist {
        let state = state.clone();
        Typist {
            slot: 2,
            keys: fake.clone(),
            lock: KeyLock::default(),
            now: Box::new(move || {
                let s = state.lock().unwrap().clone();
                SlotNow {
                    running: s.running,
                    session: s.session,
                    prompt: s.prompt,
                    working: s.working,
                }
            }),
            timing: FAST,
        }
    }

    fn reply(session: &str, prompt: Option<&str>, text: &str) -> Request {
        Request::Reply {
            slot: 2,
            session: session.into(),
            prompt: prompt.map(String::from),
            text: text.into(),
        }
    }

    fn error(message: &str) -> Response {
        Response::Error {
            message: message.into(),
        }
    }

    fn chunks(c: &[&[u8]]) -> Vec<Vec<u8>> {
        c.iter().map(|c| c.to_vec()).collect()
    }

    #[tokio::test]
    async fn multi_line_reply_needs_bracketed_paste() {
        let fake = FakeSlot::showing(Some(screen(None, false, false)));
        let t = fake_typist(&fake, &Arc::new(Mutex::new(at_rest())));
        let resp = t.handle(reply("demo-s", None, "one\ntwo")).await;
        assert_eq!(
            resp,
            error("S2 cannot take this: bracketed paste is off, not sent")
        );
        assert!(fake.typed().is_empty());
        // One line goes without it.
        assert_eq!(t.handle(reply("demo-s", None, "one")).await, Response::Ok);
        assert_eq!(fake.typed(), chunks(&[b"one", b"\r"]));
    }

    #[tokio::test]
    async fn idle_reply_is_pasted_then_returned() {
        let fake = FakeSlot::showing(idle());
        let t = fake_typist(&fake, &Arc::new(Mutex::new(at_rest())));
        assert_eq!(
            t.handle(reply("demo-s", None, "go\non")).await,
            Response::Ok
        );
        assert_eq!(fake.typed(), chunks(&[b"\x1b[200~go\ron\x1b[201~", b"\r"]));
    }

    #[tokio::test]
    async fn interrupt_types_escape() {
        let fake = FakeSlot::showing(idle());
        let state = Arc::new(Mutex::new(at_rest()));
        let t = fake_typist(&fake, &state);
        assert_eq!(t.handle(Request::Interrupt { slot: 2 }).await, Response::Ok);
        assert_eq!(fake.typed(), chunks(&[b"\x1b"]));
        state.lock().unwrap().running = false;
        assert_eq!(
            t.handle(Request::Interrupt { slot: 2 }).await,
            error("S2 is not running, not sent")
        );
        assert_eq!(fake.typed().len(), 1);
    }

    #[tokio::test]
    async fn courier_and_reply_do_not_interleave() {
        let fake = FakeSlot::showing(idle());
        let t = Arc::new(fake_typist(&fake, &Arc::new(Mutex::new(at_rest()))));
        let lock = t.lock.clone();
        let courier = {
            let keys: Arc<dyn Keyboard> = fake.clone();
            async move {
                let _typing = lock.lock().await;
                paste_then_return(keys, b"relay".to_vec(), FAST.return_delay)
                    .await
                    .map(|_| ())
            }
        };
        let replied = t.handle(reply("demo-s", None, "reply"));
        let (relayed, replied) = tokio::join!(courier, replied);
        relayed.unwrap();
        assert_eq!(replied, Response::Ok);
        let typed = fake.typed();
        let relay_first = chunks(&[b"relay", b"\r", b"\x1b[200~reply\x1b[201~", b"\r"]);
        let reply_first = chunks(&[b"\x1b[200~reply\x1b[201~", b"\r", b"relay", b"\r"]);
        assert!(typed == relay_first || typed == reply_first, "{typed:?}");
    }

    #[tokio::test]
    async fn a_working_slot_is_stopped_before_the_reply() {
        let fake = FakeSlot::showing(Some(screen(None, true, true)));
        fake.when(b"\x1b", idle());
        let state = Arc::new(Mutex::new(State {
            working: true,
            ..at_rest()
        }));
        let t = fake_typist(&fake, &state);
        assert_eq!(t.handle(reply("demo-s", None, "stop")).await, Response::Ok);
        assert_eq!(
            fake.typed(),
            chunks(&[b"\x1b", b"\x1b[200~stop\x1b[201~", b"\r"])
        );
    }

    #[tokio::test]
    async fn a_slot_that_does_not_stop_gets_no_reply() {
        let fake = FakeSlot::showing(Some(screen(None, true, true)));
        let state = Arc::new(Mutex::new(State {
            working: true,
            ..at_rest()
        }));
        let t = fake_typist(&fake, &state);
        assert_eq!(
            t.handle(reply("demo-s", None, "stop")).await,
            error("S2 did not stop, not sent")
        );
        assert_eq!(fake.typed(), chunks(&[b"\x1b"]));
        // A screen that cannot be read is never typed at blind.
        let blind = FakeSlot::showing(None);
        let t = fake_typist(&blind, &state);
        assert_eq!(
            t.handle(reply("demo-s", None, "stop")).await,
            error("S2 did not stop, not sent")
        );
        assert!(blind.typed().is_empty());
    }

    fn permission(id: &str) -> Prompt {
        Prompt {
            id: id.into(),
            subagent: false,
            info: PromptShown::Known(PromptInfo::Permission {
                tool: "Bash".into(),
                summary: "kubectl apply -f demo-api.yaml".into(),
                plan: None,
            }),
        }
    }

    #[tokio::test]
    async fn a_reply_at_the_shown_prompt_answers_no_then_sends() {
        // The dialog replaces the box; Esc brings it back.
        let fake = FakeSlot::showing(Some(screen_of(b"Do you want to proceed?\r\n", COLS, 12)));
        fake.when(b"\x1b", idle());
        let state = Arc::new(Mutex::new(State {
            prompt: Some(permission("p1")),
            ..at_rest()
        }));
        let t = fake_typist(&fake, &state);
        assert_eq!(
            t.handle(reply("demo-s", Some("p1"), "use demo-ws")).await,
            Response::Ok
        );
        assert_eq!(
            fake.typed(),
            chunks(&[b"\x1b", b"\x1b[200~use demo-ws\x1b[201~", b"\r"])
        );
    }

    #[tokio::test]
    async fn a_reply_is_refused_when_the_slot_moved_on() {
        let fake = FakeSlot::showing(idle());
        let state = Arc::new(Mutex::new(State {
            prompt: Some(permission("p2")),
            ..at_rest()
        }));
        let t = fake_typist(&fake, &state);
        assert_eq!(
            t.handle(reply("demo-s", None, "hi")).await,
            error("S2 moved on, not sent")
        );
        assert_eq!(
            t.handle(reply("demo-t", Some("p2"), "hi")).await,
            error("S2 moved on, not sent")
        );
        assert!(fake.typed().is_empty());
    }

    #[tokio::test]
    async fn a_reply_waits_for_the_lock_then_checks_again() {
        let fake = FakeSlot::showing(idle());
        let state = Arc::new(Mutex::new(at_rest()));
        let t = fake_typist(&fake, &state);
        let held = t.lock.clone().lock_owned().await;
        let sent = t.handle(reply("demo-s", None, "hi"));
        let change = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            // A prompt opens while the reply waits for the lock.
            state.lock().unwrap().prompt = Some(permission("p3"));
            drop(held);
        };
        let (resp, ()) = tokio::join!(sent, change);
        assert_eq!(resp, error("S2 moved on, not sent"));
        assert!(fake.typed().is_empty());
    }

    #[tokio::test]
    async fn a_reply_never_joins_text_in_the_box() {
        let fake = FakeSlot::showing(Some(screen(Some("half a thought"), false, true)));
        let t = fake_typist(&fake, &Arc::new(Mutex::new(at_rest())));
        assert_eq!(
            t.handle(reply("demo-s", None, "hi")).await,
            error("S2 has text in its input box, not sent")
        );
        assert!(fake.typed().is_empty());
    }

    #[tokio::test]
    async fn a_reply_left_in_the_box_is_reported() {
        let fake = FakeSlot::showing(idle());
        fake.when(b"\r", Some(screen(Some("hi"), false, true)));
        let t = fake_typist(&fake, &Arc::new(Mutex::new(at_rest())));
        assert_eq!(
            t.handle(reply("demo-s", None, "hi")).await,
            error("S2 did not take the reply")
        );
    }

    fn answer(prompt: &str, answer: Answer) -> Request {
        Request::Answer {
            slot: 2,
            prompt: prompt.into(),
            answer,
        }
    }

    #[tokio::test]
    async fn an_answer_types_its_keys_at_the_shown_prompt() {
        let fake = FakeSlot::showing(idle());
        let questions = Prompt {
            id: "q1".into(),
            subagent: false,
            info: PromptShown::Known(PromptInfo::Question {
                questions: vec![overseer_proto::msg::Question {
                    header: "Repo".into(),
                    question: "Which repo?".into(),
                    options: vec!["demo-api".into(), "demo-ws".into()],
                    multi: false,
                }],
            }),
        };
        let state = Arc::new(Mutex::new(State {
            prompt: Some(questions),
            ..at_rest()
        }));
        let t = fake_typist(&fake, &state);
        let picked = Answer::Questions {
            answers: vec![QuestionAnswer::Options { picked: vec![1] }],
        };
        assert_eq!(t.handle(answer("q1", picked.clone())).await, Response::Ok);
        assert_eq!(fake.typed(), chunks(&[b"2", b"\r"]));
        // Another prompt, none, or one Overseer cannot show: nothing typed.
        assert_eq!(
            t.handle(answer("q0", picked.clone())).await,
            error("S2 moved on, not sent")
        );
        state.lock().unwrap().prompt = Some(Prompt {
            id: "q1".into(),
            subagent: false,
            info: PromptShown::Unknown,
        });
        assert_eq!(
            t.handle(answer("q1", picked.clone())).await,
            error("S2 is at a prompt Overseer can't show, not sent")
        );
        state.lock().unwrap().prompt = None;
        assert_eq!(
            t.handle(answer("q1", picked)).await,
            error("S2 moved on, not sent")
        );
        state.lock().unwrap().prompt = Some(permission("p1"));
        assert_eq!(
            t.handle(answer("p1", Answer::Other)).await,
            error("answer kind not known")
        );
        assert_eq!(fake.typed().len(), 2);
    }

    fn dialog() -> Screen {
        Some(screen_of(
            b"Do you want to proceed?\r\n  1. Yes\r\n",
            COLS,
            12,
        ))
    }

    fn at_permission(id: &str) -> Arc<Mutex<State>> {
        Arc::new(Mutex::new(State {
            prompt: Some(permission(id)),
            ..at_rest()
        }))
    }

    #[tokio::test]
    async fn an_answer_the_session_does_not_take_is_reported() {
        let fake = FakeSlot::showing(dialog());
        let t = fake_typist(&fake, &at_permission("p1"));
        assert_eq!(
            t.handle(answer("p1", Answer::Permission { choice: 0 }))
                .await,
            error("S2 did not take the answer")
        );
        assert_eq!(fake.typed(), chunks(&[b"1"]));
    }

    #[tokio::test]
    async fn an_answer_is_taken_once_the_box_is_back_or_the_prompt_is_gone() {
        // The dialog closes and the empty box shows.
        let fake = FakeSlot::showing(dialog());
        fake.when(b"1", idle());
        let t = fake_typist(&fake, &at_permission("p1"));
        assert_eq!(
            t.handle(answer("p1", Answer::Permission { choice: 0 }))
                .await,
            Response::Ok
        );
        // The screen still shows the dialog, but the prompt has ended.
        let fake = FakeSlot::showing(dialog());
        let state = at_permission("p1");
        let t = fake_typist(&fake, &state);
        let sent = t.handle(answer("p1", Answer::Permission { choice: 2 }));
        let ended = async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            state.lock().unwrap().prompt = None;
        };
        let (resp, ()) = tokio::join!(sent, ended);
        assert_eq!(resp, Response::Ok);
        assert_eq!(fake.typed(), chunks(&[b"3"]));
    }

    #[tokio::test]
    async fn text_left_in_the_box_after_esc_is_named() {
        let fake = FakeSlot::showing(Some(screen(Some("queued draft"), true, true)));
        fake.when(b"\x1b", Some(screen(Some("queued draft"), false, true)));
        let state = Arc::new(Mutex::new(State {
            working: true,
            ..at_rest()
        }));
        let t = fake_typist(&fake, &state);
        assert_eq!(
            t.handle(reply("demo-s", None, "stop")).await,
            error("S2 has text in its input box, not sent")
        );
        assert_eq!(fake.typed(), chunks(&[b"\x1b"]));
    }
}
