//! The settings view's types, and the one place a setting's value is read
//! from text and checked against its kind. The daemon's save, `ctl settings
//! set` and the app's Settings view all use `parse_text` and `check`, so the
//! texts they show are the same.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::msg::DaemonInfo;

pub const DEFAULT_FONT_FAMILY: &str = "Menlo";
pub const DEFAULT_FONT_SIZE: f32 = 13.0;
/// Editor for Cmd+click on a path when the config does not name one.
pub const DEFAULT_EDITOR: &str = "Visual Studio Code";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum SettingKind {
    Text { allow_empty: bool },
    Path,
    Pattern,
    Date,
    Bool,
    Int { min: i64, max: i64 },
    Float { min: f64, max: f64 },
    Colour,
    List,
}

/// When a saved value takes effect.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Applies {
    /// In the app, at once.
    App,
    /// In the daemon, at its next start.
    Restart,
    /// Shown but changed only by editing `config.toml`.
    ReadOnly,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum SettingValue {
    Text(String),
    Bool(bool),
    Int(i64),
    Float(f64),
    List(Vec<String>),
}

impl std::fmt::Display for SettingValue {
    /// Text as it is, numbers and booleans as TOML writes them, a list as
    /// a JSON array (what `ctl settings set` reads back).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SettingValue::Text(s) => f.write_str(s),
            SettingValue::Bool(b) => write!(f, "{b}"),
            SettingValue::Int(n) => write!(f, "{n}"),
            SettingValue::Float(x) => write!(f, "{x:?}"),
            SettingValue::List(items) => {
                f.write_str(&serde_json::to_string(items).map_err(|_| std::fmt::Error)?)
            }
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Setting {
    pub key: String,
    pub group: String,
    pub kind: SettingKind,
    pub applies: Applies,
    /// The file's value as written (`~` not expanded), else the default.
    pub value: SettingValue,
    pub default: SettingValue,
    pub in_file: bool,
    /// The file's value differs from the one the running daemon uses.
    pub restart_needed: bool,
    pub note: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SettingsView {
    pub config_path: PathBuf,
    /// `file_hash` of the text the view was read from; a save sends it back.
    pub file_hash: String,
    pub load_error: Option<String>,
    pub settings: Vec<Setting>,
    pub targets_path: PathBuf,
    pub targets_note: String,
    pub targets_error: Option<String>,
    pub daemon: DaemonInfo,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum SettingEdit {
    Set {
        key: String,
        value: SettingValue,
    },
    /// Remove the key from the file, so the default applies.
    Reset {
        key: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ymd {
    pub y: i32,
    pub m: u32,
    pub d: u32,
}

fn is_leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap(y) => 29,
        _ => 28,
    }
}

/// `YYYY-MM-DD`, a real calendar date.
pub fn parse_ymd(s: &str) -> Option<Ymd> {
    let mut parts = s.trim().split('-');
    let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return None;
    }
    if ![y, m, d]
        .iter()
        .all(|p| p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let (y, m, d): (i32, u32, u32) = (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
    ((1..=12).contains(&m) && d >= 1 && d <= days_in_month(y, m)).then_some(Ymd { y, m, d })
}

fn is_colour(s: &str) -> bool {
    let h = s.trim().trim_start_matches('#');
    h.len() == 6 && h.chars().all(|c| c.is_ascii_hexdigit())
}

fn needs(key: &str, kind: &SettingKind) -> String {
    let what = match kind {
        SettingKind::Bool => "true or false",
        SettingKind::Int { .. } | SettingKind::Float { .. } => "a number",
        SettingKind::List => "a list",
        _ => "text",
    };
    format!("{key} needs {what}")
}

fn whole_number(key: &str, min: i64, max: i64) -> String {
    format!("{key} must be a whole number from {min} to {max}")
}

fn number(key: &str, min: f64, max: f64) -> String {
    format!("{key} must be a number from {min} to {max}")
}

/// Whether `value` fits `kind`. `Float` takes an `Int`; paths, dates,
/// patterns and colours are `Text`.
pub fn check(key: &str, kind: &SettingKind, value: &SettingValue) -> Result<(), String> {
    let empty = || Err(format!("{key} cannot be empty"));
    match (kind, value) {
        (SettingKind::Text { allow_empty }, SettingValue::Text(s)) => {
            if !allow_empty && s.trim().is_empty() {
                return empty();
            }
            Ok(())
        }
        (SettingKind::Path, SettingValue::Text(s)) => {
            if s.trim().is_empty() {
                return empty();
            }
            Ok(())
        }
        (SettingKind::Pattern, SettingValue::Text(s)) => {
            if s.is_empty() {
                return empty();
            }
            regex::Regex::new(s).map(|_| ()).map_err(|e| {
                let text = e.to_string();
                let first = text.lines().next().unwrap_or_default().to_string();
                format!("{key} is not a valid pattern: {first}")
            })
        }
        (SettingKind::Date, SettingValue::Text(s)) => {
            if s.trim().is_empty() || parse_ymd(s).is_some() {
                Ok(())
            } else {
                Err(format!("{key} must be a date like 2026-11-09, or empty"))
            }
        }
        (SettingKind::Colour, SettingValue::Text(s)) => {
            if is_colour(s) {
                Ok(())
            } else {
                Err(format!("{key} must be a colour like #1e1e2e"))
            }
        }
        (SettingKind::Bool, SettingValue::Bool(_)) => Ok(()),
        (SettingKind::Int { min, max }, SettingValue::Int(n)) => {
            if (*min..=*max).contains(n) {
                Ok(())
            } else {
                Err(whole_number(key, *min, *max))
            }
        }
        (SettingKind::Float { min, max }, SettingValue::Float(x)) => {
            if (*min..=*max).contains(x) {
                Ok(())
            } else {
                Err(number(key, *min, *max))
            }
        }
        (SettingKind::Float { min, max }, SettingValue::Int(n)) => {
            if (*min..=*max).contains(&(*n as f64)) {
                Ok(())
            } else {
                Err(number(key, *min, *max))
            }
        }
        (SettingKind::List, SettingValue::List(_)) => Ok(()),
        _ => Err(needs(key, kind)),
    }
}

/// Reads a value typed as text: numbers, `true` or `false`, and a list as
/// a JSON array or as comma-separated items (trimmed, empty ones dropped).
/// The result has passed `check`.
pub fn parse_text(kind: &SettingKind, key: &str, text: &str) -> Result<SettingValue, String> {
    let t = text.trim();
    let value = match kind {
        SettingKind::Text { .. } | SettingKind::Pattern => SettingValue::Text(text.to_string()),
        SettingKind::Path | SettingKind::Date | SettingKind::Colour => {
            SettingValue::Text(t.to_string())
        }
        SettingKind::Bool => match t {
            "true" => SettingValue::Bool(true),
            "false" => SettingValue::Bool(false),
            _ => return Err(needs(key, kind)),
        },
        SettingKind::Int { min, max } => {
            SettingValue::Int(t.parse().map_err(|_| whole_number(key, *min, *max))?)
        }
        SettingKind::Float { min, max } => {
            let x: f64 = t.parse().map_err(|_| number(key, *min, *max))?;
            SettingValue::Float(x)
        }
        SettingKind::List if t.starts_with('[') => SettingValue::List(
            serde_json::from_str::<Vec<String>>(t).map_err(|_| needs(key, kind))?,
        ),
        SettingKind::List => SettingValue::List(
            t.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
        ),
    };
    check(key, kind, &value)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLL: SettingKind = SettingKind::Int { min: 5, max: 3600 };
    const SIZE: SettingKind = SettingKind::Float {
        min: 4.0,
        max: 96.0,
    };

    #[test]
    fn numbers_are_read_and_ranged() {
        assert_eq!(parse_text(&POLL, "p", " 60 "), Ok(SettingValue::Int(60)));
        let range = Err("p must be a whole number from 5 to 3600".to_string());
        assert_eq!(parse_text(&POLL, "p", "sixty"), range);
        assert_eq!(parse_text(&POLL, "p", "2"), range);
        assert_eq!(parse_text(&SIZE, "s", "14"), Ok(SettingValue::Float(14.0)));
        assert_eq!(
            parse_text(&SIZE, "s", "200"),
            Err("s must be a number from 4 to 96".to_string())
        );
        assert_eq!(check("s", &SIZE, &SettingValue::Int(14)), Ok(()));
        assert_eq!(
            check("s", &SIZE, &SettingValue::Float(f64::NAN)),
            Err("s must be a number from 4 to 96".to_string())
        );
    }

    #[test]
    fn lists_read_json_or_commas() {
        let want = Ok(SettingValue::List(vec!["a.md".into(), "b/*.md".into()]));
        assert_eq!(parse_text(&SettingKind::List, "l", "a.md, b/*.md"), want);
        assert_eq!(
            parse_text(&SettingKind::List, "l", r#"["a.md","b/*.md"]"#),
            want
        );
        assert_eq!(
            parse_text(&SettingKind::List, "l", "[1"),
            Err("l needs a list".to_string())
        );
    }

    #[test]
    fn kinds_refuse_the_wrong_value() {
        assert_eq!(
            check(
                "push",
                &SettingKind::Bool,
                &SettingValue::Text("yes".into())
            ),
            Err("push needs true or false".to_string())
        );
        assert_eq!(
            parse_text(&SettingKind::Bool, "push", "yes"),
            Err("push needs true or false".to_string())
        );
        assert_eq!(
            check(
                "jql",
                &SettingKind::Text { allow_empty: false },
                &SettingValue::Int(1)
            ),
            Err("jql needs text".to_string())
        );
        assert_eq!(
            check("x", &POLL, &SettingValue::List(vec![])),
            Err("x needs a number".to_string())
        );
        assert_eq!(
            check("x", &SettingKind::List, &SettingValue::Bool(true)),
            Err("x needs a list".to_string())
        );
    }

    #[test]
    fn text_kinds() {
        let t = |k: &SettingKind, s: &str| check("k", k, &SettingValue::Text(s.into()));
        assert_eq!(
            t(&SettingKind::Text { allow_empty: false }, " "),
            Err("k cannot be empty".to_string())
        );
        assert_eq!(t(&SettingKind::Text { allow_empty: true }, ""), Ok(()));
        assert_eq!(
            t(&SettingKind::Path, ""),
            Err("k cannot be empty".to_string())
        );
        assert_eq!(t(&SettingKind::Date, ""), Ok(()));
        assert_eq!(t(&SettingKind::Date, "2026-11-09"), Ok(()));
        assert_eq!(
            t(&SettingKind::Date, "2026-13-01"),
            Err("k must be a date like 2026-11-09, or empty".to_string())
        );
        assert_eq!(t(&SettingKind::Colour, "#1E1e2e"), Ok(()));
        assert_eq!(
            t(&SettingKind::Colour, "blue"),
            Err("k must be a colour like #1e1e2e".to_string())
        );
        assert_eq!(t(&SettingKind::Pattern, r"DEMO-\d+"), Ok(()));
        assert_eq!(
            t(&SettingKind::Pattern, "("),
            Err("k is not a valid pattern: regex parse error:".to_string())
        );
    }

    #[test]
    fn values_display_as_ctl_reads_them() {
        assert_eq!(SettingValue::Float(13.0).to_string(), "13.0");
        assert_eq!(SettingValue::Int(15).to_string(), "15");
        assert_eq!(
            SettingValue::List(vec!["a".into(), "b".into()]).to_string(),
            r#"["a","b"]"#
        );
    }
}
