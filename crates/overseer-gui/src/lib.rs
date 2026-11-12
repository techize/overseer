//! GPUI front end for Overseer.

pub mod app;
pub mod board;
pub mod notify;
pub mod picker;
pub mod session;
pub mod start_dialog;
pub mod startup_error;
pub mod terminal_view;
pub mod theme;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use gpui::{
    px, size, App, AppContext, Application, Bounds, Global, TitlebarOptions, WindowBounds,
    WindowOptions,
};
use overseer_proto::{Client, Frame, Request, Response, SlotId};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

pub use startup_error::run_startup_error;
pub use terminal_view::TerminalView;
pub use theme::Theme;

/// Fans out the single `Client::subscribe` stream: each slot's output to the
/// view that shows it, and slot events (plus unclaimed marked replies) to the
/// app. `subscribe` only hands frames to its first caller, so everything on
/// one connection reads through this. The router drains the client inbox as
/// fast as frames arrive (into unbounded queues), so a busy GUI thread can
/// never make the client's bounded inbox overflow.
///
/// N11: a route only receives output once its own `attach` has been answered.
/// The daemon sends Attach's `Ok` and then the snapshot, and `attach` uses
/// `Client::request_marked`, so the reply's mark sits in the frame stream
/// right before the snapshot. Output for the slot that arrives earlier (live
/// output still in flight from an earlier attach on this connection) is
/// dropped, so a recreated view replays exactly the snapshot plus live
/// output.
///
/// Installed as a GPUI global by `run_gui`; `TerminalView::new` reads it.
#[derive(Clone)]
pub struct FrameHub {
    runtime: Handle,
    routes: Arc<Mutex<Routes>>,
    events: Arc<Mutex<Option<mpsc::UnboundedReceiver<Frame>>>>,
}

/// Where a route is in its attach.
#[derive(Debug, Clone, Copy, PartialEq)]
enum RouteState {
    /// Made, attach not sent yet.
    Idle,
    /// Attach sent with this sequence number; output is still stale.
    Attaching(u32),
    /// Attach answered `Ok`: output from here on is the snapshot, then live.
    Live,
}

struct Route {
    gen: u64,
    state: RouteState,
    tx: mpsc::UnboundedSender<Bytes>,
}

#[derive(Default)]
struct Routes {
    next_gen: u64,
    by_slot: HashMap<SlotId, Route>,
    /// The connection is gone; new routes get a closed queue at once.
    closed: bool,
}

impl Routes {
    /// Applies one frame. Returns it when it is for the app instead.
    fn dispatch(&mut self, frame: Frame) -> Option<Frame> {
        match frame {
            Frame::Output { id, data } => {
                if let Some(r) = self.by_slot.get(&id) {
                    if r.state == RouteState::Live {
                        let _ = r.tx.send(data);
                    }
                }
                None
            }
            Frame::Response { seq, resp } => {
                let claimed = self
                    .by_slot
                    .values_mut()
                    .find(|r| r.state == RouteState::Attaching(seq));
                match claimed {
                    Some(r) => {
                        // A failed attach stays gated; the view reports it.
                        if resp == Response::Ok {
                            r.state = RouteState::Live;
                        }
                        None
                    }
                    None => Some(Frame::Response { seq, resp }),
                }
            }
            other => Some(other),
        }
    }
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
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        runtime.spawn(async move {
            while let Some(frame) = frames.recv().await {
                let for_app = router.lock().unwrap().dispatch(frame);
                if let Some(f) = for_app {
                    let _ = events_tx.send(f);
                }
            }
            // Dropping the senders tells every view the connection ended.
            let mut r = router.lock().unwrap();
            r.closed = true;
            r.by_slot.clear();
        });
        FrameHub {
            runtime,
            routes,
            events: Arc::new(Mutex::new(Some(events_rx))),
        }
    }

    pub fn runtime(&self) -> &Handle {
        &self.runtime
    }

    /// Events and unclaimed `request_marked` replies, in wire order. Only the
    /// first call gets them.
    pub fn take_events(&self) -> Option<mpsc::UnboundedReceiver<Frame>> {
        self.events.lock().unwrap().take()
    }

    /// Makes the route for `slot`'s output, replacing any earlier route. No
    /// output reaches it until `attach` with the returned number succeeds.
    pub fn route(&self, slot: SlotId) -> (u64, mpsc::UnboundedReceiver<Bytes>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut r = self.routes.lock().unwrap();
        r.next_gen += 1;
        let gen = r.next_gen;
        if !r.closed {
            let state = RouteState::Idle;
            r.by_slot.insert(slot, Route { gen, state, tx });
        }
        (gen, rx)
    }

    /// Attaches `slot` for route `gen`. Once the daemon answers `Ok`, the
    /// route gets the snapshot and then live output, and nothing older.
    ///
    /// If a newer `route` call (or `unroute`) has superseded `gen`, the
    /// Attach is never sent and this fails with `Cancelled`: a late Attach
    /// would make the daemon restart the stream and replay its snapshot into
    /// the newer, already live route. The check runs under the client's
    /// request ordering lock, so an Attach that passes it is on the wire
    /// before the newer route's Attach.
    pub async fn attach(
        &self,
        client: &Client,
        slot: SlotId,
        gen: u64,
    ) -> anyhow::Result<Response> {
        let routes = self.routes.clone();
        client
            .request_marked(Request::Attach { id: slot }, move |seq| {
                match routes.lock().unwrap().by_slot.get_mut(&slot) {
                    Some(r) if r.gen == gen => {
                        r.state = RouteState::Attaching(seq);
                        true
                    }
                    _ => false,
                }
            })
            .await
    }

    /// Removes the route made by `route` call `gen`. Returns false when a
    /// newer route for the slot has replaced it (and so was left alone).
    pub fn unroute(&self, slot: SlotId, gen: u64) -> bool {
        let mut r = self.routes.lock().unwrap();
        match r.by_slot.get(&slot) {
            Some(route) if route.gen == gen => {
                r.by_slot.remove(&slot);
                true
            }
            _ => false,
        }
    }
}

