//! Reading Claude Code's input box from a slot's output bytes.
//!
//! The courier must know, before typing a relay, whether the input box is
//! empty, holds text Jonathan typed, or cannot be found (a permission or
//! question dialog replaces the box). The reader is pure: it replays bytes
//! into a terminal grid and classifies what it sees. Best effort, tuned to
//! Claude Code 2.1's prompt; anything it does not understand reads as
//! `NotFound`, and a relay then waits.
//!
//! It cannot detect: a placeholder drawn without the dim attribute (reads as
//! `Typed`, so relays wait), input that is only spaces (reads as `Empty`),
//! text hidden in vim normal mode or scrolled out of a box taller than the
//! screen, and a layout change in a future Claude Code (reads as `NotFound`).

use overseer_term::{Snapshot, TermBridge, TermModes};

const RULE: char = '\u{2500}';
const BORDER: char = '\u{2502}';

/// What the input box holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputBox {
    Empty,
    Typed,
    NotFound,
}

/// Replay `Slot::snapshot()` bytes (preamble plus ring) into a fresh grid.
/// Built only when a relay is due, never on every output chunk.
pub fn screen_of(snapshot_bytes: &[u8], cols: u16, rows: u16) -> (Snapshot, TermModes) {
    let mut bridge = TermBridge::new(cols as usize, rows as usize, 0);
    bridge.feed(snapshot_bytes);
    (bridge.snapshot(), bridge.modes())
}

fn row_cells(snap: &Snapshot, row: usize) -> &[overseer_term::Cell] {
    &snap.cells[row * snap.cols..(row + 1) * snap.cols]
}

/// A rule row: at least 80 percent of its non-blank cells, and at least half
/// the width, are `─`.
fn is_rule(snap: &Snapshot, row: usize) -> bool {
    let cells = row_cells(snap, row);
    let non_blank = cells.iter().filter(|c| !c.ch.is_whitespace()).count();
    let rules = cells.iter().filter(|c| c.ch == RULE).count();
    rules > 0 && rules * 2 >= snap.cols && rules * 5 >= non_blank * 4
}

/// The column just after the prompt mark (`>` or `❯`, after blanks and an
/// optional `│`) on this row, if it starts with one.
fn after_prompt_mark(cells: &[overseer_term::Cell]) -> Option<usize> {
    let mut i = 0;
    let skip_blanks = |i: &mut usize| {
        while *i < cells.len() && cells[*i].ch.is_whitespace() {
            *i += 1;
        }
    };
    skip_blanks(&mut i);
    if i < cells.len() && cells[i].ch == BORDER {
        i += 1;
        skip_blanks(&mut i);
    }
    match cells.get(i) {
        Some(c) if c.ch == '>' || c.ch == '\u{276F}' => Some(i + 1),
        _ => None,
    }
}

/// Classify Claude Code's input box on `snap`.
pub fn input_box(snap: &Snapshot) -> InputBox {
    let mut rules = (0..snap.rows).rev().filter(|&r| is_rule(snap, r));
    let (Some(lower), Some(upper)) = (rules.next(), rules.next()) else {
        return InputBox::NotFound;
    };
    let first = upper + 1;
    if first >= lower {
        return InputBox::NotFound;
    }
    let Some(start) = after_prompt_mark(row_cells(snap, first)) else {
        return InputBox::NotFound;
    };
    let typed = (first..lower).any(|row| {
        let from = if row == first { start } else { 0 };
        row_cells(snap, row)[from..]
            .iter()
            .any(|c| !c.dim && !c.ch.is_whitespace() && c.ch != BORDER)
    });
    if typed {
        InputBox::Typed
    } else {
        InputBox::Empty
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLS: u16 = 60;
    const ROWS: u16 = 12;

    fn rule(cols: usize) -> String {
        "\u{2500}".repeat(cols)
    }

    /// Output lines joined the way a PTY sends them, from a cleared screen.
    fn draw(lines: &[String]) -> Vec<u8> {
        let mut s = String::from("\x1b[2J\x1b[H");
        s.push_str(&lines.join("\r\n"));
        s.into_bytes()
    }

    fn box_with(input_lines: &[&str], cols: usize) -> Vec<u8> {
        let mut lines = vec!["Working on DEMO-12 in demo-api".to_string(), String::new()];
        lines.push(rule(cols));
        lines.extend(input_lines.iter().map(|l| l.to_string()));
        lines.push(rule(cols));
        lines.push("  ? for shortcuts".to_string());
        draw(&lines)
    }

    fn classify(bytes: &[u8], cols: u16) -> InputBox {
        let (snap, _) = screen_of(bytes, cols, ROWS);
        input_box(&snap)
    }

    #[test]
    fn empty_box_with_dim_placeholder_is_empty() {
        let bytes = box_with(
            &["\u{276F} \x1b[2mTry \"fix the failing test\"\x1b[0m"],
            COLS as usize,
        );
        assert_eq!(classify(&bytes, COLS), InputBox::Empty);
    }

    #[test]
    fn typed_text_is_typed() {
        let bytes = box_with(&["> run the demo-api tests"], COLS as usize);
        assert_eq!(classify(&bytes, COLS), InputBox::Typed);
    }

    #[test]
    fn second_line_text_is_typed() {
        let bytes = box_with(&["\u{276F}", "  second line of a draft"], COLS as usize);
        assert_eq!(classify(&bytes, COLS), InputBox::Typed);
    }

    #[test]
    fn bordered_empty_box_is_empty() {
        let bytes = box_with(
            &["\u{2502} > \x1b[2mhint\x1b[0m                    \u{2502}"],
            COLS as usize,
        );
        assert_eq!(classify(&bytes, COLS), InputBox::Empty);
    }

    #[test]
    fn permission_dialog_is_not_found() {
        let bytes = draw(&[
            "Bash command".to_string(),
            "  kubectl apply -f demo-api.yaml".to_string(),
            String::new(),
            "Do you want to proceed?".to_string(),
            "\u{276F} 1. Yes".to_string(),
            "  2. Yes, and don't ask again".to_string(),
            "  3. No, and tell Claude what to do differently".to_string(),
        ]);
        assert_eq!(classify(&bytes, COLS), InputBox::NotFound);
    }

    #[test]
    fn no_rules_is_not_found() {
        let bytes = draw(&["> hello".to_string(), "  ? for shortcuts".to_string()]);
        assert_eq!(classify(&bytes, COLS), InputBox::NotFound);
    }

    #[test]
    fn rules_without_prompt_mark_is_not_found() {
        let bytes = box_with(&["just some text"], COLS as usize);
        assert_eq!(classify(&bytes, COLS), InputBox::NotFound);
    }

    #[test]
    fn narrow_terminal_still_found() {
        let bytes = box_with(&["\u{276F} \x1b[2mTry something\x1b[0m"], 40);
        assert_eq!(classify(&bytes, 40), InputBox::Empty);
        let bytes = box_with(&["\u{276F} hello there"], 40);
        assert_eq!(classify(&bytes, 40), InputBox::Typed);
    }

    #[test]
    fn replay_follows_bracketed_paste_mode() {
        let (_, modes) = screen_of(b"\x1b[?2004h", COLS, ROWS);
        assert!(modes.bracketed_paste);
        let (_, modes) = screen_of(b"hello", COLS, ROWS);
        assert!(!modes.bracketed_paste);
    }
}
