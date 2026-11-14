//! `config.toml` as the Settings view shows it, and the only path the app
//! takes to change it (Decision 8): edits are checked, applied with
//! `toml_edit` so comments, order and unknown keys stay, refused when the
//! file changed since it was read, and written atomically through a
//! symlink. Any refusal leaves the file byte for byte as it was.

use std::path::{Path, PathBuf};

use overseer_proto::settings::{
    Applies, Setting, SettingEdit, SettingKind, SettingValue, SettingsView, DEFAULT_EDITOR,
    DEFAULT_FONT_FAMILY, DEFAULT_FONT_SIZE,
};
use overseer_proto::DaemonInfo;
use toml_edit::{DocumentMut, Item, Value};

use crate::config::Config;

/// The files Settings shows, and what the daemon found in them at start.
#[derive(Debug, Clone, Default)]
pub struct SettingsFiles {
    pub config: PathBuf,
    /// Why `config` did not load at daemon start.
    pub config_error: Option<String>,
    pub targets: PathBuf,
    pub targets_note: String,
    pub targets_error: Option<String>,
}

impl SettingsFiles {
    /// `config.toml` and `targets.toml` in `dir`, with no note: for tests
    /// and tools that run a daemon against a temp folder.
    pub fn in_dir(dir: &Path) -> SettingsFiles {
        SettingsFiles {
            config: dir.join("config.toml"),
            targets: dir.join("targets.toml"),
            ..SettingsFiles::default()
        }
    }
}

/// `<n> patterns in use, read from <path> at daemon start`, or
/// `<n> built-in patterns; <path> does not exist`.
pub fn targets_note(patterns: usize, path: &Path) -> String {
    if path.exists() {
        format!(
            "{patterns} patterns in use, read from {} at daemon start",
            path.display()
        )
    } else {
        format!(
            "{patterns} built-in patterns; {} does not exist",
            path.display()
        )
    }
}

/// One row of the catalogue. `get` reads the key from a daemon `Config`;
/// app keys have none.
pub struct Spec {
    pub key: &'static str,
    pub group: &'static str,
    pub kind: SettingKind,
    pub applies: Applies,
    pub note: Option<&'static str>,
    pub get: Option<fn(&Config) -> SettingValue>,
}

fn text(s: &str) -> SettingValue {
    SettingValue::Text(s.to_string())
}

fn path(p: &Path) -> SettingValue {
    SettingValue::Text(p.display().to_string())
}

fn list(v: &[String]) -> SettingValue {
    SettingValue::List(v.to_vec())
}

fn int(n: u64) -> SettingValue {
    SettingValue::Int(i64::try_from(n).unwrap_or(i64::MAX))
}

const WORK: &str = "Work items";
const SPRINT: &str = "Sprint";
const LOCKS: &str = "Locks and worktrees";
const BRAIN: &str = "2ndBrain";
const COMMANDS: &str = "Commands";
const TERMINAL: &str = "Terminal";
const EDITOR: &str = "Editor";

const fn daemon(
    key: &'static str,
    group: &'static str,
    kind: SettingKind,
    get: fn(&Config) -> SettingValue,
) -> Spec {
    Spec {
        key,
        group,
        kind,
        applies: Applies::Restart,
        note: None,
        get: Some(get),
    }
}

const fn read_only(
    key: &'static str,
    group: &'static str,
    kind: SettingKind,
    note: &'static str,
    get: Option<fn(&Config) -> SettingValue>,
) -> Spec {
    Spec {
        key,
        group,
        kind,
        applies: Applies::ReadOnly,
        note: Some(note),
        get,
    }
}

const fn app(key: &'static str, group: &'static str, kind: SettingKind) -> Spec {
    Spec {
        key,
        group,
        kind,
        applies: Applies::App,
        note: None,
        get: None,
    }
}

const POLL: SettingKind = SettingKind::Int { min: 5, max: 3600 };
const NOT_EMPTY: SettingKind = SettingKind::Text { allow_empty: false };
const COMMAND_NOTE: &str = "A typo turns this source off, so change it in config.toml.";

