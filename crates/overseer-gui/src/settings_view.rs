//! The Settings view (Decision 13): `config.toml` as the daemon reads it,
//! edited here and saved by the daemon, the only writer (Decision 8).
//!
//! The draft (`SettingsDraft`) and the texts are pure and unit tested; the
//! entity (`SettingsPanel`) reads the view with `GetSettings`, saves with
//! `SaveSettings` and tells the app what happened through `SettingsEvent`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    div, prelude::*, px, rgb, rgba, Context, EventEmitter, FocusHandle, Focusable, FontWeight,
    KeyDownEvent, MouseButton, ScrollHandle, Window,
};
use overseer_proto::settings::{parse_text, Applies, SettingEdit, SettingValue, SettingsView};
use overseer_proto::{
    build_notice, current_build, local_hhmm, running_sessions_count, running_sessions_text, Client,
    DaemonInfo, Request, Response, SlotInfo, SlotState,
};

use crate::app::colors;
use crate::start_dialog::{edit_key, DialogKey, TextField};

// ---------------------------------------------------------------------------
// Pure logic
// ---------------------------------------------------------------------------

/// A value as the view shows and edits it: a list as comma-separated items,
/// everything else as `ctl settings` prints it.
fn value_text(v: &SettingValue) -> String {
    match v {
        SettingValue::List(items) => items.join(", "),
        other => other.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Change {
    Text(String),
    Bool(bool),
    Reset,
}

/// The view from the daemon and what has been typed over it.
#[derive(Debug, Clone)]
pub struct SettingsDraft {
    view: SettingsView,
    changes: BTreeMap<String, Change>,
}

/// What a row shows in its value column.
#[derive(Debug, Clone, PartialEq)]
pub enum RowValue {
    Text(String),
    Toggle(bool),
    ReadOnly { text: String, note: Option<String> },
}

/// One line of the view.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingRow {
    pub group: String,
    pub key: String,
    pub value: RowValue,
    /// `default` or `from the file`.
    pub source: &'static str,
    pub restart_needed: bool,
    /// The file sets the key and no reset is pending.
    pub can_reset: bool,
    /// Changed here and not saved yet.
    pub edited: bool,
}

impl SettingsDraft {
    pub fn new(view: SettingsView) -> Self {
        SettingsDraft {
            view,
            changes: BTreeMap::new(),
        }
    }

    pub fn view(&self) -> &SettingsView {
        &self.view
    }

    /// The row for `key` when the view may change it.
    fn editable(&self, key: &str) -> Option<&overseer_proto::settings::Setting> {
        self.view
            .settings
            .iter()
            .find(|s| s.key == key && s.applies != Applies::ReadOnly)
    }

    /// The text typed for `key`. Typing the saved text back drops the edit.
    pub fn set_text(&mut self, key: &str, text: &str) {
        let Some(s) = self.editable(key) else {
            return;
        };
        if matches!(s.value, SettingValue::Bool(_)) {
            return;
        }
        if text == value_text(&s.value) {
            self.changes.remove(key);
        } else {
            self.changes
                .insert(key.to_string(), Change::Text(text.to_string()));
        }
    }

    /// Flips a `true`/`false` row.
    pub fn toggle(&mut self, key: &str) {
        let Some(s) = self.editable(key) else {
            return;
        };
        let (SettingValue::Bool(saved), SettingValue::Bool(default)) = (&s.value, &s.default)
        else {
            return;
        };
        let (saved, default) = (*saved, *default);
        let now = match self.changes.get(key) {
            Some(Change::Bool(b)) => *b,
            Some(Change::Reset) => default,
            _ => saved,
        };
        if !now == saved {
            self.changes.remove(key);
        } else {
            self.changes.insert(key.to_string(), Change::Bool(!now));
        }
    }

    /// Takes the key out of the file at the next save, so its default
    /// applies. A key the file does not set only loses its edit.
    pub fn reset(&mut self, key: &str) {
        let Some(s) = self.editable(key) else {
            return;
        };
        if s.in_file {
            self.changes.insert(key.to_string(), Change::Reset);
        } else {
            self.changes.remove(key);
        }
    }

    pub fn dirty(&self) -> bool {
        !self.changes.is_empty()
    }

    /// The edits to send, in the view's order. Text is read per kind with
    /// the daemon's own rules and texts, so a bad value is caught here.
    pub fn edits(&self) -> Result<Vec<SettingEdit>, String> {
        let mut out = Vec::new();
        for s in &self.view.settings {
            let key = s.key.clone();
            match self.changes.get(&s.key) {
                None => {}
                Some(Change::Text(t)) => out.push(SettingEdit::Set {
                    value: parse_text(&s.kind, &s.key, t)?,
                    key,
                }),
                Some(Change::Bool(b)) => out.push(SettingEdit::Set {
                    key,
                    value: SettingValue::Bool(*b),
                }),
                Some(Change::Reset) => out.push(SettingEdit::Reset { key }),
            }
        }
        Ok(out)
    }

    pub fn rows(&self) -> Vec<SettingRow> {
        self.view
            .settings
            .iter()
            .map(|s| {
                let change = self.changes.get(&s.key);
                let reset = change == Some(&Change::Reset);
                let shown = if reset { &s.default } else { &s.value };
                let value = match (change, shown) {
                    _ if s.applies == Applies::ReadOnly => RowValue::ReadOnly {
                        text: value_text(shown),
                        note: s.note.clone(),
                    },
                    (Some(Change::Text(t)), _) => RowValue::Text(t.clone()),
                    (Some(Change::Bool(b)), _) => RowValue::Toggle(*b),
                    (_, SettingValue::Bool(b)) => RowValue::Toggle(*b),
                    (_, v) => RowValue::Text(value_text(v)),
                };
                SettingRow {
                    group: s.group.clone(),
                    key: s.key.clone(),
                    value,
                    source: if s.in_file && !reset {
                        "from the file"
                    } else {
                        "default"
                    },
                    restart_needed: s.restart_needed,
                    can_reset: s.in_file && !reset && s.applies != Applies::ReadOnly,
                    edited: change.is_some(),
                }
            })
            .collect()
    }
}

/// `a`, `a and b`, `a, b and c`.
fn and_list(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// The line after a save: which of the `edited` keys the daemon only takes
/// up at its next start.
pub fn saved_text(after: &SettingsView, edited: &[String]) -> String {
    let waiting: Vec<&str> = after
        .settings
        .iter()
        .filter(|s| s.restart_needed && edited.contains(&s.key))
        .map(|s| s.key.as_str())
        .collect();
    if waiting.is_empty() {
        return "Saved.".to_string();
    }
    format!(
        "Saved. The daemon uses the new {} after it restarts.",
        and_list(&waiting)
    )
}

/// The question Restart the daemon asks first (Decision 4's warning).
pub fn restart_confirm(slots: &[SlotInfo]) -> String {
    let running: Vec<&SlotInfo> = slots
        .iter()
        .filter(|s| matches!(s.state, SlotState::Running { .. }))
        .collect();
    let what = match running.len() {
        0 => return "Restart the daemon? No session is running.".to_string(),
        1 => "ends and comes back",
        _ => "end and come back",
    };
    format!(
        "Restart the daemon? {} {what} as resumable: {}.",
        running_sessions_count(running.len()),
        running_sessions_text(running.iter().map(|s| (s.id, s.label.as_str()))),
    )
}

/// Whether the edits touch what the app itself reads (terminal and editor),
/// so it reloads them at once.
pub fn app_keys_changed(edits: &[SettingEdit]) -> bool {
    edits.iter().any(|e| {
        let key = match e {
            SettingEdit::Set { key, .. } | SettingEdit::Reset { key } => key,
        };
        key.starts_with("terminal.") || key.starts_with("editor.")
    })
}

/// The About block. `None` is a daemon that predates `DaemonInfo`.
pub fn about_lines(app_build: &str, daemon: Option<&DaemonInfo>) -> Vec<String> {
    let mut lines = vec![format!("App build {app_build}")];
    match daemon {
        Some(d) => {
            lines.push(format!(
                "Daemon build {}, pid {}, running since {}",
                d.build,
                d.pid,
                local_hhmm(d.started_at)
            ));
            lines.extend(build_notice(app_build, &d.build));
        }
        None => {
            lines.push("Daemon build unknown (older than this app)".to_string());
            lines.push("Restart the daemon to finish the upgrade.".to_string());
        }
    }
    lines
}

/// What the app runs, as its own binary, to restart the daemon (Decision 9).
pub const RESTART_ARGS: [&str; 3] = ["ctl", "restart", "--yes"];

fn last_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).rfind(|l| !l.is_empty())
}

