//! The session view: tab strip, the slot's terminal on the left (about 62%)
//! and the context panel on the right.
//!
//! Also the pure rules behind it: tab and panel text, when a slot's
//! `TerminalView` is kept or dropped, and the restart sequence (stop, wait
//! for Exited, start again with the same label, cwd and command).
//!
//! The Owns box lists the work the slot owns with a Release button each,
//! the slot's notices, Assign (the work-item picker) and Send kickoff,
//! which types the daemon's kickoff prompt into an idle session.
//! Clicking an owned row (not its Release button) shows the item's detail
//! in place of the panel's boxes, fetched from the daemon on request.

use std::path::{Path, PathBuf};

use gpui::Keystroke;
use gpui::{div, prelude::*, px, relative, rgb, rgba, Context, FontWeight, SharedString, Window};
use overseer_proto::{
    slot_name, ActivityKind, ClaimKind, CoordView, ItemDetail, ItemKind, LockView, Notice,
    RelayState, ReportKind, Request, Response, Sender, SlotId, SlotInfo, SlotState, WorkView,
};

use crate::app::{colors, OpenMessage, OpenNote, OpenRelease, OverseerApp};
use crate::board::{
    activity_detail, activity_state, format_elapsed, local_hhmm, one_line, owned_by, slot_title,
    tilde, Tone,
};
use crate::daemon_info::DaemonKnown;
use crate::start_dialog::{edit_key, DialogKey, TextField};

// ---------------------------------------------------------------------------
// Pure logic
// ---------------------------------------------------------------------------

/// Text of tab `id`: `0 S0`, `3 S3`, with a dot while it runs and a hollow one on its turn.
pub fn tab_label(info: &SlotInfo) -> String {
    let name = slot_name(info.id);
    match info.state {
        SlotState::Running { .. } if tab_your_turn(info) => format!("{} {name} ○", info.id),
        SlotState::Running { .. } => format!("{} {name} ●", info.id),
        _ => format!("{} {name}", info.id),
    }
}

/// Whether the tab of `info` shows in orange: its session is running and needs
/// Jonathan, the same rule as the board's orange card.
pub fn tab_needs_you(info: &SlotInfo) -> bool {
    matches!(info.state, SlotState::Running { .. })
        && info
            .activity
            .as_ref()
            .is_some_and(|a| a.kind == ActivityKind::NeedsYou)
}

/// Whether the tab of `info` shows the accent mark: its session is running and
/// waits for the next prompt.
pub fn tab_your_turn(info: &SlotInfo) -> bool {
    matches!(info.state, SlotState::Running { .. })
        && info
            .activity
            .as_ref()
            .is_some_and(|a| a.kind == ActivityKind::Idle)
}

/// What the context panel shows for a slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelModel {
    pub title: String,
    /// (name, value) rows: label, cwd, pid, state, started, then activity
    /// and its reason or tool when the slot reports one.
    pub rows: Vec<(&'static str, String)>,
    pub can_stop: bool,
    pub can_restart: bool,
    /// Stopped: the start dialog can run the slot again with new settings.
    pub can_start_new: bool,
    /// Resumable: Resume takes the place of Restart and Start.
    pub can_resume: bool,
}

/// The obsidian link that opens `note` (relative to `brain_dir`).
pub fn obsidian_url(brain_dir: &Path, note: &str) -> String {
    let abs = brain_dir.join(note);
    let mut url = String::from("obsidian://open?path=");
    for b in abs.to_string_lossy().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                url.push(b as char)
            }
            _ => url.push_str(&format!("%{b:02X}")),
        }
    }
    url
}

/// What the panel says about the slot's session note, `None` for an empty
/// slot.
pub fn note_line(info: &SlotInfo, brain_dir: Option<&Path>) -> Option<String> {
    match (&info.session_note, &info.state) {
        (Some(note), _) if brain_dir.is_some() => Some(format!("{note} (press n to open)")),
        (Some(note), _) => Some(note.clone()),
        (None, SlotState::Empty) => None,
        (None, _) => Some("No session note yet".into()),
    }
}

/// The link `n` opens, or `None` while the slot has no note yet or the
/// board does not know the vault.
pub fn note_target(info: &SlotInfo, brain_dir: Option<&Path>) -> Option<String> {
    Some(obsidian_url(brain_dir?, info.session_note.as_deref()?))
}

/// `14:05:12 (8m 05s ago)`. `clock` turns unix seconds into local time.
pub fn format_started(since: i64, now: i64, clock: impl Fn(i64) -> String) -> String {
    format!("{} ({} ago)", clock(since), format_elapsed(now - since))
}

/// The slot's state as the panel's State row and the pop-over header show
/// it: `Running`, `Exited (code 1) at 14:05:12`, with `, restarting` added
/// while a restart is under way.
pub fn state_text(info: &SlotInfo, restarting: bool, clock: impl Fn(i64) -> String) -> String {
    let state = match &info.state {
        SlotState::Empty => "Empty".to_string(),
        SlotState::Running { .. } => "Running".to_string(),
        SlotState::Exited { code, at } => match code {
            Some(c) => format!("Exited (code {c}) at {}", clock(*at)),
            None => format!("Exited at {}", clock(*at)),
        },
        SlotState::Resumable { .. } => "Resumable".to_string(),
    };
    if restarting {
        format!("{state}, restarting")
    } else {
        state
    }
}

pub fn panel_model(
    info: &SlotInfo,
    now: i64,
    home: Option<&Path>,
    restarting: bool,
    clock: impl Fn(i64) -> String,
) -> PanelModel {
    let dash = || "-".to_string();
    let label = if info.label.trim().is_empty() {
        dash()
    } else {
        info.label.trim().to_string()
    };
    let cwd = info
        .cwd
        .as_deref()
        .map(|p| tilde(p, home))
        .unwrap_or_else(dash);
    let (pid, started) = match &info.state {
        SlotState::Empty => (dash(), dash()),
        SlotState::Running { pid, since } => (pid.to_string(), format_started(*since, now, &clock)),
        SlotState::Exited { .. } => (dash(), dash()),
        SlotState::Resumable { at, .. } => (dash(), format_started(*at, now, &clock)),
    };
    let state = state_text(info, restarting, &clock);
    let running = matches!(info.state, SlotState::Running { .. });
    let mut rows = vec![
        ("Label", label),
        ("Cwd", cwd),
        ("Pid", pid),
        ("State", state),
        ("Started", started),
    ];
    if let Some(a) = &info.activity {
        rows.push(("Activity", activity_state(a, now)));
        if let Some(detail) = activity_detail(a) {
            let name = match a.kind {
                ActivityKind::NeedsYou => "Reason",
                _ => "Doing",
            };
            rows.push((name, detail));
        }
    }
    PanelModel {
        title: slot_title(info),
        rows,
        can_stop: running && !restarting,
        can_restart: !matches!(info.state, SlotState::Empty | SlotState::Resumable { .. })
            && info.cwd.is_some()
            && !restarting,
        can_start_new: matches!(info.state, SlotState::Exited { .. }) && !restarting,
        can_resume: matches!(info.state, SlotState::Resumable { .. }),
    }
}

/// One line of the Owns box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnsRow {
    pub key: String,
    pub title: String,
    pub status: String,
    /// The item has left the source's list but is still owned.
    pub gone: bool,
    /// Not in the list only because its source has not answered since the
    /// daemon started (implies `gone`).
    pub not_read_yet: bool,
}

/// The pop-over's heading: `S2 · DEMO-12 Title` for a slot that owns a
/// ticket, else the slot name alone.
pub fn popover_title(info: &SlotInfo, view: &WorkView) -> String {
    let name = slot_name(info.id);
    match owned_by(view, info.id)
        .into_iter()
        .find(|i| i.kind == ItemKind::Ticket)
    {
        Some(t) => format!("{name} \u{b7} {} {}", t.key, t.title.trim()),
        None => name,
    }
}

/// What the Owns box lists for `slot`, in `owned_by` order.
pub fn owns_rows(view: &WorkView, slot: SlotId) -> Vec<OwnsRow> {
    owned_by(view, slot)
        .into_iter()
        .map(|i| OwnsRow {
            key: i.key.clone(),
            title: title_or_key(&i.title, &i.key),
            status: i.status.clone(),
            gone: !i.in_list,
            not_read_yet: view.not_read_yet(i),
        })
        .collect()
}

/// `title`, or `key` when the title is empty or blank.
fn title_or_key(title: &str, key: &str) -> String {
    if title.trim().is_empty() {
        key.to_string()
    } else {
        title.to_string()
    }
}

/// One line of the detail view's body, read from the daemon's plain text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyLine {
    /// `## Goal`, without the `#`s.
    Heading(String),
    /// `- item` (text without the dash) or `1. item` (text keeps the
    /// number), `depth` from two spaces of indent per level.
    Bullet {
        depth: usize,
        text: String,
    },
    /// A line inside a fenced code block.
    Code(String),
    Text(String),
    Blank,
}

/// Body lines the detail view shows before "Show all".
pub const BODY_PREVIEW_LINES: usize = 40;

/// Where the open detail's fetch stands.
// Only one is held at a time, so the size gap costs nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum DetailState {
    Loading,
    Loaded(ItemDetail),
    Failed(String),
}

/// The item whose detail replaces the panel's boxes for `slot`.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenDetail {
    pub slot: SlotId,
    pub key: String,
    pub state: DetailState,
    /// Which request this view waits for; a reply carrying another number
    /// is stale and dropped.
    pub req: u64,
    /// The whole body shows, not only the first `BODY_PREVIEW_LINES`.
    pub show_all: bool,
    /// A refresh is on its way while the last loaded detail stays shown.
    pub refreshing: bool,
}

/// The detail to show for request `req` of `key` in `slot`, given the one
/// open before. The same item keeps `show_all`; a refresh of a loaded detail
/// keeps its text and marks it `refreshing`, anything else starts `Loading`.
pub fn reopen(
    prev: Option<&OpenDetail>,
    slot: SlotId,
    key: &str,
    req: u64,
    refresh: bool,
) -> OpenDetail {
    let same = prev.filter(|p| p.slot == slot && p.key == key);
    let kept = same
        .filter(|p| refresh && matches!(p.state, DetailState::Loaded(_)))
        .map(|p| match &p.state {
            // The old failure is over once a new fetch starts.
            DetailState::Loaded(d) => DetailState::Loaded(ItemDetail {
                error: None,
                ..d.clone()
            }),
            other => other.clone(),
        });
    OpenDetail {
        slot,
        key: key.to_string(),
        refreshing: kept.is_some(),
        state: kept.unwrap_or(DetailState::Loading),
        req,
        show_all: same.is_some_and(|p| p.show_all),
    }
}

/// What the detail view shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetailModel {
    pub key: String,
    pub title: String,
    /// (name, value): Status, Priority, Stage, PR..., Notes, Depends on,
    /// Folder, Fetched, each only when known.
    pub rows: Vec<(String, String)>,
    pub body: Vec<BodyLine>,
    /// Body lines were dropped for the preview.
    pub truncated: bool,
    /// The full body is longer than `BODY_PREVIEW_LINES`, so Show all /
    /// Show less applies.
    pub expandable: bool,
    /// What failed, shown in orange above the body.
    pub notice: Option<String>,
    /// `ov-3 Title (open)`, one per child bead.
    pub children: Vec<String>,
    pub loading: bool,
}

/// `1. ` (any number) at the start of `s`.
fn is_numbered(s: &str) -> bool {
    let digits = s.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && s[digits..].starts_with(". ")
}

/// Reads the daemon's plain text (see `adf_to_text`): `#` headings, `- ` and
/// `1. ` items indented two spaces per level, fenced code, blank lines.
pub fn body_lines(text: &str) -> Vec<BodyLine> {
    let mut out = Vec::new();
    let mut in_code = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            out.push(BodyLine::Code(line.to_string()));
            continue;
        }
        if line.trim().is_empty() {
            out.push(BodyLine::Blank);
            continue;
        }
        let rest = line.trim_start_matches(' ');
        let depth = (line.len() - rest.len()) / 2;
        let hashes = rest.bytes().take_while(|b| *b == b'#').count();
        if depth == 0 && hashes > 0 && rest[hashes..].starts_with(' ') {
            out.push(BodyLine::Heading(rest[hashes..].trim().to_string()));
        } else if rest.trim_end() == "-" {
            // An empty bullet: nothing to show.
        } else if let Some(item) = rest.strip_prefix("- ") {
            if !item.trim().is_empty() {
                out.push(BodyLine::Bullet {
                    depth,
                    text: item.trim_end().to_string(),
                });
            }
        } else if is_numbered(rest) {
            out.push(BodyLine::Bullet {
                depth,
                text: rest.trim_end().to_string(),
            });
        } else {
            out.push(BodyLine::Text(rest.trim_end().to_string()));
        }
    }
    out
}

/// Left padding of a body bullet at `depth`, in pixels.
pub fn bullet_indent(depth: usize) -> f32 {
    8. + 12. * depth as f32
}

/// `IN PROGRESS · 3h`, or the label alone when the move was not seen.
fn stage_text(stage: overseer_proto::Stage, since: Option<i64>, now: i64) -> String {
    match since {
        Some(t) => format!(
            "{} \u{b7} {}",
            stage.label(),
            crate::sprint_view::age_text(now - t)
        ),
        None => stage.label().to_string(),
    }
}

