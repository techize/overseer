//! Follows each slot's conversation for the board: tails the session's
//! transcript, tracks the prompt it is waiting on, and announces both when
//! they change.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read as _, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use overseer_proto::msg::{ConvEntry, ConversationView, HookEvent, HookKind, Prompt};
use overseer_proto::{Event, SlotId};
use tokio::sync::{broadcast, mpsc};
use tokio::time::MissedTickBehavior;

use super::parse::Tail;
use super::prompt::PromptState;
use crate::ledger::Ledger;
use crate::manager::{SlotManager, SLOT_COUNT};

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

/// Holds each slot's conversation and announces it when it changes.
pub struct ConversationService {
    ledger: Arc<Ledger>,
    running: Running,
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
    /// Entries from lines after the last read.
    More(Vec<ConvEntry>),
    /// The file was shorter than what was read before, so it was read
    /// again from the start.
    Restarted(Vec<ConvEntry>),
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
        let running = Box::new(move |slot| manager.lock().unwrap().running_session(slot));
        let (svc, wake) = Self::new(ledger, running, events);
        let loaded = svc.load();
        tokio::spawn(svc.clone().run(wake, loaded, POLL));
        svc
    }

    fn new(
        ledger: Arc<Ledger>,
        running: Running,
        events: broadcast::Sender<Event>,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<SlotId>) {
        let (wake, rx) = mpsc::unbounded_channel();
        let svc = Arc::new(ConversationService {
            ledger,
            running,
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
    /// tool call or a turn reads the transcript.
    pub fn hook(&self, ev: &HookEvent) {
        let mut read = matches!(
            ev.event,
            HookKind::PostToolUse | HookKind::Stop | HookKind::UserPromptSubmit
        ) && ev.agent_id.is_none();
        let mut store = None;
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
                        store = Some((view.session.clone(), path.clone()));
                    }
                    read = true;
                }
            }
            view.prompt.on_hook(ev);
            // A read announces the prompt with its entries.
            if !read {
                self.publish(ev.slot, view);
            }
        }
        if let Some((session, path)) = store {
            if let Err(e) = self.ledger.put_transcript(ev.slot, &session, &path) {
                tracing::warn!("conversation: S{} transcript not stored: {e:#}", ev.slot);
            }
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
                Some(path) => (path.clone(), view.generation),
                // No file to read, but a reset still clears the app's view.
                None => return self.publish(slot, view),
            }
        };
        let (path, generation) = target;
        if held.generation != generation {
            *held = Reader {
                generation,
                tail: Tail::new(),
            };
        }
        // Kept until the read is applied, so reads apply in order.
        let read = tokio::task::spawn_blocking(move || {
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
                Read::More(entries) => append(view, entries),
                Read::Restarted(entries) => {
                    view.entries.clear();
                    view.sent.full = true;
                    append(view, entries);
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
            Read::Restarted(tail.feed(&bytes))
        }
        Ok((false, bytes)) => Read::More(tail.feed(&bytes)),
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
    }

    /// A service whose slot 2 runs [`SESSION`], with no poll task: tests
    /// read with `refresh`.
    fn fixture() -> Fixture {
        let ledger = Arc::new(Ledger::in_memory());
        let dir = tempfile::tempdir().unwrap();
        let (svc, rx) = service(ledger.clone());
        Fixture {
            dir,
            ledger,
            svc,
            rx,
        }
    }

    fn service(ledger: Arc<Ledger>) -> (Arc<ConversationService>, broadcast::Receiver<Event>) {
        let (events, rx) = broadcast::channel(64);
        let running = Box::new(|slot| (slot == 2).then(|| SESSION.to_string()));
        let (svc, _wake) = ConversationService::new(ledger, running, events);
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
        assert!(matches!(read_new(&path, &mut tail), Read::More(e) if e.len() == 200));
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
        assert!(matches!(read_new(&path, &mut tail), Read::Restarted(e) if e.len() == 1));
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
        let (svc, mut rx) = service(ledger);
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
}