/// Opens the Overseer window on the board. Blocks until the app quits.
/// `runtime` must keep running for as long as the window is open; `client`
/// must belong to it.
pub fn run_gui(client: Arc<Client>, runtime: Handle) {
    Application::new().run(move |cx: &mut App| {
        cx.set_global(FrameHub::start(&client, runtime));
        app::install(cx);
        notify::install();
        let theme = Theme::load();
        let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Overseer".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| app::OverseerApp::new(client.clone(), theme, window, cx)),
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
            events: Arc::default(),
        };
        (rt, hub)
    }

    fn out(id: SlotId, data: &'static [u8]) -> Frame {
        Frame::Output {
            id,
            data: Bytes::from_static(data),
        }
    }

    fn set_state(hub: &FrameHub, slot: SlotId, state: RouteState) {
        hub.routes
            .lock()
            .unwrap()
            .by_slot
            .get_mut(&slot)
            .unwrap()
            .state = state;
    }

    #[test]
    fn newer_route_survives_older_unroute() {
        let (_rt, hub) = hub();
        let (old, _a) = hub.route(1);
        let (new, mut b) = hub.route(1);
        assert!(!hub.unroute(1, old), "old route was already replaced");
        set_state(&hub, 1, RouteState::Live);
        hub.routes.lock().unwrap().dispatch(out(1, b"x"));
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

    #[test]
    fn output_before_own_attach_reply_is_dropped() {
        let (_rt, hub) = hub();
        let (_, mut rx) = hub.route(1);
        let mut r = hub.routes.lock().unwrap();
        r.dispatch(out(1, b"idle"));
        r.by_slot.get_mut(&1).unwrap().state = RouteState::Attaching(7);
        r.dispatch(out(1, b"stale"));
        // Someone else's reply does not open the route.
        let other = Frame::Response {
            seq: 6,
            resp: Response::Ok,
        };
        assert!(
            r.dispatch(other).is_some(),
            "unclaimed reply goes to the app"
        );
        r.dispatch(out(1, b"still stale"));
        let own = Frame::Response {
            seq: 7,
            resp: Response::Ok,
        };
        assert!(r.dispatch(own).is_none());
        r.dispatch(out(1, b"snapshot"));
        r.dispatch(out(2, b"other slot"));
        drop(r);
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"snapshot"));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn failed_attach_stays_gated() {
        let (_rt, hub) = hub();
        let (_, mut rx) = hub.route(3);
        set_state(&hub, 3, RouteState::Attaching(1));
        let mut r = hub.routes.lock().unwrap();
        let err = Frame::Response {
            seq: 1,
            resp: Response::Error {
                message: "S3 is empty".into(),
            },
        };
        assert!(r.dispatch(err).is_none());
        r.dispatch(out(3, b"x"));
        drop(r);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn events_go_to_the_app() {
        let (_rt, hub) = hub();
        let ev = Frame::Event(overseer_proto::Event::SlotChanged(
            overseer_proto::SlotInfo {
                id: 1,
                label: String::new(),
                cwd: None,
                command: None,
                state: overseer_proto::SlotState::Empty,
                cols: 80,
                rows: 24,
                activity: None,
            },
        ));
        assert_eq!(hub.routes.lock().unwrap().dispatch(ev.clone()), Some(ev));
    }
}
