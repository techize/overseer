//! Start dialog: label, cwd (with a folder picker) and command for one slot.
//! Submitting sends `StartSlot`; on success the app opens that session.
//!
//! Field editing and validation are pure (`TextField`, `edit_key`,
//! `build_request`) and unit tested.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{
    div, prelude::*, px, rgb, rgba, Context, EventEmitter, FocusHandle, Focusable, FontWeight,
    KeyDownEvent, Keystroke, MouseButton, PathPromptOptions, SharedString, Window,
};
use overseer_proto::{slot_name, Client, Request, Response, SlotId, SlotInfo};

use crate::app::colors;
use crate::board::tilde;

/// Starting cwd.
pub const DEFAULT_CWD: &str = "~/Repos/2ndBrain";
/// Starting command. Left as is, the request names no command, so the daemon
/// runs its default (`claude`, or `OVERSEER_DEFAULT_COMMAND`).
pub const DEFAULT_COMMAND: &str = "claude";

// ---------------------------------------------------------------------------
// Pure logic
// ---------------------------------------------------------------------------

/// One line of editable text. `cursor` is a byte offset on a char boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextField {
    pub text: String,
    pub cursor: usize,
}

impl TextField {
    pub fn new(text: &str) -> Self {
        TextField {
            text: text.into(),
            cursor: text.len(),
        }
    }

    pub fn set(&mut self, text: &str) {
        *self = TextField::new(text);
    }

    pub fn insert(&mut self, s: &str) {
        // One line: drop newlines from pastes.
        let s: String = s.chars().filter(|c| *c != '\n' && *c != '\r').collect();
        self.text.insert_str(self.cursor, &s);
        self.cursor += s.len();
    }

    fn prev(&self) -> usize {
        self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next(&self) -> usize {
        self.text[self.cursor..]
            .chars()
            .next()
            .map_or(self.cursor, |c| self.cursor + c.len_utf8())
    }

    pub fn backspace(&mut self) {
        let p = self.prev();
        self.text.replace_range(p..self.cursor, "");
        self.cursor = p;
    }

    pub fn delete(&mut self) {
        let n = self.next();
        self.text.replace_range(self.cursor..n, "");
    }

    pub fn left(&mut self) {
        self.cursor = self.prev();
    }

    pub fn right(&mut self) {
        self.cursor = self.next();
    }

    /// The text with a bar at the cursor, for display.
    pub fn with_cursor(&self) -> String {
        format!(
            "{}|{}",
            &self.text[..self.cursor],
            &self.text[self.cursor..]
        )
    }
}

/// What a key press in the dialog means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogKey {
    /// Edited the active field.
    Edited,
    NextField,
    PrevField,
    Submit,
    Cancel,
    Paste,
    /// Not for the dialog.
    Ignored,
}

/// Applies `ks` to `field`.
pub fn edit_key(field: &mut TextField, ks: &Keystroke) -> DialogKey {
    let m = &ks.modifiers;
    if m.platform && !m.control && !m.alt {
        return match ks.key.as_str() {
            "v" => DialogKey::Paste,
            "backspace" => {
                field.text.replace_range(..field.cursor, "");
                field.cursor = 0;
                DialogKey::Edited
            }
            "left" => {
                field.cursor = 0;
                DialogKey::Edited
            }
            "right" => {
                field.cursor = field.text.len();
                DialogKey::Edited
            }
            _ => DialogKey::Ignored,
        };
    }
    match ks.key.as_str() {
        "escape" => return DialogKey::Cancel,
        "enter" => return DialogKey::Submit,
        "tab" if m.shift => return DialogKey::PrevField,
        "tab" => return DialogKey::NextField,
        "backspace" => {
            field.backspace();
            return DialogKey::Edited;
        }
        "delete" => {
            field.delete();
            return DialogKey::Edited;
        }
        "left" => {
            field.left();
            return DialogKey::Edited;
        }
        "right" => {
            field.right();
            return DialogKey::Edited;
        }
        "home" => {
            field.cursor = 0;
            return DialogKey::Edited;
        }
        "end" => {
            field.cursor = field.text.len();
            return DialogKey::Edited;
        }
        _ => {}
    }
    if m.control || m.function {
        return DialogKey::Ignored;
    }
    match &ks.key_char {
        Some(ch) if !ch.is_empty() && !ch.chars().any(char::is_control) => {
            field.insert(ch);
            DialogKey::Edited
        }
        _ => DialogKey::Ignored,
    }
}

