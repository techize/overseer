//! The kanban's height on the board: by default it fills the space left
//! below the session cards; dragging its bottom edge sets a fixed height,
//! kept as `kanban_height` in `<state dir>/gui.toml`; a double click on the
//! edge goes back to filling.
//!
//! The same file keeps `pane_hidden`, whether the board's conversation
//! pane is hidden; a file without it reads as shown.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The shortest the kanban gets, by drag or by a small window.
pub const MIN: f32 = 180.;

#[derive(Debug, Default, Serialize, Deserialize)]
struct GuiState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kanban_height: Option<f32>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pane_hidden: bool,
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