fn pr_text(pr: &overseer_proto::PullRequest) -> String {
    format!("{} #{} {}", pr.repo, pr.id, pr.title.trim())
}

/// The PRs of `key` from the poll, newest first, undated last.
fn view_prs<'a>(view: &'a WorkView, key: &str) -> Vec<&'a overseer_proto::PullRequest> {
    let mut prs: Vec<_> = view
        .prs
        .iter()
        .filter(|p| p.ticket.as_deref() == Some(key))
        .collect();
    // `None` sorts below any date, so reversed it goes last.
    prs.sort_by_key(|p| std::cmp::Reverse(p.created));
    prs
}

/// The beads under `key` from the poll, as the Child beads box lists them.
fn view_children(view: &WorkView, key: &str) -> Vec<String> {
    view.items
        .iter()
        .filter(|i| i.kind == ItemKind::Bead && i.parent.as_deref() == Some(key))
        .map(|i| format!("{} {} ({})", i.key, i.title.trim(), i.status))
        .collect()
}

/// The detail view for `open`. Until the reply is in (or when the request
/// failed) the rows come from the item in `view`, so the view is never empty.
/// Once loaded, an item still listed takes its status, priority, stage and
/// PRs from the poll, which is newer than the reply.
pub fn detail_model(
    open: &OpenDetail,
    view: &WorkView,
    now: i64,
    home: Option<&Path>,
) -> DetailModel {
    let item = view.items.iter().find(|i| i.key == open.key);
    let row = |k: &str, v: String| (k.to_string(), v);
    let priority = |p: Option<&String>| p.cloned().unwrap_or_else(|| "Not set".into());
    let pr_rows = |rows: &mut Vec<(String, String)>| {
        rows.extend(
            view_prs(view, &open.key)
                .into_iter()
                .map(|p| row("PR", pr_text(p))),
        );
    };
    let mut rows = Vec::new();
    let mut body = Vec::new();
    let mut notice = None;
    let children;
    let title;
    match &open.state {
        DetailState::Loaded(d) => {
            title = title_or_key(&d.title, &open.key);
            match item.filter(|i| i.in_list) {
                Some(i) => {
                    rows.push(row("Status", i.status.clone()));
                    rows.push(row(
                        "Priority",
                        priority(i.priority.as_ref().or(d.priority.as_ref())),
                    ));
                    if let Some(stage) = i.stage {
                        rows.push(row("Stage", stage_text(stage, i.stage_since, now)));
                    }
                    pr_rows(&mut rows);
                }
                None => {
                    rows.push(row("Status", d.status.clone()));
                    rows.push(row("Priority", priority(d.priority.as_ref())));
                    if let Some(stage) = d.stage {
                        rows.push(row("Stage", stage_text(stage, d.stage_since, now)));
                    }
                    rows.extend(d.prs.iter().map(|p| row("PR", pr_text(p))));
                }
            }
            if d.kind == ItemKind::Bead {
                if let Some(n) = &d.notes {
                    rows.push(row("Notes", n.clone()));
                }
                if !d.depends_on.is_empty() {
                    rows.push(row("Depends on", d.depends_on.join(", ")));
                }
            }
            if let Some(f) = &d.folder {
                rows.push(row("Folder", tilde(f, home)));
            }
            if open.refreshing {
                rows.push(row("Fetched", "Refreshing...".into()));
            } else if let Some(t) = d.fetched_at {
                rows.push(row(
                    "Fetched",
                    format!("{} ago", crate::sprint_view::age_text(now - t)),
                ));
            }
            let text = [d.body.as_deref(), d.overview.as_deref()]
                .into_iter()
                .flatten()
                .find(|t| !t.trim().is_empty());
            match text {
                Some(text) => body = body_lines(text),
                None if d.error.is_none() => body = vec![BodyLine::Text("No description".into())],
                None => {}
            }
            notice = d.error.clone();
            children = d
                .children
                .iter()
                .map(|c| format!("{} {} ({})", c.key, c.title.trim(), c.status))
                .collect();
        }
        DetailState::Loading | DetailState::Failed(_) => {
            if let DetailState::Failed(e) = &open.state {
                notice = Some(e.clone());
            }
            title = item
                .map(|i| title_or_key(&i.title, &open.key))
                .unwrap_or_else(|| open.key.clone());
            if let Some(i) = item {
                rows.push(row("Status", i.status.clone()));
                rows.push(row("Priority", priority(i.priority.as_ref())));
                if let Some(stage) = i.stage {
                    rows.push(row("Stage", stage_text(stage, i.stage_since, now)));
                }
                pr_rows(&mut rows);
                if let Some(f) = &i.folder {
                    rows.push(row("Folder", tilde(f, home)));
                }
            }
            children = view_children(view, &open.key);
        }
    }
    let expandable = body.len() > BODY_PREVIEW_LINES;
    let truncated = !open.show_all && expandable;
    if truncated {
        body.truncate(BODY_PREVIEW_LINES);
    }
    DetailModel {
        key: open.key.clone(),
        title,
        rows,
        body,
        truncated,
        expandable,
        notice,
        children,
        loading: open.state == DetailState::Loading,
    }
}

/// Whether `open.slot` still owns the item; the detail closes otherwise.
pub fn detail_still_owned(open: &OpenDetail, view: &WorkView) -> bool {
    view.items
        .iter()
        .any(|i| i.key == open.key && i.owner == Some(open.slot))
}

/// Whether a detail request may go to the daemon now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetailGate {
    Send,
    /// The daemon's build is being asked for; hold the request until then.
    Wait,
    /// Not sent, with why.
    Refuse(String),
}

/// Whether a detail request may be sent: an older build cannot decode
/// `GetItemDetail` and hangs up on it, which drops the app's whole
/// connection, so any build mismatch refuses. While the build probe runs the
/// request waits for it; a daemon that gave no answer is sent it.
pub fn detail_gate(daemon: &DaemonKnown, probing: bool, app_build: &str) -> DetailGate {
    match daemon {
        DaemonKnown::Unknown if probing => DetailGate::Wait,
        DaemonKnown::Unknown => DetailGate::Send,
        DaemonKnown::Old => DetailGate::Refuse(
            "The daemon runs an older build and cannot fetch details. Restart the daemon to finish the upgrade (overseer ctl restart, or Settings)."
                .to_string(),
        ),
        DaemonKnown::Info(info) => {
            match overseer_proto::build_notice(app_build, &info.build) {
                None => DetailGate::Send,
                Some(why) => DetailGate::Refuse(format!("Cannot fetch details: {why}")),
            }
        }
    }
}

/// Puts a reply for request `req` of `key` into the open detail. A reply for
/// another key, or an older request for the same key (one that was closed,
/// reopened or refreshed while it ran), is dropped.
pub fn take_reply(
    open: &mut Option<OpenDetail>,
    key: &str,
    req: u64,
    resp: Result<ItemDetail, String>,
) {
    if let Some(o) = open.as_mut().filter(|o| o.key == key && o.req == req) {
        match resp {
            Ok(d) => {
                o.refreshing = false;
                o.state = DetailState::Loaded(d);
            }
            Err(e) => fail_detail(o, e),
        }
    }
}

/// Says why the open detail could not be fetched. A loaded detail keeps its
/// text and shows `why` as its notice, so a failed or refused refresh never
/// leaves the panel emptier than before; anything else is `Failed`.
pub fn fail_detail(open: &mut OpenDetail, why: String) {
    open.refreshing = false;
    open.state = match std::mem::replace(&mut open.state, DetailState::Loading) {
        DetailState::Loaded(mut prev) => {
            prev.error = Some(why);
            DetailState::Loaded(prev)
        }
        _ => DetailState::Failed(why),
    };
}

/// One line of the Holds box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoldRow {
    pub target: String,
    /// `command`, `grant`, `claim` or `pin`.
    pub kind: String,
    /// Local clock time the hold began.
    pub since: String,
    /// `for <item or reason>` and `at <revision>`, as `ctl locks` words them,
    /// or empty.
    pub detail: String,
    /// Unused for longer than the long-held limit.
    pub flagged: bool,
    /// The slots queued for the target, first in line first.
    pub waiters: Vec<SlotId>,
}

/// The targets `slot` holds, in the daemon's order.
pub fn holds_rows(
    view: &LockView,
    slot: SlotId,
    now: i64,
    clock: impl Fn(i64) -> String,
) -> Vec<HoldRow> {
    view.claims
        .iter()
        .filter(|c| c.slot == slot)
        .map(|c| HoldRow {
            target: c.target.clone(),
            kind: match c.kind {
                ClaimKind::Command => "command",
                ClaimKind::Grant => "grant",
                ClaimKind::Claim => "claim",
                ClaimKind::Pin => "pin",
            }
            .into(),
            since: clock(c.since),
            detail: {
                let mut parts = Vec::new();
                if let Some(why) = c.item.as_deref().or(c.reason.as_deref()) {
                    parts.push(format!("for {why}"));
                }
                if let Some(rev) = &c.pin_revision {
                    parts.push(format!("at {rev}"));
                }
                parts.join("  ")
            },
            flagged: view.flagged(c, now),
            waiters: view
                .waits
                .iter()
                .filter(|w| w.target == c.target)
                .map(|w| w.slot)
                .collect(),
        })
        .collect()
}

/// `waiting on argo:uat/root, number 2` for each target `slot` is queued on.
/// The number is the 1-based place in that target's queue.
pub fn waits_rows(view: &LockView, slot: SlotId) -> Vec<String> {
    view.waits
        .iter()
        .filter(|w| w.slot == slot)
        .map(|mine| {
            let place = view
                .waits
                .iter()
                .filter(|w| w.target == mine.target)
                .position(|w| w.slot == mine.slot)
                .map_or(1, |n| n + 1);
            format!("waiting on {}, number {place}", mine.target)
        })
        .collect()
}

fn sender_name(s: Sender) -> String {
    match s {
        Sender::Overseer => "Overseer".into(),
        Sender::You => "you".into(),
        Sender::Slot { slot } => slot_name(slot),
    }
}

/// One row of a relay list: the line shown, and the relay's id while it is
/// pending, for its Drop button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayRow {
    pub line: String,
    pub pending: Option<i64>,
}

/// How long a pending relay has waited at `now`: `pending 32m`.
pub fn pending_age(r: &overseer_proto::Relay, now: i64) -> String {
    format!("pending {}", crate::sprint_view::age_text(now - r.at))
}

/// The relays to or from `slot`, newest first (by delivery time when
/// delivered, else creation time, ties by id): `14:05 from S0: <text>`,
/// `14:06 to S0: <text>`. A pending one ends `(pending 32m: <reason>)`, a
/// dropped one `(for S3, dropped: <reason>)`.
///
/// Only the current session's relays are listed: those it received or sent
/// since `start` (`CoordView::session_start`), and any still pending. With
/// no session (`None`) only pending ones are.
pub fn relay_rows(
    view: &LockView,
    slot: SlotId,
    start: Option<i64>,
    now: i64,
    clock: impl Fn(i64) -> String,
) -> Vec<RelayRow> {
    let when = |r: &overseer_proto::Relay| r.delivered_at.or(r.dropped_at).unwrap_or(r.at);
    let current = |r: &overseer_proto::Relay| {
        // A relay this slot sent belongs to the session that sent it; one
        // it received to the session it reached.
        let at = if r.to == slot { when(r) } else { r.at };
        r.state == RelayState::Pending || start.is_some_and(|s| at >= s)
    };
    let mut relays: Vec<_> = view
        .relays
        .iter()
        .filter(|r| r.to == slot || r.from == (Sender::Slot { slot }))
        .filter(|r| current(r))
        .collect();
    relays.sort_by_key(|r| std::cmp::Reverse((when(r), r.id)));
    relays
        .into_iter()
        .map(|r| {
            let time = clock(when(r));
            let who = if r.to == slot {
                format!("from {}", sender_name(r.from))
            } else {
                format!("to {}", slot_name(r.to))
            };
            let mut line = format!("{time} {who}: {}", one_line(&r.text, 80));
            match (r.state, &r.held) {
                (RelayState::Pending, Some(why)) => {
                    line.push_str(&format!(" ({}: {why})", pending_age(r, now)))
                }
                (RelayState::Pending, None) => {
                    line.push_str(&format!(" ({})", pending_age(r, now)))
                }
                (RelayState::Dropped, _) => line.push_str(&format!(
                    " (for {}, {})",
                    slot_name(r.to),
                    r.drop_text().unwrap_or_default()
                )),
                (RelayState::Delivered, _) => {}
            }
            RelayRow {
                line,
                pending: (r.state == RelayState::Pending).then_some(r.id),
            }
        })
        .collect()
}

/// What an inline entry row in the panel is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    /// `Release S1's <target>? Enter / Esc`; Up and Down pick among
    /// `targets`, each with the holder Jonathan was shown.
    Release {
        targets: Vec<(String, SlotId)>,
        picked: usize,
    },
    /// One line: `<target> [reason]`.
    Claim,
    /// One line of text for the slot's session.
    Message,
}

