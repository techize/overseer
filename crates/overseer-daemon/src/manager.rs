//! The seven slots (S0 to S6) and their lifecycle.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use overseer_proto::{slot_name, Event, SlotId, SlotInfo, SlotState};
use tokio::sync::broadcast;

use crate::slot::Slot;

pub const SLOT_COUNT: usize = 7;
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;
/// Chunks buffered per output subscriber before it starts lagging.
const OUTPUT_CHANNEL_CAP: usize = 1024;
/// How long to wait for the output stream to end after the child exits.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// N2: slot ids are validated here, in the daemon layer.
pub fn valid_id(id: SlotId) -> bool {
    (id as usize) < SLOT_COUNT
}

fn invalid_id(id: SlotId) -> String {
    format!("invalid slot id {id} (expected 0..={})", SLOT_COUNT - 1)
}

struct Entry {
    slot: Arc<Slot>,
}

pub struct SlotManager {
    slots: [Option<Entry>; SLOT_COUNT],
    default_command: Vec<String>,
    events: broadcast::Sender<Event>,
}

/// What a client needs to attach to a slot.
pub struct Attachment {
    pub slot: Arc<Slot>,
    /// Live output, gap-free and duplicate-free after `snapshot`. `None` when the
    /// slot's output stream has already ended (the snapshot is all there is).
    pub live: Option<broadcast::Receiver<Bytes>>,
    pub snapshot: Vec<u8>,
}

impl SlotManager {
    pub fn new(default_command: Vec<String>, events: broadcast::Sender<Event>) -> Self {
        SlotManager {
            slots: Default::default(),
            default_command,
            events,
        }
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn start(
        &mut self,
        id: SlotId,
        label: String,
        cwd: &Path,
        command: Option<Vec<String>>,
    ) -> Result<(), String> {
        if !valid_id(id) {
            return Err(invalid_id(id));
        }
        if let Some(entry) = &self.slots[id as usize] {
            if matches!(entry.slot.info().state, SlotState::Running { .. }) {
                return Err(format!("{} is already running", slot_name(id)));
            }
        }
        let command = command.unwrap_or_else(|| self.default_command.clone());
        let (tx, monitor_rx) = broadcast::channel(OUTPUT_CHANNEL_CAP);
        let slot = Slot::spawn(id, label, cwd, &command, DEFAULT_COLS, DEFAULT_ROWS, tx)
            .map_err(|e| format!("{e:#}"))?;
        let slot = Arc::new(slot);
        // N4: tear down whatever occupied the slot before (an exited child;
        // kill is a no-op then, but keeps the invariant in one place).
        if let Some(old) = self.slots[id as usize].replace(Entry { slot: slot.clone() }) {
            old.slot.kill();
        }
        let _ = self.events.send(Event::SlotChanged(slot.info()));
        tokio::spawn(monitor_exit(slot, monitor_rx, self.events.clone()));
        Ok(())
    }

    pub fn stop(&self, id: SlotId) -> Result<(), String> {
        if !valid_id(id) {
            return Err(invalid_id(id));
        }
        match &self.slots[id as usize] {
            Some(e) if matches!(e.slot.info().state, SlotState::Running { .. }) => {
                e.slot.kill();
                Ok(())
            }
            _ => Err(format!("{} is not running", slot_name(id))),
        }
    }

    pub fn list(&self) -> Vec<SlotInfo> {
        (0..SLOT_COUNT as u8)
            .map(|id| match &self.slots[id as usize] {
                Some(e) => e.slot.info(),
                None => SlotInfo {
                    id,
                    label: String::new(),
                    cwd: None,
                    command: None,
                    state: SlotState::Empty,
                    cols: DEFAULT_COLS,
                    rows: DEFAULT_ROWS,
                },
            })
            .collect()
    }

    /// The slot, if the id is valid and occupied.
    pub fn slot(&self, id: SlotId) -> Option<Arc<Slot>> {
        self.slots
            .get(id as usize)?
            .as_ref()
            .map(|e| e.slot.clone())
    }

    pub fn attach(&self, id: SlotId) -> Result<Attachment, String> {
        if !valid_id(id) {
            return Err(invalid_id(id));
        }
        let entry = self.slots[id as usize]
            .as_ref()
            .ok_or_else(|| format!("{} is empty", slot_name(id)))?;
        let (snapshot, live) = entry.slot.attach();
        Ok(Attachment {
            slot: entry.slot.clone(),
            live,
            snapshot,
        })
    }

    /// N4: kill every child. Called on daemon shutdown.
    pub fn shutdown_all(&mut self) {
        for entry in self.slots.iter_mut().filter_map(Option::take) {
            entry.slot.kill();
        }
    }
}

/// Emits `SlotChanged` once the child has exited AND its output stream has
/// ended (N5). `Slot::wait_exit` can fire before the reader thread has pushed
/// the final bytes, so we wait for the output channel to close (the reader
/// thread dropping its sender) before announcing the exit. Clients that attach
/// after the event therefore see the complete tail in the snapshot. The wait is
/// bounded: a grandchild that inherited the PTY could keep the stream open.
async fn monitor_exit(
    slot: Arc<Slot>,
    mut rx: broadcast::Receiver<Bytes>,
    events: broadcast::Sender<Event>,
) {
    slot.wait_exit().await;
    let _ = tokio::time::timeout(DRAIN_TIMEOUT, async {
        loop {
            match rx.recv().await {
                Err(broadcast::error::RecvError::Closed) => break,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
            }
        }
    })
    .await;
    let _ = events.send(Event::SlotChanged(slot.info()));
}
