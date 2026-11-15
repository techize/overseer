//! Spots Claude's folder-trust question in a slot's output (phase 2 F6).
//!
//! Claude asks it before the session starts, so no hook fires and the card
//! would stay plain Running. The scan reads letters only: the TUI places
//! words with cursor moves rather than spaces, and colours them with escape
//! sequences.

use overseer_proto::{Activity, ActivityKind};

/// What the card says while the question is up.
pub const REASON: &str = "Claude asks whether to trust this folder";

/// Letters-only forms of the question, old and new wording.
const NEEDLES: [&str; 2] = [
    "isthisaprojectyoucreatedoroneyoutrust",
    "doyoutrustthefilesinthisfolder",
];

/// Enough letters to hold the longest needle across chunk boundaries.
const KEEP: usize = 256;

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Esc {
    #[default]
    Text,
    /// After ESC.
    Start,
    /// Inside `ESC [ ...`, up to a final byte.
    Csi,
    /// Inside `ESC ] ...`, up to BEL or `ESC \`.
    Osc,
    /// ESC seen inside an OSC.
    OscEsc,
}

/// Feeds output in chunks and says when the question has appeared.
#[derive(Default)]
pub struct TrustScan {
    esc: Esc,
    letters: String,
}

impl TrustScan {
    /// True once the output so far contains the question.
    pub fn feed(&mut self, bytes: &[u8]) -> bool {
        for &b in bytes {
            self.esc = match (self.esc, b) {
                (Esc::Text, 0x1b) => Esc::Start,
                (Esc::Text, b) => {
                    if b.is_ascii_alphabetic() {
                        self.letters.push(b.to_ascii_lowercase() as char);
                    }
                    Esc::Text
                }
                (Esc::Start, b'[') => Esc::Csi,
                (Esc::Start, b']') => Esc::Osc,
                (Esc::Start, _) => Esc::Text,
                (Esc::Csi, 0x40..=0x7e) => Esc::Text,
                (Esc::Csi, _) => Esc::Csi,
                (Esc::Osc, 0x07) => Esc::Text,
                (Esc::Osc, 0x1b) => Esc::OscEsc,
                (Esc::Osc, _) => Esc::Osc,
                (Esc::OscEsc, _) => Esc::Text,
            };
        }
        let found = NEEDLES.iter().any(|n| self.letters.contains(n));
        if self.letters.len() > KEEP {
            self.letters.drain(..self.letters.len() - KEEP);
        }
        found
    }
}

/// The activity to show while the question waits.
pub fn activity(now: i64) -> Activity {
    Activity {
        kind: ActivityKind::NeedsYou,
        since: now,
        now_doing: None,
        reason: Some(REASON.into()),
        idle_wait: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_question_drawn_with_cursor_moves_and_colour() {
        let mut s = TrustScan::default();
        let screen = b"\x1b[2J\x1b[H\x1b]0;claude\x07\x1b[1mIs\x1b[1Cthis\x1b[1Ca\x1b[1Cproject\x1b[1Cyou\x1b[1Ccreated\x1b[0m or one you trust?";
        assert!(s.feed(screen));
    }

    #[test]
    fn finds_the_question_split_across_chunks_and_inside_an_escape() {
        let mut s = TrustScan::default();
        assert!(!s.feed(b"Do you trust the fi\x1b[3"));
        assert!(s.feed(b"8;5;12mles in this folder?"));
    }

    #[test]
    fn the_older_wording_counts_too() {
        assert!(TrustScan::default().feed(b"Do you trust the files in this folder?"));
    }

    #[test]
    fn ordinary_output_does_not_match() {
        let mut s = TrustScan::default();
        assert!(!s.feed(b"Welcome to Claude Code\r\n> do you trust me\r\n"));
        // A long stream keeps the buffer short but still finds a late match.
        for _ in 0..1000 {
            assert!(!s.feed(b"lorem ipsum dolor sit amet "));
        }
        assert!(s.letters.len() <= KEEP);
        assert!(s.feed(b"Is this a project you created or one you trust?"));
    }
}
