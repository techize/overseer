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
//! Input fidelity: mouse events are reported to the app in mouse mode and
//! otherwise (or with Shift) make a local selection by cell, word or line.
//! Cmd+C copies it, Cmd+V pastes (an image-only clipboard sends Ctrl+V so
//! Claude Code reads the image itself), Cmd+F opens a find bar, Cmd+click
//! opens links and the IME composes dead keys and accents. The selection
//! lives in the terminal, which moves, clamps or drops it as output scrolls,
//! clears and resizes, and is dropped if it would ever read other text than
//! it was made on. Find matches and the hovered link are not kept across
//! output: matches are searched again at most every 200 ms while output
//! streams, and the link is looked up again on the content shown.
//!
//! The pure helpers (cell styling, row layout, pixel to grid, scroll
//! accumulation, key, wheel and mouse routing, paste, find bar state and
//! link commands) are free functions with unit tests.

use std::cell::{Cell as StdCell, RefCell};
use std::ffi::OsString;
use std::future::Future;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll, Waker};
use std::time::{Duration, Instant};

use bytes::Bytes;
use gpui::{
    canvas, div, fill, font, outline, point, prelude::*, px, size, App, BorderStyle, Bounds,
    ClipboardEntry, ClipboardItem, ContentMask, Context, DispatchPhase, ElementInputHandler,
    Entity, EntityInputHandler, FocusHandle, Focusable, Font, FontStyle, FontWeight, Hsla,
    KeyDownEvent, Keystroke, ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Rgba, ScrollWheelEvent, StrikethroughStyle, Subscription, Task, TextRun,
    TouchPhase, UTF16Selection, UnderlineStyle, WeakEntity, Window,
};
use overseer_proto::{Client, Request, Response, SlotId, SlotState};
use overseer_term::{
    encode_key, encode_mouse, encode_paste, find_link_span_at, Cell, CellSide, CursorShape,
    GridPos, Key, KeyInput, Link, LinkSpan, MouseButton as TermButton, MouseEvent, MouseInput,
    MouseMode, Rgb, SelectionKind, Snapshot, TermBridge, TermModes, DEFAULT_SCROLLBACK,
};
use serde::Deserialize;
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
// Pure logic: selection, mouse routing, paste, find, links, composition
// ---------------------------------------------------------------------------

/// Grid line (0 is the top of the live screen, negative in scrollback) of
/// viewport `row`.
pub fn grid_line(row: usize, display_offset: usize) -> i32 {
    row as i32 - display_offset as i32
}

/// Viewport row showing grid `line`, if it is on screen.
#[cfg(test)]
fn viewport_row(line: i32, display_offset: usize, rows: usize) -> Option<usize> {
    let row = line as i64 + display_offset as i64;
    (0..rows as i64).contains(&row).then_some(row as usize)
}

/// Shortest gap between live searches while output streams. Each search
/// walks the whole scrollback under the terminal lock.
pub const SEARCH_REFRESH_INTERVAL: Duration = Duration::from_millis(200);

/// How long to wait before searching again, or None when the matches are
/// current. `dirty` is whether content changed since the last search, made
/// at `last` (None if never). Searches are at least
/// `SEARCH_REFRESH_INTERVAL` apart; the one that follows the last change
/// is the trailing refresh once output settles.
pub fn search_refresh_delay(dirty: bool, last: Option<Instant>, now: Instant) -> Option<Duration> {
    if !dirty {
        return None;
    }
    let since = last.map_or(SEARCH_REFRESH_INTERVAL, |t| {
        now.saturating_duration_since(t)
    });
    Some(SEARCH_REFRESH_INTERVAL.saturating_sub(since))
}

/// Columns of grid `line` covered by the inclusive range `start..=end`.
pub fn span_cols_in_row(
    start: GridPos,
    end: GridPos,
    line: i32,
    cols: usize,
) -> Option<(usize, usize)> {
    if cols == 0 || line < start.line || line > end.line {
        return None;
    }
    let s = if line == start.line { start.col } else { 0 };
    let e = if line == end.line { end.col } else { cols - 1 }.min(cols - 1);
    (s <= e).then_some((s, e))
}