/// `~` and `~/x` expanded against `home`; anything else as typed.
pub fn expand_tilde(s: &str, home: Option<&Path>) -> PathBuf {
    match (s, home) {
        ("~", Some(h)) => h.to_path_buf(),
        (s, Some(h)) if s.starts_with("~/") => h.join(&s[2..]),
        (s, _) => PathBuf::from(s),
    }
}

/// Label, folder and command the dialog starts with: those of `last` when
/// it ran before, else an empty label and the defaults.
pub fn initial_fields(last: Option<&SlotInfo>, home: Option<&Path>) -> [String; 3] {
    match last.and_then(|i| Some((i, i.cwd.as_deref()?))) {
        Some((info, cwd)) => [
            info.label.clone(),
            tilde(cwd, home),
            info.command
                .as_ref()
                .map_or_else(|| DEFAULT_COMMAND.to_string(), |c| c.join(" ")),
        ],
        None => [String::new(), DEFAULT_CWD.into(), DEFAULT_COMMAND.into()],
    }
}

/// Where the folder picker opens: the folder typed in the field, or the
/// nearest folder above it that exists, falling back to `home`.
pub fn picker_start(
    field: &str,
    home: Option<&Path>,
    is_dir: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let field = field.trim();
    let typed = expand_tilde(field, home);
    if !field.is_empty() && typed.is_absolute() {
        if let Some(dir) = typed.ancestors().find(|p| is_dir(p)) {
            return Some(dir.to_path_buf());
        }
    }
    home.map(Path::to_path_buf)
}

/// Makes the next macOS open panel start in `dir`. `NSOpenPanel` opens in
/// the folder stored under this defaults key, and gpui's path prompt has no
/// option for it.
// objc 0.2's `msg_send!` expands a cfg this crate does not declare.
#[allow(unexpected_cfgs)]
fn set_open_panel_start(dir: &Path) {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};
    use std::ffi::CString;
    let (Some(path), Ok(key)) = (
        dir.to_str().and_then(|p| CString::new(p).ok()),
        CString::new("NSNavLastRootDirectory"),
    ) else {
        return;
    };
    // SAFETY: plain Foundation calls on the main thread with valid,
    // NUL-terminated strings; the NSString results are autoreleased.
    unsafe {
        let defaults: *mut Object = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let value: *mut Object = msg_send![class!(NSString), stringWithUTF8String: path.as_ptr()];
        let key: *mut Object = msg_send![class!(NSString), stringWithUTF8String: key.as_ptr()];
        let _: () = msg_send![defaults, setObject: value forKey: key];
    }
}

/// Validates the form and builds its `StartSlot`. `is_dir` checks the cwd.
pub fn build_request(
    slot: SlotId,
    label: &str,
    cwd: &str,
    command: &str,
    home: Option<&Path>,
    is_dir: impl Fn(&Path) -> bool,
) -> Result<Request, String> {
    let cwd = cwd.trim();
    if cwd.is_empty() {
        return Err("Choose a folder to start in.".into());
    }
    let path = expand_tilde(cwd, home);
    if !path.is_absolute() {
        return Err("The folder must be an absolute path or start with ~.".into());
    }
    if !is_dir(&path) {
        return Err(format!("{} is not a folder.", path.display()));
    }
    let argv: Vec<String> = command.split_whitespace().map(String::from).collect();
    if argv.is_empty() {
        return Err("Enter a command to run.".into());
    }
    let command = (argv != [DEFAULT_COMMAND]).then_some(argv);
    let label = match label.trim() {
        "" => path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        l => l.to_string(),
    };
    Ok(Request::StartSlot {
        id: slot,
        label,
        cwd: path,
        command,
    })
}