/// Every key Settings shows, in the order and groups it shows them.
pub const CATALOGUE: &[Spec] = &[
    daemon("jql", WORK, NOT_EMPTY, |c| text(&c.jql)),
    daemon("jira_poll_secs", WORK, POLL, |c| int(c.jira_poll_secs)),
    daemon("beads_poll_secs", WORK, POLL, |c| int(c.beads_poll_secs)),
    daemon("ticket_pattern", WORK, SettingKind::Pattern, |c| {
        text(&c.ticket_pattern)
    }),
    daemon(
        "sprint_name",
        SPRINT,
        SettingKind::Text { allow_empty: true },
        |c| text(&c.sprint_name),
    ),
    daemon("sprint_start", SPRINT, SettingKind::Date, |c| {
        text(&c.sprint_start)
    }),
    daemon(
        "sprint_days",
        SPRINT,
        SettingKind::Int { min: 1, max: 30 },
        |c| SettingValue::Int(i64::from(c.sprint_days)),
    ),
    daemon(
        "lock_idle_mins",
        LOCKS,
        SettingKind::Int { min: 1, max: 10080 },
        |c| int(c.lock_idle_mins),
    ),
    daemon("worktree_root", LOCKS, SettingKind::Path, |c| {
        path(&c.worktree_root)
    }),
    daemon("brain_dir", BRAIN, SettingKind::Path, |c| {
        path(&c.brain_dir)
    }),
    daemon("beads_dir", BRAIN, SettingKind::Path, |c| {
        path(&c.beads_dir)
    }),
    daemon("current_session", BRAIN, SettingKind::Path, |c| {
        path(&c.current_session)
    }),
    daemon("journal_path", BRAIN, NOT_EMPTY, |c| text(&c.journal_path)),
    daemon("session_note", BRAIN, NOT_EMPTY, |c| text(&c.session_note)),
    daemon("shared_files", BRAIN, SettingKind::List, |c| {
        list(&c.shared_files)
    }),
    daemon(
        "backup_mins",
        BRAIN,
        SettingKind::Int { min: 0, max: 240 },
        |c| int(c.backup_mins),
    ),
    daemon("push", BRAIN, SettingKind::Bool, |c| {
        SettingValue::Bool(c.push)
    }),
    read_only(
        "brain_writer",
        BRAIN,
        SettingKind::Bool,
        "Turning the writer on has a setup checklist, so change it in config.toml.",
        Some(|c| SettingValue::Bool(c.brain_writer)),
    ),
    read_only(
        "jira_command",
        COMMANDS,
        SettingKind::List,
        COMMAND_NOTE,
        Some(|c| list(&c.jira_command)),
    ),
    read_only(
        "bd_command",
        COMMANDS,
        SettingKind::List,
        COMMAND_NOTE,
        Some(|c| list(&c.bd_command)),
    ),
    read_only(
        "sync_command",
        COMMANDS,
        SettingKind::List,
        COMMAND_NOTE,
        Some(|c| list(&c.sync_command)),
    ),
    app("terminal.font_family", TERMINAL, NOT_EMPTY),
    app(
        "terminal.font_size",
        TERMINAL,
        SettingKind::Float {
            min: 4.0,
            max: 96.0,
        },
    ),
    app("terminal.background", TERMINAL, SettingKind::Colour),
    app("terminal.foreground", TERMINAL, SettingKind::Colour),
    read_only(
        "terminal.ansi",
        TERMINAL,
        SettingKind::List,
        "The 16 colours are set in config.toml.",
        None,
    ),
    app("editor.name", EDITOR, NOT_EMPTY),
];

/// The catalogue row for `key`.
pub fn spec(key: &str) -> Option<&'static Spec> {
    CATALOGUE.iter().find(|s| s.key == key)
}

fn hex(c: overseer_term::Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", c.0, c.1, c.2)
}

/// The value that applies when the file does not set the key.
pub fn default_value(spec: &Spec) -> SettingValue {
    if let Some(get) = spec.get {
        return get(&Config::default());
    }
    let palette = overseer_term::Palette::default();
    match spec.key {
        "terminal.font_family" => text(DEFAULT_FONT_FAMILY),
        "terminal.font_size" => SettingValue::Float(f64::from(DEFAULT_FONT_SIZE)),
        "terminal.background" => SettingValue::Text(hex(palette.background)),
        "terminal.foreground" => SettingValue::Text(hex(palette.foreground)),
        "terminal.ansi" => SettingValue::List(palette.ansi.iter().map(|c| hex(*c)).collect()),
        "editor.name" => text(DEFAULT_EDITOR),
        other => unreachable!("catalogue key {other} has no default"),
    }
}

/// FNV-1a 64 over every byte, as 16 hex digits; `""` for no text (a
/// missing file).
pub fn file_hash(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Whether `value` fits the setting.
pub fn check(spec: &Spec, value: &SettingValue) -> Result<(), String> {
    overseer_proto::settings::check(spec.key, &spec.kind, value)
}

/// `(table, key)` for `terminal.font_size`, `(None, key)` for a top-level key.
fn split(key: &str) -> (Option<&str>, &str) {
    match key.split_once('.') {
        Some((t, k)) => (Some(t), k),
        None => (None, key),
    }
}

fn lookup<'a>(doc: &'a DocumentMut, key: &str) -> Option<&'a Item> {
    match split(key) {
        (Some(t), k) => doc.get(t)?.get(k),
        (None, k) => doc.get(k),
    }
}

