//! Bridge between PTY output bytes and an `alacritty_terminal` model.

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
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
    pub italic: bool,
    pub underline: bool,
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

#[derive(Clone, Default)]
struct Listener {
    writes: Arc<Mutex<Vec<u8>>>,
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = event {
            if let Ok(mut w) = self.writes.lock() {
                w.extend_from_slice(text.as_bytes());
            }
        }
    }
}

pub struct TermBridge {
    term: Term<Listener>,
    processor: Processor,
    writes: Arc<Mutex<Vec<u8>>>,
    palette: Palette,
}

impl TermBridge {
    pub fn new(cols: usize, rows: usize, scrollback_lines: usize) -> Self {
        let listener = Listener::default();
        let writes = listener.writes.clone();
        let config = Config {
            scrolling_history: scrollback_lines,
            ..Config::default()
        };
        let term = Term::new(config, &TermSize::new(cols.max(1), rows.max(1)), listener);
        TermBridge {
            term,
            processor: Processor::new(),
            writes,
            palette: Palette::default(),
        }
    }

    pub fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
    }

    /// Feed raw PTY bytes. Chunks may be any size and may split escape
    /// sequences or UTF-8 sequences anywhere.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.processor.advance(&mut self.term, bytes);
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.term.resize(TermSize::new(cols.max(1), rows.max(1)));
    }

    /// Scroll the viewport. Positive moves toward older history.
    pub fn scroll(&mut self, delta_lines: i32) {
        self.term.scroll_display(Scroll::Delta(delta_lines));
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
                    italic: f.contains(Flags::ITALIC),
                    underline: f.intersects(Flags::ALL_UNDERLINES),
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
    fn wide_char_flagged() {
        let mut b = TermBridge::new(20, 5, 100);
        b.feed("a\u{4e2d}b".as_bytes());
        let s = b.snapshot();
        assert!(s.cell(0, 1).unwrap().wide);
        assert_eq!(s.cell(0, 3).unwrap().ch, 'b');
    }
}
