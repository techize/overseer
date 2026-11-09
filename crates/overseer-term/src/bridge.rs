//! Bridge between PTY output bytes and an `alacritty_terminal` model.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::Direction;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::search::{RegexIter, RegexSearch};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, TermMode};
use alacritty_terminal::vte::ansi::{
    Color, CursorShape as VteCursorShape, Processor, Rgb as VteRgb,
};
use alacritty_terminal::Term;

use crate::mouse::{MouseMode, TermModes};

/// Default scrollback depth in lines.
pub const DEFAULT_SCROLLBACK: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl From<VteRgb> for Rgb {
    fn from(c: VteRgb) -> Self {
        Rgb(c.r, c.g, c.b)
    }
}

/// Colour palette used to resolve named and indexed colours. Defaults are the
/// xterm colours; every field can be overridden.
#[derive(Debug, Clone)]
pub struct Palette {
    pub ansi: [Rgb; 16],
    pub foreground: Rgb,
    pub background: Rgb,
}

impl Default for Palette {
    fn default() -> Self {
        let a = |r, g, b| Rgb(r, g, b);
        Palette {
            ansi: [
                a(0, 0, 0),
                a(205, 0, 0),
                a(0, 205, 0),
                a(205, 205, 0),
                a(0, 0, 238),
                a(205, 0, 205),
                a(0, 205, 205),
                a(229, 229, 229),
                a(127, 127, 127),
                a(255, 0, 0),
                a(0, 255, 0),
                a(255, 255, 0),
                a(92, 92, 255),
                a(255, 0, 255),
                a(0, 255, 255),
                a(255, 255, 255),
            ],
            foreground: a(229, 229, 229),
            background: a(0, 0, 0),
        }
    }
}

impl Palette {
    /// Resolve a 256-colour (or alacritty extended, 256..269) index.
    fn indexed(&self, idx: usize) -> Rgb {
        match idx {
            0..=15 => self.ansi[idx],
            16..=231 => {
                let i = idx - 16;
                let level = |n: usize| if n == 0 { 0 } else { (55 + 40 * n) as u8 };
                Rgb(level(i / 36), level((i / 6) % 6), level(i % 6))
            }
            232..=255 => {
                let v = (8 + 10 * (idx - 232)) as u8;
                Rgb(v, v, v)
            }
            256 | 258 | 267 => self.foreground,
            257 => self.background,
            259..=266 => dim(self.ansi[idx - 259]),
            268 => dim(self.foreground),
            _ => self.foreground,
        }
    }
}

fn dim(c: Rgb) -> Rgb {
    let f = |v: u8| (v as u16 * 2 / 3) as u8;
    Rgb(f(c.0), f(c.1), f(c.2))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    pub ch: char,
    pub zerowidth: Option<Vec<char>>,
    pub fg: Rgb,
    pub bg: Rgb,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub hidden: bool,
    pub inverse: bool,
    /// True for the leading cell of a double-width character. The following
    /// cell is its spacer.
    pub wide: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Underline,
    Beam,
    Hidden,
}

/// Cursor position in viewport coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
    pub shape: CursorShape,
    /// False when the cursor is outside the viewport (scrolled back).
    pub visible: bool,
}

/// The visible viewport, row-major (`cells[row * cols + col]`).
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub cols: usize,
    pub rows: usize,
    pub cells: Vec<Cell>,
    pub cursor: Cursor,
    /// Lines scrolled back from the bottom; 0 means following live output.
    pub display_offset: usize,
    /// Lines of scrollback above the live screen.
    pub history_size: usize,
    /// `line + abs_line_base` is a position that stays on its text while new
    /// output scrolls the screen, including once scrollback is full and old
    /// lines are evicted (see `TermBridge::abs_line_base`).
    pub abs_line_base: usize,
}

impl Snapshot {
    pub fn cell(&self, row: usize, col: usize) -> Option<&Cell> {
        if row < self.rows && col < self.cols {
            self.cells.get(row * self.cols + col)
        } else {
            None
        }
    }
}

/// A position in the full grid. `line` is 0 at the top of the live screen and
/// negative in scrollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridPos {
    pub line: i32,
    pub col: usize,
}

impl GridPos {
    fn point(self) -> Point {
        Point::new(Line(self.line), Column(self.col))
    }
}

/// An inclusive search match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    pub start: GridPos,
    pub end: GridPos,
}