fn from_item(item: &Item, kind: &SettingKind) -> SettingValue {
    let raw = |s: String| SettingValue::Text(s.trim().to_string());
    let Some(v) = item.as_value() else {
        return raw(item.to_string());
    };
    match v {
        Value::String(s) => SettingValue::Text(s.value().clone()),
        Value::Integer(i) if matches!(kind, SettingKind::Float { .. }) => {
            SettingValue::Float(*i.value() as f64)
        }
        Value::Integer(i) => SettingValue::Int(*i.value()),
        Value::Float(f) => SettingValue::Float(*f.value()),
        Value::Boolean(b) => SettingValue::Bool(*b.value()),
        Value::Array(a) => SettingValue::List(
            a.iter()
                .map(|v| match v.as_str() {
                    Some(s) => s.to_string(),
                    None => v.to_string().trim().to_string(),
                })
                .collect(),
        ),
        other => raw(other.to_string()),
    }
}

/// The file's text; a missing file is empty.
fn read_text(p: &Path) -> Result<String, String> {
    match std::fs::read_to_string(p) {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("{}: {e}", p.display())),
    }
}

/// The view of `files` for a daemon running `running`. A value is the
/// file's as written (`~` not expanded), else the default; a key needs a
/// restart when the file's value, loaded as the daemon would, differs from
/// the running one.
pub fn read(
    files: &SettingsFiles,
    running: &Config,
    home: &Path,
    daemon: DaemonInfo,
) -> SettingsView {
    let (text, read_error) = match read_text(&files.config) {
        Ok(t) => (t, None),
        Err(e) => (String::new(), Some(e)),
    };
    let doc = text.parse::<DocumentMut>().ok();
    let (file_config, load_error) = Config::load(&files.config, home);
    let settings = CATALOGUE
        .iter()
        .map(|spec| {
            let default = default_value(spec);
            let found = doc.as_ref().and_then(|d| lookup(d, spec.key));
            let restart_needed = spec.applies != Applies::App
                && spec
                    .get
                    .is_some_and(|get| get(&file_config) != get(running));
            Setting {
                key: spec.key.to_string(),
                group: spec.group.to_string(),
                kind: spec.kind.clone(),
                applies: spec.applies.clone(),
                value: found.map_or_else(|| default.clone(), |i| from_item(i, &spec.kind)),
                default,
                in_file: found.is_some(),
                restart_needed,
                note: spec.note.map(str::to_string),
            }
        })
        .collect();
    SettingsView {
        config_path: files.config.clone(),
        file_hash: file_hash(&text),
        load_error: read_error
            .or(load_error)
            .or_else(|| files.config_error.clone()),
        settings,
        targets_path: files.targets.clone(),
        targets_note: files.targets_note.clone(),
        targets_error: files.targets_error.clone(),
        daemon,
    }
}

const STALE: &str =
    "config.toml changed on disk since Settings read it. Close and reopen Settings to see the new text.";

fn does_not_parse(e: impl std::fmt::Display) -> String {
    format!(
        "config.toml does not parse, so Settings will not change it: {}",
        e.to_string().trim()
    )
}

fn to_toml(spec: &Spec, value: &SettingValue) -> Value {
    match value {
        SettingValue::Text(s) => Value::from(s.as_str()),
        SettingValue::Bool(b) => Value::from(*b),
        SettingValue::Int(n) if matches!(spec.kind, SettingKind::Float { .. }) => {
            Value::from(*n as f64)
        }
        SettingValue::Int(n) => Value::from(*n),
        SettingValue::Float(x) => Value::from(*x),
        SettingValue::List(items) => Value::Array(items.iter().map(String::as_str).collect()),
    }
}

/// Sets `key` in `doc`, keeping the old value's comments; a missing
/// `[terminal]` or `[editor]` table is created.
fn set(doc: &mut DocumentMut, key: &str, value: Value) -> Result<(), String> {
    let (table, k) = split(key);
    let tbl = match table {
        None => doc.as_table_mut() as &mut dyn toml_edit::TableLike,
        Some(t) => doc
            .entry(t)
            .or_insert(toml_edit::table())
            .as_table_like_mut()
            .ok_or_else(|| format!("{t} in config.toml is not a table"))?,
    };
    match tbl.get_mut(k).and_then(Item::as_value_mut) {
        Some(old) => {
            let decor = old.decor().clone();
            *old = value;
            *old.decor_mut() = decor;
        }
        None => {
            tbl.insert(k, Item::Value(value));
        }
    }
    Ok(())
}

