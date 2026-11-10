//! Keeps a mirrored session off the outer terminal's main screen.
//!
//! `overseer attach` puts the outer terminal on its alternate screen before
//! the replay, so the user's shell screen is untouched and comes back intact
//! at detach. The session's own screen switches (`?47`, `?1047`, `?1049`)
//! must then not reach the outer terminal, or a `?1049l` in the stream would
//! drop the mirror onto the user's main screen. [`ScreenFilter`] removes them
//! and stands in for each with a clear, which is what a switch to a fresh
//! screen looks like from inside one screen.

/// Sent once before the replay: save the window title, switch to the
/// alternate screen (saving the cursor) and clear it.
pub const ENTER: &[u8] = b"\x1b[22;0t\x1b[?1049h\x1b[H\x1b[2J";

/// The private modes that switch screens.
const SCREEN_MODES: [&[u8]; 3] = [b"47", b"1047", b"1049"];
/// Longest private-mode sequence held back while waiting for its end.
const MAX_PENDING: usize = 64;

/// Removes screen switches from a byte stream. Sequences split across
/// chunks are held back until they are complete.
#[derive(Default)]
pub struct ScreenFilter {
    pending: Vec<u8>,
}

impl ScreenFilter {
    pub fn new() -> ScreenFilter {
        ScreenFilter::default()
    }

    /// Returns the bytes to write to the outer terminal for `data`.
    pub fn feed(&mut self, data: &[u8]) -> Vec<u8> {
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(data);
        let mut out = Vec::with_capacity(buf.len());
        let mut i = 0;
        while i < buf.len() {
            if buf[i] != 0x1b {
                out.push(buf[i]);
                i += 1;
                continue;
            }
            match private_mode_at(&buf[i..]) {
                Scan::Other => {
                    out.push(buf[i]);
                    i += 1;
                }
                Scan::Incomplete => {
                    self.pending = buf[i..].to_vec();
                    return out;
                }
                Scan::Mode { len, params, set } => {
                    rewrite(&mut out, &buf[i..i + len], params, set);
                    i += len;
                }
            }
        }
        out
    }
}

enum Scan<'a> {
    /// Not a DEC private mode set or reset.
    Other,
    /// Could still become one when more bytes arrive.
    Incomplete,
    /// `ESC [ ? params h|l`, `len` bytes long.
    Mode {
        len: usize,
        params: &'a [u8],
        set: bool,
    },
}

/// Looks for `ESC [ ? <digits and ;> h|l` at the start of `s`.
fn private_mode_at(s: &[u8]) -> Scan<'_> {
    let prefix = b"\x1b[?";
    let n = s.len().min(prefix.len());
    if s[..n] != prefix[..n] {
        return Scan::Other;
    }
    if s.len() < prefix.len() {
        return Scan::Incomplete;
    }
    let mut j = prefix.len();
    while j < s.len() && (s[j].is_ascii_digit() || s[j] == b';') {
        j += 1;
    }
    match s.get(j) {
        None if j < MAX_PENDING => Scan::Incomplete,
        Some(&fin @ (b'h' | b'l')) => Scan::Mode {
            len: j + 1,
            params: &s[prefix.len()..j],
            set: fin == b'h',
        },
        _ => Scan::Other,
    }
}

/// Writes `seq` without its screen switches, plus a clear standing in for
/// them. Leaving the session's alternate screen also restores the cursor the
/// session saved when it entered.
fn rewrite(out: &mut Vec<u8>, seq: &[u8], params: &[u8], set: bool) {
    let (screen, keep): (Vec<&[u8]>, Vec<&[u8]>) = params
        .split(|&c| c == b';')
        .partition(|p| SCREEN_MODES.contains(p));
    if screen.is_empty() {
        out.extend_from_slice(seq);
        return;
    }
    if !keep.is_empty() {
        out.extend_from_slice(b"\x1b[?");
        out.extend_from_slice(&keep.join(&b';'));
        out.push(if set { b'h' } else { b'l' });
    }
    let save_cursor = screen.contains(&&b"1049"[..]);
    if set && save_cursor {
        out.extend_from_slice(b"\x1b7");
    }
    out.extend_from_slice(b"\x1b[H\x1b[2J");
    if !set && save_cursor {
        out.extend_from_slice(b"\x1b8");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&[u8]]) -> Vec<u8> {
        let mut f = ScreenFilter::new();
        chunks.iter().flat_map(|c| f.feed(c)).collect()
    }

    #[test]
    fn passes_text_and_other_sequences() {
        let s: &[u8] = b"hi\x1b[31mred\x1b[?25l\x1b[?2004h\x1b]0;t\x07\x1b[1049h";
        assert_eq!(run(&[s]), s.to_vec());
    }

    #[test]
    fn alt_screen_switches_become_clears() {
        assert_eq!(run(&[b"a\x1b[?1049hb"]), b"a\x1b7\x1b[H\x1b[2Jb".to_vec());
        assert_eq!(run(&[b"a\x1b[?1049lb"]), b"a\x1b[H\x1b[2J\x1b8b".to_vec());
        assert_eq!(
            run(&[b"\x1b[?47h\x1b[?1047l"]),
            b"\x1b[H\x1b[2J\x1b[H\x1b[2J".to_vec()
        );
    }

    #[test]
    fn other_modes_in_the_same_sequence_are_kept() {
        assert_eq!(
            run(&[b"\x1b[?1049;25;1006h"]),
            b"\x1b[?25;1006h\x1b7\x1b[H\x1b[2J".to_vec()
        );
        assert_eq!(run(&[b"\x1b[?10490h"]), b"\x1b[?10490h".to_vec());
    }

    #[test]
    fn switch_split_across_chunks() {
        assert_eq!(
            run(&[b"ab\x1b", b"[", b"?10", b"49l", b"cd"]),
            b"ab\x1b[H\x1b[2J\x1b8cd".to_vec()
        );
        // Held back until the sequence ends, then passed on unchanged.
        let mut f = ScreenFilter::new();
        assert_eq!(f.feed(b"x\x1b[?2"), b"x".to_vec());
        assert_eq!(f.feed(b"5h"), b"\x1b[?25h".to_vec());
    }

    #[test]
    fn broken_sequence_is_passed_through() {
        assert_eq!(run(&[b"\x1b[?10x49h"]), b"\x1b[?10x49h".to_vec());
        assert_eq!(run(&[b"\x1bc\x1b[2J"]), b"\x1bc\x1b[2J".to_vec());
    }
}
