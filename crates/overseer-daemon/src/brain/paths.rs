//! Vault paths: which of them are shared, owned by a slot, the
//! `CURRENT_SESSION` file, free or outside the vault; and the date templates.

use std::path::{Component, Path, PathBuf};

use overseer_proto::msg::{ItemKind, SlotId, WorkView};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathClass {
    Outside,
    CurrentSession,
    Shared,
    /// Owning slots with what they own: `session note` or an item key.
    Owned(Vec<(SlotId, String)>),
    Free,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Owners {
    /// Slot and vault-relative session note.
    pub notes: Vec<(SlotId, String)>,
    /// Slot, vault-relative folder and item key.
    pub folders: Vec<(SlotId, String, String)>,
}

pub struct PathContext<'a> {
    pub brain: &'a Path,
    pub home: &'a Path,
    pub cwd: Option<&'a Path>,
    pub shared_files: &'a [String],
    pub current_session: &'a Path,
    pub owners: &'a Owners,
}

/// Who owns what, from the slots that are running or resumable (`live`):
/// each live slot's session note, the folder of every item it owns, and the
/// parent ticket's folder for a bead it owns.
pub fn owners(
    work: &WorkView,
    notes: &[(SlotId, String)],
    live: &[SlotId],
    brain: &Path,
) -> Owners {
    let mut out = Owners {
        notes: notes
            .iter()
            .filter(|(s, _)| live.contains(s))
            .cloned()
            .collect(),
        folders: Vec::new(),
    };
    let rel_folder = |f: &Path| -> Option<String> {
        let r = if f.is_absolute() {
            f.strip_prefix(brain).ok()?
        } else {
            f
        };
        let s = r.to_str()?.trim_matches('/');
        (!s.is_empty()).then(|| s.to_string())
    };
    let mut add = |slot: SlotId, folder: &Path, key: &str| {
        if let Some(f) = rel_folder(folder) {
            let e = (slot, f, key.to_string());
            if !out.folders.contains(&e) {
                out.folders.push(e);
            }
        }
    };
    for it in &work.items {
        let Some(slot) = it.owner.filter(|s| live.contains(s)) else {
            continue;
        };
        if let Some(f) = &it.folder {
            add(slot, f, &it.key);
        }
        if it.kind == ItemKind::Bead {
            let parent = it
                .parent
                .as_deref()
                .and_then(|k| work.items.iter().find(|p| p.key == k));
            if let Some(p) = parent {
                if let Some(f) = &p.folder {
                    add(slot, f, &p.key);
                }
            }
        }
    }
    out
}

/// `~` and a relative path resolved, `.` and `..` folded. A relative path is
/// read against `cwd`, else against the vault.
fn absolute(brain: &Path, cwd: Option<&Path>, p: &Path, home: &Path) -> PathBuf {
    let joined = if let Ok(rest) = p.strip_prefix("~") {
        home.join(rest)
    } else if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.unwrap_or(brain).join(p)
    };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// Symlinks resolved for the longest existing prefix, the rest appended. A
/// dangling symlink in the prefix gives `None`: where it leads is unknown.
fn resolve(abs: &Path) -> Option<PathBuf> {
    let mut rest: Vec<&std::ffi::OsStr> = Vec::new();
    let mut cur = abs;
    loop {
        match cur.canonicalize() {
            Ok(mut base) => {
                base.extend(rest.iter().rev());
                return Some(base);
            }
            Err(_) => {
                if cur.symlink_metadata().is_ok() {
                    return None;
                }
                rest.push(cur.file_name()?);
                cur = cur.parent()?;
            }
        }
    }
}

/// The vault-relative form of `p`, with whether it is a folder.
fn locate(
    brain: &Path,
    cwd: Option<&Path>,
    p: &Path,
    home: &Path,
) -> Result<(String, bool), String> {
    let outside = || format!("{} is outside 2ndBrain", p.display());
    let root = resolve(&absolute(brain, None, brain, home)).ok_or_else(outside)?;
    let full = resolve(&absolute(brain, cwd, p, home)).ok_or_else(outside)?;
    let rel = full.strip_prefix(&root).map_err(|_| outside())?;
    let rel = rel.to_str().ok_or_else(outside)?;
    Ok((rel.to_string(), full.is_dir()))
}

