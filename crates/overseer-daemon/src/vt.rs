//! Small scanners over raw PTY output.
//!
//! [`ModeTracker`] follows the terminal modes a program switches with escape
//! sequences (DECSET/DECRST private modes, the keypad mode, the cursor style
//! and the kitty keyboard flags) and can write a preamble that puts a fresh
//! terminal into the same state. The ring feeds it the bytes it evicts, so
//! the preamble restores the state at the oldest byte the ring still holds,
//! and replaying preamble plus ring gives the modes the full stream would.
//!
//! [`strip_queries`] removes terminal queries (device attributes, status and
//! cursor reports, colour queries and so on) from a replayed snapshot, so the
//! terminal that replays it does not answer questions the program asked long
//! ago.

use std::collections::BTreeMap;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;
/// Longest CSI parameter string tracked; longer sequences are ignored.
const MAX_PARAMS: usize = 64;
/// Kitty keyboard stack depth kept per screen (alacritty uses 4096, real
/// programs push one or two).
const MAX_KITTY_DEPTH: usize = 16;

/// DEC private modes worth restoring, in the order they are written.
/// Alt screen (1049) is handled separately because of its ordering rules.
/// The three mouse tracking modes (1000, 1002, 1003) are mutually exclusive.
const TRACKED_MODES: &[u16] = &[
    1,    // DECCKM application cursor keys
    5,    // DECSCNM reverse video
    7,    // DECAWM autowrap
    25,   // DECTCEM cursor visible
    1000, // mouse: report clicks
    1002, // mouse: report drags
    1003, // mouse: report all motion
    1004, // focus in and out reports
    1005, // mouse: UTF-8 coordinates
    1006, // mouse: SGR coordinates
    1007, // alternate scroll
    1015, // mouse: urxvt coordinates
    2004, // bracketed paste
];
const MOUSE_MODES: [u16; 3] = [1000, 1002, 1003];
/// Alternate screen variants. Alacritty implements only 1049; 47 and 1047
/// are followed too so the preamble is right for terminals that know them.
const ALT_MODES: [u16; 3] = [47, 1047, 1049];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    #[default]
    Ground,
    Esc,
    Csi,
    /// OSC, DCS, SOS, PM or APC body; ended by BEL (OSC only) or ST.
    Str,
    StrEsc,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Kitty {
    stack: Vec<u8>,
}

/// Follows terminal mode changes in a byte stream that may be split
/// anywhere, including inside an escape sequence.
#[derive(Debug, Clone, Default)]
pub struct ModeTracker {
    state: State,
    params: Vec<u8>,
    intermediates: Vec<u8>,
    overflow: bool,
    /// Last explicit value of each tracked private mode. Modes the stream
    /// never touched are absent and left at the terminal's default.
    modes: BTreeMap<u16, bool>,
    /// Some once ESC = or ESC > was seen.
    keypad_app: Option<bool>,
    /// Some once DECSCUSR (CSI Ps SP q) was seen.
    cursor_style: Option<u16>,
    alt: bool,
    kitty_main: Kitty,
    kitty_alt: Kitty,
}

