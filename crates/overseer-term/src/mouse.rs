//! Mouse reporting encoding and the terminal mode flags that gate it.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseMode {
    #[default]
    Off,
    /// Button press and release (1000).
    Click,
    /// Press, release and motion while a button is held (1002).
    Drag,
    /// All motion (1003).
    Motion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TermModes {
    pub app_cursor: bool,
    pub bracketed_paste: bool,
    pub mouse: MouseMode,
    pub sgr_mouse: bool,
    pub alt_screen: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEvent {
    Press(MouseButton),
    Release(MouseButton),
    /// Motion with a button held.
    Drag(MouseButton),
    /// Motion with no button held.
    Move,
    ScrollUp,
    ScrollDown,
}

/// A mouse event at a zero-based viewport cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseInput {
    pub event: MouseEvent,
    pub row: usize,
    pub col: usize,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

/// Encode a mouse event, or `None` when it should not be reported: mouse mode
/// is off, Shift is held (local selection override), or the current mode does
/// not cover this event kind.
pub fn encode_mouse(input: &MouseInput, modes: &TermModes) -> Option<Vec<u8>> {
    if modes.mouse == MouseMode::Off || input.shift {
        return None;
    }
    let reportable = match input.event {
        MouseEvent::Drag(_) => matches!(modes.mouse, MouseMode::Drag | MouseMode::Motion),
        MouseEvent::Move => modes.mouse == MouseMode::Motion,
        _ => true,
    };
    if !reportable {
        return None;
    }

    let button = |b: MouseButton| match b {
        MouseButton::Left => 0u32,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    let (mut code, release) = match input.event {
        MouseEvent::Press(b) => (button(b), false),
        MouseEvent::Release(b) => (button(b), true),
        MouseEvent::Drag(b) => (button(b) + 32, false),
        MouseEvent::Move => (3 + 32, false),
        MouseEvent::ScrollUp => (64, false),
        MouseEvent::ScrollDown => (65, false),
    };
    if input.alt {
        code += 8;
    }
    if input.ctrl {
        code += 16;
    }
    let (x, y) = (input.col + 1, input.row + 1);

    if modes.sgr_mouse {
        let fin = if release { 'm' } else { 'M' };
        return Some(format!("\x1b[<{code};{x};{y}{fin}").into_bytes());
    }
    // Legacy X10 encoding: release has no button identity, and coordinates are
    // limited to one byte offset by 32.
    if release {
        code = (code & !3) | 3;
    }
    if x + 32 > 255 || y + 32 > 255 {
        return None;
    }
    Some(vec![
        0x1b,
        b'[',
        b'M',
        (code + 32) as u8,
        (x + 32) as u8,
        (y + 32) as u8,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(row: usize, col: usize) -> MouseInput {
        MouseInput {
            event: MouseEvent::Press(MouseButton::Left),
            row,
            col,
            shift: false,
            ctrl: false,
            alt: false,
        }
    }

    fn sgr() -> TermModes {
        TermModes {
            mouse: MouseMode::Click,
            sgr_mouse: true,
            ..TermModes::default()
        }
    }

    #[test]
    fn mouse_off_returns_none() {
        assert_eq!(encode_mouse(&press(3, 5), &TermModes::default()), None);
    }

    #[test]
    fn shift_overrides_mouse_mode() {
        let i = MouseInput {
            shift: true,
            ..press(3, 5)
        };
        assert_eq!(encode_mouse(&i, &sgr()), None);
    }

    #[test]
    fn sgr_left_press_at_3_5() {
        assert_eq!(encode_mouse(&press(3, 5), &sgr()).unwrap(), b"\x1b[<0;6;4M");
    }

    #[test]
    fn sgr_release_keeps_button_and_uses_m() {
        let i = MouseInput {
            event: MouseEvent::Release(MouseButton::Right),
            ..press(0, 0)
        };
        assert_eq!(encode_mouse(&i, &sgr()).unwrap(), b"\x1b[<2;1;1m");
    }

    #[test]
    fn sgr_scroll_and_modifiers() {
        let i = MouseInput {
            event: MouseEvent::ScrollUp,
            ctrl: true,
            ..press(1, 1)
        };
        assert_eq!(encode_mouse(&i, &sgr()).unwrap(), b"\x1b[<80;2;2M");
        let i = MouseInput {
            event: MouseEvent::ScrollDown,
            ..press(1, 1)
        };
        assert_eq!(encode_mouse(&i, &sgr()).unwrap(), b"\x1b[<65;2;2M");
    }

    #[test]
    fn motion_gated_by_mode() {
        let drag = MouseInput {
            event: MouseEvent::Drag(MouseButton::Left),
            ..press(0, 0)
        };
        let mv = MouseInput {
            event: MouseEvent::Move,
            ..press(0, 0)
        };
        let click = sgr();
        assert_eq!(encode_mouse(&drag, &click), None);
        assert_eq!(encode_mouse(&mv, &click), None);
        let dragm = TermModes {
            mouse: MouseMode::Drag,
            ..click
        };
        assert_eq!(encode_mouse(&drag, &dragm).unwrap(), b"\x1b[<32;1;1M");
        assert_eq!(encode_mouse(&mv, &dragm), None);
        let all = TermModes {
            mouse: MouseMode::Motion,
            ..click
        };
        assert_eq!(encode_mouse(&mv, &all).unwrap(), b"\x1b[<35;1;1M");
    }

    #[test]
    fn legacy_encoding() {
        let m = TermModes {
            mouse: MouseMode::Click,
            ..TermModes::default()
        };
        assert_eq!(
            encode_mouse(&press(3, 5), &m).unwrap(),
            vec![0x1b, b'[', b'M', 32, 38, 36]
        );
        let rel = MouseInput {
            event: MouseEvent::Release(MouseButton::Left),
            ..press(3, 5)
        };
        assert_eq!(
            encode_mouse(&rel, &m).unwrap(),
            vec![0x1b, b'[', b'M', 35, 38, 36]
        );
        assert_eq!(encode_mouse(&press(0, 300), &m), None);
    }
}
