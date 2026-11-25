//! Typing into a slot from the board: answering its prompt, replying and
//! stopping it. Every send checks the slot's state, takes its key lock,
//! checks again and only then types; afterwards it checks the session took
//! what was typed.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use overseer_proto::msg::{slot_name, Answer, Prompt, PromptShown};
use overseer_proto::{ActivityKind, Request, Response, SlotId, SlotState};
use overseer_term::TermModes;

use super::keys::{answer_keys, check_reply, moved_on, unknown_prompt};
use super::service::ConversationService;
use crate::manager::{valid_id, KeyLock, SlotManager};
use crate::relay::courier::{
    paste_then_return, strip_controls, typed_paste, Keyboard, Pasted, NO_BRACKETS, RETURN_DELAY,
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
/// How long an answer waits for its dialog to be drawn. The hooks that
/// open a prompt fire just before Claude Code draws it.
const DIALOG_WITHIN: Duration = Duration::from_millis(500);

/// How long the board's typing waits, and between what.
#[derive(Clone, Copy)]
struct Timing {
    step: Duration,
    poll: Duration,
    stop_within: Duration,
    take_within: Duration,
    return_delay: Duration,
    dialog_within: Duration,
}

const TIMING: Timing = Timing {
    step: ANSWER_STEP,
    poll: SCREEN_POLL,
    stop_within: STOP_WITHIN,
    take_within: TAKE_WITHIN,
    return_delay: RETURN_DELAY,
    dialog_within: DIALOG_WITHIN,
};

/// A slot as the board's typing sees it at one moment.
struct SlotNow {
    /// Still the process the keys go to, and running.
    running: bool,
    /// The session it runs, when its conversation is from that session.
    session: Option<String>,
    /// The prompt its conversation is waiting on.
    prompt: Option<Prompt>,
    /// A hook said it is working and its transcript shows the turn in
    /// progress (see `ConversationService::turn_open`).
    in_turn: bool,
}

/// One slot the board types into. Every send checks the slot's state, takes
/// its key lock, checks again and only then types.
pub struct Typist {
    slot: SlotId,
    keys: Arc<dyn Keyboard>,
    lock: KeyLock,
    now: Box<dyn Fn() -> SlotNow + Send + Sync>,
    /// Drops the open prompt of this id, when the screen shows it is gone.
    forget: Box<dyn Fn(&str) + Send + Sync>,
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
    let forgets = conversation.clone();
    let forget = Box::new(move |id: &str| forgets.forget_prompt(slot, id));
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
            in_turn: info
                .activity
                .is_some_and(|a| a.kind == ActivityKind::Working)
                && conversation.turn_open(slot),
        }
    });
    Ok(Typist {
        slot,
        keys,
        lock,
        now,
        forget,
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

    /// Types the keys that answer prompt `id`, `step` apart. The dialog
    /// replaces the input box, so a box on screen means the prompt was
    /// answered (its tool may still run): nothing is typed, and the prompt
    /// is dropped so its buttons go. The hooks fire just before the dialog
    /// is drawn, so the box counts only once it has stayed `dialog_within`.
    async fn answer(&self, id: &str, answer: &Answer) -> Result<(), String> {
        let (session, _) = self.answer_check(id, answer, None)?;
        let _typing = self.lock.lock().await;
        let deadline = tokio::time::Instant::now() + self.timing.dialog_within;
        let keys = loop {
            // Checked on every look: the prompt may end or change while
            // the dialog is awaited.
            let (_, keys) = self.answer_check(id, answer, Some(&session))?;
            let found = self.keys.screen().map(|(snap, _)| input_box(&snap));
            if found == Some(InputBox::NotFound) {
                break keys;
            }
            if tokio::time::Instant::now() >= deadline {
                if found.is_none() {
                    return Err(self.refusal("has a screen that cannot be read yet"));
                }
                (self.forget)(id);
                return Err(moved_on(self.slot));
            }
            tokio::time::sleep(self.timing.poll).await;
        };
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
        if !self.answered(id, true).await {
            return Err(format!("{} did not take the answer", slot_name(self.slot)));
        }
        // No hook says a permission was answered; the screen just did.
        if matches!(answer, Answer::Permission { .. }) {
            (self.forget)(id);
        }
        Ok(())
    }

    /// Whether, within `take_within`, prompt `id` is no longer open or the
    /// input box comes back. `box_gone` says the box was off the screen
    /// before the keys; a box counts only once it has been seen gone.
    async fn answered(&self, id: &str, mut box_gone: bool) -> bool {
        let deadline = tokio::time::Instant::now() + self.timing.take_within;
        loop {
            tokio::time::sleep(self.timing.poll).await;
            let gone = (self.now)().prompt.is_none_or(|p| p.id != id);
            let found = self.keys.screen().map(|(snap, _)| input_box(&snap));
            let back = match found {
                Some(InputBox::NotFound) => {
                    box_gone = true;
                    false
                }
                Some(_) => box_gone,
                None => false,
            };
            if gone || back {
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

    /// Types `text` as the user's next message. A session in a turn (even
    /// one streaming its reply, whose screen shows no working line) or at a
    /// prompt with its dialog up is stopped with Esc first (at a permission
    /// prompt that answers No), and the text goes once the screen shows the
    /// empty box and the turn is over.
    async fn reply(&self, session: &str, prompt: Option<&str>, text: &str) -> Result<(), String> {
        let text = strip_controls(text);
        if text.trim().is_empty() {
            return Err(self.refusal("has nothing to send"));
        }
        self.reply_check(session, prompt)?;
        let _typing = self.lock.lock().await;
        let now = self.reply_check(session, prompt)?;
        let screen = self.keys.screen();
        // An open prompt needs Esc only while its dialog is up: with the
        // box back it was answered, and Esc at rest opens the rewind picker.
        let stop = now.in_turn
            || screen.as_ref().is_some_and(|(s, _)| {
                shows_working(s) || (now.prompt.is_some() && input_box(s) == InputBox::NotFound)
            });
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
        let paste = typed_paste(&text, &modes);
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
    /// line, and for the turn to be over, up to `stop_within`. A streaming
    /// session looks at rest already, so only the transcript's interrupt
    /// says the Esc landed. The screen's modes when it does; else the
    /// refusal, which names text left in the box.
    async fn stopped(&self) -> Result<TermModes, String> {
        let deadline = tokio::time::Instant::now() + self.timing.stop_within;
        loop {
            tokio::time::sleep(self.timing.poll).await;
            let screen = self.keys.screen();
            if let Some((snap, modes)) = &screen {
                if input_box(snap) == InputBox::Empty
                    && !shows_working(snap)
                    && !(self.now)().in_turn
                {
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
    /// Only while the screen shows the working line, a prompt is open with
    /// its dialog in place of the input box, or the turn is in progress (a
    /// session streaming its reply shows no working line): a second Esc at
    /// a stopped session opens Claude Code's rewind picker. No hook follows
    /// an Esc, so the transcript's interrupt is what ends the turn.
    async fn interrupt(&self) -> Result<(), String> {
        if !(self.now)().running {
            return Err(not_running(self.slot));
        }
        let _typing = self.lock.lock().await;
        let now = (self.now)();
        if !now.running {
            return Err(not_running(self.slot));
        }
        let stoppable = self.keys.screen().is_some_and(|(snap, _)| {
            shows_working(&snap)
                || (now.prompt.is_some() && input_box(&snap) == InputBox::NotFound)
                || now.in_turn
        });
        if !stoppable {
            return Err(self.refusal("is not working"));
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
        /// The turn is in progress, until an Esc is typed when
        /// `esc_ends_turn` (as the transcript then records the interrupt).
        in_turn: bool,
        esc_ends_turn: bool,
    }

    fn at_rest() -> State {
        State {
            running: true,
            session: Some("demo-s".into()),
            prompt: None,
            in_turn: false,
            esc_ends_turn: true,
        }
    }

    const FAST: Timing = Timing {
        step: Duration::from_millis(1),
        poll: Duration::from_millis(2),
        stop_within: Duration::from_millis(100),
        take_within: Duration::from_millis(50),
        return_delay: Duration::from_millis(20),
        dialog_within: Duration::from_millis(50),
    };

    fn fake_typist(fake: &Arc<FakeSlot>, state: &Arc<Mutex<State>>) -> Typist {
        let forgets = state.clone();
        let forget = Box::new(move |id: &str| {
            let mut s = forgets.lock().unwrap();
            if s.prompt.as_ref().is_some_and(|p| p.id == id) {
                s.prompt = None;
            }
        });
        let state = state.clone();
        let typed = fake.clone();
        Typist {
            slot: 2,
            keys: fake.clone(),
            lock: KeyLock::default(),
            now: Box::new(move || {
                let s = state.lock().unwrap().clone();
                let stopped = s.esc_ends_turn && typed.typed().iter().any(|k| k == b"\x1b");
                SlotNow {
                    running: s.running,
                    session: s.session,
                    prompt: s.prompt,
                    in_turn: s.in_turn && !stopped,
                }
            }),
            forget,
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
        let fake = FakeSlot::showing(Some(screen(None, true, true)));
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
            in_turn: true,
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
            in_turn: true,
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
        let fake = FakeSlot::showing(Some(screen_of(
            b"Which repo?\r\n  1. demo-api\r\n  2. demo-ws\r\n",
            COLS,
            12,
        )));
        fake.when(b"\r", idle());
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
        assert_eq!(fake.typed(), chunks(&[b"\x1b"]));
    }

    #[tokio::test]
    async fn text_left_in_the_box_after_esc_is_named() {
        let fake = FakeSlot::showing(Some(screen(Some("queued draft"), true, true)));
        fake.when(b"\x1b", Some(screen(Some("queued draft"), false, true)));
        let state = Arc::new(Mutex::new(State {
            in_turn: true,
            ..at_rest()
        }));
        let t = fake_typist(&fake, &state);
        assert_eq!(
            t.handle(reply("demo-s", None, "stop")).await,
            error("S2 has text in its input box, not sent")
        );
        assert_eq!(fake.typed(), chunks(&[b"\x1b"]));
    }

    #[tokio::test]
    async fn interrupt_needs_the_working_line_a_prompt_or_a_turn() {
        // Stopped by an earlier Esc: no hook says so, so the slot still
        // reads as working, but the transcript has the interrupt and the
        // screen shows it at rest.
        let fake = FakeSlot::showing(idle());
        let state = Arc::new(Mutex::new(at_rest()));
        let t = fake_typist(&fake, &state);
        assert_eq!(
            t.handle(Request::Interrupt { slot: 2 }).await,
            error("S2 is not working, not sent")
        );
        // A screen that cannot be read is not typed at either.
        let blind = FakeSlot::showing(None);
        let t = fake_typist(&blind, &state);
        assert_eq!(
            t.handle(Request::Interrupt { slot: 2 }).await,
            error("S2 is not working, not sent")
        );
        assert!(fake.typed().is_empty() && blind.typed().is_empty());
        // At a prompt, Esc answers it No.
        let fake = FakeSlot::showing(dialog());
        let t = fake_typist(&fake, &at_permission("p1"));
        assert_eq!(t.handle(Request::Interrupt { slot: 2 }).await, Response::Ok);
        assert_eq!(fake.typed(), chunks(&[b"\x1b"]));
    }

    /// Claude Code 2.1.328 streaming its reply: the text so far, then the
    /// empty input box with the cursor and the status line, and no working
    /// line.
    fn streaming() -> Screen {
        let rule = "\u{2500}".repeat(COLS as usize);
        let out = format!(
            "\x1b[2J\x1b[H\x1b[?2004h\u{23fa} The Long Way Home\r\n\r\n  The morning comes in quiet, gold\r\n\r\n{rule}\r\n\u{276f} \x1b[7m \x1b[0m\r\n{rule}\r\n  demo-api \u{b7} DEMO-12"
        );
        Some(screen_of(out.as_bytes(), COLS, 12))
    }

    #[test]
    fn the_streaming_screen_shows_no_working_line() {
        let (snap, _) = streaming().unwrap();
        assert!(!shows_working(&snap));
        assert_eq!(input_box(&snap), InputBox::Empty);
    }

    #[tokio::test]
    async fn interrupt_stops_a_slot_streaming_its_reply() {
        let fake = FakeSlot::showing(streaming());
        let state = Arc::new(Mutex::new(State {
            in_turn: true,
            ..at_rest()
        }));
        let t = fake_typist(&fake, &state);
        assert_eq!(t.handle(Request::Interrupt { slot: 2 }).await, Response::Ok);
        assert_eq!(fake.typed(), chunks(&[b"\x1b"]));
        // Once the interrupt is recorded, a second Stop types nothing.
        assert_eq!(
            t.handle(Request::Interrupt { slot: 2 }).await,
            error("S2 is not working, not sent")
        );
        assert_eq!(fake.typed().len(), 1);
        // Nor while the turn is open but the screen cannot be read.
        let blind = FakeSlot::showing(None);
        let t = fake_typist(&blind, &state);
        assert_eq!(
            t.handle(Request::Interrupt { slot: 2 }).await,
            error("S2 is not working, not sent")
        );
        assert!(blind.typed().is_empty());
    }

    #[tokio::test]
    async fn a_reply_to_a_streaming_slot_stops_it_first() {
        let fake = FakeSlot::showing(streaming());
        let state = Arc::new(Mutex::new(State {
            in_turn: true,
            ..at_rest()
        }));
        let t = fake_typist(&fake, &state);
        assert_eq!(
            t.handle(reply("demo-s", None, "shorter")).await,
            Response::Ok
        );
        assert_eq!(
            fake.typed(),
            chunks(&[b"\x1b", b"\x1b[200~shorter\x1b[201~", b"\r"])
        );
    }

    #[tokio::test]
    async fn a_reply_waits_for_the_turn_to_end_after_esc() {
        // The Esc did not land: the turn is still open, though the screen
        // looks at rest. The reply is not pasted into a streaming session.
        let fake = FakeSlot::showing(streaming());
        let state = Arc::new(Mutex::new(State {
            in_turn: true,
            esc_ends_turn: false,
            ..at_rest()
        }));
        let t = fake_typist(&fake, &state);
        assert_eq!(
            t.handle(reply("demo-s", None, "shorter")).await,
            error("S2 did not stop, not sent")
        );
        assert_eq!(fake.typed(), chunks(&[b"\x1b"]));
    }

    #[tokio::test]
    async fn a_reply_after_an_interrupt_types_no_second_esc() {
        // The hooks still say working, but the transcript has the
        // interrupt: the session is at rest, and Esc there could open the
        // rewind picker.
        let fake = FakeSlot::showing(idle());
        let t = fake_typist(&fake, &Arc::new(Mutex::new(at_rest())));
        assert_eq!(t.handle(reply("demo-s", None, "next")).await, Response::Ok);
        assert_eq!(fake.typed(), chunks(&[b"\x1b[200~next\x1b[201~", b"\r"]));
    }

    #[tokio::test]
    async fn a_reply_at_a_dialog_with_no_prompt_is_refused() {
        // A dialog the hooks never named, as Claude Code's folder trust
        // question: no prompt is open and the box is gone.
        let fake = FakeSlot::showing(Some(screen_of(
            b"Accessing workspace:\r\n  /tmp/demo-ws\r\n  1. Yes, I trust this folder\r\n  2. No, exit\r\n",
            COLS,
            12,
        )));
        let t = fake_typist(&fake, &Arc::new(Mutex::new(at_rest())));
        assert_eq!(
            t.handle(reply("demo-s", None, "1")).await,
            error("S2 shows no input box, not sent")
        );
        assert_eq!(
            t.handle(Request::Interrupt { slot: 2 }).await,
            error("S2 is not working, not sent")
        );
        assert!(fake.typed().is_empty());
    }

    #[tokio::test]
    async fn an_answer_at_the_input_box_is_refused_and_the_prompt_dropped() {
        // Approved in the terminal: the tool runs, the prompt is still open.
        for subagent in [false, true] {
            let fake = FakeSlot::showing(Some(screen(None, true, true)));
            let state = at_permission("p1");
            state.lock().unwrap().prompt.as_mut().unwrap().subagent = subagent;
            let t = fake_typist(&fake, &state);
            for choice in [0, 2] {
                state.lock().unwrap().prompt = Some(Prompt {
                    subagent,
                    ..permission("p1")
                });
                assert_eq!(
                    t.handle(answer("p1", Answer::Permission { choice })).await,
                    error("S2 moved on, not sent")
                );
                assert_eq!(state.lock().unwrap().prompt, None);
            }
            assert!(fake.typed().is_empty());
        }
        // A screen that cannot be read is not typed at.
        let blind = FakeSlot::showing(None);
        let state = at_permission("p1");
        let t = fake_typist(&blind, &state);
        assert_eq!(
            t.handle(answer("p1", Answer::Permission { choice: 0 }))
                .await,
            error("S2 has a screen that cannot be read yet, not sent")
        );
        assert!(blind.typed().is_empty());
        assert!(state.lock().unwrap().prompt.is_some());
    }

    #[tokio::test]
    async fn an_answer_waits_a_moment_for_the_dialog() {
        // The hook comes before Claude Code draws its dialog, and the
        // click lands in between.
        let fake = FakeSlot::showing(Some(screen(None, true, true)));
        fake.when(b"1", idle());
        let state = at_permission("p1");
        let t = fake_typist(&fake, &state);
        let sent = t.handle(answer("p1", Answer::Permission { choice: 0 }));
        let drawn = async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            *fake.screen.lock().unwrap() = dialog();
        };
        let (resp, ()) = tokio::join!(sent, drawn);
        assert_eq!(resp, Response::Ok);
        assert_eq!(fake.typed(), chunks(&[b"1"]));
        // Another prompt opens while it waits: nothing typed, and the new
        // prompt stays.
        let fake = FakeSlot::showing(Some(screen(None, true, true)));
        let state = at_permission("p1");
        let t = fake_typist(&fake, &state);
        let sent = t.handle(answer("p1", Answer::Permission { choice: 0 }));
        let replaced = async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            state.lock().unwrap().prompt = Some(permission("p2"));
            *fake.screen.lock().unwrap() = dialog();
        };
        let (resp, ()) = tokio::join!(sent, replaced);
        assert_eq!(resp, error("S2 moved on, not sent"));
        assert!(fake.typed().is_empty());
        assert_eq!(state.lock().unwrap().prompt, Some(permission("p2")));
    }

    #[tokio::test]
    async fn a_stale_prompt_at_an_idle_session_gets_no_esc() {
        // Answered in the terminal with no hook to say so: the prompt is
        // still open but the box is back. Esc there could open the rewind
        // picker.
        let fake = FakeSlot::showing(idle());
        let t = fake_typist(&fake, &at_permission("p1"));
        assert_eq!(
            t.handle(Request::Interrupt { slot: 2 }).await,
            error("S2 is not working, not sent")
        );
        assert_eq!(
            t.handle(reply("demo-s", Some("p1"), "next")).await,
            Response::Ok
        );
        assert_eq!(fake.typed(), chunks(&[b"\x1b[200~next\x1b[201~", b"\r"]));
    }

    #[tokio::test]
    async fn a_box_that_never_went_is_not_an_answer() {
        let fake = FakeSlot::showing(idle());
        let t = fake_typist(&fake, &at_permission("p1"));
        assert!(!t.answered("p1", false).await);
        // Gone, then back: taken.
        let fake = FakeSlot::showing(dialog());
        let t = fake_typist(&fake, &at_permission("p1"));
        let looked = t.answered("p1", false);
        let back = async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            *fake.screen.lock().unwrap() = idle();
        };
        let (taken, ()) = tokio::join!(looked, back);
        assert!(taken);
    }

    #[tokio::test]
    async fn a_taken_permission_answer_drops_the_prompt() {
        let fake = FakeSlot::showing(dialog());
        fake.when(b"1", Some(screen(None, true, true)));
        let state = at_permission("p1");
        let t = fake_typist(&fake, &state);
        assert_eq!(
            t.handle(answer("p1", Answer::Permission { choice: 0 }))
                .await,
            Response::Ok
        );
        assert_eq!(state.lock().unwrap().prompt, None);
        // A second click finds nothing to answer.
        assert_eq!(
            t.handle(answer("p1", Answer::Permission { choice: 2 }))
                .await,
            error("S2 moved on, not sent")
        );
        assert_eq!(fake.typed(), chunks(&[b"1"]));
    }

    #[tokio::test]
    async fn a_reply_without_bracketed_paste_types_no_tab_or_delete() {
        let fake = FakeSlot::showing(Some(screen(None, false, false)));
        let t = fake_typist(&fake, &Arc::new(Mutex::new(at_rest())));
        assert_eq!(
            t.handle(reply("demo-s", None, "a\tb\x7fc")).await,
            Response::Ok
        );
        assert_eq!(fake.typed(), chunks(&[b"a    bc", b"\r"]));
    }
}
