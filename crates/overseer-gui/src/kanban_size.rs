//! The kanban's height on the board: by default it fills the space left
//! below the session cards; dragging its top edge sets a fixed height,
//! kept as `kanban_height` in `<state dir>/gui.toml`; a double click on the
//! edge goes back to filling.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The shortest the kanban gets, by drag or by a small window.
pub const MIN: f32 = 180.;

#[derive(Debug, Default, Serialize, Deserialize)]
struct GuiState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kanban_height: Option<f32>,
}

/// Where the GUI keeps its own small state.
pub fn path() -> PathBuf {
    overseer_proto::paths::state_dir().join("gui.toml")
}

/// The saved height at `path`, or `None` (fill) when the file is missing,
/// unreadable or holds no usable height.
pub fn load_from(path: &Path) -> Option<f32> {
    let text = std::fs::read_to_string(path).ok()?;
    let state: GuiState = toml::from_str(&text).ok()?;
    state.kanban_height.filter(|h| h.is_finite() && *h >= MIN)
}

/// Saves `height` to `path`; `None` removes it, so the kanban fills.
pub fn save_to(path: &Path, height: Option<f32>) -> std::io::Result<()> {
    let state = GuiState {
        kanban_height: height.map(|h| h.max(MIN).round()),
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = toml::to_string(&state).map_err(std::io::Error::other)?;
    std::fs::write(path, text)
}

/// The height while dragging: the pointer moved from `start_y` to `y`
/// (up is taller), starting at `start_h`, kept within `MIN..=max`.
pub fn dragged(start_h: f32, start_y: f32, y: f32, max: f32) -> f32 {
    (start_h + start_y - y).clamp(MIN, max.max(MIN))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dragging_up_grows_and_stays_in_bounds() {
        assert_eq!(dragged(300., 500., 450., 800.), 350.);
        assert_eq!(dragged(300., 500., 600., 800.), 200.);
        assert_eq!(dragged(300., 500., 900., 800.), MIN);
        assert_eq!(dragged(300., 500., -900., 800.), 800.);
        assert_eq!(dragged(300., 500., 450., 100.), MIN);
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
}