// ---------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------

/// What the dialog tells the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogEvent {
    Started(SlotId),
    Cancelled,
}

const FIELD_NAMES: [&str; 3] = ["Label", "Folder", "Command"];

pub struct StartDialog {
    slot: SlotId,
    client: Arc<Client>,
    home: Option<PathBuf>,
    fields: [TextField; 3],
    active: usize,
    error: Option<String>,
    busy: bool,
    focus_handle: FocusHandle,
}

impl EventEmitter<DialogEvent> for StartDialog {}

impl Focusable for StartDialog {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl StartDialog {
    /// The dialog for `slot`, filled in from `last` (the slot's previous
    /// run) when there is one, else with the defaults.
    pub fn new(
        slot: SlotId,
        client: Arc<Client>,
        last: Option<&SlotInfo>,
        cx: &mut Context<Self>,
    ) -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let [label, cwd, command] = initial_fields(last, home.as_deref());
        StartDialog {
            slot,
            client,
            home,
            fields: [
                TextField::new(&label),
                TextField::new(&cwd),
                TextField::new(&command),
            ],
            active: 0,
            error: None,
            busy: false,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn slot(&self) -> SlotId {
        self.slot
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let outcome = edit_key(&mut self.fields[self.active], &ev.keystroke);
        match outcome {
            DialogKey::Ignored => return,
            DialogKey::Edited => self.error = None,
            DialogKey::NextField => self.active = (self.active + 1) % 3,
            DialogKey::PrevField => self.active = (self.active + 2) % 3,
            DialogKey::Submit => self.submit(cx),
            DialogKey::Cancel => cx.emit(DialogEvent::Cancelled),
            DialogKey::Paste => {
                if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                    self.fields[self.active].insert(&text);
                }
            }
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn choose_folder(&mut self, cx: &mut Context<Self>) {
        let home = self.home.as_deref();
        if let Some(dir) = picker_start(&self.fields[1].text, home, Path::is_dir) {
            set_open_panel_start(&dir);
        }
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(p) = paths.into_iter().next() {
                    let _ = this.update(cx, |this, cx| {
                        let shown = tilde(&p, this.home.as_deref());
                        this.fields[1].set(&shown);
                        this.error = None;
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let [label, cwd, command] = &self.fields;
        let req = match build_request(
            self.slot,
            &label.text,
            &cwd.text,
            &command.text,
            self.home.as_deref(),
            Path::is_dir,
        ) {
            Ok(r) => r,
            Err(e) => {
                self.error = Some(e);
                cx.notify();
                return;
            }
        };
        self.busy = true;
        let client = self.client.clone();
        let slot = self.slot;
        cx.spawn(async move |this, cx| {
            let result = client.request(req).await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(Response::Ok) => cx.emit(DialogEvent::Started(slot)),
                    Ok(Response::Error { message }) => this.error = Some(message),
                    Ok(other) => this.error = Some(format!("Unexpected reply: {other:?}")),
                    Err(e) => {
                        this.error = Some(format!("{} ({e:#})", crate::terminal_view::LOST_DAEMON))
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

fn button(id: &'static str, text: &'static str, primary: bool) -> gpui::Stateful<gpui::Div> {
    div()
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
        .text_color(rgb(if primary {
            colors::ACCENT
        } else {
            colors::TEXT
        }))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(colors::CARD_HOVER)))
        .child(text)
}

impl Render for StartDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);
        let rows: Vec<_> = (0..3)
            .map(|i| {
                let active = i == self.active;
                let f = &self.fields[i];
                let text = if active && focused {
                    f.with_cursor()
                } else {
                    f.text.clone()
                };
                let field = div()
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
                    .child(SharedString::from(text))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.active = i;
                        cx.notify();
                    }));
                let row = div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(72.))
                            .text_color(rgb(colors::MUTED))
                            .child(FIELD_NAMES[i]),
                    )
                    .child(field);
                if i == 1 {
                    row.child(
                        button("choose", "Choose…", false)
                            .on_click(cx.listener(|this, _, _, cx| this.choose_folder(cx))),
                    )
                } else {
                    row
                }
            })
            .collect();
        let card = div()
            .w(px(520.))
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .rounded(px(8.))
            .bg(rgb(colors::CARD))
            .border_1()
            .border_color(rgb(colors::BORDER_COORD))
            .text_sm()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!("Start a session in {}", slot_name(self.slot))),
            )
            .children(rows)
            .children(
                self.error
                    .clone()
                    .map(|e| div().text_color(rgb(colors::RED)).child(e)),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        div()
                            .mr_auto()
                            .text_xs()
                            .text_color(rgb(colors::MUTED))
                            .child("Tab next field · Enter start · Esc cancel"),
                    )
                    .child(
                        button("cancel", "Cancel", false)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Cancelled))),
                    )
                    .child(
                        button(
                            "submit",
                            if self.busy { "Starting…" } else { "Start" },
                            true,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                    ),
            );
        div()
            .key_context("StartDialog")
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
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Cancelled)),
            )
            .child(card)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stopped_slot_reopens_with_its_last_settings() {
        let home = Some(Path::new("/Users/me"));
        let mut last = SlotInfo {
            id: 4,
            label: "eks".into(),
            cwd: Some("/Users/me/Repos/eks".into()),
            command: Some(vec!["claude".into(), "--resume".into()]),
            state: overseer_proto::SlotState::Exited {
                code: Some(0),
                at: 1,
            },
            cols: 80,
            rows: 24,
        };
        assert_eq!(
            initial_fields(Some(&last), home),
            [
                "eks".to_string(),
                "~/Repos/eks".into(),
                "claude --resume".into()
            ]
        );
        last.command = None;
        assert_eq!(initial_fields(Some(&last), home)[2], DEFAULT_COMMAND);
        last.cwd = None;
        assert_eq!(
            initial_fields(Some(&last), home),
            [String::new(), DEFAULT_CWD.into(), DEFAULT_COMMAND.into()]
        );
        assert_eq!(initial_fields(None, home)[1], "~/Repos/2ndBrain");
    }

    #[test]
    fn picker_starts_in_the_typed_folder_or_the_nearest_one_above() {
        let home = Path::new("/Users/me");
        let dirs = ["/", "/Users", "/Users/me", "/Users/me/code"];
        let is_dir = |p: &Path| dirs.iter().any(|d| Path::new(d) == p);
        let start = |f| picker_start(f, Some(home), is_dir);
        assert_eq!(start("~/code"), Some(PathBuf::from("/Users/me/code")));
        assert_eq!(
            start("/Users/me/code"),
            Some(PathBuf::from("/Users/me/code"))
        );
        assert_eq!(
            start("~/code/new/deeper"),
            Some(PathBuf::from("/Users/me/code"))
        );
        assert_eq!(start("  ~  "), Some(PathBuf::from("/Users/me")));
        // Empty or relative: home.
        assert_eq!(start(""), Some(PathBuf::from("/Users/me")));
        assert_eq!(start("code"), Some(PathBuf::from("/Users/me")));
        assert_eq!(picker_start("", None, is_dir), None);
    }

    fn key(k: &str, ch: Option<&str>) -> Keystroke {
        let mut ks = Keystroke::parse(k).unwrap();
        ks.key_char = ch.map(String::from);
        ks
    }

    #[test]
    fn field_editing() {
        let mut f = TextField::new("ab");
        assert_eq!(edit_key(&mut f, &key("c", Some("c"))), DialogKey::Edited);
        assert_eq!(f.with_cursor(), "abc|");
        edit_key(&mut f, &key("left", None));
        edit_key(&mut f, &key("left", None));
        edit_key(&mut f, &key("é", Some("é")));
        assert_eq!(f.with_cursor(), "aé|bc");
        edit_key(&mut f, &key("backspace", None));
        assert_eq!(f.with_cursor(), "a|bc");
        edit_key(&mut f, &key("delete", None));
        assert_eq!(f.with_cursor(), "a|c");
        edit_key(&mut f, &key("end", None));
        edit_key(&mut f, &key("right", None));
        assert_eq!(f.with_cursor(), "ac|");
        edit_key(&mut f, &key("cmd-backspace", None));
        assert_eq!(f.with_cursor(), "|");
        edit_key(&mut f, &key("backspace", None));
        assert_eq!(f.text, "");
        edit_key(&mut f, &key("space", Some(" ")));
        assert_eq!(f.text, " ");
    }

    #[test]
    fn dialog_keys() {
        let mut f = TextField::default();
        assert_eq!(edit_key(&mut f, &key("escape", None)), DialogKey::Cancel);
        assert_eq!(edit_key(&mut f, &key("enter", None)), DialogKey::Submit);
        assert_eq!(edit_key(&mut f, &key("tab", None)), DialogKey::NextField);
        assert_eq!(
            edit_key(&mut f, &key("shift-tab", None)),
            DialogKey::PrevField
        );
        assert_eq!(edit_key(&mut f, &key("cmd-v", None)), DialogKey::Paste);
        assert_eq!(edit_key(&mut f, &key("cmd-1", None)), DialogKey::Ignored);
        assert_eq!(
            edit_key(&mut f, &key("ctrl-a", Some("a"))),
            DialogKey::Ignored
        );
        assert_eq!(f.text, "");
    }

    #[test]
    fn paste_drops_newlines() {
        let mut f = TextField::default();
        f.insert("a\nb\r");
        assert_eq!(f.text, "ab");
    }

    #[test]
    fn tilde_expansion() {
        let h = Some(Path::new("/Users/me"));
        assert_eq!(expand_tilde("~", h), PathBuf::from("/Users/me"));
        assert_eq!(expand_tilde("~/Repos", h), PathBuf::from("/Users/me/Repos"));
        assert_eq!(expand_tilde("/tmp", h), PathBuf::from("/tmp"));
        assert_eq!(expand_tilde("~x", h), PathBuf::from("~x"));
        assert_eq!(expand_tilde("~/Repos", None), PathBuf::from("~/Repos"));
    }

    fn home() -> Option<&'static Path> {
        Some(Path::new("/Users/me"))
    }

    #[test]
    fn defaults_build_a_default_command_request() {
        let r = build_request(5, "", DEFAULT_CWD, DEFAULT_COMMAND, home(), |_| true).unwrap();
        assert_eq!(
            r,
            Request::StartSlot {
                id: 5,
                label: "2ndBrain".into(),
                cwd: "/Users/me/Repos/2ndBrain".into(),
                command: None,
            }
        );
    }

    #[test]
    fn custom_command_and_label() {
        let r = build_request(2, " eks ", "/tmp", " claude  --resume ", home(), |_| true).unwrap();
        assert_eq!(
            r,
            Request::StartSlot {
                id: 2,
                label: "eks".into(),
                cwd: "/tmp".into(),
                command: Some(vec!["claude".into(), "--resume".into()]),
            }
        );
    }

    #[test]
    fn validation_errors() {
        let ok = |_: &Path| true;
        assert!(build_request(1, "", "  ", "claude", home(), ok).is_err());
        assert!(build_request(1, "", "Repos", "claude", home(), ok).is_err());
        assert!(build_request(1, "", "/tmp", "   ", home(), ok).is_err());
        let e = build_request(1, "", "/nope", "claude", home(), |_| false).unwrap_err();
        assert_eq!(e, "/nope is not a folder.");
    }
}
