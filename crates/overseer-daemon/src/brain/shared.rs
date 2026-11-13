//! Shared vault files: journal append, exact-replacement edit, per-file locks.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::sync::OwnedMutexGuard;

use super::paths::local_date;

/// How much of the file a failed edit shows back (Ruling 16).
const CUT: usize = 16 * 1024;

/// One async mutex per path, so two writers of one file never interleave.
#[derive(Default)]
pub struct FileLocks {
    map: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
}

impl FileLocks {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn lock(&self, p: &Path) -> OwnedMutexGuard<()> {
        let m = {
            let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
            map.entry(p.to_path_buf()).or_default().clone()
        };
        m.lock_owned().await
    }

    pub fn is_locked(&self, p: &Path) -> bool {
        let map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        map.get(p).is_some_and(|m| m.try_lock().is_err())
    }
}

/// Writes `text` to `<name>.overseer-tmp` beside `path`, then renames it over
/// `path`, so a reader never sees half a file.
pub fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".overseer-tmp");
    let tmp = path.with_file_name(name);
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// The first line of a new journal, with its newline.
pub fn journal_header(now: i64) -> String {
    let d = local_date(now);
    format!(
        "# Journal - {}, {} {}, {}\n",
        d.weekday_name, d.month_name, d.day, d.year
    )
}

/// Puts `text` at the end of the `## <section>` block, after a blank line;
/// a missing section is added at the end of the file (Ruling 17).
pub fn append_section(file: &str, section: &str, text: &str) -> String {
    let text = text.replace('\u{2014}', "-");
    let text = text.trim_matches('\n');
    let heading = format!("## {section}");
    let lines: Vec<&str> = file.lines().collect();
    let Some(h) = lines.iter().position(|l| l.trim_end() == heading) else {
        let body = file.trim_end();
        return if body.is_empty() {
            format!("{heading}\n\n{text}\n")
        } else {
            format!("{body}\n\n{heading}\n\n{text}\n")
        };
    };
    let next = lines[h + 1..]
        .iter()
        .position(|l| l.starts_with("## "))
        .map_or(lines.len(), |i| h + 1 + i);
    let mut end = next;
    while end > h + 1 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    let mut out: Vec<&str> = lines[..end].to_vec();
    out.push("");
    out.extend(text.split('\n'));
    if next < lines.len() {
        out.push("");
        out.extend(&lines[next..]);
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// Replaces the one occurrence of `old`; any other count is an error that
/// leaves the caller's file alone (Ruling 16).
pub fn replace_once(file: &str, old: &str, new: &str) -> Result<String, String> {
    if old.is_empty() {
        return Err("old_text is empty".into());
    }
    match file.matches(old).count() {
        1 => Ok(file.replacen(old, new, 1)),
        0 => Err(format!(
            "old_text was not found; the file has changed. Current text:\n{}",
            tail(file)
        )),
        n => Err(format!("old_text occurs {n} times; include more context.")),
    }
}

fn tail(file: &str) -> String {
    if file.len() <= CUT {
        return file.to_string();
    }
    let mut start = file.len() - CUT;
    while !file.is_char_boundary(start) {
        start += 1;
    }
    format!(
        "(showing the last 16 KB of {} KB)\n{}",
        file.len().div_ceil(1024),
        &file[start..]
    )
}

/// Reads, replaces once and writes atomically, all under the file's lock.
pub async fn edit_file(path: &Path, old: &str, new: &str, locks: &FileLocks) -> Result<(), String> {
    let _g = locks.lock(path).await;
    let file = std::fs::read_to_string(path).map_err(|e| format!("cannot read file: {e}"))?;
    let next = replace_once(&file, old, new)?;
    write_atomic(path, &next).map_err(|e| format!("cannot write file: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "# Journal - X\n\n## Work Log\n\n- one\n\n## Context / Notes\n\nnote\n";

    #[test]
    fn append_goes_at_the_end_of_its_section() {
        let out = append_section(DOC, "Work Log", "- two");
        assert_eq!(
            out,
            "# Journal - X\n\n## Work Log\n\n- one\n\n- two\n\n## Context / Notes\n\nnote\n"
        );
        let last = append_section(&out, "Context / Notes", "more");
        assert!(last.ends_with("note\n\nmore\n"));
    }

    #[test]
    fn missing_section_is_added_at_the_end() {
        let out = append_section(DOC, "Decisions", "- chose A");
        assert!(out.starts_with(DOC.trim_end()));
        assert!(out.ends_with("\n\n## Decisions\n\n- chose A\n"));
        assert_eq!(append_section("", "A", "t"), "## A\n\nt\n");
    }

    #[test]
    fn new_journal_gets_its_header() {
        let now = 1_794_571_200; // 2026-11-13 12:00 UTC
        let d = local_date(now);
        let h = journal_header(now);
        assert_eq!(
            h,
            format!(
                "# Journal - {}, {} {}, {}\n",
                d.weekday_name, d.month_name, d.day, d.year
            )
        );
        // 12:00 UTC is 2026-11-13 in every zone from UTC-12 to UTC+11.
        if d.day == 13 {
            assert_eq!(h, "# Journal - Friday, November 13, 2026\n");
        }
    }

    #[test]
    fn em_dashes_become_hyphens() {
        let out = append_section(DOC, "Work Log", "a \u{2014} b");
        assert!(out.contains("a - b"));
        assert!(!out.contains('\u{2014}'));
    }

    #[test]
    fn replace_once_rules() {
        assert_eq!(replace_once("a b c", "b", "X").unwrap(), "a X c");
        let e = replace_once("a b c", "z", "X").unwrap_err();
        assert!(e.starts_with("old_text was not found; the file has changed. Current text:\n"));
        assert!(e.ends_with("a b c"));
        assert_eq!(
            replace_once("b b", "b", "X").unwrap_err(),
            "old_text occurs 2 times; include more context."
        );
    }

    #[test]
    fn long_files_are_cut_on_a_char_boundary() {
        let file = "\u{e9}".repeat(20_000); // 2 bytes each, 40000 bytes
        let e = replace_once(&file, "zz", "y").unwrap_err();
        assert!(e.contains("(showing the last 16 KB of 40 KB)"));
        assert!(e.len() < 17_000);
    }

    fn temp(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("ov-brain-shared-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn stale_edit_keeps_jonathans_change() {
        let dir = temp("stale");
        let f = dir.join("SPRINT-DASHBOARD.md");
        write_atomic(&f, "status: todo\n").unwrap();
        // Jonathan edits in Obsidian after the session last read it.
        write_atomic(&f, "status: doing\n").unwrap();
        let locks = FileLocks::new();
        let e = edit_file(&f, "status: todo", "status: done", &locks)
            .await
            .unwrap_err();
        assert!(e.contains("status: doing"));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "status: doing\n");
        edit_file(&f, "status: doing", "status: done", &locks)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "status: done\n");
        assert!(!dir.join("SPRINT-DASHBOARD.md.overseer-tmp").exists());
    }

    #[tokio::test]
    async fn locks_report_and_serialise() {
        let locks = FileLocks::new();
        let p = Path::new("/x/y.md");
        assert!(!locks.is_locked(p));
        let g = locks.lock(p).await;
        assert!(locks.is_locked(p));
        assert!(!locks.is_locked(Path::new("/x/z.md")));
        drop(g);
        assert!(!locks.is_locked(p));
    }
}