/// The status line once `ctl restart --yes` has exited: what it printed,
/// or the last line of its error.
pub fn restart_outcome(success: bool, stdout: &str, stderr: &str) -> String {
    if success {
        return last_line(stdout)
            .unwrap_or("Restarted the daemon.")
            .to_string();
    }
    match last_line(stderr) {
        Some(why) => format!("The daemon did not restart: {why}"),
        None => "The daemon did not restart, and said nothing about why. See gui.log.".to_string(),
    }
}

/// Runs `<exe> ctl restart --yes` with the app's environment, waits for it,
/// logs what it printed to `gui.log` and returns the status line. Blocks.
pub fn run_restart(exe: std::io::Result<PathBuf>) -> String {
    let exe = match exe {
        Ok(exe) => exe,
        Err(e) => {
            return format!("The daemon did not restart: cannot find this app's binary ({e})")
        }
    };
    let out = std::process::Command::new(&exe)
        .args(RESTART_ARGS)
        .stdin(std::process::Stdio::null())
        .output();
    match out {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !stdout.trim().is_empty() {
                tracing::info!("ctl restart: {}", stdout.trim());
            }
            if !stderr.trim().is_empty() {
                tracing::warn!("ctl restart ({}): {}", out.status, stderr.trim());
            }
            restart_outcome(out.status.success(), &stdout, &stderr)
        }
        Err(e) => format!(
            "The daemon did not restart: cannot run {} ({e})",
            exe.display()
        ),
    }
}

/// What the app knows about the daemon it is connected to.
#[derive(Debug, Clone, PartialEq)]
pub enum DaemonKnown {
    /// Not asked yet, or it did not answer.
    Unknown,
    Info(DaemonInfo),
    /// It answers but predates `DaemonInfo` (it hangs up on the request).
    Old,
}

/// How long asking the daemon about itself may take.
const PROBE_WAIT: Duration = Duration::from_secs(2);

/// Asks the daemon on `socket` about itself on a connection of its own: an
/// old daemon hangs up on a request it does not know, and that must not
/// take the app's connection down with it. Needs a tokio runtime.
pub async fn probe_daemon(socket: PathBuf) -> DaemonKnown {
    let ask = async {
        let client = Client::connect(&socket).await.ok()?;
        Some(match client.request(Request::DaemonInfo).await {
            Ok(Response::DaemonInfo(info)) => DaemonKnown::Info(info),
            Ok(Response::Error { .. }) | Err(_) => DaemonKnown::Old,
            Ok(_) => DaemonKnown::Unknown,
        })
    };
    tokio::time::timeout(PROBE_WAIT, ask)
        .await
        .ok()
        .flatten()
        .unwrap_or(DaemonKnown::Unknown)
}

