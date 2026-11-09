//! GPUI front end for Overseer.

pub mod terminal_view;
pub mod theme;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use gpui::{
    px, size, App, Application, Bounds, Global, TitlebarOptions, WindowBounds, WindowOptions,
};
use overseer_proto::{Client, Frame, SlotId};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

pub use terminal_view::TerminalView;
pub use theme::Theme;

/// Fans out the single `Client::subscribe` stream to the views that show each
/// slot. `subscribe` only hands frames to its first caller, so every view on
/// one connection reads through this. The router drains the client inbox as
/// fast as frames arrive (into unbounded per-slot queues), so a busy GUI
/// thread can never make the client's bounded inbox overflow.
///
/// Installed as a GPUI global by `run_gui`; `TerminalView::new` reads it.
#[derive(Clone)]
pub struct FrameHub {
    runtime: Handle,
    routes: Arc<Mutex<Routes>>,
}

#[derive(Default)]
struct Routes {
    next_gen: u64,
    by_slot: HashMap<SlotId, (u64, mpsc::UnboundedSender<Bytes>)>,
    /// The connection is gone; new routes get a closed queue at once.
    closed: bool,
}

impl Global for FrameHub {}

impl FrameHub {
    /// Takes `client`'s frame stream and starts routing it on `runtime`.
    pub fn start(client: &Client, runtime: Handle) -> FrameHub {
        let mut frames = {
            let _guard = runtime.enter();
            client.subscribe()
        };
        let routes = Arc::new(Mutex::new(Routes::default()));
        let router = routes.clone();
        runtime.spawn(async move {
            while let Some(frame) = frames.recv().await {
                // Events are not consumed yet; the board will read them.
                if let Frame::Output { id, data } = frame {
                    let tx = router
                        .lock()
                        .unwrap()
                        .by_slot
                        .get(&id)
                        .map(|(_, tx)| tx.clone());
                    if let Some(tx) = tx {
                        let _ = tx.send(data);
                    }
                }
            }
            // Dropping the senders tells every view the connection ended.
            let mut r = router.lock().unwrap();
            r.closed = true;
            r.by_slot.clear();
        });
        FrameHub { runtime, routes }
    }

    pub fn runtime(&self) -> &Handle {
        &self.runtime
    }

    /// Sends `slot`'s output to the returned queue from now on, replacing any
    /// earlier route. The number identifies this route for `unroute`.
    pub fn route(&self, slot: SlotId) -> (u64, mpsc::UnboundedReceiver<Bytes>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut r = self.routes.lock().unwrap();
        r.next_gen += 1;
        let gen = r.next_gen;
        if !r.closed {
            r.by_slot.insert(slot, (gen, tx));
        }
        (gen, rx)
    }

    /// Removes the route made by `route` call `gen`. Returns false when a
    /// newer route for the slot has replaced it (and so was left alone).
    pub fn unroute(&self, slot: SlotId, gen: u64) -> bool {
        let mut r = self.routes.lock().unwrap();
        match r.by_slot.get(&slot) {
            Some((g, _)) if *g == gen => {
                r.by_slot.remove(&slot);
                true
            }
            _ => false,
        }
    }
}

/// Slot shown by the temporary single-terminal window, until the board lands.
const TEMP_SLOT: SlotId = 1;

/// Opens the Overseer window. Blocks until the app quits. `runtime` must keep
/// running for as long as the window is open; `client` must belong to it.
pub fn run_gui(client: Arc<Client>, runtime: Handle) {
    Application::new().run(move |cx: &mut App| {
        cx.set_global(FrameHub::start(&client, runtime));
        let theme = Theme::load();
        let bounds = Bounds::centered(None, size(px(900.), px(600.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Overseer".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                let view = TerminalView::new(TEMP_SLOT, client.clone(), theme, window, cx);
                view.read(cx).focus(window);
                view
            },
        )
        .expect("failed to open window");
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hub() -> (tokio::runtime::Runtime, FrameHub) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let hub = FrameHub {
            runtime: rt.handle().clone(),
            routes: Arc::default(),
        };
        (rt, hub)
    }

    #[test]
    fn newer_route_survives_older_unroute() {
        let (_rt, hub) = hub();
        let (old, _a) = hub.route(1);
        let (new, mut b) = hub.route(1);
        assert!(!hub.unroute(1, old), "old route was already replaced");
        let tx = hub.routes.lock().unwrap().by_slot[&1].1.clone();
        tx.send(Bytes::from_static(b"x")).unwrap();
        assert_eq!(b.try_recv().unwrap(), Bytes::from_static(b"x"));
        assert!(hub.unroute(1, new));
        assert!(hub.routes.lock().unwrap().by_slot.is_empty());
    }

    #[test]
    fn route_after_close_is_closed() {
        let (_rt, hub) = hub();
        hub.routes.lock().unwrap().closed = true;
        let (_, mut rx) = hub.route(2);
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }
}
