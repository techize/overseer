//! Bridge between PTY output bytes and an `alacritty_terminal` model.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::Direction;
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection as TermSelection, SelectionType};
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
    /// The terminal-owned selection as an inclusive range of grid positions
    /// (viewport row = `line + display_offset`), or None.
    pub selection: Option<Selection>,
    /// `TermBridge::content_version` when this was taken.
    pub content_version: u64,
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

    fn from_point(p: Point) -> GridPos {
        GridPos {
            line: p.line.0,
            col: p.column.0,
        }
    }
}

/// How a mouse selection grows: by cell, by word, or by whole line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionKind {
    Simple,
    Word,
    Line,
}

/// Which half of a cell the pointer is over. A simple selection covers a
/// cell only once the pointer has crossed into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellSide {
    Left,
    Right,
}

impl From<CellSide> for Side {
    fn from(s: CellSide) -> Side {
        match s {
            CellSide::Left => Side::Left,
            CellSide::Right => Side::Right,
        }
    }
}

/// Characters that end a word for double-click selection. Unlike
/// alacritty's default, ':' is part of a word (so `path:line` and URLs select
/// whole) and ';' is not.
pub const WORD_SEPARATORS: &str = ",│`|\"' ()[]{}<>\t;";

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
    version: u64,
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
            semantic_escape_chars: WORD_SEPARATORS.to_owned(),
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
            version: 0,
        }
    }

    pub fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
    }

    /// Feed raw PTY bytes. Chunks may be any size and may split escape
    /// sequences or UTF-8 sequences anywhere.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.version += 1;
        self.processor.advance(&mut self.term, bytes);
        self.answer_color_requests();
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
            self.version += 1;
            self.processor.stop_sync(&mut self.term);
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
        self.version += 1;
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
            selection: self.selection_range(),
            content_version: self.version,
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

    /// Bumped whenever content may have moved: output, a flushed
    /// synchronized update, or a resize. Scrolling the view does not bump it.
    pub fn content_version(&self) -> u64 {
        self.version
    }

    /// Starts a selection at `pos`, replacing any other. The terminal owns
    /// it from here: alacritty moves it with scrolling output, and clamps,
    /// drops or clears it on eviction, screen clears, alt-screen switches,
    /// reset and resize.
    pub fn start_selection(&mut self, kind: SelectionKind, pos: GridPos, side: CellSide) {
        let ty = match kind {
            SelectionKind::Simple => SelectionType::Simple,
            SelectionKind::Word => SelectionType::Semantic,
            SelectionKind::Line => SelectionType::Lines,
        };
        let point = self.clamp_point(pos);
        self.term.selection = Some(TermSelection::new(ty, point, side.into()));
    }

    /// Moves the selection's moving end to `pos`.
    pub fn update_selection(&mut self, pos: GridPos, side: CellSide) {
        let point = self.clamp_point(pos);
        if let Some(sel) = self.term.selection.as_mut() {
            sel.update(point, side.into());
        }
    }

    pub fn clear_selection(&mut self) {
        self.term.selection = None;
    }

    /// The selection as an inclusive range, or None when there is none or
    /// it is empty (a plain click that was not dragged).
    pub fn selection_range(&self) -> Option<Selection> {
        let r = self.term.selection.as_ref()?.to_range(&self.term)?;
        Some(Selection {
            start: GridPos::from_point(r.start),
            end: GridPos::from_point(r.end),
        })
    }

    /// The selected text, or None when nothing is selected.
    pub fn selection_text(&self) -> Option<String> {
        self.term.selection_to_string().filter(|t| !t.is_empty())
    }

    fn clamp_point(&self, pos: GridPos) -> Point {
        let top = self.term.topmost_line().0;
        let bottom = self.term.bottommost_line().0;
        Point::new(
            Line(pos.line.clamp(top, bottom)),
            Column(pos.col.min(self.term.last_column().0)),
        )
    }

    /// Text between two grid positions, inclusive, clamped to the grid.
    pub fn text_in(&self, sel: Selection) -> String {
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
            b.text_in(Selection {
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

    /// A bridge 20x4 with `sb` lines of scrollback showing
    /// "r{i} word{i}" for i in 0..n, cursor on an empty last line.
    fn lines(sb: usize, n: usize) -> TermBridge {
        let mut b = TermBridge::new(20, 4, sb);
        for i in 0..n {
            b.feed(format!("r{i} word{i}\r\n").as_bytes());
        }
        b
    }

    /// Double-click the word at grid (`line`, `col`) and return its text.
    fn pick_word(b: &mut TermBridge, line: i32, col: usize) -> String {
        b.start_selection(SelectionKind::Word, GridPos { line, col }, CellSide::Left);
        b.selection_text().expect("a word is selected")
    }

    /// The selection still copies `orig`, or nothing: never other text.
    fn same_or_nothing(b: &TermBridge, orig: &str, case: &str) {
        let t = b.selection_text();
        assert!(
            t.is_none() || t.as_deref() == Some(orig),
            "{case}: copied {t:?}, selected {orig:?}"
        );
    }

    #[test]
    fn word_selection_uses_our_separators() {
        let mut b = TermBridge::new(40, 3, 10);
        b.feed(b"see src/main.rs:12; (x) a,b");
        assert_eq!(pick_word(&mut b, 0, 6), "src/main.rs:12");
        assert_eq!(pick_word(&mut b, 0, 21), "x");
        b.start_selection(
            SelectionKind::Line,
            GridPos { line: 0, col: 3 },
            CellSide::Left,
        );
        assert_eq!(
            b.selection_text().as_deref(),
            Some("see src/main.rs:12; (x) a,b\n"),
        );
    }

    #[test]
    fn simple_selection_needs_a_drag() {
        let mut b = TermBridge::new(20, 3, 10);
        b.feed(b"hello world");
        let at = |col| GridPos { line: 0, col };
        b.start_selection(SelectionKind::Simple, at(0), CellSide::Left);
        assert_eq!(b.selection_text(), None, "a plain click selects nothing");
        assert_eq!(b.snapshot().selection, None);
        b.update_selection(at(4), CellSide::Right);
        assert_eq!(b.selection_text().as_deref(), Some("hello"));
        // Dragging back past the anchor still includes the anchor cell.
        b.start_selection(SelectionKind::Simple, at(10), CellSide::Right);
        b.update_selection(at(6), CellSide::Left);
        assert_eq!(b.selection_text().as_deref(), Some("world"));
        assert_eq!(
            b.snapshot().selection,
            Some(Selection {
                start: at(6),
                end: at(10)
            })
        );
        b.clear_selection();
        assert_eq!(b.selection_text(), None);
    }

    #[test]
    fn selection_follows_output_past_the_cap() {
        for sb in [1, 5] {
            let mut b = lines(sb, 10);
            // Screen rows 0..=2 hold r7..r9; select "word8" on row 1.
            let orig = pick_word(&mut b, 1, 5);
            assert_eq!(orig, "word8");
            b.feed(b"x\r\n");
            assert_eq!(b.selection_text().as_deref(), Some("word8"), "sb {sb}");
            let sel = b.snapshot().selection.unwrap();
            assert_eq!(sel.start.line, 0, "the range moved up with its text");
            // Push it all the way out of scrollback: nothing, never other text.
            for i in 0..(sb + 6) {
                b.feed(format!("y{i} more{i}\r\n").as_bytes());
                same_or_nothing(&b, &orig, &format!("sb {sb} push {i}"));
            }
            assert_eq!(b.selection_text(), None, "sb {sb}: evicted");
            assert_eq!(b.snapshot().selection, None);
        }
    }

    #[test]
    fn screen_clears_and_reset_never_copy_other_text() {
        let cases: &[&[u8]] = &[
            b"\x1b[H\x1b[2J\x1b[3J",
            b"\x1b[H\x1b[2J\x1b[3Jr8 word8 r8 word8\r\nnew\r\n",
            b"\x1b[3Jafter\r\nmore\r\n",
            b"\x1b[3J\x1b[H\x1b[2Jr0 word0\r\nr1 word1\r\nr2 word2\r\n",
            b"\x1bc",
            b"\x1bcr0 word0\r\nr1 word1\r\nr2 word2\r\nr3 word3\r\n",
        ];
        for sb in [1, 5] {
            for (n, case) in cases.iter().enumerate() {
                // Select on screen and, separately, in scrollback.
                for line in [1, -1] {
                    let mut b = lines(sb, 10);
                    let orig = pick_word(&mut b, line, 5);
                    b.feed(case);
                    same_or_nothing(&b, &orig, &format!("sb {sb} case {n} line {line}"));
                }
            }
        }
    }

    #[test]
    fn pushes_without_history_never_copy_other_text() {
        // History is empty, so every push goes into (and past) a tiny
        // scrollback in one piece, by every way a line can be pushed.
        let long = "z".repeat(20 * 6);
        let cases: Vec<Vec<u8>> = vec![
            format!("\x1b[4;1H{long}").into_bytes(),
            b"\x1b[4;1H\x0b\x0b\x0b\x0c\x0c\x0c\x0b".to_vec(),
            b"\x1b[4;1H\x1bD\x1bD\x1bD\x1bD\x1bD\x1bD".to_vec(),
            b"\x1b[4;1H\x1bE\x1bE\x1bE\x1bE\x1bE\x1bE".to_vec(),
            b"\x1b[6S".to_vec(),
            b"\x1b[1S".to_vec(),
        ];
        for sb in [1, 5] {
            for (n, case) in cases.iter().enumerate() {
                let mut b = TermBridge::new(20, 4, sb);
                b.feed(b"r0 word0\r\nr1 word1\r\nr2 word2");
                assert_eq!(b.history_size(), 0);
                let orig = pick_word(&mut b, 1, 5);
                assert_eq!(orig, "word1");
                b.feed(case);
                same_or_nothing(&b, &orig, &format!("sb {sb} case {n}"));
            }
        }
    }

    #[test]
    fn resize_keeps_or_drops_the_selection() {
        for sb in [1, 5] {
            for (cols, rows) in [(20, 2), (20, 1), (20, 8), (10, 4), (30, 3)] {
                for line in [0, 1, -1] {
                    let mut b = lines(sb, 12);
                    let orig = pick_word(&mut b, line, 5);
                    b.resize(cols, rows);
                    same_or_nothing(&b, &orig, &format!("sb {sb} {cols}x{rows} line {line}"));
                    b.feed(b"after\r\n");
                    same_or_nothing(&b, &orig, &format!("sb {sb} {cols}x{rows} then output"));
                }
            }
        }
    }

    #[test]
    fn alt_screen_and_resize_never_copy_other_text() {
        for sb in [1, 5] {
            let mut b = lines(sb, 12);
            let orig = pick_word(&mut b, 1, 5);
            b.feed(b"\x1b[?1049h");
            same_or_nothing(&b, &orig, "enter alt");
            b.resize(20, 8);
            b.feed(b"alt\r\nalt\r\n");
            same_or_nothing(&b, &orig, "alt output");
            b.feed(b"\x1b[?1049l");
            b.resize(20, 3);
            same_or_nothing(&b, &orig, "back and shrunk");
        }
    }

    #[test]
    fn content_version_moves_with_content_not_scrolling() {
        let mut b = lines(10, 10);
        let v = b.content_version();
        b.scroll(2);
        assert_eq!(b.content_version(), v);
        b.feed(b"x");
        assert!(b.content_version() > v);
        let v = b.content_version();
        b.resize(30, 5);
        assert!(b.content_version() > v);
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
