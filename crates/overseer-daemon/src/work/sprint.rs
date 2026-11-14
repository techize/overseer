//! The sprint name and day shown in the board header.
//!
//! The name is `sprint_name` or the newest `Sprints/sprint-*` folder; the day
//! counts weekdays from `sprint_start`. The label takes `today` as an
//! argument so tests never read the clock.

use std::path::Path;

/// The settings check reads `sprint_start` with the same parser.
pub use overseer_proto::settings::{parse_ymd, Ymd};

/// The sprint length when `sprint_days` is 0 (a hand-edited config).
pub const DEFAULT_DAYS: u32 = 10;

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// Days since 1970-01-01 (civil-from-days, Howard Hinnant's algorithm).
fn to_days(t: Ymd) -> i64 {
    let y = i64::from(t.y) - i64::from(t.m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (i64::from(t.m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(t.d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn from_days(z: i64) -> Ymd {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    Ymd { y: y as i32, m, d }
}

/// 0 is Monday, 6 is Sunday. 1970-01-01 was a Thursday.
fn weekday(days: i64) -> i64 {
    (days + 3).rem_euclid(7)
}

fn short_date(days: i64) -> String {
    let t = from_days(days);
    format!(
        "{} {} {}",
        WEEKDAYS[weekday(days) as usize],
        t.d,
        MONTHS[t.m as usize - 1]
    )
}

/// The name of the last folder by name matching `Sprints/sprint-*`.
pub fn newest_sprint_folder(brain: &Path) -> Option<String> {
    let entries = std::fs::read_dir(brain.join("Sprints")).ok()?;
    entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("sprint-"))
        .max()
}

/// `sprint-26-01, day 4 of 10`, `sprint-26-01, starts Mon 9 Nov`,
/// `sprint-26-01, ended Fri 20 Nov`, or the name alone without a start.
/// Days count weekdays from the start; a weekend start begins on the Monday
/// after, and a weekend inside the sprint shows the last day reached. `None`
/// for an empty name; `days` of 0 means 10.
pub fn sprint_label(name: &str, start: Option<Ymd>, days: u32, today: Ymd) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    let Some(start) = start else {
        return Some(name.to_string());
    };
    let days = i64::from(if days == 0 { DEFAULT_DAYS } else { days });
    let mut first = to_days(start);
    let first_wd = weekday(first);
    if first_wd >= 5 {
        first += 7 - first_wd;
    }
    let now = to_days(today);
    if now < first {
        return Some(format!("{name}, starts {}", short_date(first)));
    }
    let span = now - first + 1;
    let first_wd = weekday(first);
    let reached = span / 7 * 5 + (0..span % 7).filter(|i| (first_wd + i) % 7 < 5).count() as i64;
    if reached > days {
        // The last weekday of the sprint: `days` weekdays counting `first`.
        let p = first_wd + days - 1;
        let last = first - first_wd + p / 5 * 7 + p % 5;
        return Some(format!("{name}, ended {}", short_date(last)));
    }
    Some(format!("{name}, day {reached} of {days}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ymd(y: i32, m: u32, d: u32) -> Ymd {
        Ymd { y, m, d }
    }

    fn label(today: Ymd) -> Option<String> {
        sprint_label("sprint-26-01", Some(ymd(2026, 11, 9)), 10, today)
    }

    #[test]
    fn day_counts_weekdays_from_the_start() {
        let cases = [
            (ymd(2026, 11, 9), "sprint-26-01, day 1 of 10"),
            (ymd(2026, 11, 13), "sprint-26-01, day 5 of 10"),
            (ymd(2026, 11, 14), "sprint-26-01, day 5 of 10"),
            (ymd(2026, 11, 16), "sprint-26-01, day 6 of 10"),
            (ymd(2026, 11, 20), "sprint-26-01, day 10 of 10"),
            (ymd(2026, 11, 23), "sprint-26-01, ended Fri 20 Nov"),
            (ymd(2026, 11, 6), "sprint-26-01, starts Mon 9 Nov"),
        ];
        for (today, want) in cases {
            assert_eq!(label(today).as_deref(), Some(want), "{today:?}");
        }
    }

    #[test]
    fn a_weekend_start_begins_on_the_monday() {
        let l = sprint_label(
            "sprint-26-01",
            Some(ymd(2026, 11, 14)),
            10,
            ymd(2026, 11, 14),
        );
        assert_eq!(l.as_deref(), Some("sprint-26-01, starts Mon 16 Nov"));
    }

    #[test]
    fn no_start_gives_the_name_and_no_name_gives_nothing() {
        let today = ymd(2026, 11, 9);
        assert_eq!(
            sprint_label("sprint-26-01", None, 10, today).as_deref(),
            Some("sprint-26-01")
        );
        assert_eq!(sprint_label("", Some(today), 10, today), None);
    }

    #[test]
    fn zero_days_means_ten() {
        let l = sprint_label("s", Some(ymd(2026, 11, 9)), 0, ymd(2026, 11, 20));
        assert_eq!(l.as_deref(), Some("s, day 10 of 10"));
    }

    #[test]
    fn a_short_sprint_ends_on_its_last_weekday() {
        let l = sprint_label("s", Some(ymd(2026, 11, 9)), 3, ymd(2026, 11, 13));
        assert_eq!(l.as_deref(), Some("s, ended Wed 11 Nov"));
    }

    #[test]
    fn parse_ymd_rejects_impossible_dates() {
        assert_eq!(parse_ymd("2026-02-29"), None);
        assert_eq!(parse_ymd("2026-13-01"), None);
        assert_eq!(parse_ymd("26-11-09"), None);
        assert_eq!(parse_ymd("2026-11-09-1"), None);
        assert_eq!(parse_ymd("2028-02-29"), Some(ymd(2028, 2, 29)));
        assert_eq!(parse_ymd(" 2026-11-09 "), Some(ymd(2026, 11, 9)));
    }

    #[test]
    fn civil_days_round_trip() {
        for days in [-1000, 0, 365, 11_000, 20_400, 50_000] {
            assert_eq!(to_days(from_days(days)), days);
        }
        assert_eq!(short_date(to_days(ymd(2026, 11, 9))), "Mon 9 Nov");
    }

    #[test]
    fn newest_sprint_folder_picks_the_last_by_name() {
        let dir = std::env::temp_dir().join(format!("ovs-sprint-{}", uuid::Uuid::new_v4()));
        let sprints = dir.join("Sprints");
        for d in ["sprint-26-01", "sprint-26-02", "notes"] {
            std::fs::create_dir_all(sprints.join(d)).unwrap();
        }
        std::fs::write(sprints.join("sprint-26-03"), "").unwrap();
        assert_eq!(newest_sprint_folder(&dir).as_deref(), Some("sprint-26-02"));
        assert_eq!(newest_sprint_folder(&dir.join("none")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
