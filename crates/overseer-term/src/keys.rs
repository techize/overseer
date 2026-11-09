//! Keyboard input encoding (xterm style).

use crate::mouse::TermModes;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    /// Function key, 1 to 12.
    F(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyInput {
    pub key: Key,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub cmd: bool,
}

impl KeyInput {
    pub fn new(key: Key) -> Self {
        KeyInput {
            key,
            shift: false,
            ctrl: false,
            alt: false,
            cmd: false,
        }
    }
}

/// Encode a key press for the PTY. Cmd-modified keys encode to nothing because
/// the GUI owns Cmd shortcuts.
pub fn encode_key(input: &KeyInput, modes: &TermModes) -> Vec<u8> {
    if input.cmd {
        return Vec::new();
    }
    // xterm modifier parameter: 1 + shift + 2*alt + 4*ctrl.
    let param = 1 + input.shift as u8 + 2 * input.alt as u8 + 4 * input.ctrl as u8;
    let modified = param > 1;
    let csi_final = |fin: char| -> Vec<u8> {
        if modified {
            format!("\x1b[1;{param}{fin}").into_bytes()
        } else if modes.app_cursor {
            format!("\x1bO{fin}").into_bytes()
        } else {
            format!("\x1b[{fin}").into_bytes()
        }
    };
    let tilde = |n: u8| -> Vec<u8> {
        if modified {
            format!("\x1b[{n};{param}~").into_bytes()
        } else {
            format!("\x1b[{n}~").into_bytes()
        }
    };

    match input.key {
        Key::Enter => {
            if input.shift || input.alt {
                b"\x1b\r".to_vec()
            } else {
                b"\r".to_vec()
            }
        }
        Key::Tab => {
            if input.shift {
                b"\x1b[Z".to_vec()
            } else if input.alt {
                b"\x1b\t".to_vec()
            } else {
                b"\t".to_vec()
            }
        }
        Key::Backspace => {
            let b = if input.ctrl { 0x08 } else { 0x7f };
            if input.alt {
                vec![0x1b, b]
            } else {
                vec![b]
            }
        }
        Key::Escape => b"\x1b".to_vec(),
        Key::Up => csi_final('A'),
        Key::Down => csi_final('B'),
        Key::Right => csi_final('C'),
        Key::Left => csi_final('D'),
        Key::Home => csi_final('H'),
        Key::End => csi_final('F'),
        Key::Insert => tilde(2),
        Key::Delete => tilde(3),
        Key::PageUp => tilde(5),
        Key::PageDown => tilde(6),
        Key::F(n @ 1..=4) => {
            let fin = ['P', 'Q', 'R', 'S'][(n - 1) as usize];
            if modified {
                format!("\x1b[1;{param}{fin}").into_bytes()
            } else {
                format!("\x1bO{fin}").into_bytes()
            }
        }
        Key::F(n @ 5..=12) => tilde([15, 17, 18, 19, 20, 21, 23, 24][(n - 5) as usize]),
        Key::F(_) => Vec::new(),
        Key::Char(c) => encode_char(c, input),
    }
}

fn encode_char(c: char, input: &KeyInput) -> Vec<u8> {
    let mut out = Vec::new();
    if input.alt {
        out.push(0x1b);
    }
    if input.ctrl {
        if let Some(b) = ctrl_byte(c) {
            out.push(b);
            return out;
        }
    }
    let mut buf = [0u8; 4];
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    out
}

fn ctrl_byte(c: char) -> Option<u8> {
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        '@' | ' ' | '2' => Some(0),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '7' => Some(0x1f),
        '?' | '8' => Some(0x7f),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(key: Key) -> KeyInput {
        KeyInput::new(key)
    }

    #[test]
    fn shift_enter_is_esc_cr() {
        let i = KeyInput {
            shift: true,
            ..k(Key::Enter)
        };
        assert_eq!(encode_key(&i, &TermModes::default()), b"\x1b\r");
    }

    #[test]
    fn enter_is_cr() {
        assert_eq!(encode_key(&k(Key::Enter), &TermModes::default()), b"\r");
    }

    #[test]
    fn option_b_is_meta() {
        let i = KeyInput {
            alt: true,
            ..k(Key::Char('b'))
        };
        assert_eq!(encode_key(&i, &TermModes::default()), b"\x1bb");
    }

    #[test]
    fn ctrl_c() {
        let i = KeyInput {
            ctrl: true,
            ..k(Key::Char('c'))
        };
        assert_eq!(encode_key(&i, &TermModes::default()), b"\x03");
    }

    #[test]
    fn arrow_normal_vs_app_cursor() {
        let normal = TermModes::default();
        let app = TermModes {
            app_cursor: true,
            ..TermModes::default()
        };
        assert_eq!(encode_key(&k(Key::Up), &normal), b"\x1b[A");
        assert_eq!(encode_key(&k(Key::Up), &app), b"\x1bOA");
    }

    #[test]
    fn modified_arrow_uses_csi_param() {
        let app = TermModes {
            app_cursor: true,
            ..TermModes::default()
        };
        let i = KeyInput {
            ctrl: true,
            ..k(Key::Right)
        };
        assert_eq!(encode_key(&i, &app), b"\x1b[1;5C");
        let i = KeyInput {
            shift: true,
            alt: true,
            ..k(Key::Left)
        };
        assert_eq!(encode_key(&i, &app), b"\x1b[1;4D");
    }

    #[test]
    fn cmd_keys_not_encoded() {
        let m = TermModes::default();
        for key in [Key::Char('c'), Key::Enter, Key::Up, Key::F(1), Key::Tab] {
            let i = KeyInput {
                cmd: true,
                ..k(key)
            };
            assert!(encode_key(&i, &m).is_empty());
        }
        let i = KeyInput {
            cmd: true,
            ctrl: true,
            alt: true,
            ..k(Key::Char('x'))
        };
        assert!(encode_key(&i, &m).is_empty());
    }

    #[test]
    fn misc_keys() {
        let m = TermModes::default();
        assert_eq!(encode_key(&k(Key::Backspace), &m), b"\x7f");
        assert_eq!(encode_key(&k(Key::Tab), &m), b"\t");
        assert_eq!(
            encode_key(
                &KeyInput {
                    shift: true,
                    ..k(Key::Tab)
                },
                &m
            ),
            b"\x1b[Z"
        );
        assert_eq!(encode_key(&k(Key::Escape), &m), b"\x1b");
        assert_eq!(encode_key(&k(Key::Delete), &m), b"\x1b[3~");
        assert_eq!(encode_key(&k(Key::PageUp), &m), b"\x1b[5~");
        assert_eq!(encode_key(&k(Key::F(1)), &m), b"\x1bOP");
        assert_eq!(encode_key(&k(Key::F(5)), &m), b"\x1b[15~");
        assert_eq!(encode_key(&k(Key::Char('\u{e9}')), &m), "\u{e9}".as_bytes());
        assert_eq!(encode_key(&k(Key::Char(' ')), &m), b" ");
        assert_eq!(
            encode_key(
                &KeyInput {
                    ctrl: true,
                    ..k(Key::Char(' '))
                },
                &m
            ),
            b"\x00"
        );
    }
}