/// The comment lines written above a key, if any.
fn comment_above(decor: &toml_edit::Decor) -> Option<String> {
    let p = decor.prefix()?.as_str()?;
    p.contains('#').then(|| p.to_string())
}

/// Removes `key`. Comments written above it move to whatever now comes
/// next (the next key, the next table header or the end of the file), so a
/// reset never drops your notes.
fn remove(doc: &mut DocumentMut, key: &str) {
    let (table, k) = split(key);
    let tbl: &mut dyn toml_edit::TableLike = match table {
        None => doc.as_table_mut(),
        Some(t) => match doc.get_mut(t).and_then(Item::as_table_like_mut) {
            Some(tbl) => tbl,
            None => return,
        },
    };
    let Some(pos) = tbl.iter().position(|(name, _)| name == k) else {
        return;
    };
    let comment = tbl.key(k).and_then(|key| comment_above(key.leaf_decor()));
    tbl.remove(k);
    let Some(comment) = comment else {
        return;
    };
    if let Some((mut next, _)) = tbl.iter_mut().skip(pos).find(|(_, item)| item.is_value()) {
        let old = next
            .leaf_decor()
            .prefix()
            .and_then(|p| p.as_str())
            .unwrap_or("")
            .to_string();
        next.leaf_decor_mut().set_prefix(format!("{comment}{old}"));
        return;
    }
    // Root keys are written before every table, so any table header
    // follows them; a table's last key is followed by the next table.
    let after = match table {
        None => None,
        Some(t) => doc
            .get(t)
            .and_then(Item::as_table)
            .and_then(|t| t.position()),
    };
    let next_table = doc
        .as_table_mut()
        .iter_mut()
        .filter_map(|(_, item)| item.as_table_mut())
        .filter(|t| t.position().is_some_and(|p| after.is_none_or(|a| p > a)))
        .min_by_key(|t| t.position());
    match next_table {
        Some(t) => {
            let old = t
                .decor()
                .prefix()
                .and_then(|p| p.as_str())
                .unwrap_or("")
                .to_string();
            t.decor_mut().set_prefix(format!("{comment}{old}"));
        }
        None => {
            let old = doc.trailing().as_str().unwrap_or("").to_string();
            doc.set_trailing(format!("{old}{comment}"));
        }
    }
}

/// Applies `edits` to `config.toml` when its hash is still `base_hash`.
/// Every edit is checked and the result must load as a `Config` before
/// anything is written. Values are written as given (`~` unexpanded), so
/// `home` is not needed here; it keeps the arguments in step with `read`.
pub fn save(
    files: &SettingsFiles,
    _home: &Path,
    base_hash: &str,
    edits: &[SettingEdit],
) -> Result<(), String> {
    let text = read_text(&files.config)?;
    if file_hash(&text) != base_hash {
        return Err(STALE.to_string());
    }
    let mut doc = text.parse::<DocumentMut>().map_err(does_not_parse)?;
    for edit in edits {
        let key = match edit {
            SettingEdit::Set { key, .. } | SettingEdit::Reset { key } => key,
        };
        let spec = spec(key).ok_or_else(|| format!("no setting {key}"))?;
        if spec.applies == Applies::ReadOnly {
            return Err(format!("{key} is read-only here; edit config.toml"));
        }
        match edit {
            SettingEdit::Set { value, .. } => {
                check(spec, value)?;
                set(&mut doc, key, to_toml(spec, value))?;
            }
            SettingEdit::Reset { .. } => remove(&mut doc, key),
        }
    }
    if edits.is_empty() {
        return Ok(());
    }
    let new_text = doc.to_string();
    toml::from_str::<Config>(&new_text).map_err(does_not_parse)?;
    write_atomic(&files.config, &new_text)
        .map_err(|e| format!("cannot write {}: {e}", files.config.display()))
}

