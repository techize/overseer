//! Pure terminal model and input encoding layer for Overseer.
//!
//! Bridges raw PTY bytes to an `alacritty_terminal` model and encodes keyboard,
//! paste and mouse input back into bytes. No GUI dependencies.

pub mod bridge;
pub mod keys;
pub mod links;
pub mod mouse;
pub mod paste;

pub use bridge::{
    Cell, Cursor, CursorShape, GridPos, Match, Palette, Rgb, Selection, Snapshot, TermBridge,
    DEFAULT_SCROLLBACK,
};
pub use keys::{encode_key, Key, KeyInput};
pub use links::{find_link_at, Link};
pub use mouse::{encode_mouse, MouseButton, MouseEvent, MouseInput, MouseMode, TermModes};
pub use paste::encode_paste;
