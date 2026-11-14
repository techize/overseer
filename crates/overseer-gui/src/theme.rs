//! Terminal font and colours, read from `~/.config/overseer/config.toml`.
//!
//! ```toml
//! [terminal]
//! font_family = "Menlo"
//! font_size = 13.0
//! background = "#000000"
//! foreground = "#e5e5e5"
//! ansi = ["#000000", "#cd0000", ...] # exactly 16 entries
//! ```
//!
//! Every key is optional. A missing or unreadable file gives the defaults.

use std::path::PathBuf;

use overseer_term::{Palette, Rgb};
use serde::Deserialize;

pub use overseer_proto::settings::{DEFAULT_FONT_FAMILY, DEFAULT_FONT_SIZE};

#[derive(Debug, Clone)]
pub struct Theme {
    pub font_family: String,
    pub font_size: f32,
    pub palette: Palette,
    /// Window background behind the grid. Kept equal to `palette.background`.
    pub background: Rgb,
}

impl Default for Theme {
    fn default() -> Self {
        let palette = Palette::default();
        Theme {
            font_family: DEFAULT_FONT_FAMILY.to_string(),
            font_size: DEFAULT_FONT_SIZE,
            background: palette.background,
            palette,
        }
    }
}

#[derive(Deserialize, Default)]
struct ConfigFile {
    #[serde(default)]
    terminal: TerminalSection,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct TerminalSection {
    font_family: Option<String>,
    font_size: Option<f32>,
    background: Option<String>,
    foreground: Option<String>,
    ansi: Option<Vec<String>>,
}

impl Theme {
    /// `~/.config/overseer/config.toml`.
    pub fn config_path() -> Option<PathBuf> {
        std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(|h| PathBuf::from(h).join(".config/overseer/config.toml"))
    }

    /// Reads the config file. A missing file gives the defaults; a broken one
    /// gives the defaults and a warning on stderr.
    pub fn load() -> Theme {
        let Some(path) = Theme::config_path() else {
            return Theme::default();
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Theme::default(),
            Err(e) => {
                eprintln!("overseer: cannot read {}: {e}", path.display());
                return Theme::default();
            }
        };
        Theme::from_toml(&text).unwrap_or_else(|e| {
            eprintln!("overseer: ignoring {}: {e}", path.display());
            Theme::default()
        })
    }

    /// Parses config text, filling unset keys with defaults.
    pub fn from_toml(text: &str) -> Result<Theme, String> {
        let file: ConfigFile = toml::from_str(text).map_err(|e| e.to_string())?;
        let t = file.terminal;
        let mut theme = Theme::default();
        if let Some(family) = t.font_family {
            if family.trim().is_empty() {
                return Err("terminal.font_family is empty".into());
            }
            theme.font_family = family;
        }
        if let Some(size) = t.font_size {
            if !(4.0..=96.0).contains(&size) {
                return Err(format!("terminal.font_size {size} is outside 4 to 96"));
            }
            theme.font_size = size;
        }
        if let Some(bg) = t.background {
            theme.background = parse_hex(&bg)?;
            theme.palette.background = theme.background;
        }
        if let Some(fg) = t.foreground {
            theme.palette.foreground = parse_hex(&fg)?;
        }
        if let Some(ansi) = t.ansi {
            if ansi.len() != 16 {
                return Err(format!(
                    "terminal.ansi needs 16 colours, got {}",
                    ansi.len()
                ));
            }
            for (slot, s) in theme.palette.ansi.iter_mut().zip(&ansi) {
                *slot = parse_hex(s)?;
            }
        }
        Ok(theme)
    }
}

/// Parses `#rrggbb` (the `#` is optional).
pub fn parse_hex(s: &str) -> Result<Rgb, String> {
    let h = s.trim().trim_start_matches('#');
    if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("{s:?} is not a #rrggbb colour"));
    }
    let byte = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).map_err(|e| e.to_string());
    Ok(Rgb(byte(0)?, byte(2)?, byte(4)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_menlo_13_and_xterm() {
        let t = Theme::from_toml("").unwrap();
        assert_eq!(t.font_family, "Menlo");
        assert_eq!(t.font_size, 13.0);
        assert_eq!(t.palette.ansi, Palette::default().ansi);
        assert_eq!(t.background, Rgb(0, 0, 0));
        assert_eq!(t.palette.background, t.background);
    }

    #[test]
    fn terminal_section_overrides() {
        let t = Theme::from_toml(
            r##"
            [terminal]
            font_family = "JetBrains Mono"
            font_size = 15
            background = "#101820"
            foreground = "ccddee"
            "##,
        )
        .unwrap();
        assert_eq!(t.font_family, "JetBrains Mono");
        assert_eq!(t.font_size, 15.0);
        assert_eq!(t.background, Rgb(0x10, 0x18, 0x20));
        assert_eq!(t.palette.background, Rgb(0x10, 0x18, 0x20));
        assert_eq!(t.palette.foreground, Rgb(0xcc, 0xdd, 0xee));
    }

    #[test]
    fn ansi_needs_sixteen() {
        let mut list: Vec<String> = (0..16).map(|i| format!("\"#0000{i:02x}\"")).collect();
        let t = Theme::from_toml(&format!("[terminal]\nansi = [{}]", list.join(","))).unwrap();
        assert_eq!(t.palette.ansi[15], Rgb(0, 0, 15));
        list.pop();
        assert!(Theme::from_toml(&format!("[terminal]\nansi = [{}]", list.join(","))).is_err());
    }

    #[test]
    fn bad_values_rejected() {
        assert!(Theme::from_toml("[terminal]\nbackground = \"#12345\"").is_err());
        assert!(Theme::from_toml("[terminal]\nfont_size = 0.5").is_err());
        assert!(Theme::from_toml("[terminal]\nfont_family = \" \"").is_err());
        assert!(Theme::from_toml("[terminal]\nfont_sise = 12").is_err());
        assert!(Theme::from_toml("not toml [").is_err());
    }

    #[test]
    fn other_sections_ignored() {
        let t = Theme::from_toml("[editor]\nname = \"Zed\"\n").unwrap();
        assert_eq!(t.font_family, "Menlo");
    }

    #[test]
    fn parse_hex_forms() {
        assert_eq!(parse_hex("#ff8000"), Ok(Rgb(255, 128, 0)));
        assert_eq!(parse_hex("FF8000"), Ok(Rgb(255, 128, 0)));
        assert!(parse_hex("#ggg000").is_err());
    }
}