/// The vault-relative path of a file: `~` and cwd resolved, `.` and `..`
/// folded, symlinks resolved for the existing prefix.
pub fn vault_rel(
    brain: &Path,
    cwd: Option<&Path>,
    p: &Path,
    home: &Path,
) -> Result<String, String> {
    match locate(brain, cwd, p, home)? {
        (_, true) => Err(format!("{} is a folder", p.display())),
        (rel, false) if rel.is_empty() => Err(format!("{} is a folder", p.display())),
        (rel, false) => Ok(rel),
    }
}

pub fn classify(p: &Path, ctx: &PathContext) -> PathClass {
    let abs = |x: &Path| resolve(&absolute(ctx.brain, ctx.cwd, x, ctx.home));
    if let (Some(a), Some(b)) = (abs(p), abs(ctx.current_session)) {
        if a == b {
            return PathClass::CurrentSession;
        }
    }
    let Ok((rel, _)) = locate(ctx.brain, ctx.cwd, p, ctx.home) else {
        return PathClass::Outside;
    };
    if shared_match(ctx.shared_files, &rel) {
        return PathClass::Shared;
    }
    let mut hit: Vec<(SlotId, String)> = Vec::new();
    for (slot, note) in &ctx.owners.notes {
        if *note == rel {
            hit.push((*slot, "session note".into()));
        }
    }
    for (slot, folder, key) in &ctx.owners.folders {
        let inside = rel == *folder
            || rel
                .strip_prefix(folder.as_str())
                .is_some_and(|r| r.starts_with('/'));
        if inside {
            hit.push((*slot, key.clone()));
        }
    }
    hit.sort();
    hit.dedup();
    if hit.is_empty() {
        PathClass::Free
    } else {
        PathClass::Owned(hit)
    }
}

