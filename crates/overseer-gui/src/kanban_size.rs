//! The kanban's height on the board: by default it fills the space left
//! below the session cards; dragging its bottom edge sets a fixed height,
//! kept as `kanban_height` in `<state dir>/gui.toml`; a double click on the
//! edge goes back to filling.
//!
//! The same file keeps `pane_hidden`, whether the board's conversation
//! pane is hidden; a file without it reads as shown. It also keeps
//! `pane_width`, the pane's dragged width; a double click on the pane's
//! left edge goes back to [`PANE_WIDTH`].

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The shortest the kanban gets, by drag or by a small window.
pub const MIN: f32 = 180.;

/// The conversation pane's width until it is dragged.
pub const PANE_WIDTH: f32 = 420.;
/// The narrowest the pane gets by drag.
pub const PANE_MIN: f32 = 320.;
/// The widest the pane gets by drag, however wide the window.
pub const PANE_MAX: f32 = 900.;
/// The width the board keeps beside the pane when the pane is dragged wide.
pub const BOARD_MIN: f32 = 560.;

#[derive(Debug, Default, Serialize, Deserialize)]
struct GuiState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kanban_height: Option<f32>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pane_hidden: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pane_width: Option<f32>,
}

/// What `path` holds, or the defaults when it is missing or unreadable.
fn read(path: &Path) -> GuiState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default()
}

fn write(path: &Path, state: &GuiState) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = toml::to_string(state).map_err(std::io::Error::other)?;
    std::fs::write(path, text)
}

/// Where the GUI keeps its own small state.
pub fn path() -> PathBuf {
    overseer_proto::paths::state_dir().join("gui.toml")
}

/// The saved height at `path`, or `None` (fill) when the file is missing,
/// unreadable or holds no usable height.
pub fn load_from(path: &Path) -> Option<f32> {
    read(path)
        .kanban_height
        .filter(|h| h.is_finite() && *h >= MIN)
}

/// Saves `height` to `path`; `None` removes it, so the kanban fills.
pub fn save_to(path: &Path, height: Option<f32>) -> std::io::Result<()> {
    let mut state = read(path);
    state.kanban_height = height.map(|h| h.max(MIN).round());
    write(path, &state)
}

/// Whether the conversation pane was left hidden; shown when `path` is
/// missing, unreadable or does not say.
pub fn pane_hidden_from(path: &Path) -> bool {
    read(path).pane_hidden
}

/// Saves whether the conversation pane is hidden, keeping the height.
pub fn save_pane_hidden(path: &Path, hidden: bool) -> std::io::Result<()> {
    let mut state = read(path);
    state.pane_hidden = hidden;
    write(path, &state)
}

/// The saved pane width at `path`, or [`PANE_WIDTH`] when the file is
/// missing, unreadable or holds no usable width; kept within
/// `PANE_MIN..=PANE_MAX`.
pub fn pane_width_from(path: &Path) -> f32 {
    read(path)
        .pane_width
        .filter(|w| w.is_finite())
        .map_or(PANE_WIDTH, |w| w.clamp(PANE_MIN, PANE_MAX))
}

/// Saves the pane's width, keeping the other settings. The default width
/// is not written, so a reset leaves no `pane_width` in the file.
pub fn save_pane_width(path: &Path, width: f32) -> std::io::Result<()> {
    let mut state = read(path);
    let w = width.clamp(PANE_MIN, PANE_MAX).round();
    state.pane_width = (w != PANE_WIDTH).then_some(w);
    write(path, &state)
}

/// The widest the pane may be in a window `window_w` wide: the board keeps
/// [`BOARD_MIN`], and never past [`PANE_MAX`] or under [`PANE_MIN`].
pub fn pane_max(window_w: f32) -> f32 {
    (window_w - BOARD_MIN).clamp(PANE_MIN, PANE_MAX)
}

/// The pane's width while dragging its left edge: the pointer moved from
/// `start_x` to `x` (left is wider), starting at `start_w`, kept within
/// `PANE_MIN..=max`.
pub fn pane_dragged(start_w: f32, start_x: f32, x: f32, max: f32) -> f32 {
    (start_w + start_x - x).clamp(PANE_MIN, max.max(PANE_MIN))
}