/// An inclusive linear selection. Endpoints may be given in either order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub start: GridPos,
    pub end: GridPos,
}

type ColorFmt = Arc<dyn Fn(VteRgb) -> String + Sync + Send + 'static>;

#[derive(Clone)]
struct Listener {
    writes: Arc<Mutex<Vec<u8>>>,
    /// Colour queries waiting to be resolved against the effective palette.
    color_requests: Arc<Mutex<Vec<(usize, ColorFmt)>>>,
    window_size: Arc<Mutex<WindowSize>>,
}

impl Default for Listener {
    fn default() -> Self {
        Listener {
            writes: Arc::default(),
            color_requests: Arc::default(),
            window_size: Arc::new(Mutex::new(WindowSize {
                num_lines: 0,
                num_cols: 0,
                cell_width: 0,
                cell_height: 0,
            })),
        }
    }
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => {
                if let Ok(mut w) = self.writes.lock() {
                    w.extend_from_slice(text.as_bytes());
                }
            }
            Event::ColorRequest(idx, fmt) => {
                if let Ok(mut r) = self.color_requests.lock() {
                    r.push((idx, fmt));
                }
            }
            Event::TextAreaSizeRequest(fmt) => {
                let size = self.window_size.lock().map(|s| *s).unwrap_or(WindowSize {
                    num_lines: 0,
                    num_cols: 0,
                    cell_width: 0,
                    cell_height: 0,
                });
                if let Ok(mut w) = self.writes.lock() {
                    w.extend_from_slice(fmt(size).as_bytes());
                }
            }
            _ => {}
        }
    }
}

pub struct TermBridge {
    term: Term<Listener>,
    processor: Processor,
    writes: Arc<Mutex<Vec<u8>>>,
    color_requests: Arc<Mutex<Vec<(usize, ColorFmt)>>>,
    window_size: Arc<Mutex<WindowSize>>,
    palette: Palette,
    scrollback: usize,
    /// Lines that have dropped off the top of a full scrollback, in total.
    lines_evicted: usize,
    /// A push count in progress on the primary grid (see `begin_count`).
    count: Option<PushCount>,
}

/// How lines pushed into history are counted across one parse. alacritty
/// does not report evictions, but `Grid::scroll_up` adds the lines it pushes
/// to a non-zero display offset (capped at the scrollback limit). So the
/// offset is parked at 1 for the parse and read back afterwards, then put
/// back where it was. The grid is never painted in between.
#[derive(Debug, Clone, Copy)]
struct PushCount {
    offset_before: usize,
    history_before: usize,
    /// False when history was empty, so the offset could not be parked and
    /// the growth of history is the count.
    parked: bool,
}

impl TermBridge {
    pub fn new(cols: usize, rows: usize, scrollback_lines: usize) -> Self {
        let listener = Listener::default();
        let writes = listener.writes.clone();
        let color_requests = listener.color_requests.clone();
        let window_size = listener.window_size.clone();
        if let Ok(mut w) = window_size.lock() {
            w.num_cols = cols.min(u16::MAX as usize) as u16;
            w.num_lines = rows.min(u16::MAX as usize) as u16;
        }
        let config = Config {
            scrolling_history: scrollback_lines,
            ..Config::default()
        };
        let term = Term::new(config, &TermSize::new(cols.max(1), rows.max(1)), listener);
        TermBridge {
            term,
            processor: Processor::new(),
            writes,
            color_requests,
            window_size,
            palette: Palette::default(),
            scrollback: scrollback_lines,
            lines_evicted: 0,
            count: None,
        }
    }