/// Writes through a symlink: the temp file goes beside the link's target
/// and is renamed over it, so the link stays a link.
fn write_atomic(p: &Path, text: &str) -> std::io::Result<()> {
    let target = match std::fs::symlink_metadata(p) {
        Ok(m) if m.file_type().is_symlink() => match std::fs::canonicalize(p) {
            Ok(t) => t,
            // A dangling link: create the file it names.
            Err(_) => {
                let to = std::fs::read_link(p)?;
                p.parent().unwrap_or(Path::new("/")).join(to)
            }
        },
        _ => p.to_path_buf(),
    };
    let dir = target.parent().unwrap_or(Path::new("/"));
    std::fs::create_dir_all(dir)?;
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.toml".to_string());
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let result = (|| {
        std::fs::write(&tmp, text)?;
        if let Ok(m) = std::fs::metadata(&target) {
            std::fs::set_permissions(&tmp, m.permissions())?;
        }
        std::fs::rename(&tmp, &target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> DaemonInfo {
        DaemonInfo {
            version: "0.1.0".into(),
            build: "dev".into(),
            pid: 1,
            started_at: 0,
            exe: PathBuf::from("/tmp/demo/overseer"),
        }
    }

    fn home() -> PathBuf {
        PathBuf::from("/Users/demo")
    }

    struct Rig {
        _dir: tempfile::TempDir,
        files: SettingsFiles,
    }

    impl Rig {
        fn new(body: Option<&str>) -> Rig {
            let dir = tempfile::tempdir().unwrap();
            let files = SettingsFiles::in_dir(dir.path());
            if let Some(b) = body {
                std::fs::write(&files.config, b).unwrap();
            }
            Rig { _dir: dir, files }
        }

        fn bytes(&self) -> Vec<u8> {
            std::fs::read(&self.files.config).unwrap()
        }

        fn text(&self) -> String {
            std::fs::read_to_string(&self.files.config).unwrap()
        }

        fn view(&self, running: &Config) -> SettingsView {
            read(&self.files, running, &home(), info())
        }

        fn hash(&self) -> String {
            self.view(&running_defaults()).file_hash
        }

        fn save(&self, edits: &[SettingEdit]) -> Result<(), String> {
            save(&self.files, &home(), &self.hash(), edits)
        }
    }

    fn running_defaults() -> Config {
        Config::load(Path::new("/nonexistent/overseer/config.toml"), &home()).0
    }

    fn row<'a>(v: &'a SettingsView, key: &str) -> &'a Setting {
        v.settings.iter().find(|s| s.key == key).unwrap()
    }

    fn put(key: &str, value: SettingValue) -> SettingEdit {
        SettingEdit::Set {
            key: key.into(),
            value,
        }
    }

    #[test]
    fn read_marks_file_values_and_defaults() {
        let rig = Rig::new(Some(
            "jql = \"project = DEMO\"\nworktree_root = \"~/wt\"\n[terminal]\nfont_size = 12\n",
        ));
        let v = rig.view(&running_defaults());
        assert_eq!(v.config_path, rig.files.config);
        assert_eq!(v.file_hash.len(), 16);
        assert_eq!(v.load_error, None);
        let jql = row(&v, "jql");
        assert!(jql.in_file);
        assert_eq!(jql.value, SettingValue::Text("project = DEMO".into()));
        assert_eq!(jql.group, "Work items");
        let wt = row(&v, "worktree_root");
        assert_eq!(wt.value, SettingValue::Text("~/wt".into()));
        assert_eq!(wt.default, SettingValue::Text("~/Repos/.worktrees".into()));
        let poll = row(&v, "jira_poll_secs");
        assert!(!poll.in_file);
        assert_eq!(poll.value, SettingValue::Int(300));
        assert_eq!(poll.value, poll.default);
        let size = row(&v, "terminal.font_size");
        assert!(size.in_file);
        assert_eq!(size.value, SettingValue::Float(12.0));
        assert_eq!(size.default, SettingValue::Float(13.0));
        assert_eq!(
            row(&v, "editor.name").value,
            SettingValue::Text("Visual Studio Code".into())
        );
        assert_eq!(
            row(&v, "terminal.background").default,
            SettingValue::Text("#000000".into())
        );
        assert_eq!(row(&v, "brain_writer").applies, Applies::ReadOnly);
        assert!(row(&v, "brain_writer").note.is_some());
        let keys: Vec<&str> = v.settings.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys.first(), Some(&"jql"));
        assert_eq!(keys.last(), Some(&"editor.name"));
        assert_eq!(keys.len(), CATALOGUE.len());
    }

    #[test]
    fn read_of_a_missing_file_is_all_defaults_and_no_hash() {
        let rig = Rig::new(None);
        let v = rig.view(&running_defaults());
        assert_eq!(v.file_hash, "");
        assert_eq!(v.load_error, None);
        assert!(v
            .settings
            .iter()
            .all(|s| !s.in_file && s.value == s.default));
        assert!(v.settings.iter().all(|s| !s.restart_needed));
    }

    #[test]
    fn restart_needed_when_the_file_differs_from_the_running_config() {
        let rig = Rig::new(Some(
            "lock_idle_mins = 15\nworktree_root = \"~/Repos/.worktrees\"\n",
        ));
        let v = rig.view(&running_defaults());
        assert!(row(&v, "lock_idle_mins").restart_needed);
        // `~` expands the same way on both sides.
        assert!(!row(&v, "worktree_root").restart_needed);
        assert!(!row(&v, "jql").restart_needed);
        let running = Config {
            lock_idle_mins: 15,
            ..running_defaults()
        };
        assert!(!row(&rig.view(&running), "lock_idle_mins").restart_needed);
    }

    #[test]
    fn app_keys_never_need_a_restart() {
        let rig = Rig::new(Some(
            "[terminal]\nfont_size = 20.0\nbackground = \"#1e1e2e\"\n[editor]\nname = \"Zed\"\n",
        ));
        let v = rig.view(&running_defaults());
        for s in v.settings.iter().filter(|s| s.applies == Applies::App) {
            assert!(!s.restart_needed, "{}", s.key);
        }
        assert!(row(&v, "editor.name").in_file);
    }

    #[test]
    fn save_keeps_comments_and_unknown_keys() {
        let rig = Rig::new(Some(
            "# my notes\njql = \"project = DEMO\"\n[custom]\nfoo = 1\n[terminal]\nfont_size = 12.0\n",
        ));
        rig.save(&[
            put("jql", SettingValue::Text("project = DEMO2".into())),
            put("terminal.font_size", SettingValue::Float(14.0)),
        ])
        .unwrap();
        assert_eq!(
            rig.text(),
            "# my notes\njql = \"project = DEMO2\"\n[custom]\nfoo = 1\n[terminal]\nfont_size = 14.0\n"
        );
    }

    #[test]
    fn save_keeps_a_trailing_comment_on_the_value() {
        let rig = Rig::new(Some("lock_idle_mins = 60 # an hour\n"));
        rig.save(&[put("lock_idle_mins", SettingValue::Int(15))])
            .unwrap();
        assert_eq!(rig.text(), "lock_idle_mins = 15 # an hour\n");
    }

    #[test]
    fn save_creates_a_missing_file_and_its_folder() {
        let dir = tempfile::tempdir().unwrap();
        let files = SettingsFiles::in_dir(&dir.path().join("a/b"));
        save(
            &files,
            &home(),
            "",
            &[
                put("lock_idle_mins", SettingValue::Int(15)),
                put("editor.name", SettingValue::Text("Zed".into())),
            ],
        )
        .unwrap();
        let text = std::fs::read_to_string(&files.config).unwrap();
        assert_eq!(text, "lock_idle_mins = 15\n\n[editor]\nname = \"Zed\"\n");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("a/b"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("config.toml")]);
    }

    #[test]
    fn save_keeps_a_symlinked_config_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let dots = dir.path().join("dots");
        std::fs::create_dir_all(&dots).unwrap();
        std::fs::write(dots.join("config.toml"), "jql = \"project = DEMO\"\n").unwrap();
        let files = SettingsFiles::in_dir(dir.path());
        std::os::unix::fs::symlink("dots/config.toml", &files.config).unwrap();
        let hash = read(&files, &running_defaults(), &home(), info()).file_hash;
        save(
            &files,
            &home(),
            &hash,
            &[put("jql", SettingValue::Text("project = DEMO2".into()))],
        )
        .unwrap();
        assert!(std::fs::symlink_metadata(&files.config)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_to_string(dots.join("config.toml")).unwrap(),
            "jql = \"project = DEMO2\"\n"
        );
        let names: Vec<_> = std::fs::read_dir(&dots)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("config.toml")]);
    }

    #[test]
    fn save_refuses_when_the_file_changed_since_read() {
        let rig = Rig::new(Some("jql = \"project = DEMO\"\n"));
        let hash = rig.hash();
        std::fs::write(&rig.files.config, "jql = \"by hand\"\n").unwrap();
        let err = save(
            &rig.files,
            &home(),
            &hash,
            &[put("jql", SettingValue::Text("project = DEMO2".into()))],
        )
        .unwrap_err();
        assert_eq!(
            err,
            "config.toml changed on disk since Settings read it. Close and reopen Settings to see the new text."
        );
        assert_eq!(rig.text(), "jql = \"by hand\"\n");
    }

    #[test]
    fn invalid_value_is_refused_and_the_file_is_untouched() {
        let body = "# keep\njql = \"project = DEMO\"\n";
        let cases = [
            (
                put("jira_poll_secs", SettingValue::Int(2)),
                "jira_poll_secs must be a whole number from 5 to 3600",
            ),
            (
                put("terminal.font_size", SettingValue::Float(200.0)),
                "terminal.font_size must be a number from 4 to 96",
            ),
            (
                put("terminal.background", SettingValue::Text("blue".into())),
                "terminal.background must be a colour like #1e1e2e",
            ),
            (
                put("sprint_start", SettingValue::Text("2026-13-01".into())),
                "sprint_start must be a date like 2026-11-09, or empty",
            ),
            (
                put("ticket_pattern", SettingValue::Text("(".into())),
                "ticket_pattern is not a valid pattern: regex parse error:",
            ),
            (
                put("jql", SettingValue::Text(String::new())),
                "jql cannot be empty",
            ),
            (
                put("push", SettingValue::Text("yes".into())),
                "push needs true or false",
            ),
            (
                put("shared_files", SettingValue::Int(1)),
                "shared_files needs a list",
            ),
            (
                put("journal_path", SettingValue::Bool(true)),
                "journal_path needs text",
            ),
            (
                put("backup_mins", SettingValue::Text("5".into())),
                "backup_mins needs a number",
            ),
        ];
        for (edit, want) in cases {
            let rig = Rig::new(Some(body));
            let before = rig.bytes();
            // A good edit first: nothing is written unless every edit passes.
            let err = rig
                .save(&[put("lock_idle_mins", SettingValue::Int(15)), edit])
                .unwrap_err();
            assert_eq!(err, want);
            assert_eq!(rig.bytes(), before, "{want}");
        }
    }

    #[test]
    fn float_settings_take_a_whole_number() {
        let rig = Rig::new(None);
        rig.save(&[put("terminal.font_size", SettingValue::Int(14))])
            .unwrap();
        assert_eq!(rig.text(), "[terminal]\nfont_size = 14.0\n");
    }

    #[test]
    fn read_only_keys_are_refused() {
        for key in ["brain_writer", "jira_command", "terminal.ansi"] {
            let rig = Rig::new(Some("brain_writer = false\n"));
            let before = rig.bytes();
            let value = if key == "brain_writer" {
                SettingValue::Bool(true)
            } else {
                SettingValue::List(vec!["x".into()])
            };
            assert_eq!(
                rig.save(&[put(key, value)]).unwrap_err(),
                format!("{key} is read-only here; edit config.toml")
            );
            assert_eq!(
                rig.save(&[SettingEdit::Reset { key: key.into() }])
                    .unwrap_err(),
                format!("{key} is read-only here; edit config.toml")
            );
            assert_eq!(rig.bytes(), before);
        }
    }

    #[test]
    fn unknown_key_is_refused() {
        let rig = Rig::new(Some("jql = \"project = DEMO\"\n"));
        let before = rig.bytes();
        assert_eq!(
            rig.save(&[put("colour_scheme", SettingValue::Text("dark".into()))])
                .unwrap_err(),
            "no setting colour_scheme"
        );
        assert_eq!(rig.bytes(), before);
    }

    #[test]
    fn reset_removes_the_key() {
        let rig = Rig::new(Some(
            "# top\njql = \"project = DEMO\"\nlock_idle_mins = 15\n[terminal]\nfont_size = 12.0\n",
        ));
        rig.save(&[
            SettingEdit::Reset { key: "jql".into() },
            SettingEdit::Reset {
                key: "terminal.font_size".into(),
            },
            // Not in the file: nothing to remove.
            SettingEdit::Reset {
                key: "editor.name".into(),
            },
        ])
        .unwrap();
        assert_eq!(rig.text(), "# top\nlock_idle_mins = 15\n[terminal]\n");
        let v = rig.view(&running_defaults());
        assert!(!row(&v, "jql").in_file);
        assert_eq!(row(&v, "jql").value, row(&v, "jql").default);
    }

    #[test]
    fn reset_keeps_the_comments_above_the_key() {
        let cases = [
            // The next key takes them.
            (
                "jql",
                "# about jql\njql = \"x\"\npush = false\n",
                "# about jql\npush = false\n",
            ),
            // The last root key: the next table header does.
            (
                "lock_idle_mins",
                "jql = \"x\"\n# idle\nlock_idle_mins = 15\n\n[editor]\nname = \"Zed\"\n",
                "jql = \"x\"\n# idle\n\n[editor]\nname = \"Zed\"\n",
            ),
            // A table's last key with nothing after it: the end of the file.
            (
                "editor.name",
                "jql = \"x\"\n[editor]\n# my editor\nname = \"Zed\"\n",
                "jql = \"x\"\n[editor]\n# my editor\n",
            ),
        ];
        for (key, body, want) in cases {
            let rig = Rig::new(Some(body));
            rig.save(&[SettingEdit::Reset { key: key.into() }]).unwrap();
            assert_eq!(rig.text(), want, "{key}");
        }
    }

    #[test]
    fn unparseable_file_is_never_rewritten() {
        let rig = Rig::new(Some("jql = [not toml"));
        let before = rig.bytes();
        let v = rig.view(&running_defaults());
        assert!(v.load_error.is_some());
        let err = save(
            &rig.files,
            &home(),
            &v.file_hash,
            &[put("lock_idle_mins", SettingValue::Int(15))],
        )
        .unwrap_err();
        assert!(
            err.starts_with("config.toml does not parse, so Settings will not change it: "),
            "{err}"
        );
        assert_eq!(rig.bytes(), before);
    }

    #[test]
    fn a_file_the_daemon_cannot_load_is_never_rewritten() {
        // Valid TOML, but not a Config: the daemon would start on defaults.
        let rig = Rig::new(Some("jira_poll_secs = \"often\"\n"));
        let before = rig.bytes();
        let err = rig
            .save(&[put("lock_idle_mins", SettingValue::Int(15))])
            .unwrap_err();
        assert!(
            err.starts_with("config.toml does not parse, so Settings will not change it: "),
            "{err}"
        );
        assert_eq!(rig.bytes(), before);
        // Fixing the bad key is allowed.
        rig.save(&[put("jira_poll_secs", SettingValue::Int(60))])
            .unwrap();
        assert_eq!(rig.text(), "jira_poll_secs = 60\n");
    }

    #[test]
    fn file_hash_is_fnv1a_64() {
        assert_eq!(file_hash(""), "");
        assert_eq!(file_hash("a"), "af63dc4c8601ec8c");
        assert_ne!(file_hash("jql = 1\n"), file_hash("jql = 2\n"));
    }

    /// A value of the right kind that differs from `v`.
    fn other_value(kind: &SettingKind, v: &SettingValue) -> SettingValue {
        match (kind, v) {
            (SettingKind::Int { min, max }, SettingValue::Int(n)) => {
                SettingValue::Int(if n == min { *max } else { *min })
            }
            (SettingKind::Bool, SettingValue::Bool(b)) => SettingValue::Bool(!b),
            (SettingKind::Pattern, _) => SettingValue::Text("DEMO-[0-9]+".into()),
            (SettingKind::Date, _) => SettingValue::Text("2026-11-09".into()),
            (_, SettingValue::Text(s)) => SettingValue::Text(format!("{s}x")),
            (_, SettingValue::List(_)) => SettingValue::List(vec!["x".into()]),
            other => panic!("no other value for {other:?}"),
        }
    }

    #[test]
    fn catalogue_daemon_keys_are_config_fields() {
        for spec in CATALOGUE {
            let Some(get) = spec.get else { continue };
            let default = default_value(spec);
            check(spec, &default).unwrap_or_else(|e| panic!("default of {}: {e}", spec.key));
            for value in [default.clone(), other_value(&spec.kind, &default)] {
                let mut doc = DocumentMut::new();
                super::set(&mut doc, spec.key, to_toml(spec, &value)).unwrap();
                let c: Config = toml::from_str(&doc.to_string())
                    .unwrap_or_else(|e| panic!("{}: {e}", spec.key));
                assert_eq!(get(&c), value, "{}", spec.key);
            }
        }
    }

    #[test]
    fn catalogue_app_keys_have_defaults_that_pass_their_check() {
        for spec in CATALOGUE.iter().filter(|s| s.get.is_none()) {
            let d = default_value(spec);
            check(spec, &d).unwrap_or_else(|e| panic!("{}: {e}", spec.key));
            assert!(spec.key.starts_with("terminal.") || spec.key.starts_with("editor."));
        }
        assert_eq!(
            default_value(spec("terminal.ansi").unwrap()),
            SettingValue::List(
                overseer_term::Palette::default()
                    .ansi
                    .iter()
                    .map(|c| hex(*c))
                    .collect()
            )
        );
    }

    #[test]
    fn targets_note_says_where_patterns_come_from() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("targets.toml");
        assert_eq!(
            targets_note(3, &p),
            format!("3 built-in patterns; {} does not exist", p.display())
        );
        std::fs::write(&p, "").unwrap();
        assert_eq!(
            targets_note(5, &p),
            format!(
                "5 patterns in use, read from {} at daemon start",
                p.display()
            )
        );
    }
}
