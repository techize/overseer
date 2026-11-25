//! Follows each slot's conversation for the board: tails the session's
//! transcript, tracks the prompt it is waiting on, and announces both when
//! they change.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read as _, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use overseer_proto::msg::{
    ConvEntry, ConversationView, HookEvent, HookKind, Prompt, PromptInfo, PromptShown,
};
use overseer_proto::{Event, SlotId};
use overseer_term::Snapshot;
use tokio::sync::{broadcast, mpsc};
use tokio::time::MissedTickBehavior;

use super::parse::{Fed, Tail};
use super::prompt::PromptState;
use crate::ledger::Ledger;
use crate::manager::{SlotManager, SLOT_COUNT};
use crate::relay::courier::Keyboard;
use crate::relay::screen::{input_box, shows_working, InputBox};

/// Entries kept per slot; older ones are dropped.
pub const KEEP: usize = 200;

/// How far past [`KEEP`] a slot may grow before it is trimmed back, so a
/// long session costs a full send once per this many entries, not on every
/// line.
const SLACK: usize = KEEP / 4;

/// How often a running slot's transcript is read with no hook to prompt it.
const POLL: Duration = Duration::from_secs(1);

/// The session a slot is running, if it is running one.
type Running = Box<dyn Fn(SlotId) -> Option<String> + Send + Sync>;

/// A slot's screen, when it can be read.
type Screen = Box<dyn Fn(SlotId) -> Option<Snapshot> + Send + Sync>;

/// Polls in a row whose screen shows an open permission answered before
/// it is dropped.
const ANSWERED_LOOKS: u8 = 2;

/// Holds each slot's conversation and announces it when it changes.
pub struct ConversationService {
    ledger: Arc<Ledger>,
    running: Running,
    screen: Screen,
    events: broadcast::Sender<Event>,
    views: Mutex<HashMap<SlotId, SlotView>>,
    /// One per slot, held across a read so a hook and the poll never read
    /// the same file at once.
    readers: Mutex<HashMap<SlotId, Arc<tokio::sync::Mutex<Reader>>>>,
    /// Slots to read now, from a hook.
    wake: mpsc::UnboundedSender<SlotId>,
}

/// What the service knows of one slot's conversation.
#[derive(Default)]
struct SlotView {
    session: String,
    path: Option<PathBuf>,
    /// Bumped on every reset, so a read of the file before is not applied.
    generation: u64,
    entries: Vec<ConvEntry>,
    prompt: PromptState,
    sent: Sent,
    /// The session and file to store, for the next read to write off the
    /// async runtime.
    store: Option<(String, PathBuf)>,
    /// The open permission whose screen looked answered, and on how many
    /// polls in a row.
    answered_looks: Option<(String, u8)>,
}

/// What the app was last sent of a slot.
#[derive(Default)]
struct Sent {
    /// Entries it holds.
    len: usize,
    prompt: Option<Prompt>,
    /// Its entries no longer match ours (a reset or a trim): send them all.
    full: bool,
}

/// Where a slot's transcript has been read to.
#[derive(Default)]
struct Reader {
    generation: u64,
    tail: Tail,
}

/// What one read of a transcript found.
enum Read {
    /// No file yet, or it could not be read: try again later.
    Missing,
    /// What the lines after the last read held.
    More(Fed),
    /// The file was shorter than what was read before, so it was read
    /// again from the start.
    Restarted(Fed),
}

impl ConversationService {
    /// Starts following the slots' conversations: the stored transcripts of
    /// the running slots are read from the start, then each running slot is
    /// read again every second and whenever a hook says it moved on.
    pub fn spawn(
        ledger: Arc<Ledger>,
        manager: Arc<Mutex<SlotManager>>,
        events: broadcast::Sender<Event>,
    ) -> Arc<Self> {
        let looks = manager.clone();
        let running = Box::new(move |slot| manager.lock().unwrap().running_session(slot));
        let screen = Box::new(move |slot| {
            let slot = looks.lock().unwrap().slot(slot)?;
            Keyboard::screen(&*slot).map(|(snap, _)| snap)
        });
        let (svc, wake) = Self::new(ledger, running, screen, events);
        let loaded = svc.load();
        tokio::spawn(svc.clone().run(wake, loaded, POLL));
        svc
    }

