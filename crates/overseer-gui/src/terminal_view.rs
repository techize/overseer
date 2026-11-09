//! Embedded terminal view. Renders a `TermBridge` fed from the daemon and
//! turns keyboard, wheel and focus input into PTY bytes.
//!
//! Threading: each view has a pump on its own thread (with a current-thread
//! tokio runtime for its timer). It owns the attach protocol and feeds Output
//! frames into the bridge (behind a mutex), then wakes the GPUI side, which calls `cx.notify()`. A dirty flag keeps at most one wake in
//! flight, so redraws are coalesced to at most one per frame. Keystrokes are
//! encoded and queued on the GPUI thread; `Client` writes are enqueue-only,
//! so this never blocks on the socket.
//!
//! Parsing is kept off the shared tokio runtime on purpose: that runtime runs
//! the client's socket reader, and if parsing starved it during an output
//! burst the daemon would drop the connection as a slow client.
//!
//! The pure helpers (cell styling, row layout, pixel to grid, scroll
//! accumulation, key and wheel routing) are free functions with unit tests.

use std::cell::Cell as StdCell;
use std::future::Future;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll, Waker};

use bytes::Bytes;
use gpui::{
    canvas, div, fill, font, outline, point, prelude::*, px, size, App, BorderStyle, Bounds,
    Context, Entity, FocusHandle, Focusable, Font, FontStyle, FontWeight, Hsla, KeyDownEvent,
    Keystroke, MouseButton, Pixels, Rgba, ScrollWheelEvent, StrikethroughStyle, Subscription, Task,
    TextRun, TouchPhase, UnderlineStyle, Window,
};
use overseer_proto::{Client, Request, Response, SlotId, SlotState};
use overseer_term::{
    encode_key, encode_mouse, Cell, CursorShape, Key, KeyInput, MouseEvent, MouseInput, MouseMode,
    Rgb, TermBridge, TermModes, DEFAULT_SCROLLBACK,
};
use tokio::sync::mpsc;

use crate::theme::Theme;
use crate::FrameHub;

// ---------------------------------------------------------------------------
// Pure logic
// ---------------------------------------------------------------------------

/// Size of one grid cell in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellMetrics {
    pub width: f32,
    pub height: f32,
}

/// Columns and rows that fit in a `width` x `height` area. At least 1 x 1.
pub fn grid_size(width: f32, height: f32, m: CellMetrics) -> (u16, u16) {
    let fit = |avail: f32, cell: f32| {
        if cell <= 0.0 || !avail.is_finite() {
            return 1;
        }
        (avail / cell).floor().clamp(1.0, u16::MAX as f32) as u16
    };
    (fit(width, m.width), fit(height, m.height))
}

/// The `(row, col)` under a point given relative to the grid origin, clamped
/// to the grid.
pub fn pixel_to_grid(x: f32, y: f32, m: CellMetrics, cols: usize, rows: usize) -> (usize, usize) {
    let idx = |v: f32, cell: f32, n: usize| {
        if n == 0 || cell <= 0.0 {
            return 0;
        }
        ((v / cell).floor().max(0.0) as usize).min(n - 1)
    };
    (idx(y, m.height, rows), idx(x, m.width, cols))
}

/// How far dim text moves from its colour toward the background.
const DIM_TOWARD_BG: f32 = 0.34;

fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let m = |x: u8, y: u8| (x as f32 * (1.0 - t) + y as f32 * t).round() as u8;
    Rgb(m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

/// Effective `(fg, bg)` of a cell: inverse swaps them, dim fades the
/// foreground toward the background.
pub fn cell_colors(cell: &Cell) -> (Rgb, Rgb) {
    let (mut fg, bg) = if cell.inverse {
        (cell.bg, cell.fg)
    } else {
        (cell.fg, cell.bg)
    };
    if cell.dim {
        fg = mix(fg, bg, DIM_TOWARD_BG);
    }
    (fg, bg)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextStyle {
    pub fg: Rgb,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
}

impl TextStyle {
    /// Draws a line across every cell it covers, spaces included.
    pub fn decorated(&self) -> bool {
        self.underline || self.strikethrough
    }
}

/// A run of cells shaped and painted together, starting at `col`.
#[derive(Debug, Clone, PartialEq)]
pub struct TextBatch {
    pub col: usize,
    /// Grid cells covered.
    pub cells: usize,
    pub text: String,
    pub style: TextStyle,
    /// ASCII only, one glyph per cell from the primary font, so glyphs are
    /// snapped to the cell grid. Anything else (non-ASCII, wide, combining)
    /// gets a batch of its own painted at its cell, so a fallback font with
    /// other advances cannot shift its neighbours.
    pub monospace: bool,
}

/// Background to paint behind `len` cells from `col`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BgSpan {
    pub col: usize,
    pub len: usize,
    pub color: Rgb,
}

/// Line-drawing character drawn as quads: arms from the cell centre to each
/// edge. Font glyphs rarely tile across rows, so terminals draw these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoxLines {
    pub left: bool,
    pub right: bool,
    pub up: bool,
    pub down: bool,
    pub heavy: bool,
}

/// Block element drawn as quads: which quarters of the cell are filled
/// (upper left, upper right, lower left, lower right).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quadrants {
    pub ul: bool,
    pub ur: bool,
    pub ll: bool,
    pub lr: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellShape {
    Lines(BoxLines),
    Blocks(Quadrants),
}