/// The status line for a daemon that predates `DaemonInfo`.
pub const OLD_DAEMON_NOTICE: &str = "The daemon runs a build older than this app. Restart the daemon to finish the upgrade (overseer ctl restart, or Settings).";

/// Decision 13's first Esc with unsaved edits.
pub const ESC_AGAIN: &str = "Esc again closes without saving";

/// Whether Esc closes the view now.
#[derive(Debug, Default)]
pub struct EscGuard {
    warned: bool,
}

impl EscGuard {
    /// True to close. With unsaved edits the first press only warns.
    pub fn press(&mut self, draft: &SettingsDraft) -> bool {
        if !draft.dirty() || self.warned {
            return true;
        }
        self.warned = true;
        false
    }

    /// Any other key takes the warning back.
    pub fn clear(&mut self) {
        self.warned = false;
    }

    pub fn note(&self) -> Option<&'static str> {
        self.warned.then_some(ESC_AGAIN)
    }
}

// ---------------------------------------------------------------------------
// The entity
// ---------------------------------------------------------------------------

/// What the app hears from the view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsEvent {
    /// The daemon saved the edits; `app_changed` when terminal or editor
    /// keys were among them.
    Saved {
        text: String,
        app_changed: bool,
    },
    Closed,
    /// Confirmed: restart the daemon.
    Restart,
}

enum Mode {
    Loading,
    Ready(Box<SettingsDraft>),
    /// The daemon predates Settings; only About and Restart work.
    Old,
    Failed(String),
}

pub struct SettingsPanel {
    client: Arc<Client>,
    mode: Mode,
    slots: Vec<SlotInfo>,
    /// The row being edited, and its text while it is a text row.
    active: Option<String>,
    field: TextField,
    /// A refusal or a bad value, in red.
    error: Option<String>,
    /// The last save's or restart's line.
    note: Option<String>,
    esc: EscGuard,
    /// The open restart question.
    confirm: Option<String>,
    busy: bool,
    closed: bool,
    scroll: ScrollHandle,
    focus_handle: FocusHandle,
}

impl EventEmitter<SettingsEvent> for SettingsPanel {}

impl Focusable for SettingsPanel {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

fn lost(e: impl std::fmt::Display) -> String {
    format!("{} ({e:#})", crate::terminal_view::LOST_DAEMON)
}

impl SettingsPanel {
    /// The view, reading the settings from the daemon unless it is `old`.
    pub fn new(
        client: Arc<Client>,
        slots: Vec<SlotInfo>,
        old: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut panel = SettingsPanel {
            client,
            mode: if old { Mode::Old } else { Mode::Loading },
            slots,
            active: None,
            field: TextField::default(),
            error: None,
            note: None,
            esc: EscGuard::default(),
            confirm: None,
            busy: false,
            closed: false,
            scroll: ScrollHandle::new(),
            focus_handle: cx.focus_handle(),
        };
        if !old {
            panel.load(cx);
        }
        panel
    }

    /// The client to use from now on, after a reconnect.
    pub fn set_client(&mut self, client: Arc<Client>) {
        self.client = client;
    }

    /// The slots as they are now, for the restart question.
    pub fn set_slots(&mut self, slots: &[SlotInfo]) {
        self.slots = slots.to_vec();
    }

    /// A save is on its way; the app keeps a closed view until it is answered.
    pub fn busy(&self) -> bool {
        self.busy
    }

    /// Why the last save failed, for the app once the view is closed.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The daemon answered `DaemonInfo` (after a reconnect, say): read the
    /// settings again, unless you have edits the new view would lose.
    pub fn daemon_known(&mut self, known: &DaemonKnown, cx: &mut Context<Self>) {
        match (known, &self.mode) {
            (DaemonKnown::Old, Mode::Loading | Mode::Failed(_)) => self.mode = Mode::Old,
            (DaemonKnown::Info(_), Mode::Ready(d)) if !d.dirty() && !self.busy => self.load(cx),
            (DaemonKnown::Info(_), Mode::Old | Mode::Failed(_)) => self.load(cx),
            _ => {}
        }
        cx.notify();
    }