/// What a key press in an entry row means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryAction {
    Redraw,
    Cancel,
    Submit,
    Paste,
    Ignored,
}

/// The open entry row of a slot's panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelEntry {
    pub slot: SlotId,
    pub kind: EntryKind,
    pub field: TextField,
    /// Why the last Enter did not send anything.
    pub error: Option<String>,
}

impl PanelEntry {
    fn new(slot: SlotId, kind: EntryKind) -> Self {
        PanelEntry {
            slot,
            kind,
            field: TextField::default(),
            error: None,
        }
    }

    /// The release confirm for `targets[picked]`, which `slot` holds. None
    /// without targets.
    pub fn release(slot: SlotId, targets: Vec<String>, picked: usize) -> Option<Self> {
        let picked = picked.min(targets.len().checked_sub(1)?);
        let targets = targets.into_iter().map(|t| (t, slot)).collect();
        Some(Self::new(slot, EntryKind::Release { targets, picked }))
    }

    pub fn claim(slot: SlotId) -> Self {
        Self::new(slot, EntryKind::Claim)
    }

    pub fn message(slot: SlotId) -> Self {
        Self::new(slot, EntryKind::Message)
    }

    /// The text before the field, or the whole question for a release.
    pub fn prompt(&self) -> String {
        match &self.kind {
            EntryKind::Release { targets, picked } => {
                let (target, holder) = &targets[*picked];
                format!("Release {}'s {target}? Enter / Esc", slot_name(*holder))
            }
            EntryKind::Claim => "Claim".into(),
            EntryKind::Message => format!("Message {}", slot_name(self.slot)),
        }
    }

    pub fn key(&mut self, ks: &Keystroke) -> EntryAction {
        let m = &ks.modifiers;
        if let EntryKind::Release { targets, picked } = &mut self.kind {
            let plain = !(m.platform || m.control || m.alt || m.shift || m.function);
            return match ks.key.as_str() {
                "escape" => EntryAction::Cancel,
                "enter" => EntryAction::Submit,
                "up" if plain => {
                    *picked = picked.saturating_sub(1);
                    EntryAction::Redraw
                }
                "down" if plain => {
                    *picked = (*picked + 1).min(targets.len() - 1);
                    EntryAction::Redraw
                }
                _ => EntryAction::Ignored,
            };
        }
        self.error = None;
        match edit_key(&mut self.field, ks) {
            DialogKey::Edited => EntryAction::Redraw,
            DialogKey::Submit => EntryAction::Submit,
            DialogKey::Cancel => EntryAction::Cancel,
            DialogKey::Paste => EntryAction::Paste,
            DialogKey::NextField | DialogKey::PrevField | DialogKey::Ignored => {
                EntryAction::Ignored
            }
        }
    }

    pub fn paste(&mut self, text: &str) {
        if !matches!(self.kind, EntryKind::Release { .. }) {
            self.error = None;
            self.field.insert(text);
        }
    }

    /// The request Enter sends. A release is for Jonathan acting for the
    /// holder, so it is a force release, and only of the holder he was
    /// shown; a claim is for this slot; a message is from Jonathan
    /// (`from: None`).
    pub fn request(&self) -> Result<Request, String> {
        let text = self.field.text.trim();
        match &self.kind {
            EntryKind::Release { targets, picked } => {
                let (target, holder) = &targets[*picked];
                Ok(Request::ForceRelease {
                    target: target.clone(),
                    holder: Some(*holder),
                })
            }
            EntryKind::Claim => {
                let (target, reason) = match text.split_once(char::is_whitespace) {
                    Some((t, r)) => (t, r.trim()),
                    None => (text, ""),
                };
                if target.is_empty() {
                    return Err("Enter a target, then an optional reason".into());
                }
                Ok(Request::ClaimTarget {
                    slot: self.slot,
                    target: target.into(),
                    reason: (!reason.is_empty()).then(|| reason.into()),
                    item: None,
                })
            }
            EntryKind::Message => {
                if text.is_empty() {
                    return Err("Type a message first".into());
                }
                Ok(Request::SendRelay {
                    from: None,
                    to: self.slot,
                    text: text.into(),
                })
            }
        }
    }
}

/// The inline line under an entry row for the daemon's answer, if it has
/// anything to say (`Queued` and `Error` always do).
pub fn reply_note(reply: &Result<Response, String>) -> Option<String> {
    match reply {
        Ok(Response::Error { message }) | Ok(Response::Queued { message }) => Some(message.clone()),
        Ok(Response::Text { text }) => Some(text.clone()),
        Err(e) => Some(e.clone()),
        Ok(_) => None,
    }
}

/// The notices addressed to `slot`.
pub fn slot_notices(view: &WorkView, slot: SlotId) -> Vec<&Notice> {
    view.notices.iter().filter(|n| n.slot == slot).collect()
}

/// Send kickoff types into the session, so only while it waits for input.
pub fn can_kickoff(info: &SlotInfo) -> bool {
    matches!(info.state, SlotState::Running { .. })
        && info
            .activity
            .as_ref()
            .is_some_and(|a| a.kind == ActivityKind::Idle)
}

/// Send kickoff is on offer: the session is idle and no kickoff for it is
/// already on its way (a double click must not type it twice).
pub fn kickoff_enabled(info: &SlotInfo, pending: bool) -> bool {
    can_kickoff(info) && !pending
}

/// The kickoff text as the terminal would get it from a Cmd+V paste: one
/// bracketed paste when the session turned that mode on, newlines as CR.
/// Return is sent on its own afterwards.
pub fn kickoff_bytes(text: &str, modes: &overseer_term::TermModes) -> Vec<u8> {
    overseer_term::encode_paste(text, modes)
}

/// Identifies one run of a slot: the pid and start time while running.
pub type Incarnation = Option<(u32, i64)>;

pub fn incarnation(state: &SlotState) -> Incarnation {
    match state {
        SlotState::Running { pid, since } => Some((*pid, *since)),
        _ => None,
    }
}

/// Whether a `TerminalView` made for run `view` still shows the slot in
/// `state`. An exit keeps it, so the final screen stays readable; a new run
/// (a restart) or an empty slot drops it.
pub fn keep_view(view: Incarnation, state: &SlotState) -> bool {
    match state {
        // No process to show in either.
        SlotState::Empty | SlotState::Resumable { .. } => false,
        SlotState::Running { .. } => view == incarnation(state),
        SlotState::Exited { .. } => true,
    }
}

/// What a restart needs to start the slot again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartPlan {
    pub id: SlotId,
    pub label: String,
    pub cwd: PathBuf,
    pub command: Option<Vec<String>>,
}

impl RestartPlan {
    pub fn start_request(&self) -> Request {
        Request::StartSlot {
            id: self.id,
            label: self.label.clone(),
            cwd: self.cwd.clone(),
            command: self.command.clone(),
            prompt: None,
        }
    }
}

/// First step of a restart: the request to send now, and the plan to keep
/// while waiting for the slot to exit (`None` when it already has).
pub fn restart_begin(info: &SlotInfo) -> Option<(Request, Option<RestartPlan>)> {
    let plan = RestartPlan {
        id: info.id,
        label: info.label.clone(),
        cwd: info.cwd.clone()?,
        command: info.command.clone(),
    };
    match info.state {
        // Restarting would start fresh and lose the session; Resume is the
        // way back in.
        SlotState::Empty | SlotState::Resumable { .. } => None,
        SlotState::Running { .. } => Some((Request::StopSlot { id: info.id }, Some(plan))),
        SlotState::Exited { .. } => Some((plan.start_request(), None)),
    }
}