/// The quad shape for box-drawing and block characters this view draws
/// itself, or None to draw the glyph from the font. Rounded corners are drawn
/// square; double lines and shades are left to the font.
pub fn cell_shape(ch: char) -> Option<CellShape> {
    let l = |left, right, up, down, heavy| {
        Some(CellShape::Lines(BoxLines {
            left,
            right,
            up,
            down,
            heavy,
        }))
    };
    let q = |ul, ur, ll, lr| Some(CellShape::Blocks(Quadrants { ul, ur, ll, lr }));
    let (t, f) = (true, false);
    match ch {
        '\u{2500}' => l(t, t, f, f, f),
        '\u{2501}' => l(t, t, f, f, t),
        '\u{2502}' => l(f, f, t, t, f),
        '\u{2503}' => l(f, f, t, t, t),
        '\u{250c}' | '\u{256d}' => l(f, t, f, t, f),
        '\u{250f}' => l(f, t, f, t, t),
        '\u{2510}' | '\u{256e}' => l(t, f, f, t, f),
        '\u{2513}' => l(t, f, f, t, t),
        '\u{2514}' | '\u{2570}' => l(f, t, t, f, f),
        '\u{2517}' => l(f, t, t, f, t),
        '\u{2518}' | '\u{256f}' => l(t, f, t, f, f),
        '\u{251b}' => l(t, f, t, f, t),
        '\u{251c}' => l(f, t, t, t, f),
        '\u{2523}' => l(f, t, t, t, t),
        '\u{2524}' => l(t, f, t, t, f),
        '\u{252b}' => l(t, f, t, t, t),
        '\u{252c}' => l(t, t, f, t, f),
        '\u{2533}' => l(t, t, f, t, t),
        '\u{2534}' => l(t, t, t, f, f),
        '\u{253b}' => l(t, t, t, f, t),
        '\u{253c}' => l(t, t, t, t, f),
        '\u{254b}' => l(t, t, t, t, t),
        '\u{2574}' => l(t, f, f, f, f),
        '\u{2575}' => l(f, f, t, f, f),
        '\u{2576}' => l(f, t, f, f, f),
        '\u{2577}' => l(f, f, f, t, f),
        '\u{2580}' => q(t, t, f, f),
        '\u{2584}' => q(f, f, t, t),
        '\u{2588}' => q(t, t, t, t),
        '\u{258c}' => q(t, f, t, f),
        '\u{2590}' => q(f, t, f, t),
        '\u{2596}' => q(f, f, t, f),
        '\u{2597}' => q(f, f, f, t),
        '\u{2598}' => q(t, f, f, f),
        '\u{2599}' => q(t, f, t, t),
        '\u{259a}' => q(t, f, f, t),
        '\u{259b}' => q(t, t, t, f),
        '\u{259c}' => q(t, t, f, t),
        '\u{259d}' => q(f, t, f, f),
        '\u{259e}' => q(f, t, t, f),
        '\u{259f}' => q(f, t, t, t),
        _ => None,
    }
}

/// A box or block character at `col`, drawn in `color`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShapeCell {
    pub col: usize,
    pub shape: CellShape,
    pub color: Rgb,
}

/// What to paint for one grid row.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RowLayout {
    /// Backgrounds, only where they differ from the default.
    pub spans: Vec<BgSpan>,
    pub batches: Vec<TextBatch>,
    pub shapes: Vec<ShapeCell>,
}