impl ModeTracker {
    pub fn new() -> ModeTracker {
        ModeTracker::default()
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.byte(b);
        }
    }

    fn byte(&mut self, b: u8) {
        match self.state {
            State::Ground => {
                if b == ESC {
                    self.state = State::Esc;
                }
            }
            State::Esc => self.esc(b),
            State::Csi => self.csi(b),
            State::Str => match b {
                BEL | CAN | SUB => self.state = State::Ground,
                ESC => self.state = State::StrEsc,
                _ => {}
            },
            State::StrEsc => {
                // ST ends the string; any other ESC aborts it and starts a
                // new sequence, as vte does.
                if b == b'\\' {
                    self.state = State::Ground;
                } else {
                    self.state = State::Esc;
                    self.esc(b);
                }
            }
        }
    }

    fn esc(&mut self, b: u8) {
        self.state = State::Ground;
        match b {
            b'[' => {
                self.params.clear();
                self.intermediates.clear();
                self.overflow = false;
                self.state = State::Csi;
            }
            b']' | b'P' | b'X' | b'^' | b'_' => self.state = State::Str,
            b'=' => self.keypad_app = Some(true),
            b'>' => self.keypad_app = Some(false),
            b'c' => self.reset(),
            ESC => self.state = State::Esc,
            // ESC followed by an intermediate (charset designation and the
            // like) takes one more byte; nothing there is tracked.
            0x20..=0x2f => self.state = State::Esc,
            _ => {}
        }
    }

    fn csi(&mut self, b: u8) {
        match b {
            0x30..=0x3f => {
                if self.params.len() < MAX_PARAMS {
                    self.params.push(b);
                } else {
                    self.overflow = true;
                }
            }
            0x20..=0x2f => {
                if self.intermediates.len() < 2 {
                    self.intermediates.push(b);
                } else {
                    self.overflow = true;
                }
            }
            0x40..=0x7e => {
                self.state = State::Ground;
                if !self.overflow {
                    self.dispatch_csi(b);
                }
            }
            CAN | SUB => self.state = State::Ground,
            ESC => self.state = State::Esc,
            // Other C0 controls run inside a CSI without ending it.
            _ => {}
        }
    }

    fn dispatch_csi(&mut self, fin: u8) {
        let (prefix, rest) = match self.params.first() {
            Some(&p @ (b'?' | b'>' | b'<' | b'=')) => (Some(p), &self.params[1..]),
            _ => (None, &self.params[..]),
        };
        let nums: Vec<Option<u32>> = rest
            .split(|&c| c == b';')
            .map(|s| std::str::from_utf8(s).ok().and_then(|s| s.parse().ok()))
            .collect();
        let first = nums.first().copied().flatten();
        match (prefix, self.intermediates.as_slice(), fin) {
            (Some(b'?'), [], b'h' | b'l') => {
                let on = fin == b'h';
                for n in nums.into_iter().flatten() {
                    if let Ok(n) = u16::try_from(n) {
                        self.set_private(n, on);
                    }
                }
            }
            (None, [b' '], b'q') => {
                self.cursor_style = Some(first.unwrap_or(0).min(u16::MAX as u32) as u16);
            }
            (Some(b'>'), [], b'u') => {
                let flags = first.unwrap_or(0) as u8;
                let k = self.kitty();
                if k.stack.len() >= MAX_KITTY_DEPTH {
                    k.stack.remove(0);
                }
                k.stack.push(flags);
            }
            (Some(b'<'), [], b'u') => {
                let n = first.unwrap_or(1).max(1) as usize;
                let k = self.kitty();
                let len = k.stack.len().saturating_sub(n);
                k.stack.truncate(len);
            }
            (Some(b'='), [], b'u') => {
                let flags = first.unwrap_or(0) as u8;
                let how = nums.get(1).copied().flatten().unwrap_or(1);
                let k = self.kitty();
                if k.stack.is_empty() {
                    k.stack.push(0);
                }
                let top = k.stack.last_mut().expect("just ensured");
                match how {
                    2 => *top |= flags,
                    3 => *top &= !flags,
                    _ => *top = flags,
                }
            }
            _ => {}
        }
    }

    fn kitty(&mut self) -> &mut Kitty {
        if self.alt {
            &mut self.kitty_alt
        } else {
            &mut self.kitty_main
        }
    }

    fn set_private(&mut self, n: u16, on: bool) {
        if ALT_MODES.contains(&n) {
            if on != self.alt {
                self.alt = on;
                // The kitty spec (and alacritty) keep one stack per screen.
                if !on {
                    self.kitty_alt = Kitty::default();
                }
            }
            return;
        }
        if !TRACKED_MODES.contains(&n) {
            return;
        }
        if MOUSE_MODES.contains(&n) && on {
            for m in MOUSE_MODES {
                self.modes.insert(m, false);
            }
        }
        self.modes.insert(n, on);
    }

    /// RIS (ESC c): back to power-on state.
    fn reset(&mut self) {
        let state = self.state;
        *self = ModeTracker::default();
        self.state = state;
    }

    /// Whether the alternate screen is active.
    pub fn alt_screen(&self) -> bool {
        self.alt
    }

    /// Value last set for private mode `n`, if the stream set it.
    pub fn mode(&self, n: u16) -> Option<bool> {
        if ALT_MODES.contains(&n) {
            return Some(self.alt);
        }
        self.modes.get(&n).copied()
    }

    /// Escape sequences that take a terminal in its power-on state to the
    /// tracked state. Empty when the stream changed nothing tracked.
    ///
    /// Order: the main screen's kitty flags, then the global modes, then the
    /// switch to the alternate screen (if active), then the alternate
    /// screen's kitty flags. Bytes replayed after the preamble then land on
    /// the screen they were written to.
    pub fn preamble(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let kitty = |out: &mut Vec<u8>, k: &Kitty| {
            for f in &k.stack {
                out.extend_from_slice(format!("\x1b[>{f}u").as_bytes());
            }
        };
        kitty(&mut out, &self.kitty_main);
        // Mouse modes first, resets before sets, so exclusivity works out.
        let mut ordered: Vec<(u16, bool)> = self.modes.iter().map(|(&n, &on)| (n, on)).collect();
        ordered.sort_by_key(|&(n, on)| (!MOUSE_MODES.contains(&n), on, n));
        for (n, on) in ordered {
            let c = if on { 'h' } else { 'l' };
            out.extend_from_slice(format!("\x1b[?{n}{c}").as_bytes());
        }
        if let Some(app) = self.keypad_app {
            out.extend_from_slice(if app { b"\x1b=" } else { b"\x1b>" });
        }
        if let Some(style) = self.cursor_style {
            out.extend_from_slice(format!("\x1b[{style} q").as_bytes());
        }
        if self.alt {
            out.extend_from_slice(b"\x1b[?1049h");
            kitty(&mut out, &self.kitty_alt);
        }
        out
    }
}