/// How many lines to scroll (positive is toward older output) so that grid
/// `line` is on screen. Zero when it already is.
pub fn scroll_delta_to_show(line: i32, display_offset: usize, history: usize, rows: usize) -> i32 {
    let rows = rows.max(1) as i64;
    let line = line as i64;
    let top = -(display_offset as i64);
    if (top..top + rows).contains(&line) {
        return 0;
    }
    let new_top = if line < top { line } else { line - (rows - 1) };
    let new_offset = (-new_top).clamp(0, history as i64);
    (new_offset - display_offset as i64).clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

/// Which half of its cell an x offset (from the grid origin) falls in.
pub fn cell_side(x: f32, m: CellMetrics) -> CellSide {
    if m.width <= 0.0 || (x / m.width).fract() < 0.5 {
        CellSide::Left
    } else {
        CellSide::Right
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMode {
    /// Cell by cell, as the pointer drags.
    Simple,
    Word,
    Line,
}

impl From<SelectionMode> for SelectionKind {
    fn from(m: SelectionMode) -> SelectionKind {
        match m {
            SelectionMode::Simple => SelectionKind::Simple,
            SelectionMode::Word => SelectionKind::Word,
            SelectionMode::Line => SelectionKind::Line,
        }
    }
}

/// Selection unit for the platform's click count: one click selects cell by
/// cell, a double click by word, a triple (or more) click by line.
pub fn selection_mode(click_count: usize) -> SelectionMode {
    match click_count {
        0 | 1 => SelectionMode::Simple,
        2 => SelectionMode::Word,
        _ => SelectionMode::Line,
    }
}

/// Where a mouse event goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseRoute {
    /// Encoded and sent to the app.
    Report,
    /// Drives the local selection.
    Local,
}

/// Mouse events are reported when the app enabled mouse mode, unless Shift
/// is held, which always keeps them local.
pub fn mouse_route(modes: &TermModes, shift: bool) -> MouseRoute {
    if modes.mouse != MouseMode::Off && !shift {
        MouseRoute::Report
    } else {
        MouseRoute::Local
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownAction {
    OpenLink,
    Report,
    Select(SelectionMode),
    Ignore,
}

/// What a mouse press does. Cmd+click on a link opens it whatever the mouse
/// mode; otherwise the press is reported or starts a local selection.
pub fn mouse_down_action(
    modes: &TermModes,
    button: TermButton,
    shift: bool,
    cmd: bool,
    on_link: bool,
    click_count: usize,
) -> DownAction {
    if cmd && on_link && button == TermButton::Left {
        return DownAction::OpenLink;
    }
    match mouse_route(modes, shift) {
        MouseRoute::Report => DownAction::Report,
        MouseRoute::Local if button == TermButton::Left => {
            DownAction::Select(selection_mode(click_count))
        }
        MouseRoute::Local => DownAction::Ignore,
    }
}

/// Ctrl+V. Claude Code reads an image from the clipboard itself on this key.
pub const IMAGE_PASTE_KEY: u8 = 0x16;

/// Bytes Cmd+V sends. Text (also text alongside an image) goes through
/// `encode_paste`; an image on its own sends Ctrl+V so the app fetches it;
/// an empty clipboard sends nothing.
pub fn paste_bytes(entries: &[ClipboardEntry], modes: &TermModes) -> Option<Vec<u8>> {
    let text: String = entries
        .iter()
        .filter_map(|e| match e {
            ClipboardEntry::String(s) => Some(s.text().as_str()),
            ClipboardEntry::Image(_) => None,
        })
        .collect();
    if !text.is_empty() {
        return Some(encode_paste(&text, modes));
    }
    entries
        .iter()
        .any(|e| matches!(e, ClipboardEntry::Image(_)))
        .then(|| vec![IMAGE_PASTE_KEY])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shortcut {
    Copy,
    Paste,
    Find,
}

/// The Cmd shortcut the terminal handles itself, if this is one.
pub fn shortcut(ks: &Keystroke) -> Option<Shortcut> {
    let m = &ks.modifiers;
    if !m.platform || m.control || m.alt || m.shift || m.function {
        return None;
    }
    match ks.key.as_str() {
        "c" => Some(Shortcut::Copy),
        "v" => Some(Shortcut::Paste),
        "f" => Some(Shortcut::Find),
        _ => None,
    }
}

/// True when Option plus this key starts a dead-key composition. When a
/// key is dead, gpui 0.2.2 re-translates Space with the same modifiers to get
/// a printable form, so key_char is the accent followed by Option+Space
/// (U+00A0 on a US layout): Option+E gives "\u{b4}\u{a0}", Option+I gives
/// "^\u{a0}". Such keys go to the IME so the next key composes (`e` gives
/// `\u{e9}`); other Option keys give one character and still act as Meta.
pub fn starts_composition(ks: &Keystroke) -> bool {
    let m = &ks.modifiers;
    if !m.alt || m.control || m.platform || m.function {
        return false;
    }
    let Some(text) = ks.key_char.as_deref() else {
        return false;
    };
    let mut chars = text.chars();
    matches!(
        (chars.next(), chars.next(), chars.next()),
        (Some(accent), Some(' ' | '\u{a0}'), None) if !accent.is_whitespace()
    )
}

/// A key press while the find bar is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindKey {
    Insert(String),
    Backspace,
    /// Enter: the next match up (older output).
    Older,
    /// Shift+Enter: the next match down (newer output).
    Newer,
    Close,
}

/// Maps a key press to a find bar action. None lets the key through to the
/// terminal (Ctrl keys, arrows and so on).
pub fn find_key(ks: &Keystroke) -> Option<FindKey> {
    let m = &ks.modifiers;
    if m.control || m.platform || m.function {
        return None;
    }
    match ks.key.as_str() {
        "escape" => return Some(FindKey::Close),
        "enter" => {
            return Some(if m.shift {
                FindKey::Newer
            } else {
                FindKey::Older
            })
        }
        "backspace" => return Some(FindKey::Backspace),
        "space" => return Some(FindKey::Insert(" ".into())),
        _ => {}
    }
    match ks.key_char.as_deref() {
        Some(t) if !t.is_empty() && !t.chars().any(char::is_control) => {
            Some(FindKey::Insert(t.to_string()))
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindOutcome {
    /// The query changed: search again and call `set_matches`.
    Search,
    /// The current match changed: scroll it into view.
    Moved,
    Closed,
    Nothing,
}

/// A search match as an inclusive range of grid positions.
pub type FindMatch = (GridPos, GridPos);

/// State of the find bar. Matches are grid positions from the latest search,
/// oldest first; they are not kept across output but searched again (see
/// `refresh`), and none are kept while the bar is closed.
#[derive(Debug, Default)]
pub struct FindBar {
    open: bool,
    query: String,
    matches: Rc<Vec<FindMatch>>,
    current: Option<usize>,
    /// Display offset when `matches` were found, to place the current match
    /// on screen when they are refreshed.
    offset: usize,
}

impl FindBar {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// Opens the bar, keeping the last query (searched again if not empty).
    pub fn open(&mut self) -> FindOutcome {
        self.open = true;
        if self.query.is_empty() {
            FindOutcome::Nothing
        } else {
            FindOutcome::Search
        }
    }

    /// Closes the bar and drops the highlights. The query is kept.
    pub fn close(&mut self) {
        self.open = false;
        self.matches = Rc::default();
        self.current = None;
    }

    /// Adds typed or pasted text to the query; line breaks become spaces.
    pub fn insert(&mut self, text: &str) -> FindOutcome {
        let before = self.query.len();
        self.query.extend(
            text.chars()
                .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
                .filter(|c| !c.is_control()),
        );
        if self.query.len() == before {
            FindOutcome::Nothing
        } else {
            FindOutcome::Search
        }
    }

    pub fn key(&mut self, key: FindKey) -> FindOutcome {
        match key {
            FindKey::Insert(text) => self.insert(&text),
            FindKey::Backspace => match self.query.pop() {
                Some(_) => FindOutcome::Search,
                None => FindOutcome::Nothing,
            },
            FindKey::Older => self.step(-1),
            FindKey::Newer => self.step(1),
            FindKey::Close => {
                self.close();
                FindOutcome::Closed
            }
        }
    }

    /// New results for the query, oldest first, found at `display_offset`.
    /// The newest match (nearest the live screen) becomes current.
    pub fn set_matches(&mut self, matches: Vec<FindMatch>, display_offset: usize) {
        self.current = matches.len().checked_sub(1);
        self.matches = Rc::new(matches);
        self.offset = display_offset;
    }

    /// Results of searching again after output or a resize. The current
    /// match becomes the one nearest, on screen, to where the old current
    /// match was shown (the first on a tie); with no old current match, the
    /// newest.
    pub fn refresh(&mut self, matches: Vec<FindMatch>, display_offset: usize) {
        let Some((prev, _)) = self.current() else {
            return self.set_matches(matches, display_offset);
        };
        let row = prev.line as i64 + self.offset as i64;
        self.current = matches
            .iter()
            .enumerate()
            .min_by_key(|(_, (s, _))| {
                let r = s.line as i64 + display_offset as i64;
                ((r - row).abs(), s.col.abs_diff(prev.col))
            })
            .map(|(i, _)| i);
        self.matches = Rc::new(matches);
        self.offset = display_offset;
    }

    /// Moves to the next match in `dir`, wrapping at either end.
    fn step(&mut self, dir: isize) -> FindOutcome {
        let n = self.matches.len();
        if n == 0 {
            return FindOutcome::Nothing;
        }
        let i = match self.current {
            None => n - 1,
            Some(i) => (i as isize + dir).rem_euclid(n as isize) as usize,
        };
        self.current = Some(i);
        FindOutcome::Moved
    }

    pub fn matches(&self) -> Rc<Vec<FindMatch>> {
        self.matches.clone()
    }

    pub fn current(&self) -> Option<FindMatch> {
        self.current.and_then(|i| self.matches.get(i).copied())
    }

    /// Text after the query: `3/7`, `No matches`, or nothing.
    pub fn status(&self) -> String {
        if self.query.is_empty() {
            return String::new();
        }
        match self.current {
            Some(i) if !self.matches.is_empty() => format!("{}/{}", i + 1, self.matches.len()),
            _ => "No matches".into(),
        }
    }
}

/// Editor for Cmd+click on a path when the config does not name one.
pub const DEFAULT_EDITOR: &str = "Visual Studio Code";

#[derive(Deserialize, Default)]
struct EditorConfig {
    #[serde(default)]
    editor: EditorSection,
}

#[derive(Deserialize, Default)]
struct EditorSection {
    name: Option<String>,
}

/// The editor app from config text: `[editor] name = "Zed"`. Missing,
/// empty or unreadable values give `DEFAULT_EDITOR`.
pub fn editor_from_toml(text: &str) -> String {
    toml::from_str::<EditorConfig>(text)
        .ok()
        .and_then(|c| c.editor.name)
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| DEFAULT_EDITOR.to_string())
}

/// Reads the editor from `~/.config/overseer/config.toml`.
pub fn load_editor() -> String {
    Theme::config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| editor_from_toml(&t))
        .unwrap_or_else(|| DEFAULT_EDITOR.to_string())
}

/// A process to run for a Cmd+click, as argv (never through a shell), with
/// a second one to try if the first cannot be started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkCommand {
    pub argv: Vec<OsString>,
    pub fallback: Option<Vec<OsString>>,
}

fn argv<I: IntoIterator<Item = S>, S: Into<OsString>>(items: I) -> Vec<OsString> {
    items.into_iter().map(Into::into).collect()
}

fn is_vscode(editor: &str) -> bool {
    matches!(
        editor.trim().to_ascii_lowercase().as_str(),
        "visual studio code" | "code" | "vscode" | "vs code"
    )
}

/// The local path of a `file://` URL, percent-decoded. None for anything
/// that is not an absolute local path.
fn file_url_path(url: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let scheme = url.get(..7)?;
    if !scheme.eq_ignore_ascii_case("file://") {
        return None;
    }
    let mut rest = &url[7..];
    if let Some(r) = rest.strip_prefix("localhost") {
        rest = r;
    }
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    if !rest.starts_with('/') {
        return None;
    }
    let raw = rest.as_bytes();
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' && i + 2 < raw.len() {
            let hex = std::str::from_utf8(&raw[i + 1..i + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(raw[i]);
            i += 1;
        }
    }
    if out.contains(&0) {
        return None;
    }
    Some(PathBuf::from(OsString::from_vec(out)))
}

/// Resolves `~/` against `home` and relative paths against `base`.
fn resolve_path(path: &Path, base: &Path, home: Option<&Path>) -> Option<PathBuf> {
    if let Ok(rest) = path.strip_prefix("~") {
        return Some(home?.join(rest));
    }
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    })
}

fn path_command(
    path: &Path,
    line: Option<u32>,
    editor: &str,
    exists: impl Fn(&Path) -> bool,
) -> Option<LinkCommand> {
    // Only absolute paths reach argv, whatever the base or HOME was: a
    // relative one could start with '-' and be read as an option.
    if !path.is_absolute() || !exists(path) {
        return None;
    }
    let app = if is_vscode(editor) {
        DEFAULT_EDITOR
    } else {
        editor
    };
    let open_in_editor = argv([
        OsString::from("open"),
        "-a".into(),
        app.into(),
        path.as_os_str().to_owned(),
    ]);
    match line {
        Some(n) if is_vscode(editor) => {
            let mut target = path.as_os_str().to_owned();
            target.push(format!(":{n}"));
            Some(LinkCommand {
                argv: argv([OsString::from("code"), "-g".into(), target]),
                fallback: Some(open_in_editor),
            })
        }
        _ => Some(LinkCommand {
            argv: open_in_editor,
            fallback: None,
        }),
    }
}

/// The command a Cmd+click on `link` runs, or None to ignore it. Link text
/// comes from untrusted program output, so only http and https URLs are
/// handed to `open`; `file://` URLs and paths must name something that
/// exists and open in `editor`. Relative paths resolve against `base` (the
/// slot's working directory). With VS Code and a line number the path opens
/// at that line through `code -g`, falling back to `open -a`.
pub fn link_command(
    link: &Link,
    editor: &str,
    base: &Path,
    home: Option<&Path>,
    exists: impl Fn(&Path) -> bool,
) -> Option<LinkCommand> {
    match link {
        Link::Url(url) => {
            if let Some(path) = file_url_path(url) {
                return path_command(&path, None, editor, exists);
            }
            let lower = url.to_ascii_lowercase();
            let web = lower.starts_with("http://") || lower.starts_with("https://");
            if !web || url.chars().any(|c| c.is_control() || c.is_whitespace()) {
                return None;
            }
            Some(LinkCommand {
                argv: argv(["open", url.as_str()]),
                fallback: None,
            })
        }
        Link::Path(path, line) => {
            let resolved = resolve_path(path, base, home)?;
            path_command(&resolved, *line, editor, exists)
        }
    }
}

/// Length of `s` in UTF-16 code units, which is how the IME counts.
fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
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
    /// The slot's working directory, for resolving relative link paths.
    cwd: Mutex<Option<PathBuf>>,
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

/// Feeds the attach snapshot and throws away the replies it produced. The
/// snapshot is old output: answering the queries in it (device attributes,
/// cursor reports, colour queries) would type stale answers into the live
/// session. The daemon already strips the queries it knows; this catches the
/// rest. Only queries in live output are answered.
fn feed_snapshot(shared: &Shared, data: &[u8]) {
    feed(shared, data);
    drop(shared.bridge.lock().unwrap().take_pty_writes());
}

/// Status text when the connection to the daemon has gone. The app keeps
/// trying to reconnect (see `reconnect`) and clears it once it has.
pub const LOST_DAEMON: &str = "Lost the connection to the daemon. Reconnecting\u{2026}";

/// Sizes the bridge to the slot's PTY (so the replay renders as it was
/// drawn), then attaches. Returns a message for the user on failure.
async fn attach(
    slot: SlotId,
    client: &Client,
    hub: &FrameHub,
    route_gen: u64,
    shared: &Shared,
) -> Result<(), String> {
    let lost = |e: anyhow::Error| format!("{LOST_DAEMON} ({e:#})");
    match client.request(Request::ListSlots).await.map_err(lost)? {
        Response::Slots(slots) => {
            let info = slots
                .into_iter()
                .find(|s| s.id == slot)
                .ok_or_else(|| format!("S{slot} does not exist."))?;
            *shared.cwd.lock().unwrap() = info.cwd.clone();
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
    match hub.attach(client, slot, route_gen).await.map_err(lost)? {
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

/// One attach: the hub route this view reads and its number.
struct RouteHandle {
    hub: FrameHub,
    gen: u64,
    output: mpsc::UnboundedReceiver<Bytes>,
}

async fn pump(
    slot: SlotId,
    client: Arc<Client>,
    shared: Arc<Shared>,
    route: RouteHandle,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    wake: mpsc::UnboundedSender<()>,
) {
    let RouteHandle {
        hub,
        gen,
        mut output,
    } = route;
    if let Err(msg) = attach(slot, &client, &hub, gen, &shared).await {
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
                        LOST_DAEMON.into()
                    });
                    shared.wake(&wake);
                    return;
                }
                Some(data) if !replayed => {
                    // The snapshot, alone: live output queued behind it is
                    // fed (and its queries answered) on the next turn.
                    feed_snapshot(&shared, &data);
                    replayed = true;
                    if let Some((cols, rows)) = pending.take() {
                        apply_resize(slot, &client, &shared, cols, rows);
                    }
                    shared.wake(&wake);
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
    route: RouteHandle,
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
                Ok(rt) => rt.block_on(pump(slot, client, shared, route, cmds, wake)),
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

/// Where the grid was last laid out and painted, for mapping pointer
/// positions to cells.
#[derive(Debug, Clone, Copy, Default)]
struct Geometry {
    origin: (f32, f32),
    size: (f32, f32),
    cols: usize,
    rows: usize,
    /// Cursor cell in the last frame, where the IME window goes.
    cursor: (usize, usize),
    /// Last size sent to the pump.
    requested: Option<(u16, u16)>,
}

impl Geometry {
    /// The size to send for a layout of `cols` x `rows`: Some when it is not
    /// the size sent last.
    fn take_resize(&mut self, cols: u16, rows: u16) -> Option<(u16, u16)> {
        (self.requested != Some((cols, rows))).then(|| {
            self.requested = Some((cols, rows));
            (cols, rows)
        })
    }

    /// I3: forgets the size sent last, so the next layout sends it again.
    /// Another client (`overseer attach` in a smaller pane) may have resized
    /// the PTY since. Called when the view gains focus, so the client the
    /// user turns to wins, and two clients never resize each other in a loop.
    fn forget_requested(&mut self) {
        self.requested = None;
    }
}

/// A Cmd+hovered link: the viewport cell under the pointer and the link
/// found there. Paint looks the link up again in the snapshot it draws and
/// underlines it only if the same link is still there.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HoverLink {
    row: usize,
    col: usize,
    link: Link,
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
    /// The snapshot painted last: what the pointer is over.
    frame: Rc<RefCell<Option<Snapshot>>>,
    scroll: ScrollAccumulator,
    focus_reported: bool,
    /// App that opens Cmd+clicked paths.
    editor: String,
    /// The left button is down for a local selection. The selection itself
    /// lives in the terminal (see `TermBridge::start_selection`).
    selecting: bool,
    /// Content version the find bar's matches were searched at.
    searched: u64,
    /// When the find bar last searched, for throttling live refreshes.
    last_search: Option<Instant>,
    /// Pending throttled refresh of the find bar's matches.
    search_timer: Option<Task<()>>,
    /// A press was reported to the app; motion and release follow it.
    reporting: Option<TermButton>,
    last_report_cell: Option<(usize, usize)>,
    find: FindBar,
    hover: Option<HoverLink>,
    last_mouse: Option<gpui::Point<Pixels>>,
    /// IME composition in progress, such as a dead-key accent.
    marked: Option<String>,
    _subscriptions: Vec<Subscription>,
    _notifier: Task<()>,
}

fn hsla(c: Rgb) -> Hsla {
    rgba(c, 1.0)
}

fn rgba(c: Rgb, a: f32) -> Hsla {
    Rgba {
        r: c.0 as f32 / 255.0,
        g: c.1 as f32 / 255.0,
        b: c.2 as f32 / 255.0,
        a,
    }
    .into()
}

fn term_button(b: MouseButton) -> Option<TermButton> {
    match b {
        MouseButton::Left => Some(TermButton::Left),
        MouseButton::Right => Some(TermButton::Right),
        MouseButton::Middle => Some(TermButton::Middle),
        _ => None,
    }
}

/// Starts `argv` with no shell and no inherited stdio, reaping it on a
/// thread of its own.
fn spawn_argv(argv: &[OsString]) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| std::io::Error::other("empty command"))?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn run_link_command(cmd: &LinkCommand) {
    let Err(first) = spawn_argv(&cmd.argv) else {
        return;
    };
    let result = match &cmd.fallback {
        Some(fallback) => spawn_argv(fallback),
        None => Err(first),
    };
    if let Err(e) = result {
        eprintln!("overseer: cannot open link: {e}");
    }
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
                cwd: Mutex::new(None),
            });
            let (cmds, cmd_rx) = mpsc::unbounded_channel();
            let (wake_tx, mut wake_rx) = mpsc::unbounded_channel::<()>();
            let route = RouteHandle {
                hub: hub.clone(),
                gen: route_gen,
                output,
            };
            spawn_pump(slot, client.clone(), shared.clone(), route, cmd_rx, wake_tx);
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
                frame: Rc::default(),
                scroll: ScrollAccumulator::default(),
                focus_reported: false,
                editor: load_editor(),
                selecting: false,
                searched: 0,
                last_search: None,
                search_timer: None,
                reporting: None,
                last_report_cell: None,
                find: FindBar::default(),
                hover: None,
                last_mouse: None,
                marked: None,
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

    fn modes(&self) -> TermModes {
        self.shared.bridge.lock().unwrap().modes()
    }

    /// The modes the session has set, for input sent from outside the view
    /// (the kickoff paste).
    pub fn term_modes(&self) -> TermModes {
        self.modes()
    }

    /// Sends `bytes` typed or pasted by the user, returning to live output.
    fn send_user_input(&self, bytes: &[u8]) {
        self.shared.bridge.lock().unwrap().scroll_to_bottom();
        self.send(bytes);
    }

    /// Sends a focus report when the effective focus (view focused and window
    /// active) changes.
    fn sync_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focused = self.focus_handle.is_focused(window) && window.is_window_active();
        if focused != self.focus_reported {
            self.focus_reported = focused;
            if focused {
                let mut g = self.geometry.get();
                g.forget_requested();
                self.geometry.set(g);
            }
            if let Some(report) = focus_report(focused, &self.modes()) {
                self.send(report);
            }
        }
        cx.notify();
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        if let Some(sc) = shortcut(ks) {
            let handled = match sc {
                Shortcut::Copy => self.copy(cx),
                Shortcut::Paste => {
                    self.paste(cx);
                    true
                }
                Shortcut::Find => {
                    let outcome = self.find.open();
                    self.apply_find(outcome);
                    true
                }
            };
            if handled {
                cx.stop_propagation();
                cx.notify();
            }
            return;
        }
        // Left unhandled, the platform hands the key to the IME, which marks
        // the accent and composes it with the next key.
        if starts_composition(ks) {
            return;
        }
        if self.find.is_open() {
            if let Some(key) = find_key(ks) {
                let outcome = self.find.key(key);
                self.apply_find(outcome);
                cx.stop_propagation();
                cx.notify();
                return;
            }
        }
        let bytes = keystroke_bytes(ks, &self.modes());
        if bytes.is_empty() {
            return;
        }
        self.send_user_input(&bytes);
        cx.stop_propagation();
        cx.notify();
    }

    /// Copies the selection. False when there is none, so Cmd+C propagates.
    fn copy(&mut self, cx: &mut Context<Self>) -> bool {
        let text = self.shared.bridge.lock().unwrap().selection_text();
        let Some(text) = text else {
            return false;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        true
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let item = cx.read_from_clipboard();
        if self.find.is_open() {
            if let Some(text) = item.as_ref().and_then(|i| i.text()) {
                let outcome = self.find.insert(&text);
                self.apply_find(outcome);
            }
            return;
        }
        let entries = item.as_ref().map(|i| i.entries()).unwrap_or_default();
        if let Some(bytes) = paste_bytes(entries, &self.modes()) {
            self.send_user_input(&bytes);
        }
    }

    /// Text committed by the IME (or typed while composing).
    fn commit_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        if self.find.is_open() {
            let outcome = self.find.insert(text);
            self.apply_find(outcome);
        } else {
            self.send_user_input(text.as_bytes());
        }
        cx.notify();
    }

    fn apply_find(&mut self, outcome: FindOutcome) {
        match outcome {
            FindOutcome::Search => self.run_search(),
            FindOutcome::Moved => self.reveal_current_match(),
            FindOutcome::Closed | FindOutcome::Nothing => {}
        }
    }

    /// Matches for the query now, with the display offset and content
    /// version they belong to.
    fn search_now(&self) -> (Vec<FindMatch>, usize, u64) {
        let bridge = self.shared.bridge.lock().unwrap();
        let found = bridge
            .search(self.find.query())
            .into_iter()
            .map(|m| (m.start, m.end))
            .collect();
        (found, bridge.display_offset(), bridge.content_version())
    }

    fn run_search(&mut self) {
        let (found, offset, version) = self.search_now();
        self.find.set_matches(found, offset);
        self.searched = version;
        self.last_search = Some(Instant::now());
        self.reveal_current_match();
    }

    fn live_search(&self) -> bool {
        self.find.is_open() && !self.find.query().is_empty()
    }

    /// Searches again when output or a resize changed the content since the
    /// last search, keeping the current match near where it was on screen.
    fn refresh_search(&mut self) {
        if !self.live_search() {
            return;
        }
        let version = self.shared.bridge.lock().unwrap().content_version();
        if version == self.searched {
            return;
        }
        let (found, offset, version) = self.search_now();
        self.find.refresh(found, offset);
        self.searched = version;
        self.last_search = Some(Instant::now());
    }

    /// Arms a timer for the next live search when content changed since the
    /// last one. Rendering never searches itself; the timer fires at most
    /// once per `SEARCH_REFRESH_INTERVAL`, and re-renders, which arms the
    /// next one while output keeps changing. Until then the paint skips
    /// match tints from an older content version.
    fn schedule_search_refresh(&mut self, cx: &mut Context<Self>) {
        if !self.live_search() {
            self.search_timer = None;
            return;
        }
        if self.search_timer.is_some() {
            return;
        }
        let dirty = self.shared.bridge.lock().unwrap().content_version() != self.searched;
        let Some(delay) = search_refresh_delay(dirty, self.last_search, Instant::now()) else {
            return;
        };
        self.search_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |this: &mut TerminalView, cx| {
                this.search_timer = None;
                this.refresh_search();
                cx.notify();
            });
        }));
    }

    fn reveal_current_match(&mut self) {
        let Some((start, _)) = self.find.current() else {
            return;
        };
        let rows = self.geometry.get().rows;
        let mut bridge = self.shared.bridge.lock().unwrap();
        let delta = scroll_delta_to_show(
            start.line,
            bridge.display_offset(),
            bridge.history_size(),
            rows,
        );
        if delta != 0 {
            bridge.scroll(delta);
        }
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
        let (row, col) = self.cell_at(ev.position, true).unwrap_or((0, 0));
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

    /// The viewport cell under a window position. Outside the grid this is
    /// None, or the nearest cell with `clamp`.
    fn cell_at(&self, pos: gpui::Point<Pixels>, clamp: bool) -> Option<(usize, usize)> {
        let (m, _) = self.metrics?;
        let g = self.geometry.get();
        if g.cols == 0 || g.rows == 0 {
            return None;
        }
        let x = f32::from(pos.x) - g.origin.0;
        let y = f32::from(pos.y) - g.origin.1;
        let inside = x >= 0.0 && y >= 0.0 && x < g.size.0 && y < g.size.1;
        if !inside && !clamp {
            return None;
        }
        Some(pixel_to_grid(x, y, m, g.cols, g.rows))
    }

    /// The grid position and cell half under a window position, clamped to
    /// the grid, for selecting.
    fn select_point(&self, pos: gpui::Point<Pixels>) -> Option<(GridPos, CellSide)> {
        let (row, col) = self.cell_at(pos, true)?;
        let (m, _) = self.metrics?;
        let x = f32::from(pos.x) - self.geometry.get().origin.0;
        let side = if x < 0.0 {
            CellSide::Left
        } else if x >= self.geometry.get().size.0 {
            CellSide::Right
        } else {
            cell_side(x, m)
        };
        let offset = self.shared.bridge.lock().unwrap().display_offset();
        Some((
            GridPos {
                line: grid_line(row, offset),
                col,
            },
            side,
        ))
    }

    /// The link at a viewport cell of the last frame and what opening it
    /// runs. None when there is no link or it would be ignored.
    fn link_at(&self, row: usize, col: usize) -> Option<(HoverLink, LinkCommand)> {
        let frame = self.frame.borrow();
        let snap = frame.as_ref()?;
        let span = find_link_span_at(snap, row, col)?;
        let base = self
            .shared
            .cwd
            .lock()
            .unwrap()
            .clone()
            .or_else(|| std::env::current_dir().ok())?;
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from);
        let cmd = link_command(&span.link, &self.editor, &base, home.as_deref(), |p| {
            p.exists()
        })?;
        let hover = HoverLink {
            row,
            col,
            link: span.link,
        };
        Some((hover, cmd))
    }

    fn report(&self, event: MouseEvent, (row, col): (usize, usize), mods: &gpui::Modifiers) {
        let input = MouseInput {
            event,
            row,
            col,
            shift: false,
            ctrl: mods.control,
            alt: mods.alt,
        };
        if let Some(bytes) = encode_mouse(&input, &self.modes()) {
            self.send(&bytes);
        }
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle);
        let Some(button) = term_button(ev.button) else {
            return;
        };
        let Some(cell) = self.cell_at(ev.position, false) else {
            return;
        };
        let mods = ev.modifiers;
        let link = if mods.platform {
            self.link_at(cell.0, cell.1)
        } else {
            None
        };
        let action = mouse_down_action(
            &self.modes(),
            button,
            mods.shift,
            mods.platform,
            link.is_some(),
            ev.click_count,
        );
        match action {
            DownAction::OpenLink => {
                if let Some((_, cmd)) = link {
                    run_link_command(&cmd);
                }
            }
            DownAction::Report => {
                self.shared.bridge.lock().unwrap().clear_selection();
                self.reporting = Some(button);
                self.last_report_cell = Some(cell);
                self.report(MouseEvent::Press(button), cell, &mods);
            }
            DownAction::Select(mode) => {
                if let Some((pos, side)) = self.select_point(ev.position) {
                    self.shared
                        .bridge
                        .lock()
                        .unwrap()
                        .start_selection(mode.into(), pos, side);
                    self.selecting = true;
                }
            }
            DownAction::Ignore => {}
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// Every mouse move in the window, so drags keep tracking outside the
    /// grid.
    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, cx: &mut Context<Self>) {
        self.last_mouse = Some(ev.position);
        if let Some(button) = self.reporting {
            if let Some(cell) = self.cell_at(ev.position, true) {
                if self.last_report_cell != Some(cell) {
                    self.last_report_cell = Some(cell);
                    self.report(MouseEvent::Drag(button), cell, &ev.modifiers);
                }
            }
            return;
        }
        if self.selecting {
            if let Some((pos, side)) = self.select_point(ev.position) {
                self.shared
                    .bridge
                    .lock()
                    .unwrap()
                    .update_selection(pos, side);
                cx.notify();
            }
            return;
        }
        if let Some(cell) = self.cell_at(ev.position, false) {
            let hovering = ev.pressed_button.is_none() && !ev.modifiers.shift;
            if hovering
                && self.modes().mouse == MouseMode::Motion
                && self.last_report_cell != Some(cell)
            {
                self.last_report_cell = Some(cell);
                self.report(MouseEvent::Move, cell, &ev.modifiers);
            }
        }
        self.refresh_hover(ev.modifiers.platform, cx);
    }

    /// Every mouse release in the window, so a drag ends wherever it does.
    fn on_mouse_up(&mut self, ev: &MouseUpEvent, cx: &mut Context<Self>) {
        let Some(button) = term_button(ev.button) else {
            return;
        };
        if self.reporting == Some(button) {
            self.reporting = None;
            if let Some(cell) = self.cell_at(ev.position, true) {
                self.report(MouseEvent::Release(button), cell, &ev.modifiers);
            }
            self.last_report_cell = None;
        }
        if button == TermButton::Left && self.selecting {
            self.selecting = false;
            // A plain click that was not dragged leaves an empty selection.
            let mut bridge = self.shared.bridge.lock().unwrap();
            if bridge.selection_range().is_none() {
                bridge.clear_selection();
            }
            cx.notify();
        }
    }

    /// Underlines the link under the pointer while Cmd is held.
    fn refresh_hover(&mut self, cmd: bool, cx: &mut Context<Self>) {
        let hover = if cmd {
            self.last_mouse
                .and_then(|p| self.cell_at(p, false))
                .and_then(|(row, col)| self.link_at(row, col))
                .map(|(h, _)| h)
        } else {
            None
        };
        if hover != self.hover {
            self.hover = hover;
            cx.notify();
        }
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

/// IME support: dead keys and input methods mark text while composing; the
/// committed text goes to the PTY (or the find bar). The terminal has no
/// document of its own, so ranges only cover the marked text.
impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let end = self.marked.as_deref().map(utf16_len).unwrap_or(0);
        Some(UTF16Selection {
            range: end..end,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_deref().map(|t| 0..utf16_len(t))
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked = None;
        self.commit_text(text, cx);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        new_text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked = (!new_text.is_empty()).then(|| new_text.to_string());
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let (m, _) = self.metrics?;
        let (row, col) = self.geometry.get().cursor;
        Some(Bounds::new(
            point(
                element_bounds.origin.x + px(col as f32 * m.width),
                element_bounds.origin.y + px(row as f32 * m.height),
            ),
            size(px(m.width), px(m.height)),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _: gpui::Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
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

/// Highlights and decorations drawn over the grid.
#[derive(Default)]
struct Overlay {
    matches: Rc<Vec<FindMatch>>,
    current_match: Option<FindMatch>,
    /// Content version the matches belong to. Matches are not drawn over a
    /// snapshot of other content (output landed after the search); the next
    /// render searches again.
    matches_version: u64,
    hover: Option<HoverLink>,
    marked: Option<String>,
}

const SELECTION_TINT: (Rgb, f32) = (Rgb(0x4d, 0x73, 0xcc), 0.45);
const MATCH_TINT: (Rgb, f32) = (Rgb(0xff, 0xd8, 0x33), 0.35);
const CURRENT_MATCH_TINT: (Rgb, f32) = (Rgb(0xff, 0x8c, 0x1a), 0.65);

/// Everything paint needs, captured per frame.
struct PaintStyle {
    font: Font,
    font_size: Pixels,
    metrics: CellMetrics,
    background: Rgb,
    foreground: Rgb,
    focused: bool,
    overlay: Overlay,
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

/// Paints the grid and returns the snapshot it painted.
fn paint_grid(
    bounds: Bounds<Pixels>,
    shared: &Shared,
    ps: &PaintStyle,
    window: &mut Window,
    cx: &mut App,
) -> Snapshot {
    let snap = shared.bridge.lock().unwrap().snapshot();
    // N12: the grid can be a little larger than the bounds (a resize not yet
    // applied, or a partial cell at the edge), so clip to the view.
    window.with_content_mask(Some(ContentMask { bounds }), |window| {
        paint_snapshot(&snap, bounds, ps, window, cx)
    });
    snap
}

fn paint_snapshot(
    snap: &Snapshot,
    bounds: Bounds<Pixels>,
    ps: &PaintStyle,
    window: &mut Window,
    cx: &mut App,
) {
    let m = ps.metrics;
    let (ox, oy): (f32, f32) = (bounds.origin.x.into(), bounds.origin.y.into());
    let at = |row: usize, col: usize| {
        point(
            px(ox + col as f32 * m.width),
            px(oy + row as f32 * m.height),
        )
    };

    let cells_quad = |row: usize, col: usize, len: usize, color: Hsla| {
        fill(
            Bounds::new(at(row, col), size(px(len as f32 * m.width), px(m.height))),
            color,
        )
    };
    let ov = &ps.overlay;
    let top = grid_line(0, snap.display_offset);
    let bottom = grid_line(snap.rows.max(1) - 1, snap.display_offset);
    let visible = |&(s, e): &FindMatch| e.line >= top && s.line <= bottom;
    let mut tints: Vec<(FindMatch, (Rgb, f32))> = Vec::new();
    if ov.matches_version == snap.content_version {
        tints.extend(
            ov.matches
                .iter()
                .filter(|r| visible(r))
                .map(|&r| (r, MATCH_TINT)),
        );
        tints.extend(
            ov.current_match
                .filter(visible)
                .map(|r| (r, CURRENT_MATCH_TINT)),
        );
    }
    tints.extend(
        snap.selection
            .map(|sel| (sel.start, sel.end))
            .filter(visible)
            .map(|r| (r, SELECTION_TINT)),
    );

    for row in 0..snap.rows {
        let cells = &snap.cells[row * snap.cols..(row + 1) * snap.cols];
        let RowLayout {
            spans,
            batches,
            shapes,
        } = layout_row(cells, ps.background);
        for s in spans {
            window.paint_quad(cells_quad(row, s.col, s.len, hsla(s.color)));
        }
        let line = grid_line(row, snap.display_offset);
        for &((start, end), (color, alpha)) in &tints {
            if let Some((c0, c1)) = span_cols_in_row(start, end, line, snap.cols) {
                window.paint_quad(cells_quad(row, c0, c1 - c0 + 1, rgba(color, alpha)));
            }
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

    let hovered = ov.hover.as_ref().and_then(|h| hover_span(snap, h));
    if let Some((row, span)) = hovered {
        let o = at(row, span.start_col);
        let cells = span.end_col.saturating_sub(span.start_col);
        window.paint_quad(fill(
            Bounds::new(
                point(o.x, o.y + px(m.height - 1.)),
                size(px(cells as f32 * m.width), px(1.)),
            ),
            hsla(ps.foreground),
        ));
    }

    paint_cursor(snap, ps, &at, window, cx);

    // Composition in progress, drawn over the cursor and underlined.
    if let Some(text) = ov.marked.as_deref() {
        let origin = at(snap.cursor.row, snap.cursor.col);
        let cells = text.chars().count().max(1);
        window.paint_quad(fill(
            Bounds::new(origin, size(px(cells as f32 * m.width), px(m.height))),
            hsla(ps.background),
        ));
        let style = TextStyle {
            fg: ps.foreground,
            bold: false,
            italic: false,
            underline: true,
            strikethrough: false,
        };
        paint_text(text, &style, false, origin, ps, window, cx);
    }
}

/// The span to underline for a Cmd+hovered link in `snap`: the link at the
/// hovered cell, if it is still the one that was hovered.
fn hover_span(snap: &Snapshot, h: &HoverLink) -> Option<(usize, LinkSpan)> {
    find_link_span_at(snap, h.row, h.col)
        .filter(|span| span.link == h.link)
        .map(|span| (h.row, span))
}

fn paint_cursor(
    snap: &Snapshot,
    ps: &PaintStyle,
    at: &dyn Fn(usize, usize) -> gpui::Point<Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    let m = ps.metrics;
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
        self.schedule_search_refresh(cx);
        let find_open = self.find.is_open();
        let ps = PaintStyle {
            font: font(self.theme.font_family.clone()),
            font_size: px(self.theme.font_size),
            metrics,
            background: self.theme.background,
            foreground: self.theme.palette.foreground,
            focused: self.focus_handle.is_focused(window),
            overlay: Overlay {
                matches_version: self.searched,
                matches: if find_open {
                    self.find.matches()
                } else {
                    Rc::default()
                },
                current_match: self.find.current().filter(|_| find_open),
                hover: self.hover.clone(),
                // While the find bar is open it shows the composition.
                marked: self.marked.clone().filter(|_| !find_open),
            },
        };
        let status = self.shared.status.lock().unwrap().clone();
        let shared = self.shared.clone();
        let cmds = self.cmds.clone();
        let geometry = self.geometry.clone();
        let painted = self.geometry.clone();
        let frame = self.frame.clone();
        let focus = self.focus_handle.clone();
        let entity = cx.entity();
        let weak = cx.weak_entity();

        let grid = canvas(
            move |bounds, _, _| {
                let (w, h): (f32, f32) = (bounds.size.width.into(), bounds.size.height.into());
                let (cols, rows) = grid_size(w, h, metrics);
                let mut g = geometry.get();
                g.origin = (bounds.origin.x.into(), bounds.origin.y.into());
                g.size = (w, h);
                g.cols = cols as usize;
                g.rows = rows as usize;
                if let Some((cols, rows)) = g.take_resize(cols, rows) {
                    let _ = cmds.send(Cmd::Resize { cols, rows });
                }
                geometry.set(g);
            },
            move |bounds, (), window, cx| {
                window.handle_input(&focus, ElementInputHandler::new(bounds, entity), cx);
                let snap = paint_grid(bounds, &shared, &ps, window, cx);
                let mut g = painted.get();
                g.cursor = (snap.cursor.row, snap.cursor.col);
                painted.set(g);
                *frame.borrow_mut() = Some(snap);
                listen_for_drags(weak, window);
            },
        )
        .size_full();

        let find_bar = find_open.then(|| {
            let focus = self.focus_handle.clone();
            let marked = self.marked.as_deref().unwrap_or_default();
            div()
                .absolute()
                .top_1()
                .right_2()
                .px_2()
                .py_1()
                .occlude()
                .bg(hsla(Rgb(0x30, 0x30, 0x30)))
                .text_color(hsla(Rgb(0xe5, 0xe5, 0xe5)))
                .font_family(self.theme.font_family.clone())
                .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                    window.focus(&focus);
                    cx.stop_propagation();
                })
                .child(format!(
                    "Find: {}{}_  {}",
                    self.find.query(),
                    marked,
                    self.find.status()
                ))
        });

        div()
            .key_context("Terminal")
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .bg(hsla(self.theme.background))
            .when(self.hover.is_some(), |d| d.cursor_pointer())
            .when(self.hover.is_none(), |d| d.cursor_text())
            .on_key_down(cx.listener(Self::on_key_down))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .on_modifiers_changed(cx.listener(|this, ev: &ModifiersChangedEvent, _, cx| {
                this.refresh_hover(ev.modifiers.platform, cx)
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_mouse_down))
            .child(grid)
            .children(find_bar)
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

/// Window-wide move and release listeners for this frame. They run in the
/// capture phase so a drag that leaves the grid still tracks and ends.
fn listen_for_drags(view: WeakEntity<TerminalView>, window: &mut Window) {
    let v = view.clone();
    window.on_mouse_event(move |ev: &MouseMoveEvent, phase, _, cx| {
        if phase == DispatchPhase::Capture {
            let _ = v.update(cx, |this, cx| this.on_mouse_move(ev, cx));
        }
    });
    window.on_mouse_event(move |ev: &MouseUpEvent, phase, _, cx| {
        if phase == DispatchPhase::Capture {
            let _ = view.update(cx, |this, cx| this.on_mouse_up(ev, cx));
        }
    });
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

    fn shared() -> Shared {
        Shared {
            bridge: Mutex::new(TermBridge::new(80, 24, 100)),
            dirty: AtomicBool::new(false),
            status: Mutex::new(None),
            cwd: Mutex::new(None),
        }
    }

    #[test]
    fn snapshot_replay_answers_no_queries_but_live_output_does() {
        let s = shared();
        // DA1, DSR, OSC 11 colour query and CSI 14 t, as Claude Code sends
        // them at startup, sitting in the replayed history.
        let queries = b"\x1b[c\x1b[6n\x1b]11;?\x07\x1b[14t";
        feed_snapshot(&s, queries);
        assert!(s.bridge.lock().unwrap().take_pty_writes().is_empty());
        feed(&s, b"\x1b[c");
        assert!(!s.bridge.lock().unwrap().take_pty_writes().is_empty());
    }

    #[test]
    fn focus_gain_resends_the_size() {
        let mut g = Geometry::default();
        assert_eq!(g.take_resize(100, 30), Some((100, 30)));
        // Same layout, nothing to send, even if another client has resized
        // the PTY meanwhile.
        assert_eq!(g.take_resize(100, 30), None);
        g.forget_requested();
        assert_eq!(g.take_resize(100, 30), Some((100, 30)));
        assert_eq!(g.take_resize(100, 30), None);
    }

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

    // --- Selection, mouse, paste, find, links and composition ---

    fn p(line: i32, col: usize) -> GridPos {
        GridPos { line, col }
    }

    #[test]
    fn click_count_picks_selection_mode() {
        assert_eq!(selection_mode(0), SelectionMode::Simple);
        assert_eq!(selection_mode(1), SelectionMode::Simple);
        assert_eq!(selection_mode(2), SelectionMode::Word);
        assert_eq!(selection_mode(3), SelectionMode::Line);
        assert_eq!(selection_mode(5), SelectionMode::Line);
        assert_eq!(
            SelectionKind::from(SelectionMode::Simple),
            SelectionKind::Simple
        );
        assert_eq!(
            SelectionKind::from(SelectionMode::Word),
            SelectionKind::Word
        );
        assert_eq!(
            SelectionKind::from(SelectionMode::Line),
            SelectionKind::Line
        );
    }

    #[test]
    fn span_cols_cover_selection_rows() {
        let (a, b) = (p(-2, 5), p(0, 3));
        assert_eq!(span_cols_in_row(a, b, -3, 80), None);
        assert_eq!(span_cols_in_row(a, b, -2, 80), Some((5, 79)));
        assert_eq!(span_cols_in_row(a, b, -1, 80), Some((0, 79)));
        assert_eq!(span_cols_in_row(a, b, 0, 80), Some((0, 3)));
        assert_eq!(span_cols_in_row(a, b, 1, 80), None);
        assert_eq!(span_cols_in_row(p(0, 90), p(0, 95), 0, 80), None);
        assert_eq!(span_cols_in_row(a, b, 0, 0), None);
    }

    #[test]
    fn viewport_rows_and_grid_lines() {
        assert_eq!(grid_line(0, 0), 0);
        assert_eq!(grid_line(0, 10), -10);
        assert_eq!(grid_line(3, 10), -7);
        assert_eq!(viewport_row(-7, 10, 24), Some(3));
        assert_eq!(viewport_row(-11, 10, 24), None);
        assert_eq!(viewport_row(14, 10, 24), None);
        assert_eq!(viewport_row(13, 10, 24), Some(23));
    }

    #[test]
    fn cell_halves_pick_the_side() {
        assert_eq!(cell_side(0.0, M), CellSide::Left);
        assert_eq!(cell_side(M.width * 0.49, M), CellSide::Left);
        assert_eq!(cell_side(M.width * 0.51, M), CellSide::Right);
        assert_eq!(cell_side(M.width * 3.2, M), CellSide::Left);
        assert_eq!(cell_side(M.width * 3.8, M), CellSide::Right);
    }

    /// Selects with the mouse the way the view does: a press at viewport
    /// cell (`row`, `col`) in `mode`.
    fn press(b: &mut TermBridge, mode: SelectionMode, row: usize, col: usize) {
        let pos = p(grid_line(row, b.display_offset()), col);
        b.start_selection(mode.into(), pos, CellSide::Left);
    }

    #[test]
    fn selection_survives_output_past_the_cap() {
        for sb in [1, 6] {
            let mut b = TermBridge::new(20, 4, sb);
            for i in 0..30 {
                b.feed(format!("row {i} item{i}\r\n").as_bytes());
            }
            // Row 0 of the live view reads "row 27 item27".
            press(&mut b, SelectionMode::Word, 0, 8);
            assert_eq!(b.selection_text().as_deref(), Some("item27"));
            b.feed(b"row 30 item30\r\n");
            assert_eq!(b.selection_text().as_deref(), Some("item27"), "sb {sb}");
            // The tint follows the text: it moved up into scrollback.
            let snap = b.snapshot();
            let sel = snap.selection.unwrap();
            assert_eq!(sel.start.line, -1);
            // Pushed out of scrollback: nothing, never other text.
            for i in 0..(sb + 6) {
                b.feed(format!("tail {i} more\r\n").as_bytes());
                let t = b.selection_text();
                assert!(t.is_none() || t.as_deref() == Some("item27"), "{t:?}");
            }
            assert_eq!(b.selection_text(), None);
        }
    }

    #[test]
    fn scrolled_back_selection_stays_on_its_text() {
        let mut b = TermBridge::new(20, 4, 10);
        for i in 0..12 {
            b.feed(format!("row {i} item{i}\r\n").as_bytes());
        }
        b.scroll(3);
        press(&mut b, SelectionMode::Line, 1, 0);
        let orig = b.selection_text().unwrap();
        b.feed(b"x\r\ny\r\n");
        assert_eq!(b.selection_text().as_deref(), Some(orig.as_str()));
        // Screen clear and scrollback clear in one piece.
        b.feed(b"\x1b[H\x1b[2J\x1b[3J");
        let t = b.selection_text();
        assert!(t.is_none() || t.as_deref() == Some(orig.as_str()), "{t:?}");
    }

    /// The text of every match, read back from the terminal.
    fn match_texts(b: &TermBridge, bar: &FindBar) -> Vec<String> {
        bar.matches()
            .iter()
            .map(|&(start, end)| b.text_in(overseer_term::Selection { start, end }))
            .collect()
    }

    fn search(b: &TermBridge, q: &str) -> Vec<FindMatch> {
        b.search(q).into_iter().map(|m| (m.start, m.end)).collect()
    }

    #[test]
    fn find_highlights_follow_output_past_the_cap() {
        for (sb, scrolled) in [(1, 0), (6, 0), (6, 2)] {
            let mut b = TermBridge::new(20, 4, sb);
            for i in 0..20 {
                b.feed(format!("n{i} needle{}\r\n", i % 3).as_bytes());
            }
            b.scroll(scrolled);
            let mut bar = FindBar::default();
            bar.open();
            bar.insert("needle1");
            bar.set_matches(search(&b, "needle1"), b.display_offset());
            assert!(!bar.matches().is_empty());
            for round in 0..10 {
                b.feed(format!("o{round} needle{}\r\n", round % 2).as_bytes());
                // What render does after each batch of output.
                bar.refresh(search(&b, bar.query()), b.display_offset());
                for t in match_texts(&b, &bar) {
                    assert_eq!(t, "needle1", "sb {sb} scrolled {scrolled} round {round}");
                }
                assert_eq!(bar.current().is_some(), !bar.matches().is_empty());
            }
        }
    }

    #[test]
    fn refresh_keeps_the_current_match_where_it_was_on_screen() {
        let mut bar = FindBar::default();
        bar.open();
        bar.insert("x");
        // Found at offset 0: matches on grid lines -5, -2 and 1.
        bar.set_matches(
            vec![
                (p(-5, 0), p(-5, 0)),
                (p(-2, 3), p(-2, 3)),
                (p(1, 0), p(1, 0)),
            ],
            0,
        );
        bar.key(FindKey::Older);
        assert_eq!(bar.current(), Some((p(-2, 3), p(-2, 3))));
        // The old current was on viewport row -2 (offset 0). At offset 4 the
        // new matches sit on viewport rows -5, -2 and 1.
        bar.refresh(
            vec![
                (p(-9, 0), p(-9, 0)),
                (p(-6, 3), p(-6, 3)),
                (p(-3, 0), p(-3, 0)),
            ],
            4,
        );
        assert_eq!(bar.current(), Some((p(-6, 3), p(-6, 3))));
        // A tie picks the first: rows -4 and 0 around row -2.
        bar.refresh(vec![(p(-8, 3), p(-8, 3)), (p(-4, 3), p(-4, 3))], 4);
        assert_eq!(bar.current(), Some((p(-8, 3), p(-8, 3))));
        // No matches left, then new ones with no current: the newest.
        bar.refresh(vec![], 0);
        assert_eq!(bar.current(), None);
        assert_eq!(bar.status(), "No matches");
        bar.refresh(vec![(p(0, 0), p(0, 0)), (p(2, 0), p(2, 0))], 0);
        assert_eq!(bar.current(), Some((p(2, 0), p(2, 0))));
        // Closing keeps no match state.
        bar.close();
        assert!(bar.matches().is_empty());
        assert_eq!(bar.current(), None);
    }

    #[test]
    fn hover_underline_is_recomputed_from_the_painted_snapshot() {
        let mut b = TermBridge::new(40, 3, 10);
        b.feed(b"see https://example.com/a here\r\n");
        let snap = b.snapshot();
        let span = find_link_span_at(&snap, 0, 8).unwrap();
        let hover = HoverLink {
            row: 0,
            col: 8,
            link: span.link.clone(),
        };
        assert_eq!(hover_span(&snap, &hover), Some((0, span)));
        // Output moves the link up a row: the old cell no longer shows it.
        b.feed(b"\r\n\r\nplain text on this row now\r\n");
        assert_eq!(hover_span(&b.snapshot(), &hover), None);
        // A different link at the same cell is not underlined either.
        let mut c = TermBridge::new(40, 3, 10);
        c.feed(b"see https://other.example/b here\r\n");
        assert_eq!(hover_span(&c.snapshot(), &hover), None);
    }

    #[test]
    fn live_search_is_throttled_with_a_trailing_refresh() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        // Nothing changed: no search, however long it has been.
        assert_eq!(search_refresh_delay(false, None, t0), None);
        assert_eq!(search_refresh_delay(false, Some(t0), t0 + ms(5000)), None);
        // First change, or a change after a quiet spell: search now.
        assert_eq!(search_refresh_delay(true, None, t0), Some(ms(0)));
        assert_eq!(
            search_refresh_delay(true, Some(t0), t0 + ms(200)),
            Some(ms(0))
        );
        assert_eq!(
            search_refresh_delay(true, Some(t0), t0 + ms(900)),
            Some(ms(0))
        );
        // Changes soon after a search wait out the interval, so a stream
        // is searched at most every 200 ms and the last change still is.
        assert_eq!(search_refresh_delay(true, Some(t0), t0), Some(ms(200)));
        assert_eq!(
            search_refresh_delay(true, Some(t0), t0 + ms(50)),
            Some(ms(150))
        );
        assert_eq!(
            search_refresh_delay(true, Some(t0), t0 + ms(199)),
            Some(ms(1))
        );
        // A clock that reads earlier than the last search waits the interval.
        assert_eq!(
            search_refresh_delay(true, Some(t0 + ms(10)), t0),
            Some(ms(200))
        );
    }

    #[test]
    fn a_stream_is_searched_at_most_every_interval() {
        // Content changes every 16 ms for 1 s, then stops. Simulate the
        // timer: one pending at a time, fired at its deadline.
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let (mut version, mut searched) = (0u64, 0u64);
        let mut last: Option<Instant> = None;
        let mut timer: Option<Instant> = None;
        let mut searches = Vec::new();
        for tick in 0..=1500u64 {
            let now = t0 + ms(tick);
            if tick <= 1000 && tick % 16 == 0 {
                version += 1;
            }
            if timer.is_some_and(|at| at <= now) {
                timer = None;
                if version != searched {
                    searched = version;
                    last = Some(now);
                    searches.push(tick);
                }
            }
            if timer.is_none() {
                if let Some(d) = search_refresh_delay(version != searched, last, now) {
                    timer = Some(now + d);
                }
            }
        }
        assert_eq!(searched, version, "the last change was searched");
        assert!(
            searches.windows(2).all(|w| w[1] - w[0] >= 200),
            "{searches:?}"
        );
        assert!(searches.len() <= 7, "{searches:?}");
        assert!(*searches.last().unwrap() <= 1200, "{searches:?}");
    }

    #[test]
    fn scroll_to_show_a_match() {
        // History 100, 24 rows, at the bottom: grid lines 0..=23 visible.
        assert_eq!(scroll_delta_to_show(10, 0, 100, 24), 0);
        // Line -60 is above: scroll back so it is the top row (offset 60).
        assert_eq!(scroll_delta_to_show(-60, 0, 100, 24), 60);
        // Scrolled back 60 (lines -60..=-37); line 20 becomes the bottom row
        // at offset 3.
        assert_eq!(scroll_delta_to_show(20, 60, 100, 24), -57);
        assert_eq!(scroll_delta_to_show(23, 60, 100, 24), -60);
        // Line -30 at offset 60 becomes the bottom row.
        assert_eq!(scroll_delta_to_show(-30, 60, 100, 24), -7);
        // Never past the top of history.
        assert_eq!(scroll_delta_to_show(-105, 0, 100, 24), 100);
    }

    fn mouse_on() -> TermModes {
        TermModes {
            mouse: MouseMode::Click,
            sgr_mouse: true,
            ..TermModes::default()
        }
    }

    #[test]
    fn mouse_reports_only_in_mouse_mode_without_shift() {
        assert_eq!(mouse_route(&mouse_on(), false), MouseRoute::Report);
        assert_eq!(mouse_route(&mouse_on(), true), MouseRoute::Local);
        assert_eq!(mouse_route(&TermModes::default(), false), MouseRoute::Local);
        assert_eq!(mouse_route(&TermModes::default(), true), MouseRoute::Local);
    }

    #[test]
    fn mouse_down_actions() {
        let off = TermModes::default();
        let on = mouse_on();
        let (l, r) = (TermButton::Left, TermButton::Right);
        let sel = DownAction::Select;
        assert_eq!(
            mouse_down_action(&off, l, false, false, false, 1),
            sel(SelectionMode::Simple)
        );
        assert_eq!(
            mouse_down_action(&off, l, false, false, false, 2),
            sel(SelectionMode::Word)
        );
        assert_eq!(
            mouse_down_action(&off, l, false, false, false, 3),
            sel(SelectionMode::Line)
        );
        assert_eq!(
            mouse_down_action(&off, r, false, false, false, 1),
            DownAction::Ignore
        );
        assert_eq!(
            mouse_down_action(&on, l, false, false, false, 2),
            DownAction::Report
        );
        assert_eq!(
            mouse_down_action(&on, r, false, false, false, 1),
            DownAction::Report
        );
        assert_eq!(
            mouse_down_action(&on, l, true, false, false, 1),
            sel(SelectionMode::Simple),
            "Shift-drag selects in mouse mode"
        );
        assert_eq!(
            mouse_down_action(&on, l, true, false, false, 2),
            sel(SelectionMode::Word)
        );
        // Cmd+click opens a link in either mode; without a link it is a click.
        assert_eq!(
            mouse_down_action(&on, l, false, true, true, 1),
            DownAction::OpenLink
        );
        assert_eq!(
            mouse_down_action(&off, l, false, true, true, 1),
            DownAction::OpenLink
        );
        assert_eq!(
            mouse_down_action(&off, l, false, true, false, 1),
            sel(SelectionMode::Simple)
        );
        assert_eq!(
            mouse_down_action(&off, r, false, true, true, 1),
            DownAction::Ignore
        );
    }

    /// Ruling R9: scrollback unless the alt screen and mouse mode are both
    /// on; 1007 arrows on the alt screen without mouse mode; Shift is local.
    #[test]
    fn wheel_rule_matrix() {
        let modes = |alt_screen, mouse, alternate_scroll| TermModes {
            alt_screen,
            mouse: if mouse {
                MouseMode::Click
            } else {
                MouseMode::Off
            },
            sgr_mouse: true,
            alternate_scroll,
            ..TermModes::default()
        };
        let wheel = |m: TermModes, shift| route_wheel(1, &m, 0, 0, shift, false, false);
        let back = WheelAction::Scrollback(1);
        let report = WheelAction::Send(b"\x1b[<64;1;1M".to_vec());
        let arrow = WheelAction::Send(b"\x1b[A".to_vec());
        assert_eq!(wheel(modes(false, false, false), false), back);
        assert_eq!(wheel(modes(false, true, true), false), back, "main screen");
        assert_eq!(
            wheel(modes(true, false, false), false),
            back,
            "no mouse mode"
        );
        assert_eq!(wheel(modes(true, false, true), false), arrow, "1007");
        assert_eq!(wheel(modes(true, true, false), false), report);
        assert_eq!(wheel(modes(true, true, true), false), report);
        assert_eq!(wheel(modes(true, true, true), true), back, "Shift");
        assert_eq!(wheel(modes(true, false, true), true), back, "Shift");
    }

    fn text(t: &str) -> ClipboardEntry {
        ClipboardEntry::String(gpui::ClipboardString::new(t.into()))
    }

    fn image() -> ClipboardEntry {
        ClipboardEntry::Image(gpui::Image::from_bytes(
            gpui::ImageFormat::Png,
            vec![0x89, b'P', b'N', b'G'],
        ))
    }

    #[test]
    fn paste_text_goes_through_encode_paste() {
        let bracketed = TermModes {
            bracketed_paste: true,
            ..TermModes::default()
        };
        assert_eq!(
            paste_bytes(&[text("a\nb\nc")], &bracketed),
            Some(b"\x1b[200~a\rb\rc\x1b[201~".to_vec()),
            "three lines arrive as one bracketed paste"
        );
        assert_eq!(
            paste_bytes(&[text("a\nb")], &TermModes::default()),
            Some(b"a\rb".to_vec())
        );
        assert_eq!(
            paste_bytes(&[image(), text("hi")], &bracketed),
            Some(b"\x1b[200~hi\x1b[201~".to_vec()),
            "text beside an image is pasted as text"
        );
    }

    #[test]
    fn image_only_paste_sends_ctrl_v() {
        assert_eq!(
            paste_bytes(&[image()], &TermModes::default()),
            Some(vec![0x16])
        );
        assert_eq!(
            paste_bytes(&[text(""), image()], &TermModes::default()),
            Some(vec![0x16])
        );
    }

    #[test]
    fn empty_clipboard_pastes_nothing() {
        assert_eq!(paste_bytes(&[], &TermModes::default()), None);
        assert_eq!(paste_bytes(&[text("")], &TermModes::default()), None);
    }

    #[test]
    fn cmd_shortcuts() {
        let cmd = |k: &str| ks(k, None, |m| m.platform = true);
        assert_eq!(shortcut(&cmd("c")), Some(Shortcut::Copy));
        assert_eq!(shortcut(&cmd("v")), Some(Shortcut::Paste));
        assert_eq!(shortcut(&cmd("f")), Some(Shortcut::Find));
        assert_eq!(shortcut(&cmd("q")), None);
        assert_eq!(shortcut(&ks("c", Some("c"), |_| {})), None);
        assert_eq!(shortcut(&ks("c", None, |m| m.control = true)), None);
        assert_eq!(
            shortcut(&ks("c", None, |m| {
                m.platform = true;
                m.shift = true
            })),
            None
        );
    }

    #[test]
    fn dead_keys_start_composition() {
        // key_char values as gpui 0.2.2 reports them on a US layout.
        let opt = |k: &str, ch: &str| ks(k, Some(ch), |m| m.alt = true);
        assert!(
            starts_composition(&opt("e", "\u{b4}\u{a0}")),
            "Option+E acute"
        );
        assert!(
            starts_composition(&opt("u", "\u{a8}\u{a0}")),
            "Option+U umlaut"
        );
        assert!(
            starts_composition(&opt("i", "^\u{a0}")),
            "Option+I circumflex"
        );
        assert!(starts_composition(&opt("n", "~\u{a0}")), "Option+N tilde");
        assert!(starts_composition(&opt("`", "`\u{a0}")), "Option+` grave");
        assert!(
            !starts_composition(&opt("b", "\u{222b}")),
            "Option+B is Meta"
        );
        assert!(!starts_composition(&opt("space", "\u{a0}")), "Option+Space");
        assert!(!starts_composition(&opt("e", "\u{b4}")), "no dead-key tail");
        assert!(!starts_composition(&ks("e", Some("e"), |_| {})));
        assert!(!starts_composition(&ks("e", Some("\u{b4}\u{a0}"), |m| {
            m.alt = true;
            m.platform = true
        })));
        // Option+B still sends Meta-b.
        assert_eq!(bytes(opt("b", "\u{222b}")), b"\x1bb");
    }

    #[test]
    fn find_keys() {
        assert_eq!(find_key(&ks("escape", None, |_| {})), Some(FindKey::Close));
        assert_eq!(
            find_key(&ks("enter", Some("\n"), |_| {})),
            Some(FindKey::Older)
        );
        assert_eq!(
            find_key(&ks("enter", Some("\n"), |m| m.shift = true)),
            Some(FindKey::Newer)
        );
        assert_eq!(
            find_key(&ks("backspace", None, |_| {})),
            Some(FindKey::Backspace)
        );
        assert_eq!(
            find_key(&ks("e", Some("E"), |m| m.shift = true)),
            Some(FindKey::Insert("E".into()))
        );
        assert_eq!(
            find_key(&ks("space", Some(" "), |_| {})),
            Some(FindKey::Insert(" ".into()))
        );
        assert_eq!(find_key(&ks("c", None, |m| m.control = true)), None);
        assert_eq!(find_key(&ks("up", None, |_| {})), None);
        assert_eq!(find_key(&ks("tab", Some("\t"), |_| {})), None);
    }

    fn three_matches() -> Vec<FindMatch> {
        vec![(p(1, 0), p(1, 4)), (p(5, 2), p(5, 6)), (p(9, 0), p(9, 4))]
    }

    #[test]
    fn find_bar_query_and_search() {
        let mut f = FindBar::default();
        assert!(!f.is_open());
        assert_eq!(f.open(), FindOutcome::Nothing, "nothing to search yet");
        assert!(f.is_open());
        assert_eq!(f.status(), "");
        assert_eq!(f.key(FindKey::Insert("err".into())), FindOutcome::Search);
        assert_eq!(f.key(FindKey::Insert("or".into())), FindOutcome::Search);
        assert_eq!(f.query(), "error");
        f.set_matches(Vec::new(), 0);
        assert_eq!(f.status(), "No matches");
        assert_eq!(f.key(FindKey::Older), FindOutcome::Nothing);
        assert_eq!(f.key(FindKey::Backspace), FindOutcome::Search);
        assert_eq!(f.query(), "erro");
        assert_eq!(f.insert("a\nb"), FindOutcome::Search, "pasted text");
        assert_eq!(f.query(), "erroa b");
        assert_eq!(f.insert("\u{7}"), FindOutcome::Nothing);
    }

    #[test]
    fn find_bar_cycles_and_wraps() {
        let mut f = FindBar::default();
        f.open();
        f.insert("x");
        f.set_matches(three_matches(), 0);
        // The newest match (nearest the live screen) is current first.
        assert_eq!(f.current(), Some((p(9, 0), p(9, 4))));
        assert_eq!(f.status(), "3/3");
        assert_eq!(f.key(FindKey::Older), FindOutcome::Moved);
        assert_eq!(f.current(), Some((p(5, 2), p(5, 6))));
        f.key(FindKey::Older);
        assert_eq!(f.status(), "1/3");
        f.key(FindKey::Older);
        assert_eq!(f.status(), "3/3", "Enter wraps from the oldest");
        f.key(FindKey::Newer);
        assert_eq!(f.status(), "1/3", "Shift+Enter wraps from the newest");
        assert_eq!(f.matches().len(), 3);
    }

    #[test]
    fn find_bar_escape_closes_and_reopen_searches_again() {
        let mut f = FindBar::default();
        f.open();
        f.insert("error");
        f.set_matches(three_matches(), 0);
        assert_eq!(f.key(FindKey::Close), FindOutcome::Closed);
        assert!(!f.is_open());
        assert!(f.matches().is_empty(), "highlights dropped");
        assert_eq!(f.current(), None);
        assert_eq!(f.query(), "error", "query kept");
        assert_eq!(f.open(), FindOutcome::Search);
    }

    #[test]
    fn editor_config() {
        assert_eq!(editor_from_toml(""), "Visual Studio Code");
        assert_eq!(editor_from_toml("[editor]\nname = \"Zed\"\n"), "Zed");
        assert_eq!(
            editor_from_toml("[terminal]\nfont_size = 14\n[editor]\nname = \" \"\n"),
            "Visual Studio Code"
        );
        assert_eq!(editor_from_toml("not toml ["), "Visual Studio Code");
    }

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    fn cmd_for(link: Link, editor: &str, existing: &[&str]) -> Option<LinkCommand> {
        link_command(
            &link,
            editor,
            Path::new("/work/repo"),
            Some(Path::new("/Users/me")),
            |p| existing.iter().any(|e| Path::new(e) == p),
        )
    }

    #[test]
    fn web_urls_open_with_open() {
        let c = cmd_for(Link::Url("https://example.com/a?b=1".into()), "Zed", &[]);
        assert_eq!(
            c,
            Some(LinkCommand {
                argv: os(&["open", "https://example.com/a?b=1"]),
                fallback: None,
            })
        );
        let c = cmd_for(Link::Url("HTTP://example.com".into()), "Zed", &[]).unwrap();
        assert_eq!(c.argv, os(&["open", "HTTP://example.com"]));
    }

    #[test]
    fn other_urls_are_ignored() {
        for url in [
            "javascript:alert(1)",
            "ftp://host/x",
            "ssh://host",
            "x-man-page://ls",
        ] {
            assert_eq!(cmd_for(Link::Url(url.into()), "Zed", &[]), None, "{url}");
        }
        assert_eq!(
            cmd_for(Link::Url("https://a.com/\u{7}".into()), "Zed", &[]),
            None
        );
    }

    #[test]
    fn file_urls_open_existing_files_in_the_editor() {
        let c = cmd_for(
            Link::Url("file:///tmp/my%20notes.txt".into()),
            "Zed",
            &["/tmp/my notes.txt"],
        );
        assert_eq!(
            c.unwrap().argv,
            os(&["open", "-a", "Zed", "/tmp/my notes.txt"])
        );
        assert_eq!(
            cmd_for(Link::Url("file:///tmp/missing".into()), "Zed", &[]),
            None
        );
        assert_eq!(
            cmd_for(Link::Url("file://otherhost/x".into()), "Zed", &["/x"]),
            None
        );
    }

    #[test]
    fn plain_path_opens_in_editor() {
        let c = cmd_for(
            Link::Path("/etc/hosts".into(), None),
            "Visual Studio Code",
            &["/etc/hosts"],
        );
        assert_eq!(
            c,
            Some(LinkCommand {
                argv: os(&["open", "-a", "Visual Studio Code", "/etc/hosts"]),
                fallback: None,
            })
        );
    }

    #[test]
    fn path_with_line_uses_code_g_for_vscode() {
        let c = cmd_for(
            Link::Path("crates/overseer/src/main.rs".into(), Some(1)),
            "Visual Studio Code",
            &["/work/repo/crates/overseer/src/main.rs"],
        );
        assert_eq!(
            c,
            Some(LinkCommand {
                argv: os(&["code", "-g", "/work/repo/crates/overseer/src/main.rs:1"]),
                fallback: Some(os(&[
                    "open",
                    "-a",
                    "Visual Studio Code",
                    "/work/repo/crates/overseer/src/main.rs"
                ])),
            })
        );
        // `code` as the configured name is VS Code too.
        let c = cmd_for(Link::Path("/a.rs".into(), Some(7)), "code", &["/a.rs"]).unwrap();
        assert_eq!(c.argv, os(&["code", "-g", "/a.rs:7"]));
        assert_eq!(
            c.fallback,
            Some(os(&["open", "-a", "Visual Studio Code", "/a.rs"]))
        );
    }

    #[test]
    fn path_with_line_in_other_editor_opens_the_file() {
        let c = cmd_for(Link::Path("/a.rs".into(), Some(7)), "Zed", &["/a.rs"]);
        assert_eq!(
            c,
            Some(LinkCommand {
                argv: os(&["open", "-a", "Zed", "/a.rs"]),
                fallback: None,
            })
        );
    }

    #[test]
    fn paths_resolve_and_must_exist() {
        let c = cmd_for(
            Link::Path("~/notes.md".into(), None),
            "Zed",
            &["/Users/me/notes.md"],
        );
        assert_eq!(
            c.unwrap().argv,
            os(&["open", "-a", "Zed", "/Users/me/notes.md"])
        );
        let c = cmd_for(
            Link::Path("./src/lib.rs".into(), None),
            "Zed",
            &["/work/repo/./src/lib.rs"],
        );
        assert!(c.is_some(), "relative to the slot's cwd");
        assert_eq!(
            cmd_for(Link::Path("and/or".into(), None), "Zed", &[]),
            None,
            "not on disk"
        );
        let none_home = link_command(
            &Link::Path("~/x".into(), None),
            "Zed",
            Path::new("/"),
            None,
            |_| true,
        );
        assert_eq!(none_home, None);
    }

    #[test]
    fn relative_resolved_paths_are_rejected() {
        let rel = |link: Link, base: &str, home: Option<&str>| {
            link_command(&link, "Zed", Path::new(base), home.map(Path::new), |_| true)
        };
        // A relative slot cwd never yields a command, even if it "exists".
        assert_eq!(rel(Link::Path("main.rs".into(), None), "-x", None), None);
        assert_eq!(
            rel(Link::Path("src/a.rs".into(), Some(3)), "-x", None),
            None
        );
        assert_eq!(rel(Link::Path("./a".into(), None), "rel/dir", None), None);
        // Nor does a relative HOME.
        assert_eq!(
            rel(Link::Path("~/a".into(), None), "/", Some("-home")),
            None
        );
        // An absolute base still works.
        assert!(rel(Link::Path("main.rs".into(), None), "/w", None).is_some());
    }

    #[test]
    fn utf16_lengths() {
        assert_eq!(utf16_len("\u{b4}"), 1);
        assert_eq!(utf16_len("e\u{301}"), 2);
        assert_eq!(utf16_len("\u{1f600}"), 2);
    }
}