    pub fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
    }

    /// Feed raw PTY bytes. Chunks may be any size and may split escape
    /// sequences or UTF-8 sequences anywhere.
    pub fn feed(&mut self, bytes: &[u8]) {
        // Parse in pieces of bounded line count so one parse cannot push
        // more lines than the parked offset can count.
        let max_lfs = (self.scrollback / 2).max(1);
        let mut rest = bytes;
        while !rest.is_empty() {
            let mut lfs = 0;
            let end = rest
                .iter()
                .position(|&b| {
                    lfs += usize::from(b == b'\n');
                    lfs > max_lfs
                })
                .unwrap_or(rest.len());
            let (piece, tail) = rest.split_at(end);
            self.begin_count();
            self.processor.advance(&mut self.term, piece);
            self.end_count();
            rest = tail;
        }
        self.answer_color_requests();
    }

    fn alt_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    fn begin_count(&mut self) {
        // A count left open while the alt screen was up is still running:
        // the inactive primary grid kept its parked offset.
        if self.count.is_some() || self.alt_screen() {
            return;
        }
        let grid = self.term.grid();
        let (history, offset) = (grid.history_size(), grid.display_offset());
        let parked = history >= 1 && self.scrollback >= 2;
        if parked {
            self.term
                .grid_mut()
                .scroll_display(Scroll::Delta(1 - offset as i32));
        }
        self.count = Some(PushCount {
            offset_before: offset,
            history_before: history,
            parked,
        });
    }

    fn end_count(&mut self) {
        if self.alt_screen() {
            return;
        }
        let Some(c) = self.count.take() else {
            return;
        };
        let grid = self.term.grid();
        let (history, offset) = (grid.history_size(), grid.display_offset());
        let pushed = if !c.parked {
            history.saturating_sub(c.history_before)
        } else if offset >= self.scrollback {
            // Saturated: at least a whole scrollback went by. Count every old
            // line as gone so no stored position lands on new text.
            c.history_before + grid.screen_lines() + offset
        } else {
            offset.saturating_sub(1)
        };
        // Lines pushed beyond what history grew by fell off the top. A
        // cleared history (CSI 3 J) also counts as evicted.
        let grew = history as i64 - c.history_before as i64;
        self.lines_evicted += (pushed as i64 - grew).max(0) as usize;
        // Keep a scrolled-back viewport on the same text, as alacritty does.
        let target = if c.offset_before == 0 {
            0
        } else {
            (c.offset_before + pushed).min(history)
        };
        self.term
            .grid_mut()
            .scroll_display(Scroll::Delta(target as i32 - offset as i32));
    }

    /// Lines that have dropped off the top of a full scrollback since start.
    pub fn lines_evicted(&self) -> usize {
        self.lines_evicted
    }

    /// `GridPos::line + abs_line_base()` numbers every line ever scrolled
    /// into history, so it stays on its text as output arrives, even after
    /// scrollback fills and starts evicting. Positions below
    /// `lines_evicted()` refer to text that is gone.
    pub fn abs_line_base(&self) -> usize {
        self.lines_evicted + self.term.grid().history_size()
    }

    /// When a synchronized update (DEC 2026) times out, or None when none is
    /// pending. Call `tick` at or after this instant.
    pub fn sync_deadline(&self) -> Option<Instant> {
        self.processor.sync_timeout().sync_timeout()
    }

    /// Flush a synchronized update whose timeout has elapsed.
    pub fn tick(&mut self) {
        self.tick_at(Instant::now());
    }

    /// `tick` with an explicit clock, for tests and schedulers.
    pub fn tick_at(&mut self, now: Instant) {
        if self.sync_deadline().is_some_and(|d| d <= now) {
            self.begin_count();
            self.processor.stop_sync(&mut self.term);
            self.end_count();
            self.answer_color_requests();
        }
    }

    /// Pixel size of one cell, reported to programs that query the text area
    /// size (CSI 14 t / 16 t).
    pub fn set_cell_size(&mut self, width_px: u16, height_px: u16) {
        if let Ok(mut w) = self.window_size.lock() {
            w.cell_width = width_px;
            w.cell_height = height_px;
        }
    }

    fn answer_color_requests(&mut self) {
        let pending = match self.color_requests.lock() {
            Ok(mut r) if !r.is_empty() => std::mem::take(&mut *r),
            _ => return,
        };
        for (idx, fmt) in pending {
            let Rgb(r, g, b) = self.resolve_index(idx.min(268));
            let reply = fmt(VteRgb { r, g, b });
            if let Ok(mut w) = self.writes.lock() {
                w.extend_from_slice(reply.as_bytes());
            }
        }
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.term.resize(TermSize::new(cols.max(1), rows.max(1)));
        if let Ok(mut w) = self.window_size.lock() {
            w.num_cols = cols.min(u16::MAX as usize) as u16;
            w.num_lines = rows.min(u16::MAX as usize) as u16;
        }
    }

    /// Scroll the viewport. Positive moves toward older history.
    pub fn scroll(&mut self, delta_lines: i32) {
        self.term.scroll_display(Scroll::Delta(delta_lines));
    }

    /// Lines of scrollback above the live screen (see `Snapshot::history_size`).
    pub fn history_size(&self) -> usize {
        self.term.grid().history_size()
    }

    /// Lines the viewport is scrolled back from live output.
    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// Return the viewport to live output.
    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    fn resolve(&self, color: Color) -> Rgb {
        match color {
            Color::Spec(rgb) => rgb.into(),
            Color::Indexed(i) => self.resolve_index(i as usize),
            Color::Named(n) => self.resolve_index(n as usize),
        }
    }

    fn resolve_index(&self, idx: usize) -> Rgb {
        // Colours set at runtime by the program (OSC 4/10/11) win.
        if let Some(rgb) = self.term.colors()[idx] {
            return rgb.into();
        }
        self.palette.indexed(idx)
    }

    pub fn snapshot(&self) -> Snapshot {
        let cols = self.term.columns();
        let rows = self.term.screen_lines();
        let grid = self.term.grid();
        let offset = grid.display_offset();
        let mut cells = Vec::with_capacity(cols * rows);
        for row in 0..rows {
            let line = Line(row as i32 - offset as i32);
            for col in 0..cols {
                let c = &grid[line][Column(col)];
                let f = c.flags;
                cells.push(Cell {
                    ch: c.c,
                    zerowidth: c.zerowidth().map(|z| z.to_vec()),
                    fg: self.resolve(c.fg),
                    bg: self.resolve(c.bg),
                    bold: f.contains(Flags::BOLD),
                    dim: f.contains(Flags::DIM),
                    italic: f.contains(Flags::ITALIC),
                    underline: f.intersects(Flags::ALL_UNDERLINES),
                    strikethrough: f.contains(Flags::STRIKEOUT),
                    hidden: f.contains(Flags::HIDDEN),
                    inverse: f.contains(Flags::INVERSE),
                    wide: f.contains(Flags::WIDE_CHAR),
                });
            }
        }
        let content = self.term.renderable_content();
        let cur_row = content.cursor.point.line.0 + offset as i32;
        let in_view = cur_row >= 0 && (cur_row as usize) < rows;
        let shape = match content.cursor.shape {
            VteCursorShape::Block | VteCursorShape::HollowBlock => CursorShape::Block,
            VteCursorShape::Underline => CursorShape::Underline,
            VteCursorShape::Beam => CursorShape::Beam,
            VteCursorShape::Hidden => CursorShape::Hidden,
        };
        Snapshot {
            cols,
            rows,
            cells,
            cursor: Cursor {
                row: cur_row.max(0) as usize,
                col: content.cursor.point.column.0,
                shape,
                visible: in_view && shape != CursorShape::Hidden,
            },
            display_offset: offset,
            history_size: grid.history_size(),
            abs_line_base: self.abs_line_base(),
        }
    }

    pub fn modes(&self) -> TermModes {
        let m = *self.term.mode();
        let mouse = if m.contains(TermMode::MOUSE_MOTION) {
            MouseMode::Motion
        } else if m.contains(TermMode::MOUSE_DRAG) {
            MouseMode::Drag
        } else if m.contains(TermMode::MOUSE_REPORT_CLICK) {
            MouseMode::Click
        } else {
            MouseMode::Off
        };
        TermModes {
            app_cursor: m.contains(TermMode::APP_CURSOR),
            bracketed_paste: m.contains(TermMode::BRACKETED_PASTE),
            mouse,
            sgr_mouse: m.contains(TermMode::SGR_MOUSE),
            alt_screen: m.contains(TermMode::ALT_SCREEN),
            focus_reporting: m.contains(TermMode::FOCUS_IN_OUT),
            alternate_scroll: m.contains(TermMode::ALTERNATE_SCROLL),
        }
    }

    /// Case-insensitive (unless the query has uppercase) literal search over
    /// scrollback and the live screen.
    pub fn search(&self, query: &str) -> Vec<Match> {
        if query.is_empty() {
            return Vec::new();
        }
        let Ok(mut regex) = RegexSearch::new(&escape_regex(query)) else {
            return Vec::new();
        };
        let start = Point::new(self.term.topmost_line(), Column(0));
        let end = Point::new(self.term.bottommost_line(), self.term.last_column());
        RegexIter::new(start, end, Direction::Right, &self.term, &mut regex)
            .map(|m| {
                let (s, e) = (m.start(), m.end());
                Match {
                    start: GridPos {
                        line: s.line.0,
                        col: s.column.0,
                    },
                    end: GridPos {
                        line: e.line.0,
                        col: e.column.0,
                    },
                }
            })
            .collect()
    }

    pub fn selection_text(&self, sel: Selection) -> String {
        let (a, b) = if (sel.start.line, sel.start.col) <= (sel.end.line, sel.end.col) {
            (sel.start, sel.end)
        } else {
            (sel.end, sel.start)
        };
        let top = self.term.topmost_line().0;
        let bottom = self.term.bottommost_line().0;
        if b.line < top || a.line > bottom {
            return String::new();
        }
        let last = self.term.last_column().0;
        let a = GridPos {
            line: a.line.max(top),
            col: a.col.min(last),
        };
        let b = GridPos {
            line: b.line.min(bottom),
            col: b.col.min(last),
        };
        self.term.bounds_to_string(a.point(), b.point())
    }

    /// Replies the terminal wants sent back to the PTY (device attributes,
    /// cursor position reports and so on). Drains the buffer.
    pub fn take_pty_writes(&mut self) -> Vec<u8> {
        match self.writes.lock() {
            Ok(mut w) => std::mem::take(&mut *w),
            Err(_) => Vec::new(),
        }
    }
}