    fn new(
        ledger: Arc<Ledger>,
        running: Running,
        screen: Screen,
        events: broadcast::Sender<Event>,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<SlotId>) {
        let (wake, rx) = mpsc::unbounded_channel();
        let svc = Arc::new(ConversationService {
            ledger,
            running,
            screen,
            events,
            views: Mutex::new(HashMap::new()),
            readers: Mutex::new(HashMap::new()),
            wake,
        });
        (svc, rx)
    }

    /// Takes the stored transcript of each slot still running the session
    /// it was stored for. Returns the slots taken, to be read.
    fn load(&self) -> Vec<SlotId> {
        let mut loaded = Vec::new();
        for slot in 0..SLOT_COUNT as SlotId {
            let stored = match self.ledger.transcript(slot) {
                Ok(Some(stored)) => stored,
                Ok(None) => continue,
                Err(e) => {
                    tracing::warn!("conversation: reading S{slot}'s transcript failed: {e:#}");
                    continue;
                }
            };
            let (session, path) = stored;
            if (self.running)(slot).as_deref() != Some(session.as_str()) {
                continue;
            }
            self.views.lock().unwrap().insert(
                slot,
                SlotView {
                    session,
                    path: Some(path),
                    ..SlotView::default()
                },
            );
            loaded.push(slot);
        }
        loaded
    }

