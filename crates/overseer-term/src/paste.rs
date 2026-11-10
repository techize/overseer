//! Paste encoding.

use crate::mouse::TermModes;

const START: &str = "\x1b[200~";
const END: &str = "\x1b[201~";

/// Encode pasted text. Newlines become CR (what Enter sends). With bracketed
/// paste enabled the text is wrapped in markers, after removing any embedded
/// end marker so pasted content cannot break out of the bracket.
pub fn encode_paste(text: &str, modes: &TermModes) -> Vec<u8> {
    let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
    if !modes.bracketed_paste {
        return normalized.into_bytes();
    }
    // Loop so removal cannot splice a new marker together.
    let mut body = normalized;
    while body.contains(END) {
        body = body.replace(END, "");
    }
    format!("{START}{body}{END}").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bracketed() -> TermModes {
        TermModes {
            bracketed_paste: true,
            ..TermModes::default()
        }
    }

    #[test]
    fn paste_plain_when_mode_off() {
        assert_eq!(encode_paste("a\nb", &TermModes::default()), b"a\rb");
        assert_eq!(encode_paste("a\r\nb", &TermModes::default()), b"a\rb");
    }

    #[test]
    fn paste_bracketed() {
        assert_eq!(
            encode_paste("a\nb", &bracketed()),
            b"\x1b[200~a\rb\x1b[201~"
        );
    }

    #[test]
    fn paste_strips_end_marker() {
        assert_eq!(
            encode_paste("x\x1b[201~y", &bracketed()),
            b"\x1b[200~xy\x1b[201~"
        );
        assert_eq!(
            encode_paste("x\x1b[20\x1b[201~1~y", &bracketed()),
            b"\x1b[200~xy\x1b[201~"
        );
    }
}