    /// The app's restart ended with `text`.
    pub fn restart_done(&mut self, text: &str, cx: &mut Context<Self>) {
        self.note = Some(text.to_string());
        cx.notify();
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.mode, Mode::Ready(_)) {
            self.mode = Mode::Loading;
        }
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let reply = client.request_marked(Request::GetSettings, |_| true).await;
            let _ = this.update(cx, |this, cx| {
                match reply {
                    Ok(Response::Settings(view)) => this.show(view),
                    Ok(Response::Error { message }) => this.mode = Mode::Failed(message),
                    Ok(other) => this.mode = Mode::Failed(format!("Unexpected reply: {other:?}")),
                    Err(e) => this.mode = Mode::Failed(lost(e)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// A fresh draft over `view`, keeping the row being edited.
    fn show(&mut self, view: SettingsView) {
        self.mode = Mode::Ready(Box::new(SettingsDraft::new(view)));
        self.esc.clear();
        let key = self.active.take();
        let editable = self.editable_keys();
        let key = key
            .filter(|k| editable.contains(k))
            .or_else(|| editable.into_iter().next());
        self.activate(key);
    }

    fn draft(&self) -> Option<&SettingsDraft> {
        match &self.mode {
            Mode::Ready(d) => Some(d),
            _ => None,
        }
    }

    fn draft_mut(&mut self) -> Option<&mut SettingsDraft> {
        match &mut self.mode {
            Mode::Ready(d) => Some(d),
            _ => None,
        }
    }

    fn row(&self, key: &str) -> Option<SettingRow> {
        self.draft()?.rows().into_iter().find(|r| r.key == key)
    }

    /// Text and toggle rows, in order: what Tab moves between.
    fn editable_keys(&self) -> Vec<String> {
        self.draft()
            .map(|d| d.rows())
            .unwrap_or_default()
            .into_iter()
            .filter(|r| !matches!(r.value, RowValue::ReadOnly { .. }))
            .map(|r| r.key)
            .collect()
    }

    fn activate(&mut self, key: Option<String>) {
        let text = match key.as_deref().and_then(|k| self.row(k)) {
            Some(SettingRow {
                value: RowValue::Text(t),
                ..
            }) => t,
            _ => String::new(),
        };
        self.field = TextField::new(&text);
        self.active = key;
    }

    fn step(&mut self, back: bool) {
        let keys = self.editable_keys();
        if keys.is_empty() {
            return;
        }
        let at = self
            .active
            .as_ref()
            .and_then(|k| keys.iter().position(|x| x == k));
        let next = match (at, back) {
            (None, _) => 0,
            (Some(i), false) => (i + 1) % keys.len(),
            (Some(i), true) => (i + keys.len() - 1) % keys.len(),
        };
        self.activate(Some(keys[next].clone()));
    }

    fn toggle(&mut self, key: &str) {
        if let Some(d) = self.draft_mut() {
            d.toggle(key);
        }
        self.error = None;
    }

    fn reset(&mut self, key: &str) {
        if let Some(d) = self.draft_mut() {
            d.reset(key);
        }
        self.error = None;
        if self.active.as_deref() == Some(key) {
            self.activate(Some(key.to_string()));
        }
    }

    /// The active text row's field changed.
    fn typed(&mut self) {
        let (Some(key), text) = (self.active.clone(), self.field.text.clone()) else {
            return;
        };
        if let Some(d) = self.draft_mut() {
            d.set_text(&key, &text);
        }
        self.error = None;
    }

    fn active_is_toggle(&self) -> bool {
        self.active
            .as_deref()
            .and_then(|k| self.row(k))
            .is_some_and(|r| matches!(r.value, RowValue::Toggle(_)))
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.closed {
            return;
        }
        let Some(draft) = self.draft() else {
            return;
        };
        let edits = match draft.edits() {
            Ok(e) if e.is_empty() => {
                self.note = Some("Nothing to save.".to_string());
                cx.notify();
                return;
            }
            Ok(e) => e,
            Err(e) => {
                self.error = Some(e);
                cx.notify();
                return;
            }
        };
        let base_hash = draft.view().file_hash.clone();
        let keys: Vec<String> = edits
            .iter()
            .map(|e| match e {
                SettingEdit::Set { key, .. } | SettingEdit::Reset { key } => key.clone(),
            })
            .collect();
        let app_changed = app_keys_changed(&edits);
        self.busy = true;
        self.error = None;
        self.note = None;
        let client = self.client.clone();
        let req = Request::SaveSettings { base_hash, edits };
        cx.spawn(async move |this, cx| {
            let reply = client.request(req).await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match reply {
                    Ok(Response::Settings(after)) => {
                        let text = saved_text(&after, &keys);
                        this.show(after);
                        this.note = Some(text.clone());
                        cx.emit(SettingsEvent::Saved { text, app_changed });
                    }
                    Ok(Response::Error { message }) => this.error = Some(message),
                    Ok(other) => this.error = Some(format!("Unexpected reply: {other:?}")),
                    Err(e) => this.error = Some(lost(e)),
                }
                // Closed while saving: the app reads `error` now.
                if this.closed && this.error.is_some() {
                    cx.emit(SettingsEvent::Closed);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.closed = true;
        cx.emit(SettingsEvent::Closed);
    }

    fn ask_restart(&mut self, cx: &mut Context<Self>) {
        self.confirm = Some(restart_confirm(&self.slots));
        cx.notify();
    }

    fn confirm_restart(&mut self, cx: &mut Context<Self>) {
        if self.confirm.take().is_some() {
            self.note = Some("Restarting the daemon".to_string());
            cx.emit(SettingsEvent::Restart);
        }
        cx.notify();
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let m = &ks.modifiers;
        let plain = !m.platform && !m.control && !m.alt;
        // Cmd keys the view does not use (Cmd+Q) go on to the app.
        let ours = ["s", "a", "v", "backspace", "left", "right"];
        if m.platform && !ours.contains(&ks.key.as_str()) {
            return;
        }
        cx.stop_propagation();
        if self.confirm.is_some() {
            match ks.key.as_str() {
                "enter" if plain => self.confirm_restart(cx),
                "escape" if plain => self.confirm = None,
                _ => {}
            }
            cx.notify();
            return;
        }
        if ks.key == "escape" && plain {
            let close = match &self.mode {
                Mode::Ready(d) => self.esc.press(d),
                _ => true,
            };
            if close {
                self.close(cx);
            }
            cx.notify();
            return;
        }
        self.esc.clear();
        if ks.key == "s" && m.platform && !m.control && !m.alt {
            self.save(cx);
            return;
        }
        if ks.key == "tab" && !m.platform && !m.control {
            self.step(m.shift);
            cx.notify();
            return;
        }
        if self.active_is_toggle() {
            if matches!(ks.key.as_str(), "space" | "enter") && plain {
                if let Some(key) = self.active.clone() {
                    self.toggle(&key);
                }
            } else if ks.key == "enter" {
                self.save(cx);
            }
            cx.notify();
            return;
        }
        if self.active.is_none() {
            if ks.key == "enter" {
                self.save(cx);
            }
            cx.notify();
            return;
        }
        match edit_key(&mut self.field, ks) {
            DialogKey::Edited => self.typed(),
            DialogKey::Paste => {
                if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                    self.field.insert(&text);
                    self.typed();
                }
            }
            DialogKey::Submit => self.save(cx),
            DialogKey::NextField
            | DialogKey::PrevField
            | DialogKey::Cancel
            | DialogKey::Ignored => {}
        }
        cx.notify();
    }
}

/// Opens `path` in the default text editor, off the UI thread.
fn open_in_editor(path: PathBuf) {
    std::thread::spawn(move || {
        match std::process::Command::new("open")
            .arg("-t")
            .arg(&path)
            .status()
        {
            Ok(_) => {}
            Err(e) => tracing::warn!("could not open {}: {e}", path.display()),
        }
    });
}

fn button(id: &'static str, text: &str, primary: bool, enabled: bool) -> gpui::Stateful<gpui::Div> {
    let b = div()
        .id(id)
        .px_3()
        .py_1()
        .rounded(px(5.))
        .border_1()
        .border_color(rgb(colors::BUTTON_BORDER))
        .bg(rgb(if primary {
            colors::ACCENT_BG
        } else {
            colors::BUTTON
        }))
        .text_color(rgb(if !enabled {
            colors::MUTED
        } else if primary {
            colors::ACCENT
        } else {
            colors::TEXT
        }))
        .child(text.to_string());
    if enabled {
        b.cursor_pointer().hover(|s| s.bg(rgb(colors::CARD_HOVER)))
    } else {
        b
    }
}

fn heading(text: &str) -> gpui::Div {
    div()
        .mt_2()
        .pt_2()
        .border_t_1()
        .border_color(rgb(colors::BORDER))
        .font_weight(FontWeight::SEMIBOLD)
        .child(text.to_string())
}

fn muted(text: impl Into<gpui::SharedString>) -> gpui::Div {
    div()
        .text_xs()
        .text_color(rgb(colors::MUTED))
        .child(text.into())
}

impl SettingsPanel {
    fn render_row(
        &self,
        i: usize,
        row: SettingRow,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let active = self.active.as_deref() == Some(row.key.as_str());
        let key = row.key.clone();
        let value: gpui::AnyElement = match &row.value {
            RowValue::Text(t) => {
                let text = if active {
                    self.field.display(focused)
                } else {
                    gpui::StyledText::new(t.clone())
                };
                let k = key.clone();
                div()
                    .id(("field", i))
                    .flex_1()
                    .min_w(px(0.))
                    .px_2()
                    .py_1()
                    .rounded(px(4.))
                    .bg(rgb(colors::FIELD))
                    .border_1()
                    .border_color(rgb(if active {
                        colors::ACCENT
                    } else {
                        colors::BORDER
                    }))
                    .cursor_text()
                    .child(text)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.esc.clear();
                        this.activate(Some(k.clone()));
                        cx.notify();
                    }))
                    .into_any_element()
            }
            RowValue::Toggle(b) => {
                let k = key.clone();
                div()
                    .flex_1()
                    .child(
                        div()
                            .id(("toggle", i))
                            .px_2()
                            .py_1()
                            .rounded(px(4.))
                            .border_1()
                            .border_color(rgb(if active {
                                colors::ACCENT
                            } else {
                                colors::BORDER
                            }))
                            .bg(rgb(colors::FIELD))
                            .cursor_pointer()
                            .child(if *b { "true" } else { "false" })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.esc.clear();
                                this.activate(Some(k.clone()));
                                this.toggle(&k);
                                cx.notify();
                            })),
                    )
                    .into_any_element()
            }
            RowValue::ReadOnly { text, note } => div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .child(
                    div()
                        .px_2()
                        .text_color(rgb(colors::TEXT_DIM))
                        .child(text.clone()),
                )
                .children(note.clone().map(|n| muted(n).px_2()))
                .into_any_element(),
        };
        let reset = row.can_reset.then(|| {
            let k = key.clone();
            div()
                .id(("reset", i))
                .text_xs()
                .text_color(rgb(colors::ACCENT))
                .cursor_pointer()
                .hover(|s| s.underline())
                .child("Reset")
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.esc.clear();
                    this.reset(&k);
                    cx.notify();
                }))
        });
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .w(px(170.))
                    .flex_none()
                    .text_color(rgb(if row.edited {
                        colors::ACCENT
                    } else {
                        colors::TEXT
                    }))
                    .child(row.key.clone()),
            )
            .child(value)
            .child(muted(row.source).w(px(84.)).flex_none())
            .children(row.restart_needed.then(|| {
                div()
                    .text_xs()
                    .flex_none()
                    .text_color(rgb(colors::ORANGE))
                    .child("restart needed")
            }))
            .children(reset)
            .into_any_element()
    }

    fn render_files(&self, view: &SettingsView, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let mut out = vec![heading("Files").into_any_element()];
        let file = |id: &'static str, label: &str, path: &Path, cx: &mut Context<Self>| {
            let p = path.to_path_buf();
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().w(px(170.)).flex_none().child(label.to_string()))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .text_color(rgb(colors::TEXT_DIM))
                        .child(path.display().to_string()),
                )
                .children(path.exists().then(|| {
                    button(id, "Open", false, true)
                        .on_click(cx.listener(move |_, _, _, _| open_in_editor(p.clone())))
                }))
                .into_any_element()
        };
        out.push(file("open-config", "config.toml", &view.config_path, cx));
        out.extend(view.load_error.clone().map(|e| {
            div()
                .text_color(rgb(colors::RED))
                .child(e)
                .into_any_element()
        }));
        out.push(file("open-targets", "targets.toml", &view.targets_path, cx));
        out.push(muted(view.targets_note.clone()).into_any_element());
        out.extend(view.targets_error.clone().map(|e| {
            div()
                .text_color(rgb(colors::RED))
                .child(e)
                .into_any_element()
        }));
        out
    }
}