/// While a restart waits: the start request once the slot has exited.
pub fn restart_on_change(plan: &RestartPlan, info: &SlotInfo) -> Option<Request> {
    (info.id == plan.id && matches!(info.state, SlotState::Exited { .. }))
        .then(|| plan.start_request())
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn button(
    id: impl Into<gpui::ElementId>,
    text: &'static str,
    enabled: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded(px(5.))
        .text_sm()
        .bg(rgb(colors::BUTTON))
        .border_1()
        .border_color(rgb(colors::BUTTON_BORDER))
        .text_color(rgb(if enabled {
            colors::ACCENT
        } else {
            colors::MUTED
        }))
        .when(enabled, |b| {
            b.cursor_pointer().hover(|s| s.bg(rgb(colors::CARD_HOVER)))
        })
        .child(text)
}

fn panel_box(title: &'static str) -> gpui::Div {
    // Kept at its full height: the panel scrolls instead of squeezing boxes.
    div()
        .flex_none()
        .flex()
        .flex_col()
        .gap_1()
        .p_2()
        .rounded(px(6.))
        .bg(rgb(colors::CARD))
        .border_1()
        .border_color(rgb(colors::BORDER))
        .child(
            div()
                .text_xs()
                .text_color(rgb(colors::MUTED))
                .child(title.to_uppercase()),
        )
}

/// How many relays the Relays box lists.
const RELAY_ROWS: usize = 6;

/// How many lines the Progress box lists.
const PROGRESS_ROWS: usize = 10;

/// `slot`'s reports and state changes, newest first, `max` of them:
/// `14:05 tests written`. Reports are `Tone::Running` (normal text), state
/// changes `Tone::Idle` (muted).
pub fn progress_rows(
    view: &CoordView,
    slot: SlotId,
    clock: impl Fn(i64) -> String,
    max: usize,
) -> Vec<(String, Tone)> {
    view.reports_for(slot)
        .take(max)
        .map(|r| {
            let tone = match r.kind {
                ReportKind::Report => Tone::Running,
                ReportKind::State => Tone::Idle,
            };
            (format!("{} {}", clock(r.at), one_line(&r.text, 120)), tone)
        })
        .collect()
}

impl OverseerApp {
    fn render_tabs(&self, current: SlotId, cx: &mut Context<Self>) -> impl IntoElement {
        let tabs: Vec<_> = self
            .slots
            .iter()
            .map(|info| {
                let id = info.id;
                let on = id == current;
                div()
                    .id(("tab", id as usize))
                    .px_2()
                    .py_1()
                    .rounded(px(5.))
                    .bg(rgb(colors::CARD))
                    .border_1()
                    .border_color(rgb(if on { colors::ACCENT } else { colors::BORDER }))
                    .text_color(rgb(if tab_needs_you(info) {
                        colors::ORANGE
                    } else if tab_your_turn(info) {
                        colors::ACCENT
                    } else {
                        colors::TEXT
                    }))
                    .cursor_pointer()
                    .child(tab_label(info))
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.go_session(id, window, cx)),
                    )
            })
            .collect();
        let focus = self.session_focus.clone();
        div()
            .flex()
            .items_center()
            .gap_2()
            .text_xs()
            .on_mouse_down(gpui::MouseButton::Left, move |_, window, _| {
                window.focus(&focus)
            })
            .children(tabs)
            .child(
                div()
                    .ml_auto()
                    .text_color(rgb(colors::MUTED))
                    .child("Cmd+0 to Cmd+6 switch · Cmd+Esc close"),
            )
    }

    fn render_panel(&self, id: SlotId, now: i64, cx: &mut Context<Self>) -> impl IntoElement {
        let info = &self.slots[id as usize];
        let restarting = self.restarting(id);
        let m = panel_model(info, now, self.home.as_deref(), restarting, local_clock);
        let rows: Vec<_> = m
            .rows
            .into_iter()
            .map(|(k, v)| {
                div()
                    .flex()
                    .gap_2()
                    .text_sm()
                    .child(div().w(px(64.)).text_color(rgb(colors::MUTED)).child(k))
                    .child(div().flex_1().min_w(px(0.)).child(v))
            })
            .collect();
        let stop = button("stop", "Stop", m.can_stop).when(m.can_stop, |b| {
            b.on_click(cx.listener(move |this, _, _, cx| this.stop_slot(id, cx)))
        });
        let restart = button("restart", "Restart", m.can_restart).when(m.can_restart, |b| {
            b.on_click(cx.listener(move |this, _, _, cx| this.restart_slot(id, cx)))
        });
        let start_new =
            button("start-new", "Start\u{2026}", m.can_start_new).when(m.can_start_new, |b| {
                b.on_click(cx.listener(move |this, _, window, cx| this.open_dialog(id, window, cx)))
            });
        let resume = button("resume", "Resume", m.can_resume).when(m.can_resume, |b| {
            b.on_click(cx.listener(move |this, _, window, cx| this.resume_slot(id, window, cx)))
        });
        // Resume takes the place of Restart and Start while it is on offer.
        let buttons = if m.can_resume {
            div().flex().gap_2().pt_1().child(stop).child(resume)
        } else {
            div()
                .flex()
                .gap_2()
                .pt_1()
                .child(stop)
                .child(restart)
                .child(start_new)
        };
        // A click in the panel keeps the keyboard with an open entry row.
        let focus = match self.entry {
            Some(_) => self.entry_focus.clone(),
            None => self.session_focus.clone(),
        };
        // Scrolls as a whole, so a long Relays box does not hide Progress.
        div()
            .id(("session-panel", usize::from(id)))
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_2()
            .on_mouse_down(gpui::MouseButton::Left, move |_, window, _| {
                window.focus(&focus)
            })
            .map(|panel| match self.render_detail(id, now, cx) {
                // The detail takes the place of every box.
                Some(detail) => panel.child(detail),
                None => panel
                    .child(
                        panel_box("Session")
                            .child(div().font_weight(FontWeight::SEMIBOLD).child(m.title))
                            .children(rows)
                            .children(note_line(info, self.brain.brain_dir.as_deref()).map(
                                |text| {
                                    let missing = info.session_note.is_none();
                                    div()
                                        .flex()
                                        .gap_2()
                                        .text_sm()
                                        .child(
                                            div()
                                                .w(px(64.))
                                                .text_color(rgb(colors::MUTED))
                                                .child("Note"),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w(px(0.))
                                                .truncate()
                                                .when(missing, |d| {
                                                    d.text_color(rgb(colors::TEXT_DIM))
                                                })
                                                .child(text),
                                        )
                                },
                            ))
                            .child(buttons),
                    )
                    .child(self.render_owns(id, cx))
                    .child(self.render_holds(id, now, cx))
                    .child(self.render_relays(id, cx))
                    .child(self.render_progress(id)),
            })
            .children(
                self.status
                    .clone()
                    .map(|s| div().text_sm().text_color(rgb(colors::ORANGE)).child(s)),
            )
    }

    /// The open detail of an item `id` owns, in place of the panel's boxes.
    fn render_detail(&self, id: SlotId, now: i64, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let open = self.detail.as_ref().filter(|d| d.slot == id)?;
        let m = detail_model(open, &self.work, now, self.home.as_deref());
        let refresh_key = m.key.clone();
        let show_all = open.show_all;
        let bar = div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                button("detail-back", "Back", true)
                    .on_click(cx.listener(|this, _, _, cx| this.close_detail(cx))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(m.key.clone()),
            )
            .child(
                button("detail-refresh", "Refresh", true).on_click(cx.listener(
                    move |this, _, window, cx| {
                        this.open_detail(id, refresh_key.clone(), true, window, cx)
                    },
                )),
            );
        let rows: Vec<_> = m
            .rows
            .into_iter()
            .map(|(k, v)| {
                div()
                    .flex()
                    .gap_2()
                    .text_sm()
                    .child(
                        div()
                            .flex_none()
                            .w(px(72.))
                            .text_color(rgb(colors::MUTED))
                            .child(k),
                    )
                    .child(div().flex_1().min_w(px(0.)).child(v))
            })
            .collect();
        let font = self.theme.font_family.clone();
        let body: Vec<_> = m
            .body
            .into_iter()
            .map(|line| match line {
                BodyLine::Heading(t) => div().pt_1().font_weight(FontWeight::SEMIBOLD).child(t),
                BodyLine::Bullet { depth, text } => {
                    let mark = if is_numbered(&text) { "" } else { "\u{2022} " };
                    div()
                        .pl(px(bullet_indent(depth)))
                        .child(format!("{mark}{text}"))
                }
                BodyLine::Code(t) => div()
                    .px_1()
                    .bg(rgb(colors::FIELD))
                    .font_family(font.clone())
                    .text_xs()
                    .child(if t.is_empty() { " ".to_string() } else { t }),
                BodyLine::Text(t) => div().child(t),
                BodyLine::Blank => div().h(px(6.)),
            })
            .collect();
        let toggle = m.expandable.then(|| {
            button(
                "detail-all",
                if show_all { "Show less" } else { "Show all" },
                true,
            )
            .on_click(cx.listener(|this, _, _, cx| this.toggle_detail_all(cx)))
        });
        let children = (!m.children.is_empty()).then(|| {
            panel_box("Child beads")
                .children(m.children.into_iter().map(|c| div().text_sm().child(c)))
        });
        Some(
            div()
                .flex_none()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .p_2()
                        .rounded(px(6.))
                        .bg(rgb(colors::CARD))
                        .border_1()
                        .border_color(rgb(colors::BORDER))
                        .child(bar)
                        .child(div().font_weight(FontWeight::SEMIBOLD).child(m.title))
                        .children(rows)
                        .children(
                            m.notice
                                .map(|n| div().text_sm().text_color(rgb(colors::ORANGE)).child(n)),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .pt_1()
                                .text_sm()
                                .children(m.loading.then(|| {
                                    div()
                                        .text_color(rgb(colors::MUTED))
                                        .child("Loading description...")
                                }))
                                .children(body),
                        )
                        .children(toggle.map(|t| div().flex().child(t))),
                )
                .children(children),
        )
    }

    fn render_owns(&self, id: SlotId, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<_> = owns_rows(&self.work, id)
            .into_iter()
            .enumerate()
            .map(|(n, row)| {
                let release_key = row.key.clone();
                let open_key = row.key.clone();
                div()
                    .id(("own", n))
                    .flex()
                    .items_center()
                    .gap_2()
                    .p_1()
                    .rounded(px(4.))
                    .text_sm()
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(colors::CARD_HOVER)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_detail(id, open_key.clone(), false, window, cx)
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .flex()
                                    .items_baseline()
                                    .gap_2()
                                    .child(
                                        div()
                                            .flex_none()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(row.key),
                                    )
                                    .child(
                                        div()
                                            .min_w(px(0.))
                                            .truncate()
                                            .text_xs()
                                            .text_color(rgb(colors::MUTED))
                                            .child(row.status),
                                    ),
                            )
                            .child(div().line_clamp(2).child(row.title))
                            .children(row.gone.then(|| {
                                if row.not_read_yet {
                                    div()
                                        .text_xs()
                                        .text_color(rgb(colors::MUTED))
                                        .child("not read yet")
                                } else {
                                    div()
                                        .text_xs()
                                        .text_color(rgb(colors::ORANGE))
                                        .child("not in the current list")
                                }
                            })),
                    )
                    .child(
                        button(("release", n), "Release", true)
                            .flex_none()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                // The row behind opens the detail; this click
                                // must not reach it.
                                cx.stop_propagation();
                                this.release_item(release_key.clone(), cx)
                            })),
                    )
            })
            .collect();
        let notices: Vec<_> = slot_notices(&self.work, id)
            .into_iter()
            .map(|n| {
                div()
                    .text_sm()
                    .text_color(rgb(colors::ORANGE))
                    .child(format!("{} {}", crate::board::local_hhmm(n.at), n.text))
            })
            .collect();
        let has_notices = !notices.is_empty();
        let empty = rows.is_empty();
        let kickoff = kickoff_enabled(&self.slots[id as usize], self.kickoff_pending(id));
        panel_box("Owns")
            .children(empty.then(|| {
                div()
                    .text_sm()
                    .text_color(rgb(colors::TEXT_DIM))
                    .child("Nothing assigned")
            }))
            .children(rows)
            .children(notices)
            .child(
                div()
                    .flex()
                    .gap_2()
                    .pt_1()
                    .child(button("assign", "Assign\u{2026}", true).on_click(
                        cx.listener(move |this, _, window, cx| this.open_picker(id, window, cx)),
                    ))
                    .child(
                        button("kickoff", "Send kickoff", kickoff).when(kickoff, |b| {
                            b.on_click(cx.listener(move |this, _, _, cx| this.send_kickoff(id, cx)))
                        }),
                    )
                    .children(has_notices.then(|| {
                        button("clear-notices", "Clear", true)
                            .on_click(cx.listener(move |this, _, _, cx| this.clear_notices(id, cx)))
                    })),
            )
    }

    /// The open entry row for `slot`, if it belongs in the box that asks
    /// (`holds`: Holds box, otherwise Relays box).
    fn render_entry(
        &self,
        slot: SlotId,
        holds: bool,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Stateful<gpui::Div>> {
        let entry = self
            .entry
            .as_ref()
            .filter(|e| e.slot == slot && !matches!(e.kind, EntryKind::Message) == holds)?;
        let focused = self.entry_focus.clone();
        let shown = match entry.kind {
            EntryKind::Release { .. } => None,
            _ => Some(entry.field.display(true)),
        };
        let hint = matches!(entry.kind, EntryKind::Claim) && entry.field.text.is_empty();
        Some(
            div()
                .id("panel-entry")
                .key_context("PanelEntry")
                .track_focus(&self.entry_focus)
                .on_key_down(cx.listener(|this, ev, window, cx| this.on_entry_key(ev, window, cx)))
                .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
                    window.focus(&focused);
                    cx.stop_propagation();
                })
                .flex()
                .flex_col()
                .gap_1()
                .p_1()
                .rounded(px(4.))
                .bg(rgb(colors::FIELD))
                .border_1()
                .border_color(rgb(colors::ACCENT))
                .text_sm()
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(div().text_color(rgb(colors::ACCENT)).child(entry.prompt()))
                        .children(shown.map(|t| div().flex_1().min_w(px(0.)).child(t)))
                        .children(hint.then(|| {
                            div()
                                .text_color(rgb(colors::MUTED))
                                .child("<target> [reason]  Enter / Esc")
                        })),
                )
                .children(
                    entry
                        .error
                        .clone()
                        .map(|e| div().text_color(rgb(colors::ORANGE)).child(e)),
                ),
        )
    }

    fn render_note(&self, slot: SlotId, holds: bool) -> Option<gpui::Div> {
        self.entry_note
            .as_ref()
            .filter(|(s, h, _)| *s == slot && *h == holds)
            .map(|(_, _, text)| {
                div()
                    .text_sm()
                    .text_color(rgb(colors::ORANGE))
                    .child(text.clone())
            })
    }

    fn render_holds(&self, id: SlotId, now: i64, cx: &mut Context<Self>) -> impl IntoElement {
        let holds = holds_rows(&self.locks, id, now, local_hhmm);
        let empty = holds.is_empty();
        let rows: Vec<_> = holds
            .into_iter()
            .enumerate()
            .map(|(n, h)| {
                let queued = if h.waiters.is_empty() {
                    None
                } else {
                    let names: Vec<_> = h.waiters.iter().map(|w| slot_name(*w)).collect();
                    Some(format!("waiting: {}", names.join(", ")))
                };
                // The target and its time on the first line, never cut; what
                // it is for, the flag and the queue on the second, which may
                // be cut at the right.
                let detail: Vec<String> = [Some(h.detail), queued]
                    .into_iter()
                    .flatten()
                    .filter(|t| !t.is_empty())
                    .collect();
                let second = (h.flagged || !detail.is_empty()).then(|| {
                    div()
                        .flex()
                        .gap_2()
                        .min_w(px(0.))
                        .children(h.flagged.then(|| {
                            div()
                                .flex_none()
                                .text_color(rgb(colors::ORANGE))
                                .child("held long")
                        }))
                        .children((!detail.is_empty()).then(|| {
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .truncate()
                                .text_color(rgb(colors::TEXT_DIM))
                                .child(detail.join("  "))
                        }))
                });
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_sm()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .min_w(px(0.))
                                    .child(
                                        div()
                                            .flex_shrink()
                                            .min_w(px(0.))
                                            .truncate()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(h.target),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_color(rgb(colors::MUTED))
                                            .child(format!("{} since {}", h.kind, h.since)),
                                    ),
                            )
                            .children(second),
                    )
                    .child(
                        button(("release-hold", n), "Release", true)
                            .flex_none()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_release(id, n, window, cx)
                            })),
                    )
            })
            .collect();
        let waits: Vec<_> = waits_rows(&self.locks, id)
            .into_iter()
            .map(|w| div().text_sm().text_color(rgb(colors::ORANGE)).child(w))
            .collect();
        let none = empty && waits.is_empty();
        panel_box("Holds")
            .children(none.then(|| {
                div()
                    .text_sm()
                    .text_color(rgb(colors::TEXT_DIM))
                    .child("Nothing held")
            }))
            .children(rows)
            .children(waits)
            .children(self.render_entry(id, true, cx))
            .children(self.render_note(id, true))
            .child(div().flex().gap_2().pt_1().child(
                button("claim", "Claim\u{2026}", true).on_click(
                    cx.listener(move |this, _, window, cx| this.open_claim(id, window, cx)),
                ),
            ))
    }

    fn render_progress(&self, id: SlotId) -> impl IntoElement {
        let rows = progress_rows(&self.coord, id, local_hhmm, PROGRESS_ROWS);
        let empty = rows.is_empty();
        panel_box("Progress")
            .children(empty.then(|| {
                div()
                    .text_sm()
                    .text_color(rgb(colors::TEXT_DIM))
                    .child("Nothing reported yet")
            }))
            .children(rows.into_iter().map(|(line, tone)| {
                let color = match tone {
                    Tone::Idle => colors::TEXT_DIM,
                    _ => colors::TEXT,
                };
                div()
                    .text_sm()
                    .truncate()
                    .text_color(rgb(color))
                    .child(line)
            }))
    }

    fn render_relays(&self, id: SlotId, cx: &mut Context<Self>) -> impl IntoElement {
        let start = self.coord.session_start(&self.slots[id as usize]);
        let now = crate::app::now_secs();
        let rows: Vec<_> = relay_rows(&self.locks, id, start, now, local_hhmm)
            .into_iter()
            .take(RELAY_ROWS)
            .map(|row| {
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_sm()
                    .child(div().flex_1().min_w(px(0.)).truncate().child(row.line))
                    .children(row.pending.map(|relay| {
                        button(("drop-relay", relay as u64), "Drop", true)
                            .flex_none()
                            .on_click(cx.listener(move |this, _, _, cx| this.drop_relay(relay, cx)))
                    }))
            })
            .collect();
        let empty = rows.is_empty();
        panel_box("Relays")
            .children(empty.then(|| {
                div()
                    .text_sm()
                    .text_color(rgb(colors::TEXT_DIM))
                    .child("No relays")
            }))
            .children(rows)
            .children(self.render_entry(id, false, cx))
            .children(self.render_note(id, false))
            .child(div().flex().gap_2().pt_1().child(
                button("message", "Message\u{2026}", true).on_click(
                    cx.listener(move |this, _, window, cx| this.open_message(id, window, cx)),
                ),
            ))
    }

    /// The pop-over's top row: slot, ticket, state and the close button.
    fn render_popover_header(&self, id: SlotId, cx: &mut Context<Self>) -> impl IntoElement {
        let info = &self.slots[id as usize];
        let title = popover_title(info, &self.work);
        let state = state_text(info, self.restarting(id), local_clock);
        let close = div()
            .id("popover-close")
            .px_2()
            .rounded(px(5.))
            .text_color(rgb(colors::TEXT_DIM))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(colors::CARD_HOVER)).text_color(rgb(colors::TEXT)))
            .child("x")
            .on_click(cx.listener(|this, _, window, cx| this.go_board(window, cx)));
        div()
            .flex()
            .items_center()
            .gap_3()
            .child(
                div()
                    .flex_shrink()
                    .min_w(px(0.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(title),
            )
            .child(
                div()
                    .flex_none()
                    .ml_3()
                    .text_sm()
                    .text_color(rgb(colors::MUTED))
                    .whitespace_nowrap()
                    .child(state),
            )
            .child(div().ml_auto().child(close))
    }

    /// Slot `id`'s session over the board: a scrim that closes the
    /// pop-over when clicked, and a centred panel holding the session.
    pub(crate) fn render_popover(
        &mut self,
        id: SlotId,
        now: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let session = self.render_session(id, now, window, cx);
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba((colors::BG << 8) | 0xb3))
            .occlude()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, cx| this.go_board(window, cx)),
            )
            .child(
                div()
                    .w(relative(0.9))
                    .h(relative(0.9))
                    .rounded(px(8.))
                    .border_1()
                    .border_color(rgb(colors::BORDER))
                    .bg(rgb(colors::BG))
                    .overflow_hidden()
                    // A click inside the panel never reaches the scrim.
                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(session),
            )
    }

    pub(crate) fn render_session(
        &mut self,
        id: SlotId,
        now: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let view = self.ensure_terminal(id, window, cx);
        let left = match view {
            Some(view) => div()
                .w(relative(0.62))
                .h_full()
                .rounded(px(6.))
                .border_1()
                .border_color(rgb(colors::BORDER))
                .overflow_hidden()
                .child(view),
            None => div()
                .w(relative(0.62))
                .h_full()
                .rounded(px(6.))
                .border_1()
                .border_color(rgb(colors::BORDER))
                .bg(rgb(colors::TERM_EMPTY))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    div()
                        .text_color(rgb(colors::TEXT_DIM))
                        .child(SharedString::from(format!("{} is empty", slot_name(id)))),
                )
                .child(button("start-empty", "+ start", true).on_click(
                    cx.listener(move |this, _, window, cx| this.open_dialog(id, window, cx)),
                )),
        };
        let resumable = matches!(self.slots[id as usize].state, SlotState::Resumable { .. });
        let left = if resumable {
            // `left` above is the empty-slot pane; this slot has a session
            // to continue, not nothing to start.
            div()
                .w(relative(0.62))
                .h_full()
                .rounded(px(6.))
                .border_1()
                .border_color(rgb(colors::BORDER))
                .bg(rgb(colors::TERM_EMPTY))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    div()
                        .text_color(rgb(colors::TEXT_DIM))
                        .child(SharedString::from(format!(
                            "{} was running when the daemon stopped",
                            slot_name(id)
                        ))),
                )
                .child(button("resume-pane", "Resume", true).on_click(
                    cx.listener(move |this, _, window, cx| this.resume_slot(id, window, cx)),
                ))
        } else {
            left
        };
        let header = self.render_popover_header(id, cx);
        let tabs = self.render_tabs(id, cx);
        let panel = self.render_panel(id, now, cx);
        div()
            .key_context("Session")
            .track_focus(&self.session_focus)
            .on_action(
                cx.listener(|this, _: &OpenMessage, window, cx| this.open_message_key(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &OpenRelease, window, cx| this.open_release_key(window, cx)),
            )
            .on_action(cx.listener(|this, _: &OpenNote, _, _| this.open_note_key()))
            .size_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .child(header)
            .child(tabs)
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .gap_2()
                    .child(left)
                    .child(panel),
            )
    }
}