    async fn run(
        self: Arc<Self>,
        mut wake: mpsc::UnboundedReceiver<SlotId>,
        loaded: Vec<SlotId>,
        every: Duration,
    ) {
        for slot in loaded {
            self.refresh(slot).await;
        }
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    for slot in self.running_slots() {
                        self.refresh(slot).await;
                        self.look(slot);
                    }
                }
                Some(slot) = wake.recv() => self.refresh(slot).await,
            }
        }
    }

    /// The followed slots that are running. A slot that exited keeps its
    /// last view but is not read.
    fn running_slots(&self) -> Vec<SlotId> {
        let slots: Vec<SlotId> = self.views.lock().unwrap().keys().copied().collect();
        slots
            .into_iter()
            .filter(|slot| (self.running)(*slot).is_some())
            .collect()
    }

    /// Applies a hook event of the slot's current run. A new transcript or
    /// session starts the slot's conversation over; an event that ends a
    /// tool call or a turn, or opens a permission prompt (whose call is
    /// found in the transcript), reads the transcript.
    pub fn hook(&self, ev: &HookEvent) {
        let mut read = matches!(
            ev.event,
            HookKind::PostToolUse | HookKind::Stop | HookKind::UserPromptSubmit
        ) && ev.agent_id.is_none();
        let mut reset = false;
        {
            let mut views = self.views.lock().unwrap();
            let view = views.entry(ev.slot).or_default();
            // A subagent's events say nothing of the main conversation's
            // file.
            if ev.agent_id.is_none() {
                let session = ev.session_id.as_ref().filter(|s| **s != view.session);
                let path = ev
                    .transcript_path
                    .as_ref()
                    .map(PathBuf::from)
                    .filter(|p| view.path.as_ref() != Some(p));
                if session.is_some() || path.is_some() {
                    match session {
                        // The new session's file is the one it reports, even
                        // the same as before; none reported is unknown.
                        Some(session) => {
                            view.session = session.clone();
                            view.path = ev.transcript_path.as_ref().map(PathBuf::from);
                        }
                        None => view.path = path,
                    }
                    view.generation += 1;
                    view.entries.clear();
                    view.prompt.clear();
                    view.sent.full = true;
                    if let (Some(path), false) = (&view.path, view.session.is_empty()) {
                        view.store = Some((view.session.clone(), path.clone()));
                    }
                    read = true;
                    reset = true;
                }
            }
            let changed = view.prompt.on_hook(ev);
            let asked =
                changed && ev.event == HookKind::PermissionRequest && view.prompt.open().is_some();
            // A read after a reset or a finished call announces the prompt
            // with its entries; a new prompt is shown at once.
            if !reset && (asked || !read) {
                self.publish(ev.slot, view);
            }
            read |= asked;
        }
        if read {
            // Nobody waiting is only a test without the task.
            let _ = self.wake.send(ev.slot);
        }
    }

    /// Reads what the slot's transcript gained since the last read and
    /// announces any change.
    async fn refresh(&self, slot: SlotId) {
        let reader = self
            .readers
            .lock()
            .unwrap()
            .entry(slot)
            .or_default()
            .clone();
        let mut held = reader.lock_owned().await;
        let target = {
            let mut views = self.views.lock().unwrap();
            let Some(view) = views.get_mut(&slot) else {
                return;
            };
            match &view.path {
                Some(path) => (path.clone(), view.generation, view.store.take()),
                // No file to read, but a reset still clears the app's view.
                None => return self.publish(slot, view),
            }
        };
        let (path, generation, store) = target;
        let ledger = self.ledger.clone();
        if held.generation != generation {
            *held = Reader {
                generation,
                tail: Tail::new(),
            };
        }
        // Kept until the read is applied, so reads apply in order.
        let read = tokio::task::spawn_blocking(move || {
            if let Some((session, path)) = store {
                if let Err(e) = ledger.put_transcript(slot, &session, &path) {
                    tracing::warn!("conversation: S{slot} transcript not stored: {e:#}");
                }
            }
            let read = read_new(&path, &mut held.tail);
            (held, read)
        })
        .await;
        let (_held, read) = match read {
            Ok(done) => done,
            Err(e) => {
                tracing::warn!("conversation: reading S{slot}'s transcript failed: {e}");
                return;
            }
        };
        let mut views = self.views.lock().unwrap();
        let Some(view) = views.get_mut(&slot) else {
            return;
        };
        // A reset since the read began: the next read starts the new file.
        if view.generation == generation {
            match read {
                Read::Missing => {}
                Read::More(fed) => {
                    append(view, fed.entries);
                    view.prompt.on_tools(&fed.tools);
                }
                Read::Restarted(fed) => {
                    view.entries.clear();
                    view.sent.full = true;
                    append(view, fed.entries);
                    view.prompt.on_tools(&fed.tools);
                }
            }
        }
        self.publish(slot, view);
    }

    /// Sends what the app lacks of the view, if anything: everything after
    /// a reset or trim, else the new entries, else the changed prompt.
    fn publish(&self, slot: SlotId, view: &mut SlotView) {
        let prompt = view.prompt.open().cloned();
        let from = if view.sent.full {
            0
        } else if view.entries.len() > view.sent.len {
            view.sent.len
        } else if prompt != view.sent.prompt {
            view.entries.len()
        } else {
            return;
        };
        view.sent = Sent {
            len: view.entries.len(),
            prompt: prompt.clone(),
            full: false,
        };
        // Nobody listening is fine.
        let _ = self
            .events
            .send(Event::ConversationChanged(ConversationView {
                slot,
                session: view.session.clone(),
                entries_from: from,
                entries: view.entries[from..].to_vec(),
                prompt,
            }));
    }

    /// The slot's whole conversation as last read.
    pub fn current(&self, slot: SlotId) -> ConversationView {
        let views = self.views.lock().unwrap();
        let view = views.get(&slot);
        ConversationView {
            slot,
            session: view.map(|v| v.session.clone()).unwrap_or_default(),
            entries_from: 0,
            entries: view.map(|v| v.entries.clone()).unwrap_or_default(),
            prompt: view.and_then(|v| v.prompt.open().cloned()),
        }
    }

    /// Drops the slot's open prompt `id`, as when the screen shows its
    /// dialog has gone, and tells the app.
    pub fn forget_prompt(&self, slot: SlotId, id: &str) {
        let mut views = self.views.lock().unwrap();
        if let Some(view) = views.get_mut(&slot) {
            if view.prompt.forget(id) {
                self.publish(slot, view);
            }
        }
    }

    /// Drops an open permission the slot's screen shows answered in the
    /// terminal: no hook says so, but the input box and the working line
    /// are back, here on [`ANSWERED_LOOKS`] polls in a row.
    fn look(&self, slot: SlotId) {
        let permission = |p: &Prompt| {
            matches!(p.info, PromptShown::Known(PromptInfo::Permission { .. }))
                .then(|| p.id.clone())
        };
        let Some(id) = self.open_prompt(slot).as_ref().and_then(permission) else {
            return;
        };
        let answered = (self.screen)(slot)
            .is_some_and(|snap| input_box(&snap) != InputBox::NotFound && shows_working(&snap));
        let mut views = self.views.lock().unwrap();
        let Some(view) = views.get_mut(&slot) else {
            return;
        };
        let still = view.prompt.open().is_some_and(|p| p.id == id);
        if !answered || !still {
            view.answered_looks = None;
            return;
        }
        let looks = match &view.answered_looks {
            Some((was, n)) if *was == id => n + 1,
            _ => 1,
        };
        if looks < ANSWERED_LOOKS {
            view.answered_looks = Some((id, looks));
            return;
        }
        view.answered_looks = None;
        if view.prompt.forget(&id) {
            self.publish(slot, view);
        }
    }

    /// The prompt the slot's session is waiting on.
    pub fn open_prompt(&self, slot: SlotId) -> Option<Prompt> {
        let views = self.views.lock().unwrap();
        views.get(&slot)?.prompt.open().cloned()
    }

    /// The session the slot's conversation is from, once known.
    pub fn session(&self, slot: SlotId) -> Option<String> {
        let views = self.views.lock().unwrap();
        Some(views.get(&slot)?.session.clone()).filter(|s| !s.is_empty())
    }
}