/// Splits one grid row into background spans, text batches of equal style
/// and self-drawn box and block characters.
pub fn layout_row(cells: &[Cell], default_bg: Rgb) -> RowLayout {
    let mut out = RowLayout::default();
    let mut col = 0;
    while col < cells.len() {
        let c = &cells[col];
        let width = if c.wide { 2.min(cells.len() - col) } else { 1 };
        let (fg, bg) = cell_colors(c);

        if bg != default_bg {
            match out.spans.last_mut() {
                Some(s) if s.color == bg && s.col + s.len == col => s.len += width,
                _ => out.spans.push(BgSpan {
                    col,
                    len: width,
                    color: bg,
                }),
            }
        }

        let decorated = c.underline || c.strikethrough;
        let plain_space = (c.ch == ' ' || c.ch == '\0') && c.zerowidth.is_none() && !decorated;
        let shape = if c.hidden || c.zerowidth.is_some() || decorated {
            None
        } else {
            cell_shape(c.ch)
        };
        if let Some(shape) = shape {
            out.shapes.push(ShapeCell {
                col,
                shape,
                color: fg,
            });
        } else if c.hidden || plain_space {
            // Invisible: may join a neighbouring batch to save shaping calls,
            // but never a decorated one, or its line would run under this cell.
            if let Some(b) = out.batches.last_mut() {
                if b.monospace && !b.style.decorated() && b.col + b.cells == col && width == 1 {
                    b.text.push(' ');
                    b.cells += 1;
                }
            }
        } else {
            let style = TextStyle {
                fg,
                bold: c.bold,
                italic: c.italic,
                underline: c.underline,
                strikethrough: c.strikethrough,
            };
            let ch = if c.ch == '\0' { ' ' } else { c.ch };
            let simple = width == 1 && c.zerowidth.is_none() && ch.is_ascii();
            match out.batches.last_mut() {
                Some(b) if simple && b.monospace && b.style == style && b.col + b.cells == col => {
                    b.text.push(ch);
                    b.cells += 1;
                }
                _ => {
                    let mut text = String::from(ch);
                    if let Some(z) = &c.zerowidth {
                        text.extend(z.iter());
                    }
                    out.batches.push(TextBatch {
                        col,
                        cells: width,
                        text,
                        style,
                        monospace: simple,
                    });
                }
            }
        }
        col += width;
    }
    // Trailing spaces in an undecorated batch draw nothing; in a decorated
    // one they carry the underline or strikethrough, so they stay.
    for b in &mut out.batches {
        while b.monospace && !b.style.decorated() && b.text.ends_with(' ') && b.cells > 1 {
            b.text.pop();
            b.cells -= 1;
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorPaint {
    Block { filled: bool },
    Beam,
    Underline,
}

/// How to draw the cursor. An unfocused terminal shows a hollow block.
pub fn cursor_paint(shape: CursorShape, focused: bool) -> Option<CursorPaint> {
    match shape {
        CursorShape::Hidden => None,
        _ if !focused => Some(CursorPaint::Block { filled: false }),
        CursorShape::Block => Some(CursorPaint::Block { filled: true }),
        CursorShape::Beam => Some(CursorPaint::Beam),
        CursorShape::Underline => Some(CursorPaint::Underline),
    }
}

/// Turns pixel wheel deltas into whole lines, carrying the remainder so slow
/// trackpad scrolls still move.
#[derive(Debug, Default)]
pub struct ScrollAccumulator {
    remainder: f32,
}

impl ScrollAccumulator {
    /// Positive `delta_px` (and result) scrolls toward older output.
    pub fn add(&mut self, delta_px: f32, line_height: f32) -> i32 {
        if line_height <= 0.0 || !delta_px.is_finite() {
            return 0;
        }
        self.remainder += delta_px / line_height;
        let lines = self.remainder.trunc();
        self.remainder -= lines;
        lines as i32
    }

    pub fn reset(&mut self) {
        self.remainder = 0.0;
    }
}

/// Cap on lines sent per wheel event, so one fling cannot flood the PTY.
const MAX_WHEEL_LINES: u32 = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WheelAction {
    /// Bytes for the PTY (mouse reports or arrow keys).
    Send(Vec<u8>),
    /// Move the local scrollback; positive is toward older output.
    Scrollback(i32),
    Ignore,
}

/// Routes `lines` of wheel movement (positive is up). On the alternate screen
/// the app gets mouse wheel reports when it enabled mouse mode, else arrow
/// keys when alternate scroll (DEC 1007) is on. Otherwise, or with Shift held,
/// the wheel scrolls local scrollback.
pub fn route_wheel(
    lines: i32,
    modes: &TermModes,
    row: usize,
    col: usize,
    shift: bool,
    ctrl: bool,
    alt: bool,
) -> WheelAction {
    if lines == 0 {
        return WheelAction::Ignore;
    }
    let up = lines > 0;
    let n = lines.unsigned_abs().min(MAX_WHEEL_LINES) as usize;
    if modes.alt_screen && !shift {
        if modes.mouse != MouseMode::Off {
            let input = MouseInput {
                event: if up {
                    MouseEvent::ScrollUp
                } else {
                    MouseEvent::ScrollDown
                },
                row,
                col,
                shift: false,
                ctrl,
                alt,
            };
            return match encode_mouse(&input, modes) {
                Some(b) => WheelAction::Send(b.repeat(n)),
                None => WheelAction::Ignore,
            };
        }
        if modes.alternate_scroll {
            let key = KeyInput::new(if up { Key::Up } else { Key::Down });
            return WheelAction::Send(encode_key(&key, modes).repeat(n));
        }
    }
    WheelAction::Scrollback(lines)
}

/// Maps a named GPUI key to a terminal key.
fn named_key(key: &str) -> Option<Key> {
    Some(match key {
        "enter" => Key::Enter,
        "tab" => Key::Tab,
        "backspace" => Key::Backspace,
        "escape" => Key::Escape,
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "insert" => Key::Insert,
        "delete" => Key::Delete,
        "space" => Key::Char(' '),
        f if f.len() >= 2 && f.starts_with('f') => match f[1..].parse::<u8>() {
            Ok(n @ 1..=12) => Key::F(n),
            _ => return None,
        },
        _ => return None,
    })
}

/// Bytes a key press sends to the PTY; empty when it sends nothing (Cmd
/// shortcuts, bare modifiers, unknown keys). Option acts as Meta: it sends
/// ESC plus the unmodified key, not the macOS composed character.
pub fn keystroke_bytes(ks: &Keystroke, modes: &TermModes) -> Vec<u8> {
    let m = &ks.modifiers;
    if m.platform {
        return Vec::new();
    }
    let input = |key| KeyInput {
        key,
        shift: m.shift,
        ctrl: m.control,
        alt: m.alt,
        cmd: false,
    };
    if let Some(key) = named_key(&ks.key) {
        // Shift+Space is a plain space.
        if key == Key::Char(' ') && !m.control && !m.alt {
            return b" ".to_vec();
        }
        return encode_key(&input(key), modes);
    }
    let mut chars = ks.key.chars();
    let (Some(base), None) = (chars.next(), chars.next()) else {
        return Vec::new();
    };
    if m.control || m.alt {
        let ch = if m.shift && base.is_ascii_lowercase() {
            base.to_ascii_uppercase()
        } else {
            base
        };
        return encode_key(
            &KeyInput {
                shift: false,
                ..input(Key::Char(ch))
            },
            modes,
        );
    }
    match &ks.key_char {
        Some(text) if !text.is_empty() => text.as_bytes().to_vec(),
        _ => {
            let ch = if m.shift {
                base.to_ascii_uppercase()
            } else {
                base
            };
            encode_key(&KeyInput::new(Key::Char(ch)), modes)
        }
    }
}

/// The focus report for a focus change, when the app asked for them (DEC 1004).
pub fn focus_report(focused: bool, modes: &TermModes) -> Option<&'static [u8]> {
    modes
        .focus_reporting
        .then_some(if focused { b"\x1b[I" } else { b"\x1b[O" })
}

/// Polls a future once. `Client::send_input` completes on its first poll and
/// `Client::request` enqueues its frame on its first poll (then waits for a
/// reply, which is safe to drop), so this queues a write synchronously, in
/// call order, from any thread.
fn poll_once<F: Future>(fut: F) -> Poll<F::Output> {
    let mut fut = std::pin::pin!(fut);
    fut.as_mut()
        .poll(&mut TaskContext::from_waker(Waker::noop()))
}

// ---------------------------------------------------------------------------
// Pump (tokio side)
// ---------------------------------------------------------------------------

enum Cmd {
    Resize { cols: u16, rows: u16 },
}

/// State shared between the GPUI thread and the pump.
struct Shared {
    bridge: Mutex<TermBridge>,
    /// A wake is in flight; the GPUI side clears it before notifying.
    dirty: AtomicBool,
    /// A message shown over the grid (attach failure, lost connection).
    status: Mutex<Option<String>>,
}

impl Shared {
    fn wake(&self, tx: &mpsc::UnboundedSender<()>) {
        if !self.dirty.swap(true, Ordering::SeqCst) {
            let _ = tx.send(());
        }
    }

    fn flush_writes(&self, client: &Client, slot: SlotId) {
        let writes = self.bridge.lock().unwrap().take_pty_writes();
        if !writes.is_empty() {
            let _ = poll_once(client.send_input(slot, &writes));
        }
    }
}

/// Largest slice fed under one lock, so a 4 MiB replay does not stall paint.
const FEED_CHUNK: usize = 64 * 1024;
/// Bytes of already-queued output folded into one wake.
const COALESCE_BUDGET: usize = 1024 * 1024;

fn feed(shared: &Shared, data: &[u8]) {
    for chunk in data.chunks(FEED_CHUNK) {
        shared.bridge.lock().unwrap().feed(chunk);
    }
}

/// Sizes the bridge to the slot's PTY (so the replay renders as it was
/// drawn), then attaches. Returns a message for the user on failure.
async fn attach(slot: SlotId, client: &Client, shared: &Shared) -> Result<(), String> {
    let lost = |e: anyhow::Error| format!("Lost the daemon: {e:#}");
    match client.request(Request::ListSlots).await.map_err(lost)? {
        Response::Slots(slots) => {
            let info = slots
                .into_iter()
                .find(|s| s.id == slot)
                .ok_or_else(|| format!("S{slot} does not exist."))?;
            if info.state == SlotState::Empty {
                return Err(format!(
                    "S{slot} is empty. Start it with: overseer ctl start {slot} --cwd <dir>"
                ));
            }
            if info.cols > 0 && info.rows > 0 {
                shared
                    .bridge
                    .lock()
                    .unwrap()
                    .resize(info.cols as usize, info.rows as usize);
            }
        }
        Response::Error { message } => return Err(message),
        other => return Err(format!("Unexpected reply: {other:?}")),
    }
    match client
        .request(Request::Attach { id: slot })
        .await
        .map_err(lost)?
    {
        Response::Ok => Ok(()),
        Response::Error { message } => Err(message),
        other => Err(format!("Unexpected reply: {other:?}")),
    }
}

fn apply_resize(slot: SlotId, client: &Client, shared: &Shared, cols: u16, rows: u16) {
    shared
        .bridge
        .lock()
        .unwrap()
        .resize(cols as usize, rows as usize);
    let _ = poll_once(client.request(Request::Resize {
        id: slot,
        cols,
        rows,
    }));
}

async fn pump(
    slot: SlotId,
    client: Arc<Client>,
    shared: Arc<Shared>,
    mut output: mpsc::UnboundedReceiver<Bytes>,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    wake: mpsc::UnboundedSender<()>,
) {
    if let Err(msg) = attach(slot, &client, &shared).await {
        *shared.status.lock().unwrap() = Some(msg);
        shared.wake(&wake);
        return;
    }
    // The first Output after Attach is the ring snapshot. Resizes wait until
    // it is fed, so it replays at the size it was drawn at.
    let mut replayed = false;
    let mut pending: Option<(u16, u16)> = None;
    loop {
        let deadline = shared.bridge.lock().unwrap().sync_deadline();
        let sleep_until =
            tokio::time::Instant::from_std(deadline.unwrap_or_else(std::time::Instant::now));
        tokio::select! {
            cmd = cmds.recv() => match cmd {
                // The view is gone.
                None => return,
                Some(Cmd::Resize { cols, rows }) => {
                    if replayed {
                        apply_resize(slot, &client, &shared, cols, rows);
                        shared.wake(&wake);
                    } else {
                        pending = Some((cols, rows));
                    }
                }
            },
            data = output.recv() => match data {
                None => {
                    *shared.status.lock().unwrap() = Some(if client.inbox_overflowed() {
                        "The view fell too far behind the daemon, so the connection was dropped."
                            .into()
                    } else {
                        "Lost the connection to the daemon.".into()
                    });
                    shared.wake(&wake);
                    return;
                }
                Some(data) => {
                    feed(&shared, &data);
                    let mut budget = COALESCE_BUDGET;
                    while budget > 0 {
                        let Ok(more) = output.try_recv() else { break };
                        feed(&shared, &more);
                        budget = budget.saturating_sub(more.len());
                    }
                    shared.flush_writes(&client, slot);
                    if !replayed {
                        replayed = true;
                        if let Some((cols, rows)) = pending.take() {
                            apply_resize(slot, &client, &shared, cols, rows);
                        }
                    }
                    shared.wake(&wake);
                }
            },
            _ = tokio::time::sleep_until(sleep_until), if deadline.is_some() => {
                shared.bridge.lock().unwrap().tick();
                shared.flush_writes(&client, slot);
                shared.wake(&wake);
            }
        }
    }
}

/// Runs `pump` on a thread of its own. It ends when the view drops its
/// command sender or the connection goes.
fn spawn_pump(
    slot: SlotId,
    client: Arc<Client>,
    shared: Arc<Shared>,
    output: mpsc::UnboundedReceiver<Bytes>,
    cmds: mpsc::UnboundedReceiver<Cmd>,
    wake: mpsc::UnboundedSender<()>,
) {
    let fail = shared.clone();
    let fail_wake = wake.clone();
    let started = std::thread::Builder::new()
        .name(format!("overseer-term-S{slot}"))
        .spawn(move || {
            match tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
            {
                Ok(rt) => rt.block_on(pump(slot, client, shared, output, cmds, wake)),
                Err(e) => {
                    *shared.status.lock().unwrap() = Some(format!("Cannot start terminal: {e}"));
                    shared.wake(&wake);
                }
            }
        });
    if let Err(e) = started {
        *fail.status.lock().unwrap() = Some(format!("Cannot start terminal: {e}"));
        fail.wake(&fail_wake);
    }
}

// ---------------------------------------------------------------------------
// View (GPUI side)
// ---------------------------------------------------------------------------

/// Where the grid was last laid out, for mapping wheel positions to cells.
#[derive(Debug, Clone, Copy, Default)]
struct Geometry {
    origin: (f32, f32),
    cols: usize,
    rows: usize,
    /// Last size sent to the pump.
    requested: Option<(u16, u16)>,
}

pub struct TerminalView {
    slot: SlotId,
    client: Arc<Client>,
    hub: FrameHub,
    route_gen: u64,
    theme: Theme,
    focus_handle: FocusHandle,
    shared: Arc<Shared>,
    cmds: mpsc::UnboundedSender<Cmd>,
    metrics: Option<(CellMetrics, f32)>,
    geometry: Rc<StdCell<Geometry>>,
    scroll: ScrollAccumulator,
    focus_reported: bool,
    _subscriptions: Vec<Subscription>,
    _notifier: Task<()>,
}

fn hsla(c: Rgb) -> Hsla {
    Rgba {
        r: c.0 as f32 / 255.0,
        g: c.1 as f32 / 255.0,
        b: c.2 as f32 / 255.0,
        a: 1.0,
    }
    .into()
}

impl TerminalView {
    /// Creates a view for `slot` and attaches to it. Needs the `FrameHub`
    /// global. Dropping the view detaches.
    pub fn new(
        slot: SlotId,
        client: Arc<Client>,
        theme: Theme,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<TerminalView> {
        cx.new(|cx| {
            let hub = cx.global::<FrameHub>().clone();
            let (route_gen, output) = hub.route(slot);
            let mut bridge = TermBridge::new(80, 24, DEFAULT_SCROLLBACK);
            bridge.set_palette(theme.palette.clone());
            let shared = Arc::new(Shared {
                bridge: Mutex::new(bridge),
                dirty: AtomicBool::new(false),
                status: Mutex::new(None),
            });
            let (cmds, cmd_rx) = mpsc::unbounded_channel();
            let (wake_tx, mut wake_rx) = mpsc::unbounded_channel::<()>();
            spawn_pump(
                slot,
                client.clone(),
                shared.clone(),
                output,
                cmd_rx,
                wake_tx,
            );
            let notifier = cx.spawn(async move |this, cx| {
                while wake_rx.recv().await.is_some() {
                    let alive = this.update(cx, |this: &mut TerminalView, cx| {
                        this.shared.dirty.store(false, Ordering::SeqCst);
                        cx.notify();
                    });
                    if alive.is_err() {
                        break;
                    }
                }
            });
            let focus_handle = cx.focus_handle();
            let subscriptions = vec![
                cx.on_focus_in(&focus_handle, window, |this, window, cx| {
                    this.sync_focus(window, cx)
                }),
                cx.on_focus_out(&focus_handle, window, |this, _, window, cx| {
                    this.sync_focus(window, cx)
                }),
                cx.observe_window_activation(window, |this, window, cx| {
                    this.sync_focus(window, cx)
                }),
            ];
            TerminalView {
                slot,
                client,
                hub,
                route_gen,
                theme,
                focus_handle,
                shared,
                cmds,
                metrics: None,
                geometry: Rc::default(),
                scroll: ScrollAccumulator::default(),
                focus_reported: false,
                _subscriptions: subscriptions,
                _notifier: notifier,
            }
        })
    }

    pub fn focus(&self, window: &mut Window) {
        window.focus(&self.focus_handle);
    }

    fn send(&self, bytes: &[u8]) {
        // An error means the connection is gone; the pump reports that.
        let _ = poll_once(self.client.send_input(self.slot, bytes));
    }

    /// Sends a focus report when the effective focus (view focused and window
    /// active) changes.
    fn sync_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focused = self.focus_handle.is_focused(window) && window.is_window_active();
        if focused != self.focus_reported {
            self.focus_reported = focused;
            let modes = self.shared.bridge.lock().unwrap().modes();
            if let Some(report) = focus_report(focused, &modes) {
                self.send(report);
            }
        }
        cx.notify();
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let bytes = {
            let mut bridge = self.shared.bridge.lock().unwrap();
            let bytes = keystroke_bytes(&ev.keystroke, &bridge.modes());
            if !bytes.is_empty() {
                bridge.scroll_to_bottom();
            }
            bytes
        };
        if bytes.is_empty() {
            return;
        }
        self.send(&bytes);
        cx.stop_propagation();
        cx.notify();
    }

    fn on_scroll_wheel(&mut self, ev: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some((m, _)) = self.metrics else { return };
        if matches!(ev.touch_phase, TouchPhase::Started) {
            self.scroll.reset();
        }
        let dy: f32 = ev.delta.pixel_delta(px(m.height)).y.into();
        let lines = self.scroll.add(dy, m.height);
        if lines == 0 {
            return;
        }
        let g = self.geometry.get();
        let (x, y): (f32, f32) = (ev.position.x.into(), ev.position.y.into());
        let (row, col) = pixel_to_grid(x - g.origin.0, y - g.origin.1, m, g.cols, g.rows);
        let mods = &ev.modifiers;
        let mut bridge = self.shared.bridge.lock().unwrap();
        let action = route_wheel(
            lines,
            &bridge.modes(),
            row,
            col,
            mods.shift,
            mods.control,
            mods.alt,
        );
        match action {
            WheelAction::Send(bytes) => {
                drop(bridge);
                self.send(&bytes);
            }
            WheelAction::Scrollback(n) => {
                bridge.scroll(n);
                cx.notify();
            }
            WheelAction::Ignore => {}
        }
        cx.stop_propagation();
    }

    /// Cell size from the font: width is the advance of `M`. Tells the bridge
    /// the device-pixel cell size whenever it changes.
    fn update_metrics(&mut self, window: &Window) -> CellMetrics {
        let ts = window.text_system();
        let id = ts.resolve_font(&font(self.theme.font_family.clone()));
        let size = px(self.theme.font_size);
        let width: f32 = ts
            .advance(id, size, 'M')
            .map(|s| s.width.into())
            .unwrap_or(self.theme.font_size * 0.6);
        let ascent: f32 = ts.ascent(id, size).into();
        let descent: f32 = ts.descent(id, size).into();
        let m = CellMetrics {
            width,
            height: (ascent + descent.abs()).ceil(),
        };
        let scale = window.scale_factor();
        if self.metrics != Some((m, scale)) {
            self.metrics = Some((m, scale));
            let dev = |v: f32| (v * scale).round().clamp(1.0, u16::MAX as f32) as u16;
            self.shared
                .bridge
                .lock()
                .unwrap()
                .set_cell_size(dev(m.width), dev(m.height));
        }
        m
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Drop for TerminalView {
    fn drop(&mut self) {
        // A newer view for this slot on the same connection owns the stream
        // now; detaching would cut it off.
        if self.hub.unroute(self.slot, self.route_gen) {
            let _ = poll_once(self.client.request(Request::Detach { id: self.slot }));
        }
    }
}

/// Everything paint needs, captured per frame.
struct PaintStyle {
    font: Font,
    font_size: Pixels,
    metrics: CellMetrics,
    background: Rgb,
    focused: bool,
}

impl PaintStyle {
    fn font_for(&self, bold: bool, italic: bool) -> Font {
        let mut f = self.font.clone();
        if bold {
            f.weight = FontWeight::BOLD;
        }
        if italic {
            f.style = FontStyle::Italic;
        }
        f
    }
}

fn paint_text(
    text: &str,
    style: &TextStyle,
    monospace: bool,
    origin: gpui::Point<Pixels>,
    ps: &PaintStyle,
    window: &mut Window,
    cx: &mut App,
) {
    let color = hsla(style.fg);
    let run = TextRun {
        len: text.len(),
        font: ps.font_for(style.bold, style.italic),
        color,
        background_color: None,
        underline: style.underline.then_some(UnderlineStyle {
            thickness: px(1.),
            color: Some(color),
            wavy: false,
        }),
        strikethrough: style.strikethrough.then_some(StrikethroughStyle {
            thickness: px(1.),
            color: Some(color),
        }),
    };
    let line = window.text_system().shape_line(
        text.to_string().into(),
        ps.font_size,
        &[run],
        monospace.then_some(px(ps.metrics.width)),
    );
    let _ = line.paint(origin, px(ps.metrics.height), window, cx);
}

/// Draws a box or block character filling its cell exactly, so lines join
/// across cells and rows.
fn paint_shape(
    origin: gpui::Point<Pixels>,
    shape: CellShape,
    color: Rgb,
    m: CellMetrics,
    window: &mut Window,
) {
    let (x, y): (f32, f32) = (origin.x.into(), origin.y.into());
    let rect =
        |x: f32, y: f32, w: f32, h: f32| Bounds::new(point(px(x), px(y)), size(px(w), px(h)));
    let color = hsla(color);
    match shape {
        CellShape::Lines(b) => {
            let t = (m.width / 8.0).round().max(1.0) * if b.heavy { 2.0 } else { 1.0 };
            // Snap the stroke to whole pixels so it stays crisp.
            let cx = (x + (m.width - t) / 2.0).round();
            let cy = (y + (m.height - t) / 2.0).round();
            let (right, bottom) = (x + m.width, y + m.height);
            if b.left {
                window.paint_quad(fill(rect(x, cy, cx + t - x, t), color));
            }
            if b.right {
                window.paint_quad(fill(rect(cx, cy, right - cx, t), color));
            }
            if b.up {
                window.paint_quad(fill(rect(cx, y, t, cy + t - y), color));
            }
            if b.down {
                window.paint_quad(fill(rect(cx, cy, t, bottom - cy), color));
            }
        }
        CellShape::Blocks(q) => {
            let (hw, hh) = (m.width / 2.0, m.height / 2.0);
            for (on, dx, dy) in [
                (q.ul, 0.0, 0.0),
                (q.ur, hw, 0.0),
                (q.ll, 0.0, hh),
                (q.lr, hw, hh),
            ] {
                if on {
                    window.paint_quad(fill(rect(x + dx, y + dy, hw, hh), color));
                }
            }
        }
    }
}

fn paint_grid(
    bounds: Bounds<Pixels>,
    shared: &Shared,
    ps: &PaintStyle,
    window: &mut Window,
    cx: &mut App,
) {
    let snap = shared.bridge.lock().unwrap().snapshot();
    let m = ps.metrics;
    let (ox, oy): (f32, f32) = (bounds.origin.x.into(), bounds.origin.y.into());
    let at = |row: usize, col: usize| {
        point(
            px(ox + col as f32 * m.width),
            px(oy + row as f32 * m.height),
        )
    };

    for row in 0..snap.rows {
        let cells = &snap.cells[row * snap.cols..(row + 1) * snap.cols];
        let RowLayout {
            spans,
            batches,
            shapes,
        } = layout_row(cells, ps.background);
        for s in spans {
            window.paint_quad(fill(
                Bounds::new(
                    at(row, s.col),
                    size(px(s.len as f32 * m.width), px(m.height)),
                ),
                hsla(s.color),
            ));
        }
        for b in batches {
            paint_text(
                &b.text,
                &b.style,
                b.monospace,
                at(row, b.col),
                ps,
                window,
                cx,
            );
        }
        for sc in shapes {
            paint_shape(at(row, sc.col), sc.shape, sc.color, m, window);
        }
    }

    let cur = snap.cursor;
    if !cur.visible {
        return;
    }
    let Some(kind) = cursor_paint(cur.shape, ps.focused) else {
        return;
    };
    let Some(cell) = snap.cell(cur.row, cur.col) else {
        return;
    };
    let (fg, bg) = cell_colors(cell);
    let width = if cell.wide { 2.0 } else { 1.0 } * m.width;
    let origin = at(cur.row, cur.col);
    match kind {
        CursorPaint::Block { filled: true } => {
            window.paint_quad(fill(
                Bounds::new(origin, size(px(width), px(m.height))),
                hsla(fg),
            ));
            if !cell.hidden && cell.ch != ' ' && cell.ch != '\0' {
                let mut text = String::from(cell.ch);
                if let Some(z) = &cell.zerowidth {
                    text.extend(z.iter());
                }
                let style = TextStyle {
                    fg: bg,
                    bold: cell.bold,
                    italic: cell.italic,
                    underline: cell.underline,
                    strikethrough: cell.strikethrough,
                };
                paint_text(&text, &style, false, origin, ps, window, cx);
            }
        }
        CursorPaint::Block { filled: false } => {
            window.paint_quad(outline(
                Bounds::new(origin, size(px(width), px(m.height))),
                hsla(fg),
                BorderStyle::Solid,
            ));
        }
        CursorPaint::Beam => {
            window.paint_quad(fill(
                Bounds::new(origin, size(px(2.), px(m.height))),
                hsla(fg),
            ));
        }
        CursorPaint::Underline => {
            let o = point(origin.x, origin.y + px(m.height - 2.));
            window.paint_quad(fill(Bounds::new(o, size(px(width), px(2.))), hsla(fg)));
        }
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let metrics = self.update_metrics(window);
        let ps = PaintStyle {
            font: font(self.theme.font_family.clone()),
            font_size: px(self.theme.font_size),
            metrics,
            background: self.theme.background,
            focused: self.focus_handle.is_focused(window),
        };
        let status = self.shared.status.lock().unwrap().clone();
        let shared = self.shared.clone();
        let cmds = self.cmds.clone();
        let geometry = self.geometry.clone();

        let grid = canvas(
            move |bounds, _, _| {
                let (w, h): (f32, f32) = (bounds.size.width.into(), bounds.size.height.into());
                let (cols, rows) = grid_size(w, h, metrics);
                let mut g = geometry.get();
                g.origin = (bounds.origin.x.into(), bounds.origin.y.into());
                g.cols = cols as usize;
                g.rows = rows as usize;
                if g.requested != Some((cols, rows)) {
                    g.requested = Some((cols, rows));
                    let _ = cmds.send(Cmd::Resize { cols, rows });
                }
                geometry.set(g);
            },
            move |bounds, (), window, cx| paint_grid(bounds, &shared, &ps, window, cx),
        )
        .size_full();

        div()
            .key_context("Terminal")
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .bg(hsla(self.theme.background))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, _| window.focus(&this.focus_handle)),
            )
            .child(grid)
            .children(status.map(|s| {
                div()
                    .absolute()
                    .top_2()
                    .left_2()
                    .px_2()
                    .py_1()
                    .bg(hsla(Rgb(0x30, 0x30, 0x30)))
                    .text_color(hsla(Rgb(0xff, 0xcc, 0x66)))
                    .child(s)
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Modifiers;

    const FG: Rgb = Rgb(229, 229, 229);
    const BG: Rgb = Rgb(0, 0, 0);

    fn cell(ch: char) -> Cell {
        Cell {
            ch,
            zerowidth: None,
            fg: FG,
            bg: BG,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            strikethrough: false,
            hidden: false,
            inverse: false,
            wide: false,
        }
    }

    fn row(s: &str) -> Vec<Cell> {
        s.chars().map(cell).collect()
    }

    const M: CellMetrics = CellMetrics {
        width: 8.0,
        height: 16.0,
    };

    #[test]
    fn grid_size_floors_and_clamps() {
        assert_eq!(grid_size(800.0, 480.0, M), (100, 30));
        assert_eq!(grid_size(807.9, 495.0, M), (100, 30));
        assert_eq!(grid_size(3.0, 3.0, M), (1, 1));
        assert_eq!(grid_size(0.0, 0.0, M), (1, 1));
    }

    #[test]
    fn pixel_to_grid_maps_and_clamps() {
        assert_eq!(pixel_to_grid(0.0, 0.0, M, 80, 24), (0, 0));
        assert_eq!(pixel_to_grid(17.0, 33.0, M, 80, 24), (2, 2));
        assert_eq!(pixel_to_grid(-5.0, -5.0, M, 80, 24), (0, 0));
        assert_eq!(pixel_to_grid(10_000.0, 10_000.0, M, 80, 24), (23, 79));
    }

    #[test]
    fn inverse_swaps_and_dim_fades() {
        let mut c = cell('x');
        c.fg = Rgb(255, 0, 0);
        c.bg = Rgb(0, 0, 255);
        c.inverse = true;
        assert_eq!(cell_colors(&c), (Rgb(0, 0, 255), Rgb(255, 0, 0)));
        let mut d = cell('x');
        d.fg = Rgb(255, 255, 255);
        d.dim = true;
        let (fg, bg) = cell_colors(&d);
        assert_eq!(bg, BG);
        assert!(fg.0 < 255 && fg.0 > 128, "dimmed to {fg:?}");
    }

    #[test]
    fn same_style_batches_join_across_spaces() {
        let RowLayout { spans, batches, .. } = layout_row(&row("hello world   "), BG);
        assert!(spans.is_empty());
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].text, "hello world");
        assert_eq!((batches[0].col, batches[0].cells), (0, 11));
        assert!(batches[0].monospace);
    }

    #[test]
    fn style_change_splits_batches() {
        let mut cells = row("abcd");
        cells[2].bold = true;
        cells[3].bold = true;
        let b = layout_row(&cells, BG).batches;
        assert_eq!(b.len(), 2);
        assert_eq!((b[0].text.as_str(), b[1].text.as_str()), ("ab", "cd"));
        assert_eq!(b[1].col, 2);
        assert!(b[1].style.bold);
    }

    #[test]
    fn backgrounds_merge_and_skip_default() {
        let mut cells = row("  x  ");
        for c in &mut cells[1..4] {
            c.bg = Rgb(1, 2, 3);
        }
        let spans = layout_row(&cells, BG).spans;
        assert_eq!(
            spans,
            vec![BgSpan {
                col: 1,
                len: 3,
                color: Rgb(1, 2, 3)
            }]
        );
    }

    #[test]
    fn inverse_space_paints_background() {
        let mut cells = row(" ");
        cells[0].inverse = true;
        let RowLayout { spans, batches, .. } = layout_row(&cells, BG);
        assert_eq!(spans[0].color, FG);
        assert!(batches.is_empty());
    }

    #[test]
    fn wide_char_is_its_own_two_cell_batch() {
        let mut cells = row("a\u{4e2d} b");
        cells[1].wide = true;
        let b = layout_row(&cells, BG).batches;
        assert_eq!(b.len(), 3);
        assert_eq!((b[1].col, b[1].cells, b[1].monospace), (1, 2, false));
        assert_eq!(b[2].col, 3);
        assert_eq!(b[2].text, "b");
    }

    #[test]
    fn combining_marks_kept_with_their_cell() {
        let mut cells = row("ex");
        cells[0].zerowidth = Some(vec!['\u{301}']);
        let b = layout_row(&cells, BG).batches;
        assert_eq!(b[0].text, "e\u{301}");
        assert!(!b[0].monospace);
        assert_eq!(b[1].text, "x");
    }

    /// Words decorated, the gap between them not: the gap must stay bare.
    fn gap_layout(decorate: fn(&mut Cell), hide_gap: bool) -> Vec<TextBatch> {
        let mut cells = row("foo bar");
        for (i, c) in cells.iter_mut().enumerate() {
            if i == 3 {
                if hide_gap {
                    c.ch = 'x';
                    c.hidden = true;
                }
            } else {
                decorate(c);
            }
        }
        layout_row(&cells, BG).batches
    }

    fn assert_gap_bare(b: &[TextBatch]) {
        let texts: Vec<_> = b
            .iter()
            .map(|b| (b.text.as_str(), b.col, b.cells))
            .collect();
        assert_eq!(texts, vec![("foo", 0, 3), ("bar", 4, 3)]);
    }

    #[test]
    fn underline_does_not_bleed_across_plain_space() {
        let b = gap_layout(|c| c.underline = true, false);
        assert_gap_bare(&b);
        assert!(b.iter().all(|b| b.style.underline));
    }

    #[test]
    fn strikethrough_does_not_bleed_across_plain_space() {
        let b = gap_layout(|c| c.strikethrough = true, false);
        assert_gap_bare(&b);
        assert!(b.iter().all(|b| b.style.strikethrough));
    }

    #[test]
    fn decoration_does_not_bleed_across_hidden_cell() {
        assert_gap_bare(&gap_layout(|c| c.underline = true, true));
        assert_gap_bare(&gap_layout(|c| c.strikethrough = true, true));
    }

    #[test]
    fn underlined_trailing_spaces_keep_decoration() {
        let mut cells = row("ab  ");
        for c in &mut cells {
            c.underline = true;
        }
        let b = layout_row(&cells, BG).batches;
        assert_eq!(b.len(), 1);
        assert_eq!((b[0].text.as_str(), b[0].col, b[0].cells), ("ab  ", 0, 4));
        assert!(b[0].style.underline);
    }

    #[test]
    fn hidden_text_not_drawn_but_underlined_space_is() {
        let mut cells = row("ab ");
        cells[0].hidden = true;
        cells[2].underline = true;
        let b = layout_row(&cells, BG).batches;
        assert_eq!(b.len(), 2);
        assert_eq!((b[0].text.as_str(), b[0].col), ("b", 1));
        assert_eq!((b[1].text.as_str(), b[1].col), (" ", 2));
        assert!(b[1].style.underline);
    }

    #[test]
    fn non_ascii_gets_its_own_batch() {
        let b = layout_row(&row("ab\u{e9}cd"), BG).batches;
        let parts: Vec<_> = b
            .iter()
            .map(|b| (b.col, b.text.as_str(), b.monospace))
            .collect();
        assert_eq!(
            parts,
            vec![(0, "ab", true), (2, "\u{e9}", false), (3, "cd", true)]
        );
    }

    #[test]
    fn box_and_block_chars_are_drawn_not_shaped() {
        let r = layout_row(&row("\u{256d}\u{2500}x\u{2588}"), BG);
        assert_eq!(r.batches.len(), 1);
        assert_eq!((r.batches[0].col, r.batches[0].text.as_str()), (2, "x"));
        let cols: Vec<_> = r.shapes.iter().map(|s| s.col).collect();
        assert_eq!(cols, vec![0, 1, 3]);
        assert_eq!(r.shapes[0].color, FG);
        assert_eq!(
            cell_shape('\u{256d}'),
            Some(CellShape::Lines(BoxLines {
                left: false,
                right: true,
                up: false,
                down: true,
                heavy: false
            }))
        );
        assert_eq!(
            cell_shape('\u{259b}'),
            Some(CellShape::Blocks(Quadrants {
                ul: true,
                ur: true,
                ll: true,
                lr: false
            }))
        );
        assert_eq!(cell_shape('\u{2550}'), None, "double lines use the font");
        assert_eq!(cell_shape('a'), None);
    }

    #[test]
    fn hidden_box_char_not_drawn() {
        let mut cells = row("\u{2500}");
        cells[0].hidden = true;
        let r = layout_row(&cells, BG);
        assert!(r.shapes.is_empty() && r.batches.is_empty());
    }

    #[test]
    fn cursor_shapes() {
        assert_eq!(
            cursor_paint(CursorShape::Block, true),
            Some(CursorPaint::Block { filled: true })
        );
        assert_eq!(
            cursor_paint(CursorShape::Beam, true),
            Some(CursorPaint::Beam)
        );
        assert_eq!(
            cursor_paint(CursorShape::Beam, false),
            Some(CursorPaint::Block { filled: false })
        );
        assert_eq!(cursor_paint(CursorShape::Hidden, true), None);
    }

    #[test]
    fn scroll_accumulates_fractions() {
        let mut s = ScrollAccumulator::default();
        assert_eq!(s.add(6.0, 16.0), 0);
        assert_eq!(s.add(6.0, 16.0), 0);
        assert_eq!(s.add(6.0, 16.0), 1);
        assert_eq!(s.add(-40.0, 16.0), -2);
        s.reset();
        assert_eq!(s.add(15.0, 16.0), 0);
        assert_eq!(s.add(1.0, 16.0), 1);
    }

    fn alt() -> TermModes {
        TermModes {
            alt_screen: true,
            alternate_scroll: true,
            ..TermModes::default()
        }
    }

    #[test]
    fn wheel_on_main_screen_scrolls_back() {
        let modes = TermModes {
            alternate_scroll: true,
            mouse: MouseMode::Click,
            ..TermModes::default()
        };
        assert_eq!(
            route_wheel(3, &modes, 0, 0, false, false, false),
            WheelAction::Scrollback(3)
        );
        assert_eq!(
            route_wheel(0, &modes, 0, 0, false, false, false),
            WheelAction::Ignore
        );
    }

    #[test]
    fn wheel_on_alt_screen_sends_arrows() {
        assert_eq!(
            route_wheel(2, &alt(), 0, 0, false, false, false),
            WheelAction::Send(b"\x1b[A\x1b[A".to_vec())
        );
        let app = TermModes {
            app_cursor: true,
            ..alt()
        };
        assert_eq!(
            route_wheel(-1, &app, 0, 0, false, false, false),
            WheelAction::Send(b"\x1bOB".to_vec())
        );
        let off = TermModes {
            alternate_scroll: false,
            ..alt()
        };
        assert_eq!(
            route_wheel(1, &off, 0, 0, false, false, false),
            WheelAction::Scrollback(1)
        );
    }

    #[test]
    fn wheel_with_mouse_mode_reports_mouse() {
        let modes = TermModes {
            mouse: MouseMode::Click,
            sgr_mouse: true,
            ..alt()
        };
        assert_eq!(
            route_wheel(1, &modes, 4, 9, false, false, false),
            WheelAction::Send(b"\x1b[<64;10;5M".to_vec())
        );
        // Shift keeps the wheel local.
        assert_eq!(
            route_wheel(1, &modes, 4, 9, true, false, false),
            WheelAction::Scrollback(1)
        );
    }

    #[test]
    fn wheel_lines_capped() {
        match route_wheel(1000, &alt(), 0, 0, false, false, false) {
            WheelAction::Send(b) => assert_eq!(b.len(), 3 * MAX_WHEEL_LINES as usize),
            other => panic!("{other:?}"),
        }
    }

    fn ks(key: &str, key_char: Option<&str>, f: impl FnOnce(&mut Modifiers)) -> Keystroke {
        let mut modifiers = Modifiers::default();
        f(&mut modifiers);
        Keystroke {
            modifiers,
            key: key.into(),
            key_char: key_char.map(Into::into),
        }
    }

    fn bytes(k: Keystroke) -> Vec<u8> {
        keystroke_bytes(&k, &TermModes::default())
    }

    #[test]
    fn plain_typing_uses_key_char() {
        assert_eq!(bytes(ks("h", Some("h"), |_| {})), b"h");
        assert_eq!(bytes(ks("a", Some("A"), |m| m.shift = true)), b"A");
        assert_eq!(bytes(ks("!", Some("!"), |_| {})), b"!");
        assert_eq!(bytes(ks("space", Some(" "), |m| m.shift = true)), b" ");
    }

    #[test]
    fn named_keys() {
        assert_eq!(bytes(ks("enter", Some("\n"), |_| {})), b"\r");
        assert_eq!(
            bytes(ks("enter", Some("\n"), |m| m.shift = true)),
            b"\x1b\r"
        );
        assert_eq!(bytes(ks("backspace", None, |_| {})), b"\x7f");
        assert_eq!(bytes(ks("escape", None, |_| {})), b"\x1b");
        assert_eq!(bytes(ks("left", None, |m| m.alt = true)), b"\x1b[1;3D");
        assert_eq!(bytes(ks("f5", None, |_| {})), b"\x1b[15~");
        assert!(bytes(ks("f13", None, |_| {})).is_empty());
    }

    #[test]
    fn option_is_meta_not_compose() {
        assert_eq!(bytes(ks("b", Some("\u{222b}"), |m| m.alt = true)), b"\x1bb");
        assert_eq!(
            bytes(ks("b", Some("\u{131}"), |m| {
                m.alt = true;
                m.shift = true
            })),
            b"\x1bB"
        );
    }

    #[test]
    fn ctrl_keys() {
        assert_eq!(bytes(ks("c", None, |m| m.control = true)), b"\x03");
        assert_eq!(bytes(ks("space", None, |m| m.control = true)), b"\x00");
    }

    #[test]
    fn cmd_keys_send_nothing() {
        assert!(bytes(ks("c", None, |m| m.platform = true)).is_empty());
        assert!(bytes(ks("enter", None, |m| m.platform = true)).is_empty());
    }

    #[test]
    fn unknown_keys_send_nothing() {
        assert!(bytes(ks("shift", None, |_| {})).is_empty());
    }

    #[test]
    fn focus_reports_only_when_enabled() {
        let on = TermModes {
            focus_reporting: true,
            ..TermModes::default()
        };
        assert_eq!(focus_report(true, &on), Some(&b"\x1b[I"[..]));
        assert_eq!(focus_report(false, &on), Some(&b"\x1b[O"[..]));
        assert_eq!(focus_report(true, &TermModes::default()), None);
    }

    #[test]
    fn poll_once_completes_ready_futures() {
        assert_eq!(poll_once(async { 7 }), Poll::Ready(7));
        assert!(poll_once(std::future::pending::<()>()).is_pending());
    }
}