/// `*` matches within one path segment, never across a `/`.
fn seg_match(pat: &str, s: &str) -> bool {
    let parts: Vec<&str> = pat.split('*').collect();
    if parts.len() == 1 {
        return pat == s;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if s.len() < first.len() + last.len() || !s.starts_with(first) || !s.ends_with(last) {
        return false;
    }
    let mut at = first.len();
    let end = s.len() - last.len();
    for mid in &parts[1..parts.len() - 1] {
        match s[at..end].find(mid) {
            Some(i) => at += i + mid.len(),
            None => return false,
        }
    }
    true
}

pub fn shared_match(globs: &[String], rel: &str) -> bool {
    let segs: Vec<&str> = rel.split('/').collect();
    globs.iter().any(|g| {
        let pats: Vec<&str> = g.split('/').collect();
        pats.len() == segs.len() && pats.iter().zip(&segs).all(|(p, s)| seg_match(p, s))
    })
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

pub struct LocalDate {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub month_name: &'static str,
    pub weekday_name: &'static str,
}

/// The local calendar date of unix seconds, from `localtime_r`.
pub fn local_date(now: i64) -> LocalDate {
    // SAFETY: localtime_r only writes the `tm` we pass it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let time = now as libc::time_t;
    let ok = !unsafe { libc::localtime_r(&time, &mut tm) }.is_null();
    let (m, w) = if ok {
        (
            tm.tm_mon.clamp(0, 11) as usize,
            tm.tm_wday.clamp(0, 6) as usize,
        )
    } else {
        (0, 0)
    };
    LocalDate {
        year: if ok { tm.tm_year + 1900 } else { 1970 },
        month: m as u32 + 1,
        day: if ok { tm.tm_mday as u32 } else { 1 },
        month_name: MONTHS[m],
        weekday_name: WEEKDAYS[w],
    }
}

/// Lower case, `[a-z0-9-]` only, runs of other characters one `-`, no edge
/// `-`; empty becomes `session`.
pub fn slug(label: &str) -> String {
    let mut out = String::new();
    for c in label.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-');
    if out.is_empty() {
        "session".into()
    } else {
        out.into()
    }
}

/// Fills `{yyyy}`, `{mm}`, `{dd}`, `{Month}`, `{slot}` (`s3`) and `{label}`
/// (slugged) from the local date of `now`.
pub fn fill_template(t: &str, now: i64, slot: SlotId, label: &str) -> String {
    let d = local_date(now);
    t.replace("{yyyy}", &format!("{:04}", d.year))
        .replace("{mm}", &format!("{:02}", d.month))
        .replace("{dd}", &format!("{:02}", d.day))
        .replace("{Month}", d.month_name)
        .replace("{slot}", &format!("s{slot}"))
        .replace("{label}", &slug(label))
}

#[cfg(test)]
mod tests {
    use super::*;
    use overseer_proto::msg::WorkItem;

    fn item(
        key: &str,
        kind: ItemKind,
        owner: Option<SlotId>,
        parent: Option<&str>,
        folder: Option<&str>,
    ) -> WorkItem {
        WorkItem {
            key: key.into(),
            kind,
            title: String::new(),
            status: String::new(),
            priority: None,
            parent: parent.map(Into::into),
            owner,
            in_list: true,
            folder: folder.map(PathBuf::from),
        }
    }

    fn view(items: Vec<WorkItem>) -> WorkView {
        WorkView {
            items,
            sources: vec![],
            notices: vec![],
        }
    }

    fn globs() -> Vec<String> {
        vec![
            "06-Daily/*/*/*-journal.md".into(),
            "Sprints/*/SPRINT-DASHBOARD.md".into(),
            "Sprints/*/sprint-*.md".into(),
        ]
    }

    const FOLDER: &str = "Sprints/sprint-26-01/DEMO-12-demo-lane";

    #[test]
    fn shared_globs_match_one_segment() {
        let g = globs();
        assert!(shared_match(
            &g,
            "06-Daily/2026/11-November/2026-11-13-journal.md"
        ));
        assert!(!shared_match(&g, "06-Daily/2026/x.md"));
        assert!(shared_match(&g, "Sprints/sprint-26-01/SPRINT-DASHBOARD.md"));
        assert!(!shared_match(
            &g,
            "Sprints/sprint-26-01/DEMO-12-demo-lane/notes.md"
        ));
    }

    #[test]
    fn ticket_folder_is_owned_by_owner_and_bead_owner() {
        let brain = Path::new("/vault");
        let w = view(vec![
            item(
                "DEMO-12",
                ItemKind::Ticket,
                Some(1),
                None,
                Some("/vault/Sprints/sprint-26-01/DEMO-12-demo-lane"),
            ),
            item("ov-3", ItemKind::Bead, Some(2), Some("DEMO-12"), None),
        ]);
        let o = owners(&w, &[(1, "Sessions/a.md".into())], &[1, 2], brain);
        assert_eq!(o.notes, vec![(1, "Sessions/a.md".to_string())]);
        assert!(o.folders.contains(&(1, FOLDER.into(), "DEMO-12".into())));
        assert!(o.folders.contains(&(2, FOLDER.into(), "DEMO-12".into())));
        assert_eq!(o.folders.len(), 2);
    }

    #[test]
    fn two_owners_are_both_listed() {
        let o = Owners {
            notes: vec![],
            folders: vec![
                (1, FOLDER.into(), "DEMO-12".into()),
                (2, FOLDER.into(), "DEMO-12".into()),
            ],
        };
        let g = globs();
        let ctx = PathContext {
            brain: Path::new("/vault"),
            home: Path::new("/home/x"),
            cwd: None,
            shared_files: &g,
            current_session: Path::new("/home/x/.claude/CURRENT_SESSION.md"),
            owners: &o,
        };
        let c = classify(
            Path::new("/vault/Sprints/sprint-26-01/DEMO-12-demo-lane/notes.md"),
            &ctx,
        );
        assert_eq!(
            c,
            PathClass::Owned(vec![(1, "DEMO-12".into()), (2, "DEMO-12".into())])
        );
        // A sibling folder with the same prefix is not owned.
        let c = classify(
            Path::new("/vault/Sprints/sprint-26-01/DEMO-12-demo-lane-old/n.md"),
            &ctx,
        );
        assert_eq!(c, PathClass::Free);
    }

    #[test]
    fn exited_slots_own_nothing() {
        let w = view(vec![item(
            "DEMO-12",
            ItemKind::Ticket,
            Some(1),
            None,
            Some(FOLDER),
        )]);
        let o = owners(
            &w,
            &[(1, "Sessions/a.md".into()), (3, "Sessions/b.md".into())],
            &[3],
            Path::new("/vault"),
        );
        assert_eq!(o.notes, vec![(3, "Sessions/b.md".to_string())]);
        assert!(o.folders.is_empty());
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ov-brain-paths-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    #[test]
    fn odd_paths() {
        let root = tmp("odd");
        let brain = root.join("vault");
        let outside = root.join("elsewhere");
        let home = root.join("home");
        std::fs::create_dir_all(brain.join("Sessions/notes")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(outside.join("secret.md"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, brain.join("link")).unwrap();

        let cwd = brain.join("Sessions");
        let rel = |p: &str| vault_rel(&brain, Some(&cwd), Path::new(p), &home);

        assert_eq!(
            rel("../../outside.md").unwrap_err(),
            "../../outside.md is outside 2ndBrain"
        );
        assert_eq!(
            vault_rel(
                &brain,
                Some(&cwd),
                &brain.join("Sessions/./x/../a.md"),
                &home
            )
            .unwrap(),
            "Sessions/a.md"
        );
        assert!(rel("../link/secret.md")
            .unwrap_err()
            .ends_with("is outside 2ndBrain"));
        assert!(rel("../link/new.md")
            .unwrap_err()
            .ends_with("is outside 2ndBrain"));
        assert_eq!(
            rel("notes/demo api é.md").unwrap(),
            "Sessions/notes/demo api é.md"
        );
        assert_eq!(rel("notes").unwrap_err(), "notes is a folder");
        // `~` resolves against home; home here is outside the vault.
        assert!(rel("~/a.md").unwrap_err().ends_with("is outside 2ndBrain"));
        // No cwd: a relative path is read against the vault.
        assert_eq!(
            vault_rel(&brain, None, Path::new("Sessions/a.md"), &home).unwrap(),
            "Sessions/a.md"
        );
    }

    #[test]
    fn current_session_is_its_own_class() {
        let root = tmp("cur");
        let home = root.join("home");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        let o = Owners::default();
        let g = globs();
        let cs = home.join(".claude/CURRENT_SESSION.md");
        let brain = root.join("vault");
        let ctx = PathContext {
            brain: &brain,
            home: &home,
            cwd: None,
            shared_files: &g,
            current_session: &cs,
            owners: &o,
        };
        assert_eq!(
            classify(Path::new("~/.claude/CURRENT_SESSION.md"), &ctx),
            PathClass::CurrentSession
        );
        assert_eq!(classify(&cs, &ctx), PathClass::CurrentSession);
        assert_eq!(
            classify(&home.join(".claude/other.md"), &ctx),
            PathClass::Outside
        );
    }

    #[test]
    fn templates_fill() {
        // Midday UTC on the 13th is the 13th in every zone.
        let now = 1_794_571_200; // 2026-11-13 12:00:00 UTC
        let d = local_date(now);
        assert_eq!((d.year, d.month, d.day), (2026, 11, 13));
        assert_eq!(d.month_name, "November");
        assert_eq!(d.weekday_name, "Friday");
        assert_eq!(
            fill_template(
                "Sessions/{yyyy}/{mm}-{Month}/{yyyy}-{mm}-{dd}-{slot}-{label}.md",
                now,
                3,
                "Demo API!"
            ),
            "Sessions/2026/11-November/2026-11-13-s3-demo-api.md"
        );
        assert_eq!(fill_template("{slot}-{label}", now, 0, ""), "s0-session");
        assert_eq!(slug("  --Ünï  x "), "n-x");
    }
}