impl Render for SettingsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);
        let mut body: Vec<gpui::AnyElement> = Vec::new();
        let mut daemon: Option<DaemonInfo> = None;
        let mut known = true;
        match &self.mode {
            Mode::Loading => {
                body.push(muted("Reading the settings from the daemon\u{2026}").into_any_element())
            }
            Mode::Failed(e) => body.push(
                div()
                    .text_color(rgb(colors::RED))
                    .child(e.clone())
                    .into_any_element(),
            ),
            Mode::Old => {
                known = false;
                body.push(
                    div()
                        .text_color(rgb(colors::TEXT_DIM))
                        .child("This daemon is older than the app and cannot show its settings. Restart it to edit them here.")
                        .into_any_element(),
                );
            }
            Mode::Ready(d) => {
                let view = d.view().clone();
                daemon = Some(view.daemon.clone());
                let mut group = String::new();
                for (i, row) in d.rows().into_iter().enumerate() {
                    if row.group != group {
                        group = row.group.clone();
                        body.push(heading(&group).into_any_element());
                    }
                    body.push(self.render_row(i, row, focused, cx));
                }
                body.extend(self.render_files(&view, cx));
            }
        }
        if known && daemon.is_none() {
            // Loading or failed: the app's own build is still worth showing.
            body.push(heading("About").into_any_element());
            body.push(
                div()
                    .child(format!("App build {}", current_build()))
                    .into_any_element(),
            );
        } else {
            body.push(heading("About").into_any_element());
            for line in about_lines(current_build(), daemon.as_ref()) {
                body.push(div().child(line).into_any_element());
            }
        }
        let ready = matches!(self.mode, Mode::Ready(_));
        let hint = if self.busy {
            "Saving\u{2026}"
        } else {
            "Cmd+S save · Tab next field · Esc close"
        };
        let card =
            div()
                .w(px(760.))
                .max_h(px(680.))
                .flex()
                .flex_col()
                .gap_2()
                .p_4()
                .rounded(px(8.))
                .bg(rgb(colors::CARD))
                .border_1()
                .border_color(rgb(colors::BORDER_COORD))
                .text_sm()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    div()
                        .text_base()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Settings"),
                )
                .child(
                    div()
                        .id("settings-rows")
                        .flex()
                        .flex_col()
                        .gap_1()
                        .flex_1()
                        .min_h(px(0.))
                        .overflow_y_scroll()
                        .track_scroll(&self.scroll)
                        .children(body),
                )
                .children(
                    self.error
                        .clone()
                        .map(|e| div().text_color(rgb(colors::RED)).child(e)),
                )
                .children(
                    self.esc
                        .note()
                        .or(self.note.as_deref())
                        .map(|n| div().text_color(rgb(colors::TEXT_DIM)).child(n.to_string())),
                )
                .children(self.confirm.clone().map(|q| {
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().flex_1().text_color(rgb(colors::ORANGE)).child(q))
                        .child(button("restart-cancel", "Cancel", false, true).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.confirm = None;
                                cx.notify();
                            }),
                        ))
                        .child(
                            button("restart-yes", "Restart", true, true)
                                .on_click(cx.listener(|this, _, _, cx| this.confirm_restart(cx))),
                        )
                }))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_end()
                        .gap_2()
                        .child(muted(hint).mr_auto())
                        .child(
                            button(
                                "settings-restart",
                                "Restart the daemon",
                                false,
                                self.confirm.is_none(),
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.ask_restart(cx))),
                        )
                        .child(
                            button("settings-close", "Close", false, true)
                                .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
                        )
                        .child(
                            button("settings-save", "Save", true, ready && !self.busy)
                                .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                        ),
                );
        div()
            .key_context("Settings")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x00000099))
            .occlude()
            .child(card)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use overseer_proto::settings::{Applies, Setting, SettingKind, SettingValue};
    use overseer_proto::{DaemonInfo, SlotState};

    fn setting(
        key: &str,
        group: &str,
        kind: SettingKind,
        applies: Applies,
        value: SettingValue,
    ) -> Setting {
        Setting {
            key: key.into(),
            group: group.into(),
            kind,
            applies,
            value: value.clone(),
            default: value,
            in_file: false,
            restart_needed: false,
            note: None,
        }
    }

    fn view() -> SettingsView {
        let text = |s: &str| SettingValue::Text(s.into());
        let mut jql = setting(
            "jql",
            "Work items",
            SettingKind::Text { allow_empty: false },
            Applies::Restart,
            text("project = DEMO"),
        );
        jql.in_file = true;
        let mut writer = setting(
            "brain_writer",
            "2ndBrain",
            SettingKind::Bool,
            Applies::ReadOnly,
            SettingValue::Bool(false),
        );
        writer.note = Some("Change it in config.toml.".into());
        SettingsView {
            config_path: "/tmp/demo/config.toml".into(),
            file_hash: "abc".into(),
            load_error: None,
            settings: vec![
                jql,
                setting(
                    "jira_poll_secs",
                    "Work items",
                    SettingKind::Int { min: 5, max: 3600 },
                    Applies::Restart,
                    SettingValue::Int(300),
                ),
                setting(
                    "lock_idle_mins",
                    "Locks and worktrees",
                    SettingKind::Int { min: 1, max: 10080 },
                    Applies::Restart,
                    SettingValue::Int(30),
                ),
                setting(
                    "shared_files",
                    "2ndBrain",
                    SettingKind::List,
                    Applies::Restart,
                    SettingValue::List(vec!["CURRENT.md".into()]),
                ),
                setting(
                    "push",
                    "2ndBrain",
                    SettingKind::Bool,
                    Applies::Restart,
                    SettingValue::Bool(true),
                ),
                writer,
                setting(
                    "terminal.font_size",
                    "Terminal",
                    SettingKind::Float {
                        min: 4.0,
                        max: 96.0,
                    },
                    Applies::App,
                    SettingValue::Float(13.0),
                ),
            ],
            targets_path: "/tmp/demo/targets.toml".into(),
            targets_note: "3 built-in patterns; /tmp/demo/targets.toml does not exist".into(),
            targets_error: None,
            daemon: DaemonInfo {
                version: "0.1.0".into(),
                build: "b1".into(),
                pid: 42,
                started_at: 0,
                exe: "/tmp/demo/overseer".into(),
            },
        }
    }

    fn set(key: &str, value: SettingValue) -> SettingEdit {
        SettingEdit::Set {
            key: key.into(),
            value,
        }
    }

    #[test]
    fn draft_starts_clean() {
        let d = SettingsDraft::new(view());
        assert!(!d.dirty());
        assert_eq!(d.edits(), Ok(vec![]));
        let rows = d.rows();
        assert_eq!(rows.len(), 7);
        let jql = &rows[0];
        assert_eq!(jql.key, "jql");
        assert_eq!(jql.group, "Work items");
        assert_eq!(jql.value, RowValue::Text("project = DEMO".into()));
        assert_eq!(jql.source, "from the file");
        assert!(jql.can_reset);
        assert_eq!(rows[1].source, "default");
        assert!(!rows[1].can_reset);
        assert_eq!(rows[3].value, RowValue::Text("CURRENT.md".into()));
        assert_eq!(rows[4].value, RowValue::Toggle(true));
        assert_eq!(
            rows[5].value,
            RowValue::ReadOnly {
                text: "false".into(),
                note: Some("Change it in config.toml.".into())
            }
        );
    }

    #[test]
    fn editing_marks_dirty_and_produces_typed_edits() {
        let mut d = SettingsDraft::new(view());
        d.set_text("jira_poll_secs", "60");
        assert!(d.dirty());
        d.set_text("terminal.font_size", "14");
        d.set_text("shared_files", "a.md, b/*.md");
        d.toggle("push");
        assert_eq!(
            d.edits(),
            Ok(vec![
                set("jira_poll_secs", SettingValue::Int(60)),
                set(
                    "shared_files",
                    SettingValue::List(vec!["a.md".into(), "b/*.md".into()])
                ),
                set("push", SettingValue::Bool(false)),
                set("terminal.font_size", SettingValue::Float(14.0)),
            ])
        );
        let rows = d.rows();
        assert_eq!(rows[1].value, RowValue::Text("60".into()));
        assert!(rows[1].edited);
        assert_eq!(rows[4].value, RowValue::Toggle(false));
        // Typing the old text back is no edit.
        d.set_text("jira_poll_secs", "300");
        d.set_text("terminal.font_size", "13.0");
        d.set_text("shared_files", "CURRENT.md");
        d.toggle("push");
        assert!(!d.dirty());
    }

    #[test]
    fn bad_number_is_caught_before_sending() {
        let mut d = SettingsDraft::new(view());
        d.set_text("jira_poll_secs", "sixty");
        assert_eq!(
            d.edits(),
            Err("jira_poll_secs must be a whole number from 5 to 3600".to_string())
        );
    }

    #[test]
    fn reset_produces_a_reset_edit() {
        let mut d = SettingsDraft::new(view());
        d.set_text("jql", "project = DEMO2");
        d.reset("jql");
        assert!(d.dirty());
        assert_eq!(
            d.edits(),
            Ok(vec![SettingEdit::Reset { key: "jql".into() }])
        );
        let jql = &d.rows()[0];
        assert_eq!(jql.source, "default");
        assert!(!jql.can_reset, "already reset");
        // A key not in the file has nothing to reset.
        d.reset("jira_poll_secs");
        assert_eq!(d.edits().unwrap().len(), 1);
    }

    #[test]
    fn read_only_rows_are_not_editable() {
        let mut d = SettingsDraft::new(view());
        d.set_text("brain_writer", "true");
        d.toggle("brain_writer");
        d.reset("brain_writer");
        d.set_text("no_such_key", "1");
        assert!(!d.dirty());
        assert_eq!(d.edits(), Ok(vec![]));
    }

    #[test]
    fn saved_text_names_restart_keys() {
        let mut after = view();
        assert_eq!(saved_text(&after, &["jql".into()]), "Saved.");
        for s in &mut after.settings {
            if ["jql", "lock_idle_mins", "push"].contains(&s.key.as_str()) {
                s.restart_needed = true;
            }
        }
        let edited = |keys: &[&str]| keys.iter().map(|k| k.to_string()).collect::<Vec<_>>();
        assert_eq!(
            saved_text(
                &after,
                &edited(&["jql", "lock_idle_mins", "terminal.font_size"])
            ),
            "Saved. The daemon uses the new jql and lock_idle_mins after it restarts."
        );
        assert_eq!(
            saved_text(&after, &edited(&["lock_idle_mins"])),
            "Saved. The daemon uses the new lock_idle_mins after it restarts."
        );
        assert_eq!(
            saved_text(&after, &edited(&["jql", "lock_idle_mins", "push"])),
            "Saved. The daemon uses the new jql, lock_idle_mins and push after it restarts."
        );
    }

    fn slot(id: overseer_proto::SlotId, label: &str, running: bool) -> SlotInfo {
        SlotInfo {
            id,
            label: label.into(),
            cwd: None,
            command: None,
            state: if running {
                SlotState::Running { pid: 1, since: 0 }
            } else {
                SlotState::Empty
            },
            cols: 80,
            rows: 24,
            activity: None,
            session_note: None,
        }
    }

    #[test]
    fn restart_confirm_lists_running_sessions() {
        let slots = vec![
            slot(0, "", false),
            slot(1, "demo lane", true),
            slot(2, "idle", false),
            slot(3, "docs", true),
        ];
        assert_eq!(
            restart_confirm(&slots),
            "Restart the daemon? 2 running sessions end and come back as resumable: S1 demo lane, S3 docs."
        );
        assert_eq!(
            restart_confirm(&slots[..2]),
            "Restart the daemon? 1 running session ends and comes back as resumable: S1 demo lane."
        );
        assert_eq!(
            restart_confirm(&[slot(1, "x", false)]),
            "Restart the daemon? No session is running."
        );
    }

    #[test]
    fn app_keys_are_terminal_and_editor() {
        let reset = |k: &str| SettingEdit::Reset { key: k.into() };
        assert!(!app_keys_changed(&[
            reset("jql"),
            set("push", SettingValue::Bool(true))
        ]));
        assert!(app_keys_changed(&[
            reset("jql"),
            reset("terminal.font_size")
        ]));
        assert!(app_keys_changed(&[set(
            "editor.name",
            SettingValue::Text("Zed".into())
        )]));
        assert!(!app_keys_changed(&[]));
    }

    #[test]
    fn about_lines_name_both_builds() {
        let mut info = view().daemon;
        info.started_at = 1_700_000_000;
        let since = overseer_proto::local_hhmm(info.started_at);
        assert_eq!(
            about_lines("b1", Some(&info)),
            vec![
                "App build b1".to_string(),
                format!("Daemon build b1, pid 42, running since {since}"),
            ]
        );
        let lines = about_lines("b2", Some(&info));
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[2], overseer_proto::build_notice("b2", "b1").unwrap());
        assert_eq!(
            about_lines("b2", None),
            vec![
                "App build b2".to_string(),
                "Daemon build unknown (older than this app)".to_string(),
                "Restart the daemon to finish the upgrade.".to_string(),
            ]
        );
    }

    #[test]
    fn restart_outcome_reports_the_result_or_the_error() {
        assert_eq!(RESTART_ARGS, ["ctl", "restart", "--yes"]);
        assert_eq!(
            restart_outcome(true, "Restarted the daemon (build b1 to b2).\n", ""),
            "Restarted the daemon (build b1 to b2)."
        );
        assert_eq!(restart_outcome(true, "", ""), "Restarted the daemon.");
        assert_eq!(
            restart_outcome(
                false,
                "",
                "warning\nError: launchctl kickstart -k demo failed\n\n"
            ),
            "The daemon did not restart: Error: launchctl kickstart -k demo failed"
        );
        assert_eq!(
            restart_outcome(false, "", ""),
            "The daemon did not restart, and said nothing about why. See gui.log."
        );
    }

    #[test]
    fn first_escape_with_edits_warns() {
        let mut d = SettingsDraft::new(view());
        let mut esc = EscGuard::default();
        assert!(esc.press(&d), "clean: closes at once");
        d.set_text("jql", "x");
        let mut esc = EscGuard::default();
        assert!(!esc.press(&d));
        assert_eq!(esc.note(), Some(ESC_AGAIN));
        assert!(esc.press(&d));
        // Any other key clears the warning.
        let mut esc = EscGuard::default();
        esc.press(&d);
        esc.clear();
        assert!(!esc.press(&d));
    }

    /// A fake daemon on a temp socket: answers each request with `reply`,
    /// or hangs up on it when `reply` is None, as a daemon that predates
    /// `DaemonInfo` does.
    async fn fake_daemon(socket: PathBuf, reply: Option<Response>) {
        use overseer_proto::{read_frame, write_frame, Frame};
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (mut rd, mut wr) = stream.into_split();
            if let Ok(Some(Frame::Request { seq, .. })) = read_frame(&mut rd).await {
                if let Some(resp) = reply {
                    let _ = write_frame(&mut wr, &Frame::Response { seq, resp }).await;
                }
            }
        });
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn probe_tells_a_known_daemon_from_an_old_one() {
        let dir = tempfile::tempdir().unwrap();
        let info = view().daemon;
        let new = dir.path().join("new.sock");
        fake_daemon(new.clone(), Some(Response::DaemonInfo(info.clone()))).await;
        assert_eq!(probe_daemon(new).await, DaemonKnown::Info(info));
        let old = dir.path().join("old.sock");
        fake_daemon(old.clone(), None).await;
        assert_eq!(probe_daemon(old).await, DaemonKnown::Old);
        let refusing = dir.path().join("refusing.sock");
        let error = Response::Error {
            message: "unknown request".into(),
        };
        fake_daemon(refusing.clone(), Some(error)).await;
        assert_eq!(probe_daemon(refusing).await, DaemonKnown::Old);
        assert_eq!(
            probe_daemon(dir.path().join("none.sock")).await,
            DaemonKnown::Unknown
        );
    }
}