/// The height while dragging: the pointer moved from `start_y` to `y`
/// (down is taller, as the handle is the bottom edge), starting at
/// `start_h`, kept within `MIN..=max`.
pub fn dragged(start_h: f32, start_y: f32, y: f32, max: f32) -> f32 {
    (start_h + y - start_y).clamp(MIN, max.max(MIN))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dragging_down_grows_and_stays_in_bounds() {
        assert_eq!(dragged(300., 500., 550., 800.), 350.);
        assert_eq!(dragged(300., 500., 400., 800.), 200.);
        assert_eq!(dragged(300., 500., 500., 800.), 300.);
        assert_eq!(dragged(300., 500., -900., 800.), MIN);
        assert_eq!(dragged(300., 500., 1900., 800.), 800.);
        assert_eq!(dragged(300., 500., 550., 100.), MIN);
    }

    #[test]
    fn dragging_the_pane_left_widens_it_within_bounds() {
        assert_eq!(pane_dragged(420., 1000., 900., 800.), 520.);
        assert_eq!(pane_dragged(420., 1000., 1050., 800.), 370.);
        assert_eq!(pane_dragged(420., 1000., 1000., 800.), 420.);
        assert_eq!(pane_dragged(420., 1000., 2000., 800.), PANE_MIN);
        assert_eq!(pane_dragged(420., 1000., -500., 800.), 800.);
        // A window too narrow for any drag keeps the narrowest pane.
        assert_eq!(pane_dragged(420., 1000., 900., 100.), PANE_MIN);
    }

    #[test]
    fn the_board_keeps_its_room_beside_the_pane() {
        assert_eq!(pane_max(1400.), 1400. - BOARD_MIN);
        assert_eq!(pane_max(3000.), PANE_MAX);
        assert_eq!(pane_max(700.), PANE_MIN);
    }

    #[test]
    fn pane_width_is_remembered_with_the_rest() {
        let dir = std::env::temp_dir().join(format!("ovw-{}", std::process::id()));
        let p = dir.join("gui.toml");
        assert_eq!(pane_width_from(&p), PANE_WIDTH);
        save_to(&p, Some(400.)).unwrap();
        save_pane_hidden(&p, true).unwrap();
        save_pane_width(&p, 612.6).unwrap();
        assert_eq!(pane_width_from(&p), 613.);
        assert_eq!(load_from(&p), Some(400.));
        assert!(pane_hidden_from(&p));
        // Out of bounds by hand: kept in bounds.
        std::fs::write(&p, "pane_width = 5.0\n").unwrap();
        assert_eq!(pane_width_from(&p), PANE_MIN);
        std::fs::write(&p, "pane_width = 5000.0\n").unwrap();
        assert_eq!(pane_width_from(&p), PANE_MAX);
        // The default width is not written: a reset leaves no key.
        save_pane_width(&p, PANE_WIDTH).unwrap();
        assert!(!std::fs::read_to_string(&p).unwrap().contains("pane_width"));
        assert_eq!(pane_width_from(&p), PANE_WIDTH);
        std::fs::write(&p, "not toml [").unwrap();
        assert_eq!(pane_width_from(&p), PANE_WIDTH);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn height_round_trips_and_reset_fills() {
        let dir = std::env::temp_dir().join(format!("ovk-{}", std::process::id()));
        let p = dir.join("gui.toml");
        assert_eq!(load_from(&p), None);
        save_to(&p, Some(412.4)).unwrap();
        assert_eq!(load_from(&p), Some(412.));
        save_to(&p, None).unwrap();
        assert_eq!(load_from(&p), None);
        std::fs::write(&p, "kanban_height = 12.0\n").unwrap();
        assert_eq!(load_from(&p), None);
        std::fs::write(&p, "not toml [").unwrap();
        assert_eq!(load_from(&p), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pane_hidden_is_remembered() {
        let dir = std::env::temp_dir().join(format!("ovp-{}", std::process::id()));
        let p = dir.join("gui.toml");
        // No file, or one from before the pane: shown.
        assert!(!pane_hidden_from(&p));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&p, "kanban_height = 400.0\n").unwrap();
        assert!(!pane_hidden_from(&p));
        save_pane_hidden(&p, true).unwrap();
        assert!(pane_hidden_from(&p));
        // Each setting keeps the other.
        assert_eq!(load_from(&p), Some(400.));
        save_to(&p, Some(500.)).unwrap();
        assert!(pane_hidden_from(&p));
        save_pane_hidden(&p, false).unwrap();
        assert!(!pane_hidden_from(&p));
        assert_eq!(load_from(&p), Some(500.));
        std::fs::write(&p, "not toml [").unwrap();
        assert!(!pane_hidden_from(&p));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