fn escape_regex(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        if "\\.+*?()|[]{}^$#&-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_text(s: &Snapshot, row: usize) -> String {
        (0..s.cols)
            .map(|c| s.cell(row, c).unwrap().ch)
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn feed_plain_text() {
        let mut b = TermBridge::new(80, 24, DEFAULT_SCROLLBACK);
        b.feed(b"hello");
        let s = b.snapshot();
        assert_eq!(s.cell(0, 0).unwrap().ch, 'h');
        assert_eq!(s.cell(0, 4).unwrap().ch, 'o');
        assert_eq!(s.cursor.col, 5);
    }

    #[test]
    fn truecolor_fg() {
        let mut b = TermBridge::new(80, 24, DEFAULT_SCROLLBACK);
        b.feed(b"\x1b[38;2;10;20;30mX");
        assert_eq!(b.snapshot().cell(0, 0).unwrap().fg, Rgb(10, 20, 30));
    }

    #[test]
    fn indexed_and_named_colors_resolve() {
        let mut b = TermBridge::new(80, 24, DEFAULT_SCROLLBACK);
        b.feed(b"\x1b[31mA\x1b[38;5;196mB\x1b[0mC");
        let s = b.snapshot();
        assert_eq!(s.cell(0, 0).unwrap().fg, Rgb(205, 0, 0));
        assert_eq!(s.cell(0, 1).unwrap().fg, Rgb(255, 0, 0));
        assert_eq!(s.cell(0, 2).unwrap().fg, Palette::default().foreground);
    }

    #[test]
    fn alt_screen_mode() {
        let mut b = TermBridge::new(80, 24, DEFAULT_SCROLLBACK);
        assert!(!b.modes().alt_screen);
        b.feed(b"\x1b[?1049h");
        assert!(b.modes().alt_screen);
    }

    #[test]
    fn focus_reporting_mode() {
        let mut b = TermBridge::new(80, 24, DEFAULT_SCROLLBACK);
        assert!(!b.modes().focus_reporting);
        b.feed(b"\x1b[?1004h");
        assert!(b.modes().focus_reporting);
        b.feed(b"\x1b[?1004l");
        assert!(!b.modes().focus_reporting);
    }

    #[test]
    fn alternate_scroll_mode() {
        let mut b = TermBridge::new(80, 24, DEFAULT_SCROLLBACK);
        // On by default, as in alacritty and iTerm2.
        assert!(b.modes().alternate_scroll);
        b.feed(b"\x1b[?1007l");
        assert!(!b.modes().alternate_scroll);
        b.feed(b"\x1b[?1007h");
        assert!(b.modes().alternate_scroll);
    }

    #[test]
    fn bracketed_paste_mode() {
        let mut b = TermBridge::new(80, 24, DEFAULT_SCROLLBACK);
        assert!(!b.modes().bracketed_paste);
        b.feed(b"\x1b[?2004h");
        assert!(b.modes().bracketed_paste);
    }

    #[test]
    fn mouse_modes_track_sequences() {
        let mut b = TermBridge::new(80, 24, DEFAULT_SCROLLBACK);
        b.feed(b"\x1b[?1000h\x1b[?1006h");
        let m = b.modes();
        assert_eq!(m.mouse, MouseMode::Click);
        assert!(m.sgr_mouse);
        b.feed(b"\x1b[?1003h");
        assert_eq!(b.modes().mouse, MouseMode::Motion);
        b.feed(b"\x1b[?1003l\x1b[?1000l");
        assert_eq!(b.modes().mouse, MouseMode::Off);
    }

    #[test]
    fn device_attributes_reply() {
        let mut b = TermBridge::new(80, 24, DEFAULT_SCROLLBACK);
        assert!(b.take_pty_writes().is_empty());
        b.feed(b"\x1b[c");
        let w = b.take_pty_writes();
        assert!(!w.is_empty());
        assert!(b.take_pty_writes().is_empty(), "buffer drains");
    }

    #[test]
    fn search_finds_in_scrollback() {
        let mut b = TermBridge::new(80, 10, DEFAULT_SCROLLBACK);
        for i in 0..200 {
            b.feed(format!("line {i} \r\n").as_bytes());
        }
        let m = b.search("line 5 ");
        assert_eq!(m.len(), 1);
        assert!(m[0].start.line < 0, "match is in scrollback");
        assert_eq!(
            b.selection_text(Selection {
                start: m[0].start,
                end: m[0].end
            }),
            "line 5"
        );
    }

    #[test]
    fn history_and_offset_track_scrolling() {
        let mut b = TermBridge::new(80, 10, DEFAULT_SCROLLBACK);
        for i in 0..30 {
            b.feed(format!("line {i}\r\n").as_bytes());
        }
        // 31 lines used (30 plus the empty cursor line), 10 on screen.
        assert_eq!(b.history_size(), 21);
        assert_eq!(b.snapshot().history_size, 21);
        assert_eq!(b.display_offset(), 0);
        b.scroll(5);
        assert_eq!(b.display_offset(), 5);
        assert_eq!(b.snapshot().display_offset, 5);
        b.scroll_to_bottom();
        assert_eq!(b.display_offset(), 0);
    }

    /// Text at absolute line `abs` (columns `a..=b`), or "" once evicted.
    fn text_at(b: &TermBridge, abs: usize, a: usize, e: usize) -> String {
        let line = abs as i64 - b.abs_line_base() as i64;
        b.selection_text(Selection {
            start: GridPos {
                line: line as i32,
                col: a,
            },
            end: GridPos {
                line: line as i32,
                col: e,
            },
        })
    }

    fn feed_lines(b: &mut TermBridge, range: std::ops::Range<usize>) {
        for i in range {
            b.feed(format!("w{i:03}\r\n").as_bytes());
        }
    }

    #[test]
    fn abs_lines_stay_on_their_text_after_scrollback_fills() {
        let mut b = TermBridge::new(20, 5, 10);
        feed_lines(&mut b, 0..12);
        assert_eq!(b.history_size(), 8);
        assert_eq!(b.lines_evicted(), 0);
        // "w005" is on grid line 5 - 8 = -3.
        let abs = (-3 + b.abs_line_base() as i64) as usize;
        assert_eq!(text_at(&b, abs, 0, 3), "w005");

        // Fill scrollback; history stops growing and lines are evicted.
        feed_lines(&mut b, 12..15);
        assert_eq!(b.history_size(), 10);
        assert_eq!(b.lines_evicted(), 1);
        assert_eq!(text_at(&b, abs, 0, 3), "w005");

        // One big chunk of several lines at once.
        let chunk: String = (15..18).map(|i| format!("w{i:03}\r\n")).collect();
        b.feed(chunk.as_bytes());
        assert_eq!(b.lines_evicted(), 4);
        assert_eq!(text_at(&b, abs, 0, 3), "w005");

        // Once w005 itself is evicted its position yields nothing, never
        // another line's text.
        feed_lines(&mut b, 18..30);
        assert!(abs < b.lines_evicted());
        assert_eq!(text_at(&b, abs, 0, 3), "");
    }

    #[test]
    fn a_chunk_longer_than_scrollback_still_counts() {
        let mut b = TermBridge::new(20, 5, 10);
        feed_lines(&mut b, 0..3);
        let base0 = b.abs_line_base();
        let chunk: String = (3..103).map(|i| format!("w{i:03}\r\n")).collect();
        b.feed(chunk.as_bytes());
        // The cursor starts on row 3 of 5, so the first newline moves it to
        // the last row and the other 99 each push a line: base moves by 99.
        assert_eq!(b.abs_line_base(), base0 + 99);
        let m = b.search("w099");
        assert_eq!(m.len(), 1);
        let abs = (m[0].start.line as i64 + b.abs_line_base() as i64) as usize;
        feed_lines(&mut b, 103..106);
        assert_eq!(text_at(&b, abs, 0, 3), "w099");
    }

    #[test]
    fn scrolled_back_view_and_matches_stay_put_when_full() {
        let mut b = TermBridge::new(20, 5, 10);
        feed_lines(&mut b, 0..40);
        b.scroll(4);
        let top = |b: &TermBridge| -> String {
            (0..4)
                .map(|c| b.snapshot().cell(0, c).map(|x| x.ch).unwrap_or(' '))
                .collect()
        };
        let shown = top(&b);
        let m = b.search(&shown);
        assert_eq!(m.len(), 1);
        let abs = (m[0].start.line as i64 + b.abs_line_base() as i64) as usize;
        let offset = b.display_offset();

        feed_lines(&mut b, 40..43);
        assert_eq!(top(&b), shown, "viewport stays on its text");
        assert_eq!(b.display_offset(), offset + 3);
        assert_eq!(text_at(&b, abs, 0, 3), shown);

        // When the shown text is evicted the view pins to the oldest line.
        feed_lines(&mut b, 43..60);
        assert_eq!(b.display_offset(), b.history_size());
        assert_eq!(text_at(&b, abs, 0, 3), "");
    }

    #[test]
    fn live_view_stays_live_while_counting() {
        let mut b = TermBridge::new(20, 5, 10);
        feed_lines(&mut b, 0..30);
        assert_eq!(b.display_offset(), 0);
        assert_eq!(b.snapshot().display_offset, 0);
    }

    #[test]
    fn alt_screen_output_does_not_disturb_the_count() {
        let mut b = TermBridge::new(20, 5, 10);
        feed_lines(&mut b, 0..20);
        let m = b.search("w012");
        let abs = (m[0].start.line as i64 + b.abs_line_base() as i64) as usize;
        // Enter the alt screen, scroll it a lot, then come back in pieces.
        b.feed(b"w020\r\n\x1b[?1049h");
        for _ in 0..30 {
            b.feed(b"alt\r\n");
        }
        b.feed(b"\x1b[?1049l");
        assert_eq!(b.display_offset(), 0, "primary view is live again");
        assert_eq!(text_at(&b, abs, 0, 3), "w012");
        feed_lines(&mut b, 21..23);
        assert_eq!(text_at(&b, abs, 0, 3), "w012");
    }

    #[test]
    fn clearing_history_evicts_it() {
        let mut b = TermBridge::new(20, 5, 10);
        feed_lines(&mut b, 0..8);
        let m = b.search("w001");
        let abs = (m[0].start.line as i64 + b.abs_line_base() as i64) as usize;
        let base = b.abs_line_base();
        b.feed(b"\x1b[3J");
        assert_eq!(b.history_size(), 0);
        assert_eq!(b.abs_line_base(), base, "screen lines keep their numbers");
        assert_eq!(text_at(&b, abs, 0, 3), "");
    }

    #[test]
    fn search_treats_query_literally() {
        let mut b = TermBridge::new(80, 10, DEFAULT_SCROLLBACK);
        b.feed(b"a.b (x) a+b\r\n");
        assert_eq!(b.search("(x)").len(), 1);
        assert_eq!(b.search("a.b").len(), 1);
        assert_eq!(b.search("").len(), 0);
    }

    #[test]
    fn scroll_changes_display_offset() {
        let mut b = TermBridge::new(20, 5, 100);
        for i in 0..30 {
            b.feed(format!("l{i}\r\n").as_bytes());
        }
        assert_eq!(b.snapshot().display_offset, 0);
        b.scroll(3);
        assert_eq!(b.snapshot().display_offset, 3);
        b.scroll(-10);
        assert_eq!(b.snapshot().display_offset, 0);
        b.scroll(7);
        b.scroll_to_bottom();
        assert_eq!(b.snapshot().display_offset, 0);
    }

    #[test]
    fn resize_changes_snapshot_dims() {
        let mut b = TermBridge::new(80, 24, 100);
        b.resize(100, 30);
        let s = b.snapshot();
        assert_eq!((s.cols, s.rows, s.cells.len()), (100, 30, 3000));
    }

    #[test]
    fn large_first_chunk_then_stream_matches_single_feed() {
        // Snapshot-sized first chunk (about 1 MiB here) then live chunks split
        // mid-escape and mid-UTF-8.
        let mut big = Vec::new();
        while big.len() < 1 << 20 {
            big.extend_from_slice("line \u{e9}\u{1f600} of output\r\n".as_bytes());
        }
        let tail = b"\x1b[31mred\x1b[0m done \xf0\x9f\x98\x80";
        let mut a = TermBridge::new(80, 24, DEFAULT_SCROLLBACK);
        a.feed(&big);
        for chunk in tail.chunks(3) {
            a.feed(chunk);
        }
        let mut whole = big.clone();
        whole.extend_from_slice(tail);
        let mut c = TermBridge::new(80, 24, DEFAULT_SCROLLBACK);
        c.feed(&whole);
        let (sa, sc) = (a.snapshot(), c.snapshot());
        assert_eq!(sa.cells, sc.cells);
        assert_eq!(row_text(&sa, sa.cursor.row), "red done \u{1f600}");
    }

    #[test]
    fn sync_update_held_until_timeout() {
        let mut b = TermBridge::new(20, 5, 100);
        assert!(b.sync_deadline().is_none());
        b.feed(b"\x1b[?2026hhello");
        assert_eq!(
            b.snapshot().cell(0, 0).unwrap().ch,
            ' ',
            "buffered during sync"
        );
        let deadline = b.sync_deadline().expect("deadline set");
        b.tick_at(deadline - std::time::Duration::from_millis(1));
        assert_eq!(b.snapshot().cell(0, 0).unwrap().ch, ' ');
        b.tick_at(deadline);
        assert_eq!(row_text(&b.snapshot(), 0), "hello");
        assert!(b.sync_deadline().is_none());
    }

    #[test]
    fn sync_update_flushes_on_esu() {
        let mut b = TermBridge::new(20, 5, 100);
        b.feed(b"\x1b[?2026hhi\x1b[?2026l");
        assert_eq!(row_text(&b.snapshot(), 0), "hi");
    }

    #[test]
    fn dim_strikethrough_hidden_attributes() {
        let mut b = TermBridge::new(20, 5, 100);
        b.feed(b"\x1b[2mA\x1b[0m\x1b[9mB\x1b[0m\x1b[8mC\x1b[0mD");
        let s = b.snapshot();
        assert!(s.cell(0, 0).unwrap().dim);
        assert!(s.cell(0, 1).unwrap().strikethrough);
        assert!(s.cell(0, 2).unwrap().hidden);
        let d = s.cell(0, 3).unwrap();
        assert!(!d.dim && !d.strikethrough && !d.hidden);
    }

    #[test]
    fn osc_background_query_gets_reply() {
        let mut b = TermBridge::new(20, 5, 100);
        b.feed(b"\x1b]11;?\x07");
        let w = String::from_utf8(b.take_pty_writes()).unwrap();
        assert!(w.contains("rgb:"), "reply was {w:?}");
        // Program override wins over the palette.
        b.feed(b"\x1b]11;rgb:10/20/30\x07\x1b]11;?\x07");
        let w = String::from_utf8(b.take_pty_writes()).unwrap();
        assert!(w.contains("rgb:1010/2020/3030"), "reply was {w:?}");
    }

    #[test]
    fn text_area_size_query_gets_reply() {
        let mut b = TermBridge::new(80, 24, 100);
        b.set_cell_size(9, 18);
        b.feed(b"\x1b[14t");
        assert_eq!(b.take_pty_writes(), b"\x1b[4;432;720t");
        b.feed(b"\x1b[18t");
        assert_eq!(b.take_pty_writes(), b"\x1b[8;24;80t");
    }

    #[test]
    fn wide_char_flagged() {
        let mut b = TermBridge::new(20, 5, 100);
        b.feed("a\u{4e2d}b".as_bytes());
        let s = b.snapshot();
        assert!(s.cell(0, 1).unwrap().wide);
        assert_eq!(s.cell(0, 3).unwrap().ch, 'b');
    }
}