/// Local `HH:MM:SS` for unix seconds.
pub fn local_clock(t: i64) -> String {
    // SAFETY: localtime_r only writes the `tm` we pass it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let time: libc::time_t = t as libc::time_t;
    let ok = unsafe { !libc::localtime_r(&time, &mut tm).is_null() };
    if !ok {
        return "?".into();
    }
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(state: SlotState) -> SlotInfo {
        SlotInfo {
            id: 2,
            label: "magento".into(),
            cwd: Some("/Users/me/Repos/m".into()),
            command: Some(vec!["claude".into(), "--resume".into()]),
            state,
            cols: 80,
            rows: 24,
            activity: None,
            session_note: None,
        }
    }

    #[test]
    fn obsidian_url_encodes_spaces_and_unicode() {
        let url = obsidian_url(
            Path::new("/Users/me/My Brain"),
            "Sessions/2026/11-November/2026-11-13-s1-caf\u{e9} & co.md",
        );
        assert_eq!(
            url,
            "obsidian://open?path=/Users/me/My%20Brain/Sessions/2026/11-November/2026-11-13-s1-caf%C3%A9%20%26%20co.md"
        );
        // Unreserved characters stay as they are.
        assert_eq!(
            obsidian_url(Path::new("/b"), "a-b_c.d~e/f.md"),
            "obsidian://open?path=/b/a-b_c.d~e/f.md"
        );
    }

    #[test]
    fn the_note_key_needs_a_note_and_a_vault() {
        let mut i = info(RUN);
        assert_eq!(
            note_line(&i, Some(Path::new("/b"))).as_deref(),
            Some("No session note yet")
        );
        assert_eq!(note_target(&i, Some(Path::new("/b"))), None);
        i.session_note = Some("Sessions/s2.md".into());
        assert_eq!(
            note_line(&i, Some(Path::new("/b"))).as_deref(),
            Some("Sessions/s2.md (press n to open)")
        );
        assert_eq!(note_target(&i, None), None);
        // No vault known: the path alone, with no key to press.
        assert_eq!(note_line(&i, None).as_deref(), Some("Sessions/s2.md"));
        assert_eq!(
            note_target(&i, Some(Path::new("/b"))).as_deref(),
            Some("obsidian://open?path=/b/Sessions/s2.md")
        );
        i.state = SlotState::Empty;
        i.session_note = None;
        assert_eq!(note_line(&i, Some(Path::new("/b"))), None);
    }

    fn clock(t: i64) -> String {
        format!("T{t}")
    }

    const RUN: SlotState = SlotState::Running {
        pid: 77,
        since: 100,
    };

    #[test]
    fn tabs() {
        assert_eq!(tab_label(&info(RUN)), "2 S2 ●");
        assert_eq!(tab_label(&info(SlotState::Empty)), "2 S2");
    }

    #[test]
    fn tab_is_orange_only_while_running_and_needing_you() {
        use overseer_proto::Activity;
        let with = |state, kind| SlotInfo {
            activity: Some(Activity {
                kind,
                since: 100,
                now_doing: None,
                reason: None,
                idle_wait: false,
            }),
            ..info(state)
        };
        assert!(tab_needs_you(&with(RUN, ActivityKind::NeedsYou)));
        assert!(!tab_needs_you(&with(RUN, ActivityKind::Working)));
        assert!(!tab_needs_you(&with(RUN, ActivityKind::Idle)));
        assert!(!tab_needs_you(&info(RUN)));
        assert!(tab_your_turn(&with(RUN, ActivityKind::Idle)));
        assert_eq!(tab_label(&with(RUN, ActivityKind::Idle)), "2 S2 ○");
        assert!(!tab_your_turn(&with(RUN, ActivityKind::Working)));
        let exited = SlotState::Exited {
            code: Some(0),
            at: 200,
        };
        assert!(!tab_needs_you(&with(
            exited.clone(),
            ActivityKind::NeedsYou
        )));
        assert!(!tab_your_turn(&with(exited, ActivityKind::Idle)));
    }

    #[test]
    fn panel_running() {
        let m = panel_model(&info(RUN), 165, Some(Path::new("/Users/me")), false, clock);
        assert_eq!(m.title, "S2 · magento");
        assert_eq!(
            m.rows,
            vec![
                ("Label", "magento".to_string()),
                ("Cwd", "~/Repos/m".to_string()),
                ("Pid", "77".to_string()),
                ("State", "Running".to_string()),
                ("Started", "T100 (1m 05s ago)".to_string()),
            ]
        );
        assert!(m.can_stop && m.can_restart && !m.can_start_new);
    }

    #[test]
    fn panel_resumable_offers_resume_not_restart() {
        let st = SlotState::Resumable {
            session_id: "abc".into(),
            at: 100,
        };
        let m = panel_model(&info(st.clone()), 165, None, false, clock);
        assert!(m.rows.contains(&("State", "Resumable".to_string())));
        assert!(m
            .rows
            .contains(&("Started", "T100 (1m 05s ago)".to_string())));
        assert!(m.can_resume && !m.can_stop && !m.can_restart && !m.can_start_new);
        assert!(restart_begin(&info(st.clone())).is_none());
        assert!(!keep_view(None, &st));
        assert!(!panel_model(&info(RUN), 165, None, false, clock).can_resume);
    }

    #[test]
    fn panel_shows_activity_and_reason() {
        use overseer_proto::{Activity, ActivityKind};
        let mut i = info(RUN);
        i.activity = Some(Activity {
            kind: ActivityKind::NeedsYou,
            since: 105,
            now_doing: Some("Bash".into()),
            reason: Some("Permission to run Bash".into()),
            idle_wait: false,
        });
        let m = panel_model(&i, 165, None, false, clock);
        assert_eq!(m.rows[3], ("State", "Running".to_string()));
        assert_eq!(m.rows[5], ("Activity", "Needs you 1m 00s".to_string()));
        assert_eq!(m.rows[6], ("Reason", "Permission to run Bash".to_string()));
        assert_eq!(m.rows.len(), 7);

        i.activity = Some(Activity {
            kind: ActivityKind::Working,
            since: 160,
            now_doing: Some("Edit".into()),
            reason: None,
            idle_wait: false,
        });
        let m = panel_model(&i, 165, None, false, clock);
        assert_eq!(m.rows[5], ("Activity", "Working 5s".to_string()));
        assert_eq!(m.rows[6], ("Doing", "Edit".to_string()));

        i.activity = Some(Activity {
            kind: ActivityKind::Idle,
            since: 160,
            now_doing: None,
            reason: None,
            idle_wait: false,
        });
        let m = panel_model(&i, 165, None, false, clock);
        assert_eq!(m.rows[5], ("Activity", "Your turn 5s".to_string()));
        assert_eq!(m.rows.len(), 6);
    }

    #[test]
    fn panel_exited_and_empty() {
        let ex = SlotState::Exited {
            code: Some(1),
            at: 50,
        };
        let m = panel_model(&info(ex), 60, None, false, clock);
        assert_eq!(m.rows[3].1, "Exited (code 1) at T50");
        assert_eq!(m.rows[2].1, "-");
        assert!(!m.can_stop && m.can_restart && m.can_start_new);

        let mut empty = info(SlotState::Empty);
        empty.cwd = None;
        empty.label.clear();
        let m = panel_model(&empty, 60, None, false, clock);
        assert_eq!(m.title, "S2");
        assert_eq!(m.rows[0].1, "-");
        assert!(!m.can_stop && !m.can_restart && !m.can_start_new);
    }

    #[test]
    fn panel_while_restarting_disables_buttons() {
        let m = panel_model(&info(RUN), 165, None, true, clock);
        assert_eq!(m.rows[3].1, "Running, restarting");
        assert!(!m.can_stop && !m.can_restart);
    }

    #[test]
    fn views_survive_exit_but_not_a_new_run() {
        let inc = incarnation(&RUN);
        assert_eq!(inc, Some((77, 100)));
        assert!(keep_view(inc, &RUN));
        let ex = SlotState::Exited { code: None, at: 9 };
        assert!(keep_view(inc, &ex));
        let again = SlotState::Running {
            pid: 78,
            since: 200,
        };
        assert!(!keep_view(inc, &again));
        assert!(!keep_view(inc, &SlotState::Empty));
        // A view made while exited is dropped when the slot runs again.
        assert!(keep_view(None, &ex));
        assert!(!keep_view(None, &again));
    }

    #[test]
    fn restart_running_stops_then_starts_same_spec() {
        let i = info(RUN);
        let (first, plan) = restart_begin(&i).unwrap();
        assert_eq!(first, Request::StopSlot { id: 2 });
        let plan = plan.unwrap();
        // Still running (or another slot changed): keep waiting.
        assert_eq!(restart_on_change(&plan, &i), None);
        let mut other = info(SlotState::Exited { code: None, at: 1 });
        other.id = 3;
        assert_eq!(restart_on_change(&plan, &other), None);
        let exited = info(SlotState::Exited { code: None, at: 1 });
        assert_eq!(
            restart_on_change(&plan, &exited),
            Some(Request::StartSlot {
                id: 2,
                label: "magento".into(),
                cwd: "/Users/me/Repos/m".into(),
                command: Some(vec!["claude".into(), "--resume".into()]),
                prompt: None,
            })
        );
    }

    #[test]
    fn restart_exited_starts_at_once_and_empty_cannot() {
        let (first, plan) = restart_begin(&info(SlotState::Exited {
            code: Some(0),
            at: 1,
        }))
        .unwrap();
        assert!(matches!(first, Request::StartSlot { id: 2, .. }));
        assert!(plan.is_none());
        assert!(restart_begin(&info(SlotState::Empty)).is_none());
    }

    #[test]
    fn owns_rows_mark_items_off_the_list() {
        use overseer_proto::{ItemKind, WorkItem};
        let item = |key: &str, kind, owner, in_list| WorkItem {
            key: key.into(),
            kind,
            title: format!("title of {key}"),
            status: "In Progress".into(),
            priority: None,
            parent: None,
            owner,
            in_list,
            folder: None,
            stage: None,
            stage_since: None,
        };
        let view = WorkView {
            notes_off: None,
            items: vec![
                item("ov-3", ItemKind::Bead, Some(2), true),
                item("DEMO-12", ItemKind::Ticket, Some(2), false),
                item("DEMO-7", ItemKind::Ticket, Some(4), true),
            ],
            sources: vec![],
            notices: vec![
                Notice {
                    slot: 2,
                    at: 5,
                    text: "DEMO-9 moved to S4".into(),
                },
                Notice {
                    slot: 4,
                    at: 6,
                    text: "other".into(),
                },
            ],
            sprint: None,
            prs: Vec::new(),
            sprint_day: None,
        };
        let rows = owns_rows(&view, 2);
        assert_eq!(
            rows.iter()
                .map(|r| (r.key.as_str(), r.gone))
                .collect::<Vec<_>>(),
            [("DEMO-12", true), ("ov-3", false)]
        );
        assert_eq!(rows[0].status, "In Progress");
        assert!(!rows[0].not_read_yet, "no source is waiting to be read");
        let notes = slot_notices(&view, 2);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].text, "DEMO-9 moved to S4");
    }

    #[test]
    fn state_text_matches_the_state_row() {
        let clock = |_| "14:05:12".to_string();
        let i = info(RUN);
        assert_eq!(state_text(&i, false, clock), "Running");
        assert_eq!(state_text(&i, true, clock), "Running, restarting");
        let m = panel_model(&i, 100, None, true, clock);
        let row = m.rows.iter().find(|(n, _)| *n == "State").unwrap();
        assert_eq!(row.1, state_text(&i, true, clock));
    }

    #[test]
    fn popover_title_names_the_slot_and_its_ticket() {
        use overseer_proto::{ItemKind, WorkItem};
        let item = |key: &str, kind, owner| WorkItem {
            key: key.into(),
            kind,
            title: format!("Title of {key}"),
            status: "In Progress".into(),
            priority: None,
            parent: None,
            owner,
            in_list: true,
            folder: None,
            stage: None,
            stage_since: None,
        };
        let view = WorkView {
            notes_off: None,
            items: vec![
                item("ov-3", ItemKind::Bead, Some(2)),
                item("DEMO-12", ItemKind::Ticket, Some(2)),
                item("DEMO-7", ItemKind::Ticket, Some(4)),
            ],
            sources: vec![],
            notices: vec![],
            sprint: None,
            prs: Vec::new(),
            sprint_day: None,
        };
        let mut i = info(RUN);
        assert_eq!(
            popover_title(&i, &view),
            "S2 \u{b7} DEMO-12 Title of DEMO-12"
        );
        // No ticket: the slot name alone.
        i.id = 1;
        assert_eq!(popover_title(&i, &view), "S1");
    }

    #[test]
    fn owns_rows_say_not_read_yet_before_the_first_read() {
        use overseer_proto::{ItemKind, SourceStatus, WorkItem};
        let view = WorkView {
            notes_off: None,
            items: vec![WorkItem {
                key: "ov-3".into(),
                kind: ItemKind::Bead,
                title: "ov-3".into(),
                status: "unknown".into(),
                priority: None,
                parent: None,
                owner: Some(2),
                in_list: false,
                folder: None,
                stage: None,
                stage_since: None,
            }],
            sources: vec![SourceStatus {
                name: "beads".into(),
                ok: false,
                count: 0,
                at: None,
                message: Some("not read yet".into()),
            }],
            notices: vec![],
            sprint: None,
            prs: Vec::new(),
            sprint_day: None,
        };
        let rows = owns_rows(&view, 2);
        assert!(rows[0].gone && rows[0].not_read_yet, "{rows:?}");
    }

    #[test]
    fn kickoff_only_when_idle() {
        use overseer_proto::Activity;
        let mut i = info(RUN);
        assert!(!can_kickoff(&i), "no activity reported");
        for (kind, ok) in [
            (ActivityKind::Idle, true),
            (ActivityKind::Working, false),
            (ActivityKind::NeedsYou, false),
        ] {
            i.activity = Some(Activity {
                kind,
                since: 1,
                now_doing: None,
                reason: None,
                idle_wait: false,
            });
            assert_eq!(can_kickoff(&i), ok);
        }
        i.state = SlotState::Exited { code: None, at: 2 };
        assert!(!can_kickoff(&i));
    }

    #[test]
    fn kickoff_disabled_while_one_is_in_flight() {
        use overseer_proto::Activity;
        let mut i = info(RUN);
        i.activity = Some(Activity {
            kind: ActivityKind::Idle,
            since: 1,
            now_doing: None,
            reason: None,
            idle_wait: false,
        });
        assert!(kickoff_enabled(&i, false));
        assert!(!kickoff_enabled(&i, true));
        i.activity = None;
        assert!(!kickoff_enabled(&i, false));
    }

    #[test]
    fn kickoff_text_is_pasted() {
        use overseer_term::TermModes;
        let text = "Work on DEMO-12\nand ov-3";
        let plain = kickoff_bytes(text, &TermModes::default());
        assert_eq!(plain, b"Work on DEMO-12\rand ov-3");
        let bracketed = TermModes {
            bracketed_paste: true,
            ..TermModes::default()
        };
        assert_eq!(
            kickoff_bytes(text, &bracketed),
            b"\x1b[200~Work on DEMO-12\rand ov-3\x1b[201~"
        );
    }

    #[test]
    fn started_format() {
        assert_eq!(format_started(10, 20, clock), "T10 (10s ago)");
        assert_eq!(local_clock(0).len(), 8);
    }
    fn claim(
        slot: SlotId,
        target: &str,
        kind: ClaimKind,
        since: i64,
        last_used: i64,
    ) -> overseer_proto::Claim {
        overseer_proto::Claim {
            target: target.into(),
            slot,
            kind,
            reason: None,
            item: None,
            since,
            last_used,
            pin_revision: None,
        }
    }

    fn wait(slot: SlotId, target: &str) -> overseer_proto::Wait {
        overseer_proto::Wait {
            target: target.into(),
            slot,
            at: 0,
            reason: None,
        }
    }

    fn relay(
        id: i64,
        from: Sender,
        to: SlotId,
        at: i64,
        delivered_at: Option<i64>,
        held: Option<&str>,
        text: &str,
    ) -> overseer_proto::Relay {
        overseer_proto::Relay {
            id,
            at,
            from,
            to,
            text: text.into(),
            state: if delivered_at.is_some() {
                RelayState::Delivered
            } else {
                RelayState::Pending
            },
            delivered_at,
            dropped_at: None,
            kind: overseer_proto::RelayKind::Note,
            drop_reason: None,
            held: held.map(Into::into),
        }
    }

    fn locks() -> LockView {
        crate::app::empty_locks()
    }

    fn lines(rows: &[RelayRow]) -> Vec<String> {
        rows.iter().map(|r| r.line.clone()).collect()
    }

    #[test]
    fn pending_age_counts_from_when_it_was_queued() {
        let r = relay(1, Sender::You, 2, 1000, None, None, "hi");
        assert_eq!(pending_age(&r, 1000 + 32 * 60), "pending 32m");
        assert_eq!(pending_age(&r, 1000 + 5 * 3600), "pending 5h");
        assert_eq!(pending_age(&r, 900), "pending 0m", "clock skew");
    }

    #[test]
    fn holds_rows_list_kind_since_and_waiters() {
        let mut v = locks();
        v.flag_after_secs = 100;
        v.claims = vec![
            overseer_proto::Claim {
                reason: Some("lane test".into()),
                item: Some("DEMO-12".into()),
                pin_revision: Some("abc1234".into()),
                ..claim(2, "argo:uat/root", ClaimKind::Pin, 10, 10)
            },
            claim(3, "repo:demo-api:main", ClaimKind::Command, 20, 190),
            overseer_proto::Claim {
                reason: Some("lane test".into()),
                ..claim(2, "tf:default", ClaimKind::Claim, 30, 30)
            },
        ];
        v.waits = vec![
            wait(4, "argo:uat/root"),
            wait(1, "repo:demo-api:main"),
            wait(5, "argo:uat/root"),
        ];
        let rows = holds_rows(&v, 2, 200, clock);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            HoldRow {
                target: "argo:uat/root".into(),
                kind: "pin".into(),
                since: "T10".into(),
                detail: "for DEMO-12  at abc1234".into(),
                flagged: true,
                waiters: vec![4, 5],
            }
        );
        assert_eq!(rows[1].kind, "claim");
        assert_eq!(rows[1].detail, "for lane test");
        assert!(rows[1].waiters.is_empty());
        let other = holds_rows(&v, 3, 200, clock);
        assert_eq!(other.len(), 1);
        assert!(!other[0].flagged);
        assert_eq!(other[0].kind, "command");
        assert_eq!(other[0].waiters, vec![1]);
        assert!(holds_rows(&v, 6, 200, clock).is_empty());
    }

    #[test]
    fn waits_rows_number_the_queue_place() {
        let mut v = locks();
        v.waits = vec![
            wait(4, "argo:uat/root"),
            wait(1, "repo:demo-api:main"),
            wait(5, "argo:uat/root"),
        ];
        assert_eq!(
            waits_rows(&v, 5),
            vec!["waiting on argo:uat/root, number 2".to_string()]
        );
        assert_eq!(
            waits_rows(&v, 4),
            vec!["waiting on argo:uat/root, number 1".to_string()]
        );
        assert!(waits_rows(&v, 0).is_empty());
    }

    #[test]
    fn relay_rows_both_directions_with_pending_reason() {
        let mut v = locks();
        let me = Sender::Slot { slot: 2 };
        v.relays = vec![
            relay(
                1,
                Sender::Slot { slot: 0 },
                2,
                100,
                Some(105),
                None,
                "argo is free",
            ),
            relay(2, me, 0, 106, Some(106), None, "done with DEMO-33"),
            relay(
                3,
                Sender::You,
                2,
                107,
                None,
                Some("session is working"),
                "line one\nline two",
            ),
            relay(4, Sender::Overseer, 2, 108, None, None, "moved"),
            // Neither to nor from this slot.
            relay(
                5,
                Sender::Slot { slot: 1 },
                3,
                109,
                Some(109),
                None,
                "other",
            ),
        ];
        let rows = relay_rows(&v, 2, Some(0), 108 + 32 * 60, clock);
        assert_eq!(
            lines(&rows),
            vec![
                "T108 from Overseer: moved (pending 32m)".to_string(),
                "T107 from you: line one line two (pending 32m: session is working)".to_string(),
                "T106 to S0: done with DEMO-33".to_string(),
                "T105 from S0: argo is free".to_string(),
            ]
        );
        // Only pending rows get a Drop button.
        let drops: Vec<_> = rows.iter().map(|r| r.pending).collect();
        assert_eq!(drops, vec![Some(4), Some(3), None, None]);
    }

    #[test]
    fn relay_rows_show_dropped_relays() {
        let mut v = locks();
        v.relays = vec![
            overseer_proto::Relay {
                state: RelayState::Dropped,
                dropped_at: Some(120),
                drop_reason: Some("dropped by S0".into()),
                ..relay(1, Sender::Slot { slot: 0 }, 2, 100, None, None, "status?")
            },
            overseer_proto::Relay {
                state: RelayState::Dropped,
                dropped_at: Some(110),
                drop_reason: Some("session ended".into()),
                ..relay(2, Sender::Slot { slot: 2 }, 0, 105, None, None, "on it")
            },
        ];
        let clock = |t: i64| format!("T{t}");
        assert_eq!(
            lines(&relay_rows(&v, 2, Some(0), 200, clock)),
            vec![
                "T120 from S0: status? (for S2, dropped: dropped by S0)".to_string(),
                "T110 to S0: on it (for S0, dropped: session ended)".to_string(),
            ]
        );
    }

    #[test]
    fn relay_rows_keep_to_the_current_session() {
        let mut v = locks();
        v.relays = vec![
            // Delivered to the session before this one.
            relay(1, Sender::You, 2, 100, Some(101), None, "old news"),
            // Sent by the session before this one, delivered later.
            relay(
                2,
                Sender::Slot { slot: 2 },
                0,
                102,
                Some(210),
                None,
                "old reply",
            ),
            // Queued before this session, delivered to it.
            relay(3, Sender::You, 2, 150, Some(201), None, "welcome"),
            relay(
                4,
                Sender::Slot { slot: 2 },
                0,
                205,
                Some(206),
                None,
                "on it",
            ),
            // Still waiting: always shown.
            relay(5, Sender::Overseer, 2, 90, None, None, "queued"),
        ];
        let clock = |t: i64| format!("T{t}");
        assert_eq!(
            lines(&relay_rows(&v, 2, Some(200), 210, clock)),
            vec![
                "T206 to S0: on it".to_string(),
                "T201 from you: welcome".to_string(),
                "T90 from Overseer: queued (pending 2m)".to_string(),
            ]
        );
        // No session: only what is waiting.
        assert_eq!(
            lines(&relay_rows(&v, 2, None, 210, clock)),
            vec!["T90 from Overseer: queued (pending 2m)".to_string()]
        );
    }

    #[test]
    fn claim_entry_splits_target_and_reason() {
        let mut e = PanelEntry::claim(2);
        assert!(e.request().is_err());
        e.field.set("  repo:demo-api:main  hotfix for DEMO-33 ");
        assert_eq!(
            e.request(),
            Ok(Request::ClaimTarget {
                slot: 2,
                target: "repo:demo-api:main".into(),
                reason: Some("hotfix for DEMO-33".into()),
                item: None,
            })
        );
        e.field.set("argo:uat/root");
        assert_eq!(
            e.request(),
            Ok(Request::ClaimTarget {
                slot: 2,
                target: "argo:uat/root".into(),
                reason: None,
                item: None,
            })
        );
    }

    #[test]
    fn message_entry_sends_from_you_to_the_slot() {
        let mut e = PanelEntry::message(4);
        assert!(e.request().is_err());
        e.field.set(" check the pin ");
        assert_eq!(
            e.request(),
            Ok(Request::SendRelay {
                from: None,
                to: 4,
                text: "check the pin".into(),
            })
        );
    }

    fn key(s: &str) -> Keystroke {
        Keystroke::parse(s).unwrap()
    }

    #[test]
    fn release_entry_confirms_picks_and_force_releases() {
        assert!(PanelEntry::release(2, vec![], 0).is_none());
        let targets = vec!["argo:uat/root".to_string(), "tf:default".to_string()];
        let mut e = PanelEntry::release(2, targets, 0).unwrap();
        assert_eq!(e.prompt(), "Release S2's argo:uat/root? Enter / Esc");
        assert_eq!(e.key(&key("up")), EntryAction::Redraw);
        assert_eq!(e.key(&key("down")), EntryAction::Redraw);
        assert_eq!(e.key(&key("down")), EntryAction::Redraw);
        assert_eq!(e.prompt(), "Release S2's tf:default? Enter / Esc");
        assert_eq!(e.key(&key("x")), EntryAction::Ignored);
        assert_eq!(e.key(&key("enter")), EntryAction::Submit);
        assert_eq!(
            e.request(),
            // Only while S2 still holds it, so a target handed on since
            // the confirm opened is not taken from its new holder.
            Ok(Request::ForceRelease {
                target: "tf:default".into(),
                holder: Some(2),
            })
        );
        assert_eq!(e.key(&key("escape")), EntryAction::Cancel);
    }

    #[test]
    fn text_entries_type_submit_and_cancel() {
        let mut e = PanelEntry::message(1);
        let mut k = key("h");
        k.key_char = Some("h".into());
        assert_eq!(e.key(&k), EntryAction::Redraw);
        assert_eq!(e.field.text, "h");
        assert_eq!(e.key(&key("enter")), EntryAction::Submit);
        assert_eq!(e.key(&key("escape")), EntryAction::Cancel);
    }

    #[test]
    fn replies_show_inline() {
        let queued = Response::Queued {
            message: "queued behind S3".into(),
        };
        assert_eq!(reply_note(&Ok(queued)).as_deref(), Some("queued behind S3"));
        let err = Response::Error {
            message: "no such target".into(),
        };
        assert_eq!(reply_note(&Ok(err)).as_deref(), Some("no such target"));
        assert_eq!(reply_note(&Ok(Response::Ok)), None);
        assert_eq!(reply_note(&Err("lost".into())).as_deref(), Some("lost"));
    }
    #[test]
    fn progress_rows_newest_first_states_muted() {
        use crate::board::Tone;
        use overseer_proto::{Report, ReportKind};
        let mut view = crate::app::empty_coord();
        let r = |id, at, slot, kind, text: &str| Report {
            id,
            at,
            slot,
            kind,
            text: text.into(),
        };
        view.reports = vec![
            r(3, 30, 1, ReportKind::State, "idle"),
            r(2, 20, 2, ReportKind::Report, "other slot"),
            r(1, 10, 1, ReportKind::Report, "tests written"),
        ];
        // Out of order on purpose: the rows are sorted, not trusted.
        view.reports.reverse();
        assert_eq!(
            progress_rows(&view, 1, clock, 10),
            vec![
                ("T30 idle".to_string(), Tone::Idle),
                ("T10 tests written".to_string(), Tone::Running),
            ]
        );
        assert_eq!(progress_rows(&view, 1, clock, 1).len(), 1);
        assert!(progress_rows(&view, 4, clock, 10).is_empty());
    }

    fn demo_item(key: &str, owner: Option<SlotId>) -> overseer_proto::WorkItem {
        overseer_proto::WorkItem {
            key: key.into(),
            kind: ItemKind::Ticket,
            title: format!("Title of {key}"),
            status: "In Progress".into(),
            priority: Some("High".into()),
            parent: None,
            owner,
            in_list: true,
            folder: None,
            stage: None,
            stage_since: None,
        }
    }

    fn demo_view(items: Vec<overseer_proto::WorkItem>) -> WorkView {
        WorkView {
            notes_off: None,
            items,
            sources: vec![],
            notices: vec![],
            sprint: None,
            prs: Vec::new(),
            sprint_day: None,
        }
    }

    fn demo_detail(key: &str) -> overseer_proto::ItemDetail {
        overseer_proto::ItemDetail {
            key: key.into(),
            kind: ItemKind::Ticket,
            title: format!("Full title of {key}"),
            status: "In Review".into(),
            priority: None,
            stage: None,
            stage_since: None,
            folder: None,
            body: None,
            notes: None,
            depends_on: vec![],
            children: vec![],
            prs: vec![],
            overview: None,
            fetched_at: None,
            error: None,
        }
    }

    fn open(key: &str, state: DetailState) -> OpenDetail {
        OpenDetail {
            slot: 2,
            key: key.into(),
            state,
            req: 1,
            show_all: false,
            refreshing: false,
        }
    }

    #[test]
    fn body_lines_reads_headings_bullets_and_code() {
        let text =
            "## Goal\nShow the detail view.\n\n- First\n  - Nested\n- Second\n```\ncargo test\n```";
        assert_eq!(
            body_lines(text),
            vec![
                BodyLine::Heading("Goal".into()),
                BodyLine::Text("Show the detail view.".into()),
                BodyLine::Blank,
                BodyLine::Bullet {
                    depth: 0,
                    text: "First".into()
                },
                BodyLine::Bullet {
                    depth: 1,
                    text: "Nested".into()
                },
                BodyLine::Bullet {
                    depth: 0,
                    text: "Second".into()
                },
                BodyLine::Code("cargo test".into()),
            ]
        );
        // Numbered items keep their number; code keeps its blank lines and `#`.
        assert_eq!(
            body_lines("1. One\n  2. Two\n```\n# not a heading\n\n```"),
            vec![
                BodyLine::Bullet {
                    depth: 0,
                    text: "1. One".into()
                },
                BodyLine::Bullet {
                    depth: 1,
                    text: "2. Two".into()
                },
                BodyLine::Code("# not a heading".into()),
                BodyLine::Code(String::new()),
            ]
        );
    }

    #[test]
    fn detail_model_while_loading_uses_the_cache() {
        let view = demo_view(vec![demo_item("DEMO-1", Some(2))]);
        let m = detail_model(&open("DEMO-1", DetailState::Loading), &view, 100, None);
        assert!(m.loading);
        assert_eq!(m.key, "DEMO-1");
        assert_eq!(m.title, "Title of DEMO-1");
        assert_eq!(m.rows[0], ("Status".to_string(), "In Progress".to_string()));
        assert_eq!(m.rows[1], ("Priority".to_string(), "High".to_string()));
        assert!(m.body.is_empty());
        assert!(!m.truncated);
        assert_eq!(m.notice, None);
    }

    #[test]
    fn detail_model_rows_from_a_loaded_detail() {
        use overseer_proto::{ChildBead, PullRequest, Stage};
        // Not listed, so every row comes from the reply.
        let mut item = demo_item("DEMO-1", Some(2));
        item.in_list = false;
        let view = demo_view(vec![item]);
        let mut d = demo_detail("DEMO-1");
        d.stage = Some(Stage::InProgress);
        d.stage_since = Some(100 - 3 * 3600);
        d.folder = Some("/Users/demo/notes/DEMO-1".into());
        d.fetched_at = Some(100 - 120);
        d.prs = vec![PullRequest {
            repo: "demo-repo".into(),
            id: 7,
            title: "Add the view".into(),
            branch: "DEMO-1-view".into(),
            url: None,
            author: None,
            created: None,
            ticket: Some("DEMO-1".into()),
            approvals: None,
        }];
        d.children = vec![ChildBead {
            key: "ov-3".into(),
            title: "Write tests".into(),
            status: "open".into(),
        }];
        let m = detail_model(
            &open("DEMO-1", DetailState::Loaded(d)),
            &view,
            100,
            Some(Path::new("/Users/demo")),
        );
        assert!(!m.loading);
        assert_eq!(m.title, "Full title of DEMO-1");
        let rows: Vec<(&str, &str)> = m
            .rows
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                ("Status", "In Review"),
                ("Priority", "Not set"),
                ("Stage", "IN PROGRESS \u{b7} 3h"),
                ("PR", "demo-repo #7 Add the view"),
                ("Folder", "~/notes/DEMO-1"),
                ("Fetched", "2m ago"),
            ]
        );
        assert_eq!(m.children, ["ov-3 Write tests (open)"]);
    }

    #[test]
    fn detail_model_shows_bead_notes_and_dependencies() {
        let mut item = demo_item("ov-3", Some(2));
        item.kind = ItemKind::Bead;
        let view = demo_view(vec![item]);
        let mut d = demo_detail("ov-3");
        d.kind = ItemKind::Bead;
        d.notes = Some("tried it\nworks".into());
        d.depends_on = vec!["ov-1".into(), "ov-2 (blocks)".into()];
        let m = detail_model(&open("ov-3", DetailState::Loaded(d)), &view, 100, None);
        assert!(m.rows.contains(&("Notes".into(), "tried it\nworks".into())));
        assert!(m
            .rows
            .contains(&("Depends on".into(), "ov-1, ov-2 (blocks)".into())));
    }

    #[test]
    fn detail_model_caps_long_bodies() {
        let view = demo_view(vec![demo_item("DEMO-1", Some(2))]);
        let mut d = demo_detail("DEMO-1");
        d.body = Some(
            (1..=100)
                .map(|n| format!("line {n}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let mut o = open("DEMO-1", DetailState::Loaded(d));
        let m = detail_model(&o, &view, 100, None);
        assert_eq!(m.body.len(), BODY_PREVIEW_LINES);
        assert!(m.truncated);
        o.show_all = true;
        let m = detail_model(&o, &view, 100, None);
        assert_eq!(m.body.len(), 100);
        assert!(!m.truncated);
    }

    #[test]
    fn detail_model_shows_the_failure_and_overview() {
        let view = demo_view(vec![demo_item("DEMO-1", Some(2))]);
        let mut d = demo_detail("DEMO-1");
        d.error = Some("jira: cannot reach demo.atlassian.net".into());
        d.overview = Some("The project overview.".into());
        let m = detail_model(&open("DEMO-1", DetailState::Loaded(d)), &view, 100, None);
        assert_eq!(
            m.notice.as_deref(),
            Some("jira: cannot reach demo.atlassian.net")
        );
        assert_eq!(m.body, [BodyLine::Text("The project overview.".into())]);
        // A failed request keeps the cached rows and says why.
        let m = detail_model(
            &open("DEMO-1", DetailState::Failed("daemon gone".into())),
            &view,
            100,
            None,
        );
        assert_eq!(m.notice.as_deref(), Some("daemon gone"));
        assert_eq!(m.title, "Title of DEMO-1");
        assert!(!m.loading);
    }

    #[test]
    fn detail_closes_when_the_item_leaves_the_slot() {
        let o = open("DEMO-1", DetailState::Loading);
        assert!(detail_still_owned(
            &o,
            &demo_view(vec![demo_item("DEMO-1", Some(2))])
        ));
        assert!(!detail_still_owned(
            &o,
            &demo_view(vec![demo_item("DEMO-1", Some(3))])
        ));
        assert!(!detail_still_owned(
            &o,
            &demo_view(vec![demo_item("DEMO-1", None)])
        ));
        assert!(!detail_still_owned(&o, &demo_view(vec![])));
    }

    #[test]
    fn a_late_reply_for_another_key_is_ignored() {
        let mut o = Some(open("DEMO-2", DetailState::Loading));
        take_reply(&mut o, "DEMO-1", 1, Ok(demo_detail("DEMO-1")));
        assert_eq!(o.as_ref().unwrap().state, DetailState::Loading);
        take_reply(&mut o, "DEMO-2", 1, Err("no".into()));
        assert_eq!(o.as_ref().unwrap().state, DetailState::Failed("no".into()));
        take_reply(&mut o, "DEMO-2", 1, Ok(demo_detail("DEMO-2")));
        assert!(matches!(o.unwrap().state, DetailState::Loaded(_)));
        // Closed meanwhile: nothing reopens.
        let mut none = None;
        take_reply(&mut none, "DEMO-2", 1, Ok(demo_detail("DEMO-2")));
        assert!(none.is_none());
    }

    #[test]
    fn an_older_request_for_the_same_key_cannot_overwrite_a_newer_one() {
        // Refresh pressed while the first fetch still hangs: request 2 is
        // the one the view waits for.
        let mut o = Some(open("DEMO-1", DetailState::Loading));
        o.as_mut().unwrap().req = 2;
        take_reply(&mut o, "DEMO-1", 2, Ok(demo_detail("DEMO-1")));
        assert!(matches!(o.as_ref().unwrap().state, DetailState::Loaded(_)));
        // The first request times out late.
        take_reply(&mut o, "DEMO-1", 1, Err("timed out".into()));
        assert!(matches!(o.unwrap().state, DetailState::Loaded(_)));
    }

    #[test]
    fn an_empty_description_says_so() {
        let view = demo_view(vec![demo_item("DEMO-1", Some(2))]);
        for body in [None, Some(String::new()), Some("  \n ".to_string())] {
            let mut d = demo_detail("DEMO-1");
            d.body = body;
            let o = open("DEMO-1", DetailState::Loaded(d));
            let m = detail_model(&o, &view, 0, None);
            assert_eq!(m.body, [BodyLine::Text("No description".into())]);
        }
        // A failed fetch says why instead.
        let mut d = demo_detail("DEMO-1");
        d.error = Some("Jira is off".into());
        let o = open("DEMO-1", DetailState::Loaded(d));
        assert!(detail_model(&o, &view, 0, None).body.is_empty());
    }

    fn info_build(build: &str) -> DaemonKnown {
        DaemonKnown::Info(overseer_proto::DaemonInfo {
            version: "0".into(),
            build: build.into(),
            pid: 1,
            started_at: 0,
            exe: "/demo/overseer".into(),
        })
    }

    fn refused(gate: DetailGate) -> String {
        match gate {
            DetailGate::Refuse(why) => why,
            other => panic!("expected Refuse, got {other:?}"),
        }
    }

    #[test]
    fn detail_waits_for_the_build_probe() {
        assert_eq!(
            detail_gate(&DaemonKnown::Unknown, true, "b2"),
            DetailGate::Wait
        );
        assert_eq!(
            detail_gate(&DaemonKnown::Unknown, false, "b2"),
            DetailGate::Send
        );
        let why = refused(detail_gate(&DaemonKnown::Old, false, "b2"));
        assert!(why.contains("older build"), "{why}");
        assert!(why.contains("Restart the daemon"), "{why}");
        let why = refused(detail_gate(
            &info_build("abc1234-202611140930"),
            false,
            "def5678-202611141000",
        ));
        assert!(why.contains("Restart the daemon"), "{why}");
        let why = refused(detail_gate(
            &info_build("def5678-202611141000"),
            false,
            "abc1234-202611140930",
        ));
        assert!(why.contains("Restart Overseer"), "{why}");
        let why = refused(detail_gate(&info_build("b1"), false, "b2"));
        assert!(why.contains("restart both"), "{why}");
        assert_eq!(
            detail_gate(&info_build("dev"), false, "b2"),
            DetailGate::Send
        );
        assert_eq!(
            detail_gate(&info_build("b2"), false, "b2"),
            DetailGate::Send
        );
    }

    fn row_pairs(m: &DetailModel) -> Vec<(&str, &str)> {
        m.rows
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }

    fn demo_pr(id: u64, created: Option<i64>) -> overseer_proto::PullRequest {
        overseer_proto::PullRequest {
            repo: "demo-repo".into(),
            id,
            title: format!("Change {id}"),
            branch: format!("DEMO-1-{id}"),
            url: None,
            author: None,
            created,
            ticket: Some("DEMO-1".into()),
            approvals: None,
        }
    }

    #[test]
    fn refresh_keeps_the_loaded_text() {
        let view = demo_view(vec![demo_item("DEMO-1", Some(2))]);
        let mut d = demo_detail("DEMO-1");
        d.body = Some("Kept body".into());
        let prev = open("DEMO-1", DetailState::Loaded(d));
        let o = reopen(Some(&prev), 2, "DEMO-1", 2, true);
        assert!(matches!(o.state, DetailState::Loaded(_)));
        assert!(o.refreshing);
        assert_eq!(o.req, 2);
        let m = detail_model(&o, &view, 100, None);
        assert_eq!(m.body, [BodyLine::Text("Kept body".into())]);
        assert!(row_pairs(&m).contains(&("Fetched", "Refreshing...")));
        assert!(!m.loading);
        let o = reopen(Some(&prev), 2, "DEMO-1", 3, false);
        assert_eq!(o.state, DetailState::Loading);
        assert!(!o.refreshing);
    }

    #[test]
    fn a_refresh_clears_the_old_error_on_the_kept_copy() {
        let view = demo_view(vec![demo_item("DEMO-1", Some(2))]);
        let mut d = demo_detail("DEMO-1");
        d.body = Some("Kept body".into());
        d.error = Some("acli failed".into());
        let prev = open("DEMO-1", DetailState::Loaded(d));
        let o = reopen(Some(&prev), 2, "DEMO-1", 2, true);
        assert!(o.refreshing);
        match &o.state {
            DetailState::Loaded(kept) => {
                assert_eq!(kept.error, None);
                assert_eq!(kept.body.as_deref(), Some("Kept body"));
            }
            other => panic!("expected Loaded, got {other:?}"),
        }
        let m = detail_model(&o, &view, 100, None);
        assert_eq!(m.notice, None);
    }

    #[test]
    fn a_failed_refresh_keeps_the_text() {
        let view = demo_view(vec![demo_item("DEMO-1", Some(2))]);
        let mut d = demo_detail("DEMO-1");
        d.body = Some("Kept body".into());
        let prev = open("DEMO-1", DetailState::Loaded(d));
        let mut o = Some(reopen(Some(&prev), 2, "DEMO-1", 2, true));
        take_reply(&mut o, "DEMO-1", 2, Err("lost".into()));
        let o = o.unwrap();
        assert!(matches!(o.state, DetailState::Loaded(_)));
        assert!(!o.refreshing);
        let m = detail_model(&o, &view, 100, None);
        assert_eq!(m.body, [BodyLine::Text("Kept body".into())]);
        assert_eq!(m.notice.as_deref(), Some("lost"));
    }

    #[test]
    fn a_refused_refresh_keeps_the_text() {
        let view = demo_view(vec![demo_item("DEMO-1", Some(2))]);
        let mut d = demo_detail("DEMO-1");
        d.body = Some("Kept body".into());
        let prev = open("DEMO-1", DetailState::Loaded(d));
        let mut o = reopen(Some(&prev), 2, "DEMO-1", 2, true);
        fail_detail(&mut o, "Cannot fetch details: restart both".into());
        assert!(matches!(o.state, DetailState::Loaded(_)));
        assert!(!o.refreshing);
        let m = detail_model(&o, &view, 100, None);
        assert_eq!(m.body, [BodyLine::Text("Kept body".into())]);
        assert_eq!(
            m.notice.as_deref(),
            Some("Cannot fetch details: restart both")
        );
        let mut o = open("DEMO-1", DetailState::Loading);
        fail_detail(&mut o, "why".into());
        assert_eq!(o.state, DetailState::Failed("why".into()));
    }

    #[test]
    fn loading_and_failed_show_child_beads_and_prs_newest_first() {
        let mut bead = demo_item("ov-1", None);
        bead.kind = ItemKind::Bead;
        bead.parent = Some("DEMO-1".into());
        bead.title = "First step".into();
        bead.status = "open".into();
        let mut view = demo_view(vec![demo_item("DEMO-1", Some(2)), bead]);
        view.prs = vec![
            demo_pr(1, None),
            demo_pr(2, Some(100)),
            demo_pr(4, Some(200)),
        ];
        for state in [DetailState::Loading, DetailState::Failed("x".into())] {
            let m = detail_model(&open("DEMO-1", state), &view, 300, None);
            assert_eq!(m.children, ["ov-1 First step (open)"]);
            let prs: Vec<&str> = row_pairs(&m)
                .into_iter()
                .filter(|(k, _)| *k == "PR")
                .map(|(_, v)| v)
                .collect();
            assert_eq!(
                prs,
                [
                    "demo-repo #4 Change 4",
                    "demo-repo #2 Change 2",
                    "demo-repo #1 Change 1"
                ]
            );
        }
    }

    #[test]
    fn live_fields_follow_polls_while_listed() {
        use overseer_proto::Stage;
        let mut item = demo_item("DEMO-1", Some(2));
        item.status = "In Review".into();
        item.priority = Some("High".into());
        item.stage = Some(Stage::InProgress);
        item.stage_since = Some(40);
        let mut view = demo_view(vec![item]);
        view.prs = vec![demo_pr(5, Some(10))];
        let mut d = demo_detail("DEMO-1");
        d.status = "To Do".into();
        d.priority = None;
        d.body = Some("B".into());
        let o = open("DEMO-1", DetailState::Loaded(d));
        let m = detail_model(&o, &view, 100, None);
        let rows = row_pairs(&m);
        assert!(rows.contains(&("Status", "In Review")), "{rows:?}");
        assert!(rows.contains(&("Priority", "High")), "{rows:?}");
        assert!(rows.iter().any(|(k, _)| *k == "Stage"), "{rows:?}");
        assert!(rows.contains(&("PR", "demo-repo #5 Change 5")), "{rows:?}");
        assert_eq!(m.body, [BodyLine::Text("B".into())]);
        view.items[0].in_list = false;
        let m = detail_model(&o, &view, 100, None);
        let rows = row_pairs(&m);
        assert!(rows.contains(&("Status", "To Do")), "{rows:?}");
        assert!(!rows.iter().any(|(k, _)| *k == "PR"), "{rows:?}");
    }

    #[test]
    fn live_priority_falls_back_to_the_reply() {
        let mut item = demo_item("DEMO-1", Some(2));
        item.priority = None;
        let view = demo_view(vec![item]);
        let mut d = demo_detail("DEMO-1");
        d.priority = Some("Low".into());
        let m = detail_model(&open("DEMO-1", DetailState::Loaded(d)), &view, 100, None);
        assert!(row_pairs(&m).contains(&("Priority", "Low")));
    }

    #[test]
    fn show_all_only_past_forty_lines() {
        let view = demo_view(vec![demo_item("DEMO-1", Some(2))]);
        let lines = |n: usize| {
            let mut d = demo_detail("DEMO-1");
            d.body = Some(
                (1..=n)
                    .map(|i| format!("line {i}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
            open("DEMO-1", DetailState::Loaded(d))
        };
        let m = detail_model(&lines(40), &view, 100, None);
        assert!(!m.expandable && !m.truncated);
        let mut o = lines(41);
        let m = detail_model(&o, &view, 100, None);
        assert!(m.expandable && m.truncated);
        o.show_all = true;
        let m = detail_model(&o, &view, 100, None);
        assert!(m.expandable && !m.truncated);
    }

    #[test]
    fn an_empty_title_shows_the_key() {
        let mut item = demo_item("DEMO-1", Some(2));
        item.title = String::new();
        let view = demo_view(vec![item]);
        let mut d = demo_detail("DEMO-1");
        d.title = "  ".into();
        let m = detail_model(&open("DEMO-1", DetailState::Loaded(d)), &view, 100, None);
        assert_eq!(m.title, "DEMO-1");
        let m = detail_model(&open("DEMO-1", DetailState::Loading), &view, 100, None);
        assert_eq!(m.title, "DEMO-1");
        assert_eq!(owns_rows(&view, 2)[0].title, "DEMO-1");
    }

    #[test]
    fn empty_bullets_are_dropped() {
        assert_eq!(
            body_lines("- \n-\n- Real\n  - "),
            [BodyLine::Bullet {
                depth: 0,
                text: "Real".into()
            }]
        );
    }

    #[test]
    fn bullet_indent_grows_twelve_per_level() {
        assert_eq!(bullet_indent(0), 8.);
        assert_eq!(bullet_indent(2), 32.);
    }
}