/// Whether a complete CSI sequence is a query the terminal would answer.
fn csi_is_query(params: &[u8], intermediates: &[u8], fin: u8) -> bool {
    let (prefix, rest) = match params.first() {
        Some(&p @ (b'?' | b'>' | b'<' | b'=')) => (Some(p), &params[1..]),
        _ => (None, params),
    };
    let first: Option<u32> = rest
        .split(|&c| c == b';')
        .next()
        .and_then(|s| std::str::from_utf8(s).ok())
        .and_then(|s| s.parse().ok());
    match (prefix, intermediates, fin) {
        // DA1, DA2, DA3.
        (_, [], b'c') => true,
        // DSR: status, cursor position and the DEC variants. (CSI > n is
        // xterm's modifier reset, not a query.)
        (None | Some(b'?'), [], b'n') => true,
        // Window and text area size reports.
        (None, [], b't') => matches!(first, Some(11 | 13 | 14 | 15 | 16 | 18 | 19 | 20 | 21)),
        // Kitty keyboard flags query.
        (Some(b'?'), [], b'u') => true,
        // DECRQM, ANSI and DEC.
        (None | Some(b'?'), [b'$'], b'p') => true,
        // XTVERSION.
        (Some(b'>'), [], b'q') => true,
        // XTQMODKEYS.
        (Some(b'?'), [], b'm') => true,
        _ => false,
    }
}

/// Whether an OSC body (without the leading `ESC ]` and the terminator) asks
/// for a value: colour queries (4, 5, 10 to 19 and friends) and clipboard
/// reads (52) carry a `?` field.
fn osc_is_query(body: &[u8]) -> bool {
    let mut fields = body.split(|&c| c == b';');
    let Some(Ok(Ok(ps))) = fields
        .next()
        .map(|s| std::str::from_utf8(s).map(str::parse::<u32>))
    else {
        return false;
    };
    matches!(ps, 4 | 5 | 10..=19 | 52) && fields.any(|f| f == b"?")
}

/// Whether a DCS body asks for a value: DECRQSS (`$q`) or XTGETTCAP (`+q`).
fn dcs_is_query(body: &[u8]) -> bool {
    body.starts_with(b"$q") || body.starts_with(b"+q")
}

