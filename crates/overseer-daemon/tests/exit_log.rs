//! What the daemon logs when a slot ends. On its own in this file: the
//! subscriber that captures the log is the process's global one, which a
//! test binary can set only once.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use overseer_daemon::manager::SlotManager;
use overseer_proto::{Event, SlotState};
use tokio::sync::broadcast;

/// Everything logged, as text.
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
    type Writer = Logs;
    fn make_writer(&'a self) -> Logs {
        self.clone()
    }
}

impl Logs {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// A slot's own exit is logged with its code. One that shutdown ends is
/// not logged as an exit (the slot is no longer its entry's): shutdown says
/// it ended it.
#[tokio::test]
async fn only_a_slots_own_exit_is_logged_as_an_exit() {
    let logs = Logs::default();
    tracing_subscriber::fmt().with_writer(logs.clone()).init();
    let (events, mut ev_rx) = broadcast::channel(64);
    let mut mgr = SlotManager::new(vec!["/bin/sleep".into(), "30".into()], events);
    let own = ["/bin/sh", "-c", "sleep 0.2; exit 3"]
        .map(String::from)
        .to_vec();
    mgr.start(1, "own".into(), Path::new("/tmp"), Some(own), None)
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(Event::SlotChanged(info)) = ev_rx.recv().await {
                if info.id == 1 && matches!(info.state, SlotState::Exited { .. }) {
                    return;
                }
            }
        }
    })
    .await
    .expect("S1 never exited");

    mgr.start(2, "kept".into(), Path::new("/tmp"), None, None)
        .unwrap();
    let kept = mgr.slot(2).unwrap();
    mgr.shutdown_all();
    kept.wait_exit().await;
    // Past the exit monitor's drain (DRAIN_TIMEOUT, 2s).
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let text = logs.text();
    assert!(text.contains("S1 exited (code Some(3))"), "{text}");
    assert!(!text.contains("S2 exited"), "{text}");
    assert!(text.contains("S2 ended at shutdown"), "{text}");
}