/// Adds entries. Past [`KEEP`] and [`SLACK`] the oldest are dropped down
/// to [`KEEP`], as they are when one read alone brings more than [`KEEP`].
/// A drop makes the next send a full one, since the app's indices no longer
/// match.
fn append(view: &mut SlotView, entries: Vec<ConvEntry>) {
    let read = entries.len();
    view.entries.extend(entries);
    if view.entries.len() > KEEP + SLACK || read > KEEP {
        let extra = view.entries.len() - KEEP;
        view.entries.drain(..extra);
        view.sent.full = true;
    }
}

/// Reads the transcript from where `tail` left off to its end. A file
/// shorter than that was cut or replaced, and is read from the start. Runs
/// off the async runtime.
fn read_new(path: &Path, tail: &mut Tail) -> Read {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Read::Missing,
        Err(e) => {
            tracing::warn!("conversation: {} not opened: {e}", path.display());
            return Read::Missing;
        }
    };
    let read = (|| {
        let restarted = file.metadata()?.len() < tail.offset();
        let from = if restarted { 0 } else { tail.offset() };
        file.seek(SeekFrom::Start(from))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        std::io::Result::Ok((restarted, bytes))
    })();
    // The tail starts over only once the file was read, so a failed read
    // leaves it where it was.
    match read {
        Ok((true, bytes)) => {
            *tail = Tail::new();
            Read::Restarted(tail.read(&bytes))
        }
        Ok((false, bytes)) => Read::More(tail.read(&bytes)),
        Err(e) => {
            tracing::warn!("conversation: {} not read: {e}", path.display());
            Read::Missing
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use overseer_proto::msg::{ConvKind, PromptInfo, PromptShown};
    use std::io::Write as _;

    const SESSION: &str = "demo-session";

    fn line(text: &str) -> String {
        format!(
            "{}\n",
            serde_json::json!({"type":"user","message":{"role":"user","content":text}})
        )
    }

    fn write(path: &Path, texts: &[&str]) {
        let body: String = texts.iter().map(|t| line(t)).collect();
        std::fs::write(path, body).unwrap();
    }

    fn add(path: &Path, texts: &[&str]) {
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        for t in texts {
            f.write_all(line(t).as_bytes()).unwrap();
        }
    }

    fn texts(entries: &[ConvEntry]) -> Vec<String> {
        entries
            .iter()
            .map(|e| match &e.kind {
                ConvKind::User { text } => text.clone(),
                other => panic!("not a user entry: {other:?}"),
            })
            .collect()
    }

    fn hook(event: HookKind, path: &Path) -> HookEvent {
        HookEvent {
            slot: 2,
            event,
            session_id: Some(SESSION.into()),
            tool: None,
            detail: None,
            notification: None,
            tool_use_id: None,
            notification_type: None,
            run: None,
            agent_id: None,
            transcript_path: Some(path.to_string_lossy().into_owned()),
            prompt: None,
        }
    }

    struct Fixture {
        dir: tempfile::TempDir,
        ledger: Arc<Ledger>,
        svc: Arc<ConversationService>,
        rx: broadcast::Receiver<Event>,
        /// What slot 2's screen shows.
        screen: Shown,
    }

    type Shown = Arc<Mutex<Option<Snapshot>>>;

    /// A service whose slot 2 runs [`SESSION`], with no poll task: tests
    /// read with `refresh`.
    fn fixture() -> Fixture {
        let ledger = Arc::new(Ledger::in_memory());
        let dir = tempfile::tempdir().unwrap();
        let screen = Shown::default();
        let (svc, rx) = service(ledger.clone(), screen.clone());
        Fixture {
            dir,
            ledger,
            svc,
            rx,
            screen,
        }
    }

    fn service(
        ledger: Arc<Ledger>,
        shown: Shown,
    ) -> (Arc<ConversationService>, broadcast::Receiver<Event>) {
        let (events, rx) = broadcast::channel(64);
        let running = Box::new(|slot| (slot == 2).then(|| SESSION.to_string()));
        let screen =
            Box::new(move |slot| (slot == 2).then(|| shown.lock().unwrap().clone()).flatten());
        let (svc, _wake) = ConversationService::new(ledger, running, screen, events);
        (svc, rx)
    }

    fn sent(rx: &mut broadcast::Receiver<Event>) -> Vec<ConversationView> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let Event::ConversationChanged(v) = ev {
                out.push(v);
            }
        }
        out
    }

    #[tokio::test]
    async fn appending_lines_sends_only_the_new_entries() {
        let mut f = fixture();
        let path = f.dir.path().join("one.jsonl");
        write(&path, &["fix DEMO-1", "in demo-api"]);
        f.svc.hook(&hook(HookKind::SessionStart, &path));
        f.svc.refresh(2).await;
        add(&path, &["and run the tests"]);
        f.svc.refresh(2).await;
        let views = sent(&mut f.rx);
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].entries_from, 0);
        assert_eq!(texts(&views[0].entries), ["fix DEMO-1", "in demo-api"]);
        assert_eq!(views[0].session, SESSION);
        assert_eq!(views[1].entries_from, 2);
        assert_eq!(texts(&views[1].entries), ["and run the tests"]);
        assert_eq!(f.svc.current(2).entries.len(), 3);
        assert_eq!(f.svc.session(2).as_deref(), Some(SESSION));
        assert_eq!(
            f.ledger.transcript(2).unwrap(),
            Some((SESSION.to_string(), path))
        );
    }

    #[tokio::test]
    async fn a_new_transcript_resets_the_slot() {
        let mut f = fixture();
        let one = f.dir.path().join("one.jsonl");
        let two = f.dir.path().join("two.jsonl");
        write(&one, &["first", "second"]);
        write(&two, &["after clear"]);
        f.svc.hook(&hook(HookKind::SessionStart, &one));
        f.svc.refresh(2).await;
        sent(&mut f.rx);
        f.svc.hook(&hook(HookKind::SessionStart, &two));
        f.svc.refresh(2).await;
        let views = sent(&mut f.rx);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].entries_from, 0);
        assert_eq!(texts(&views[0].entries), ["after clear"]);
        assert_eq!(texts(&f.svc.current(2).entries), ["after clear"]);
        assert_eq!(f.ledger.transcript(2).unwrap().unwrap().1, two);
        // A new session with no file yet still clears the app's view.
        let other = HookEvent {
            session_id: Some("demo-next".into()),
            transcript_path: None,
            ..hook(HookKind::SessionStart, &two)
        };
        f.svc.hook(&other);
        f.svc.refresh(2).await;
        let views = sent(&mut f.rx);
        assert_eq!(views.len(), 1);
        assert!(views[0].entries.is_empty());
        assert_eq!(views[0].session, "demo-next");
    }

    #[tokio::test]
    async fn a_long_transcript_keeps_the_last_entries() {
        let mut f = fixture();
        let path = f.dir.path().join("long.jsonl");
        let lines: Vec<String> = (1..=500).map(|n| format!("line {n}")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        write(&path, &refs);
        f.svc.hook(&hook(HookKind::SessionStart, &path));
        f.svc.refresh(2).await;
        let now = f.svc.current(2).entries;
        assert_eq!(now.len(), KEEP);
        assert_eq!(texts(&now)[0], "line 301");
        assert_eq!(texts(&now)[KEEP - 1], "line 500");
        sent(&mut f.rx);
        // The next lines are appends while within the slack.
        add(&path, &["line 501"]);
        f.svc.refresh(2).await;
        let views = sent(&mut f.rx);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].entries_from, KEEP);
        assert_eq!(texts(&views[0].entries), ["line 501"]);
        let more: Vec<String> = (502..=550).map(|n| format!("line {n}")).collect();
        let refs: Vec<&str> = more.iter().map(String::as_str).collect();
        add(&path, &refs);
        f.svc.refresh(2).await;
        let views = sent(&mut f.rx);
        assert_eq!(views[0].entries_from, KEEP + 1);
        assert_eq!(f.svc.current(2).entries.len(), KEEP + SLACK);
        // One past the slack trims back to KEEP: the whole view is sent.
        add(&path, &["line 551"]);
        f.svc.refresh(2).await;
        let views = sent(&mut f.rx);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].entries_from, 0);
        assert_eq!(views[0].entries.len(), KEEP);
        assert_eq!(texts(&views[0].entries)[0], "line 352");
        assert_eq!(texts(&views[0].entries)[KEEP - 1], "line 551");
    }

    #[tokio::test]
    async fn a_new_session_in_the_same_file_keeps_the_path() {
        let mut f = fixture();
        let path = f.dir.path().join("one.jsonl");
        write(&path, &["hello"]);
        f.svc.hook(&hook(HookKind::SessionStart, &path));
        f.svc.refresh(2).await;
        sent(&mut f.rx);
        let next = HookEvent {
            session_id: Some("demo-next".into()),
            ..hook(HookKind::SessionStart, &path)
        };
        f.svc.hook(&next);
        f.svc.refresh(2).await;
        let views = sent(&mut f.rx);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].entries_from, 0);
        assert_eq!(views[0].session, "demo-next");
        assert_eq!(texts(&views[0].entries), ["hello"]);
        assert_eq!(
            f.ledger.transcript(2).unwrap(),
            Some(("demo-next".to_string(), path))
        );
    }

    #[test]
    fn a_failed_read_keeps_where_the_tail_was() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("one.jsonl");
        let body: String = (0..200).map(|n| line(&format!("line {n}"))).collect();
        std::fs::write(&path, &body).unwrap();
        let mut tail = Tail::new();
        assert!(matches!(read_new(&path, &mut tail), Read::More(f) if f.entries.len() == 200));
        let offset = tail.offset();
        // A directory opens but does not read, and is shorter than the
        // offset: the read fails and the tail is untouched.
        let cut = dir.path().join("cut");
        std::fs::create_dir(&cut).unwrap();
        assert!(std::fs::metadata(&cut).unwrap().len() < offset);
        assert!(matches!(read_new(&cut, &mut tail), Read::Missing));
        assert_eq!(tail.offset(), offset);
        // Once the file reads, a shorter one starts over.
        write(&path, &["new"]);
        assert!(matches!(read_new(&path, &mut tail), Read::Restarted(f) if f.entries.len() == 1));
    }

    #[tokio::test]
    async fn restart_reloads_the_stored_transcript() {
        let ledger = Arc::new(Ledger::in_memory());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("one.jsonl");
        write(&path, &["before the restart"]);
        ledger.put_transcript(2, SESSION, &path).unwrap();
        // Slot 3's stored session is not the one it runs: not loaded.
        let stale = dir.path().join("stale.jsonl");
        write(&stale, &["old"]);
        ledger.put_transcript(3, "demo-old", &stale).unwrap();
        let (svc, mut rx) = service(ledger, Shown::default());
        assert_eq!(svc.load(), vec![2]);
        svc.refresh(2).await;
        assert_eq!(texts(&svc.current(2).entries), ["before the restart"]);
        assert_eq!(svc.session(2).as_deref(), Some(SESSION));
        assert_eq!(sent(&mut rx).len(), 1);
        assert!(svc.current(3).entries.is_empty());
        assert_eq!(svc.session(3), None);
    }

    #[tokio::test]
    async fn nothing_changed_sends_nothing() {
        let mut f = fixture();
        let path = f.dir.path().join("one.jsonl");
        write(&path, &["hello"]);
        f.svc.hook(&hook(HookKind::SessionStart, &path));
        f.svc.refresh(2).await;
        assert_eq!(sent(&mut f.rx).len(), 1);
        f.svc.refresh(2).await;
        // A hook that changes neither entries nor the prompt.
        let idle = HookEvent {
            notification_type: Some("idle_prompt".into()),
            ..hook(HookKind::Notification, &path)
        };
        f.svc.hook(&idle);
        f.svc.refresh(2).await;
        assert!(sent(&mut f.rx).is_empty());
    }

    #[tokio::test]
    async fn a_prompt_change_sends_the_prompt_alone() {
        let mut f = fixture();
        let path = f.dir.path().join("one.jsonl");
        write(&path, &["hello"]);
        f.svc.hook(&hook(HookKind::SessionStart, &path));
        f.svc.refresh(2).await;
        sent(&mut f.rx);
        // As Claude Code sends it: no tool_use_id.
        let ask = HookEvent {
            tool: Some("Bash".into()),
            detail: Some("rm -rf /tmp/demo/target".into()),
            prompt: Some(PromptInfo::Permission {
                tool: "Bash".into(),
                summary: "rm -rf /tmp/demo/target".into(),
                plan: None,
            }),
            ..hook(HookKind::PermissionRequest, &path)
        };
        f.svc.hook(&ask);
        let views = sent(&mut f.rx);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].entries_from, 1);
        assert!(views[0].entries.is_empty());
        let open = views[0].prompt.clone().unwrap();
        assert_eq!(open.id, "n1");
        assert!(matches!(open.info, PromptShown::Known(_)));
        assert_eq!(f.svc.open_prompt(2), Some(open));
    }

    #[tokio::test]
    async fn a_denied_permission_clears_from_the_transcript() {
        let mut f = fixture();
        let path = f.dir.path().join("one.jsonl");
        write(&path, &["clean up"]);
        f.svc.hook(&hook(HookKind::SessionStart, &path));
        f.svc.refresh(2).await;
        let rm = "rm -rf /tmp/demo/target";
        let call = serde_json::json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","id":"tu-1","name":"Bash","input":{"command":rm}}]}});
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(format!("{call}\n").as_bytes())
            .unwrap();
        let ask = HookEvent {
            tool: Some("Bash".into()),
            detail: Some(rm.into()),
            prompt: Some(PromptInfo::Permission {
                tool: "Bash".into(),
                summary: rm.into(),
                plan: None,
            }),
            ..hook(HookKind::PermissionRequest, &path)
        };
        f.svc.hook(&ask);
        f.svc.refresh(2).await;
        assert!(f.svc.open_prompt(2).is_some());
        sent(&mut f.rx);
        // Denied: the answer is written and no hook fires.
        let denial = serde_json::json!({"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"tu-1","is_error":true,
             "content":"The user doesn't want to proceed with this tool use."}]}});
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(format!("{denial}\n").as_bytes())
            .unwrap();
        f.svc.refresh(2).await;
        assert_eq!(f.svc.open_prompt(2), None);
        let views = sent(&mut f.rx);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].prompt, None);
    }

    #[tokio::test]
    async fn a_shorter_or_missing_file_is_handled() {
        let mut f = fixture();
        let path = f.dir.path().join("one.jsonl");
        // Not there yet: nothing read, nothing wrong.
        f.svc.hook(&hook(HookKind::SessionStart, &path));
        f.svc.refresh(2).await;
        assert!(f.svc.current(2).entries.is_empty());
        sent(&mut f.rx);
        write(&path, &["one", "two"]);
        f.svc.refresh(2).await;
        assert_eq!(f.svc.current(2).entries.len(), 2);
        sent(&mut f.rx);
        // Cut short and rewritten: read again from the start, sent whole.
        write(&path, &["new"]);
        f.svc.refresh(2).await;
        let views = sent(&mut f.rx);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].entries_from, 0);
        assert_eq!(texts(&views[0].entries), ["new"]);
    }

    fn permission_request(path: &Path) -> HookEvent {
        let rm = "rm -rf /tmp/demo/target";
        HookEvent {
            tool: Some("Bash".into()),
            detail: Some(rm.into()),
            prompt: Some(PromptInfo::Permission {
                tool: "Bash".into(),
                summary: rm.into(),
                plan: None,
            }),
            ..hook(HookKind::PermissionRequest, path)
        }
    }

    /// Claude Code's screen with the empty input box, under the working
    /// line when `working`; else a permission dialog.
    fn claude_screen(working: Option<bool>) -> Snapshot {
        let rule = "\u{2500}".repeat(60);
        let body = match working {
            None => "Do you want to proceed?\r\n  1. Yes\r\n".to_string(),
            Some(working) => format!(
                "{}\r\n\r\n{rule}\r\n\u{276f} \r\n{rule}\r\n",
                if working {
                    "\u{273b} Churning\u{2026} (4s \u{b7} 323 tokens)"
                } else {
                    "Working on DEMO-12 in demo-api"
                }
            ),
        };
        crate::relay::screen::screen_of(format!("\x1b[2J\x1b[H{body}").as_bytes(), 60, 12).0
    }

    #[tokio::test]
    async fn a_permission_answered_in_the_terminal_is_dropped_on_two_looks() {
        let mut f = fixture();
        let path = f.dir.path().join("one.jsonl");
        write(&path, &["build demo-api"]);
        f.svc.hook(&hook(HookKind::SessionStart, &path));
        f.svc.refresh(2).await;
        f.svc.hook(&permission_request(&path));
        sent(&mut f.rx);
        // The dialog shows, or nothing can be read: kept.
        for shown in [
            Some(claude_screen(None)),
            None,
            Some(claude_screen(Some(false))),
        ] {
            *f.screen.lock().unwrap() = shown;
            f.svc.look(2);
            f.svc.look(2);
            assert!(f.svc.open_prompt(2).is_some());
        }
        // One look at the box and the working line is not enough, and a
        // dialog between starts the count again.
        *f.screen.lock().unwrap() = Some(claude_screen(Some(true)));
        f.svc.look(2);
        *f.screen.lock().unwrap() = Some(claude_screen(None));
        f.svc.look(2);
        *f.screen.lock().unwrap() = Some(claude_screen(Some(true)));
        f.svc.look(2);
        assert!(f.svc.open_prompt(2).is_some());
        assert!(sent(&mut f.rx).is_empty());
        f.svc.look(2);
        assert_eq!(f.svc.open_prompt(2), None);
        let views = sent(&mut f.rx);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].prompt, None);
    }

    #[tokio::test]
    async fn forgetting_a_prompt_tells_the_app() {
        let mut f = fixture();
        let path = f.dir.path().join("one.jsonl");
        write(&path, &["build demo-api"]);
        f.svc.hook(&hook(HookKind::SessionStart, &path));
        f.svc.refresh(2).await;
        f.svc.hook(&permission_request(&path));
        sent(&mut f.rx);
        f.svc.forget_prompt(2, "n9");
        assert!(sent(&mut f.rx).is_empty());
        f.svc.forget_prompt(2, "n1");
        assert_eq!(f.svc.open_prompt(2), None);
        let views = sent(&mut f.rx);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].prompt, None);
    }

    #[tokio::test]
    async fn the_transcript_is_stored_by_the_read_not_the_hook() {
        let f = fixture();
        let path = f.dir.path().join("one.jsonl");
        write(&path, &["hello"]);
        f.svc.hook(&hook(HookKind::SessionStart, &path));
        assert_eq!(f.ledger.transcript(2).unwrap(), None);
        f.svc.refresh(2).await;
        assert_eq!(
            f.ledger.transcript(2).unwrap(),
            Some((SESSION.to_string(), path))
        );
    }
}