/// Returns `data` without the complete terminal queries in it. Sequences cut
/// off at the end are kept, so a query whose tail arrives live is still
/// answered.
pub fn strip_queries(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i] != ESC || i + 1 >= data.len() {
            out.push(data[i]);
            i += 1;
            continue;
        }
        let start = i;
        let (end, drop) = match data[i + 1] {
            b'[' => {
                let mut j = i + 2;
                let p0 = j;
                while j < data.len() && (0x30..=0x3f).contains(&data[j]) {
                    j += 1;
                }
                let p1 = j;
                while j < data.len() && (0x20..=0x2f).contains(&data[j]) {
                    j += 1;
                }
                if j < data.len() && (0x40..=0x7e).contains(&data[j]) {
                    (j + 1, csi_is_query(&data[p0..p1], &data[p1..j], data[j]))
                } else {
                    // Incomplete, or broken by a control byte: keep as is.
                    (j, false)
                }
            }
            kind @ (b']' | b'P') => {
                let body_start = i + 2;
                let mut j = body_start;
                let mut found = None;
                while j < data.len() {
                    match data[j] {
                        BEL if kind == b']' => {
                            found = Some((j, j + 1));
                            break;
                        }
                        ESC if data.get(j + 1) == Some(&b'\\') => {
                            found = Some((j, j + 2));
                            break;
                        }
                        ESC | CAN | SUB => break,
                        _ => j += 1,
                    }
                }
                match found {
                    Some((body_end, end)) => {
                        let body = &data[body_start..body_end];
                        let q = if kind == b']' {
                            osc_is_query(body)
                        } else {
                            dcs_is_query(body)
                        };
                        (end, q)
                    }
                    None => (j, false),
                }
            }
            // DECID, an old spelling of DA1.
            b'Z' => (i + 2, true),
            _ => (i + 2, false),
        };
        if !drop {
            out.extend_from_slice(&data[start..end]);
        }
        i = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracked(chunks: &[&[u8]]) -> ModeTracker {
        let mut t = ModeTracker::new();
        for c in chunks {
            t.feed(c);
        }
        t
    }

    #[test]
    fn tracks_modes_split_across_chunks() {
        let t = tracked(&[b"ab\x1b", b"[?20", b"04;10", b"04h\x1b[?1h", b"\x1b[?1l"]);
        assert_eq!(t.mode(2004), Some(true));
        assert_eq!(t.mode(1004), Some(true));
        assert_eq!(t.mode(1), Some(false));
        assert_eq!(t.mode(1006), None);
        assert_eq!(t.preamble(), b"\x1b[?1l\x1b[?1004h\x1b[?2004h");
    }

    #[test]
    fn untouched_stream_needs_no_preamble() {
        assert!(tracked(&[b"hello\r\n\x1b[31mred\x1b[0m"])
            .preamble()
            .is_empty());
    }

    #[test]
    fn mouse_modes_are_exclusive() {
        let t = tracked(&[b"\x1b[?1000h\x1b[?1003h\x1b[?1006h"]);
        assert_eq!(t.mode(1000), Some(false));
        assert_eq!(t.mode(1003), Some(true));
        assert_eq!(
            t.preamble(),
            b"\x1b[?1000l\x1b[?1002l\x1b[?1003h\x1b[?1006h".to_vec()
        );
    }

    #[test]
    fn mode_text_inside_strings_is_ignored() {
        let t = tracked(&[b"\x1b]0;title [?2004h\x07", b"\x1bP$q[?25l\x1b\\"]);
        assert!(t.preamble().is_empty(), "{:?}", t.preamble());
        // As in vte, an ESC inside a string ends it and starts a sequence.
        let t = tracked(&[b"\x1b]0;title \x1b[?2004h"]);
        assert_eq!(t.mode(2004), Some(true));
    }

    #[test]
    fn reset_clears_everything() {
        let t = tracked(&[b"\x1b[?2004h\x1b[?1049h\x1b=", b"\x1bc"]);
        assert!(t.preamble().is_empty());
        assert!(!t.alt_screen());
    }

    #[test]
    fn alt_screen_comes_after_global_modes_with_its_kitty_flags() {
        let t = tracked(&[b"\x1b[>1u\x1b[?1049h\x1b[>3u\x1b[?2004h\x1b= \x1b[5 q"]);
        assert!(t.alt_screen());
        assert_eq!(
            t.preamble(),
            b"\x1b[>1u\x1b[?2004h\x1b=\x1b[5 q\x1b[?1049h\x1b[>3u".to_vec()
        );
        // Leaving the alt screen drops its stack and keeps the main one.
        let t = tracked(&[b"\x1b[>1u\x1b[?1049h\x1b[>3u\x1b[?1049l"]);
        assert_eq!(t.preamble(), b"\x1b[>1u".to_vec());
    }

    #[test]
    fn kitty_pop_and_set() {
        let t = tracked(&[b"\x1b[>1u\x1b[>5u\x1b[<u\x1b[=8;2u"]);
        assert_eq!(t.preamble(), b"\x1b[>9u".to_vec());
        let t = tracked(&[b"\x1b[>1u\x1b[>5u\x1b[<9u"]);
        assert!(t.preamble().is_empty());
    }

    #[test]
    fn strips_queries_and_keeps_the_rest() {
        let input: &[u8] = b"a\x1b[c\x1b[>0c\x1b[6n\x1b[?6n\x1b[5nb\x1b[14t\x1b[18t\x1b[8;24;80t\
\x1b]11;?\x07\x1b]10;?\x1b\\\x1b]4;1;?\x07\x1b]4;1;rgb:00/00/00\x07\x1b]0;title\x07\
\x1b[?u\x1b[>1u\x1b[?2004$p\x1b[>q\x1bP$qm\x1b\\\x1bP+q544e\x1b\\\x1bZ\x1b[31mc";
        let out = strip_queries(input);
        assert_eq!(
            out,
            b"ab\x1b[8;24;80t\x1b]4;1;rgb:00/00/00\x07\x1b]0;title\x07\x1b[>1u\x1b[31mc".to_vec()
        );
    }

    #[test]
    fn strip_keeps_incomplete_tail() {
        assert_eq!(strip_queries(b"x\x1b[6"), b"x\x1b[6".to_vec());
        assert_eq!(strip_queries(b"x\x1b]11;?"), b"x\x1b]11;?".to_vec());
        assert_eq!(strip_queries(b"x\x1b"), b"x\x1b".to_vec());
    }
}
