//! The attention list at the top of the board: what is waiting on you,
//! oldest first. The daemon derives the list; the app only shows it, opens
//! what a row points at, and keeps the Dock badge in step.

use gpui::{div, prelude::*, px, rgb, Context, FontWeight};
use overseer_proto::{
    age_text, slot_name, AttentionItem, AttentionKind, AttentionView, Request, SlotId,
};

use crate::app::{colors, open_external, OverseerApp};
use crate::sprint_view::is_http_url;

/// One row of the panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub key: String,
    /// `S3`, `demo-api#7 S2` or `demo-api#9`, as `ctl attention` shows it.
    pub who: String,
    pub text: String,
    /// `5m`, `2h`, `1d`.
    pub age: String,
}

/// The items oldest first; items of the same age keep the daemon's order.
/// Kinds this build does not know and rows the user dismissed are left out.
pub(crate) fn oldest_first(v: &AttentionView) -> Vec<&AttentionItem> {
    let mut items: Vec<_> = v
        .items
        .iter()
        .filter(|i| i.kind != AttentionKind::Other && !i.dismissed)
        .collect();
    items.sort_by_key(|i| i.since);
    items
}

fn who(i: &AttentionItem) -> String {
    let mut who = match &i.pr {
        Some((repo, id)) => format!("{repo}#{id}"),
        None => String::new(),
    };
    if let Some(s) = i.slot {
        if !who.is_empty() {
            who.push(' ');
        }
        who.push_str(&slot_name(s));
    }
    who
}

pub fn rows(v: &AttentionView, now: i64) -> Vec<Row> {
    oldest_first(v)
        .into_iter()
        .map(|i| Row {
            key: i.key.clone(),
            who: who(i),
            text: i.text.clone(),
            age: age_text(now - i.since),
        })
        .collect()
}

/// The Dock badge: nothing at zero, else the count.
pub fn badge_label(count: usize) -> Option<String> {
    (count > 0).then(|| count.to_string())
}

/// What clicking a row does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Click {
    /// Open the slot as its board card would.
    Slot(SlotId),
    /// Open the pull request in the browser (when it has a web link) and
    /// mark it seen.
    Pr {
        repo: String,
        id: u64,
        url: Option<String>,
    },
    Nothing,
}

/// A PR item opens its PR even when a slot owns it: the PR is what changed.
pub fn click(i: &AttentionItem) -> Click {
    match (&i.pr, i.slot) {
        (Some((repo, id)), _) => Click::Pr {
            repo: repo.clone(),
            id: *id,
            url: i.url.clone().filter(|u| is_http_url(u)),
        },
        (None, Some(s)) => Click::Slot(s),
        (None, None) => Click::Nothing,
    }
}

impl OverseerApp {
    pub(crate) fn attention_click(
        &mut self,
        key: &str,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        if self.modal_open() {
            return;
        }
        let Some(item) = self.attention.items.iter().find(|i| i.key == key) else {
            return;
        };
        match click(item) {
            Click::Slot(id) => self.open_card(id, window, cx),
            Click::Pr { repo, id, url } => {
                if let Some(url) = url {
                    open_external(url, "the pull request");
                }
                self.pr_seen(repo, id, cx);
            }
            Click::Nothing => {}
        }
    }

    /// Hides the row `key` as it is now. The daemon stores it and sends the
    /// list again; the row is marked here too so it goes at once.
    pub(crate) fn attention_dismiss(&mut self, key: &str, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        let Some(item) = self.attention.items.iter_mut().find(|i| i.key == key) else {
            return;
        };
        item.dismissed = true;
        let req = Request::DismissAttention {
            key: item.key.clone(),
            since: item.since,
            text: item.text.clone(),
        };
        crate::notify::set_dock_badge(self.attention.count());
        self.send(req, cx);
        cx.notify();
    }

    pub(crate) fn render_attention(&self, now: i64, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = rows(&self.attention, now);
        let panel = div()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .rounded(px(6.))
            .bg(rgb(colors::CARD))
            .border_1()
            .border_color(rgb(colors::BORDER));
        if rows.is_empty() {
            return panel
                .text_xs()
                .text_color(rgb(colors::MUTED))
                .child("Nothing waiting");
        }
        let header = div()
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(rgb(colors::ORANGE))
            .child(format!("WAITING ON YOU \u{b7} {}", rows.len()));
        let lines: Vec<_> = rows
            .into_iter()
            .enumerate()
            .map(|(n, r)| {
                let key = r.key;
                let dismiss_key = key.clone();
                div()
                    .id(("attention-row", n))
                    .group("attention-row")
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_xs()
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(colors::CARD_HOVER)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.attention_click(&key, window, cx)
                    }))
                    .child(
                        div()
                            .flex_none()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(colors::ACCENT))
                            .child(r.who),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_color(rgb(colors::TEXT_DIM))
                            .child(r.text),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(rgb(colors::MUTED))
                            .child(r.age),
                    )
                    .child(
                        div()
                            .id(("attention-dismiss", n))
                            .flex_none()
                            .px_1()
                            .text_color(rgb(colors::MUTED))
                            .invisible()
                            .group_hover("attention-row", |s| s.visible())
                            .hover(|s| s.text_color(rgb(colors::TEXT)))
                            .child("\u{d7}")
                            // Not also the row's own click behind it.
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.attention_dismiss(&dismiss_key, cx)
                            })),
                    )
            })
            .collect();
        panel
            .border_color(rgb(colors::ORANGE))
            .child(header)
            .children(lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use overseer_proto::{AttentionItem, AttentionKind};

    fn item(
        key: &str,
        kind: AttentionKind,
        slot: Option<SlotId>,
        pr: Option<(&str, u64)>,
        since: i64,
        text: &str,
    ) -> AttentionItem {
        AttentionItem {
            key: key.to_string(),
            kind,
            slot,
            pr: pr.map(|(r, id)| (r.to_string(), id)),
            url: pr.map(|(r, id)| format!("https://example.test/{r}/pull-requests/{id}")),
            since,
            text: text.to_string(),
            dismissed: false,
        }
    }

    fn row(key: &str, who: &str, text: &str, age: &str) -> Row {
        Row {
            key: key.to_string(),
            who: who.to_string(),
            text: text.to_string(),
            age: age.to_string(),
        }
    }

    #[test]
    fn rows_show_slot_and_pr_items_oldest_first() {
        let v = AttentionView {
            items: vec![
                item(
                    "needs_you:3",
                    AttentionKind::NeedsYou,
                    Some(3),
                    None,
                    1000,
                    "wants to run a command",
                ),
                item(
                    "pr_comments:demo-api:9",
                    AttentionKind::PrComments,
                    None,
                    Some(("demo-api", 9)),
                    1000 - 90000,
                    "2 new comments",
                ),
                item(
                    "pr_build_failed:demo-api:7",
                    AttentionKind::PrBuildFailed,
                    Some(2),
                    Some(("demo-api", 7)),
                    1000 - 7200,
                    "build failed",
                ),
            ],
        };
        assert_eq!(
            rows(&v, 1000 + 300),
            vec![
                row(
                    "pr_comments:demo-api:9",
                    "demo-api#9",
                    "2 new comments",
                    "1d"
                ),
                row(
                    "pr_build_failed:demo-api:7",
                    "demo-api#7 S2",
                    "build failed",
                    "2h"
                ),
                row("needs_you:3", "S3", "wants to run a command", "5m"),
            ]
        );
    }

    #[test]
    fn rows_skip_kinds_this_build_does_not_know() {
        let v = AttentionView {
            items: vec![
                item("other:1", AttentionKind::Other, Some(1), None, 1000, "x"),
                item(
                    "log_gap:2",
                    AttentionKind::LogGap,
                    Some(2),
                    None,
                    1000,
                    "gap",
                ),
            ],
        };
        assert_eq!(rows(&v, 1300), vec![row("log_gap:2", "S2", "gap", "5m")]);
    }

    #[test]
    fn rows_leave_out_dismissed_items() {
        let mut gone = item(
            "needs_you:3",
            AttentionKind::NeedsYou,
            Some(3),
            None,
            1000,
            "prompt",
        );
        gone.dismissed = true;
        let v = AttentionView {
            items: vec![
                gone,
                item(
                    "log_gap:2",
                    AttentionKind::LogGap,
                    Some(2),
                    None,
                    1000,
                    "gap",
                ),
            ],
        };
        assert_eq!(rows(&v, 1300), vec![row("log_gap:2", "S2", "gap", "5m")]);
        assert_eq!(oldest_first(&v).len(), 1);
        assert_eq!(v.count(), 1);
    }

    #[test]
    fn a_pr_row_opens_the_pr_and_a_slot_row_its_session() {
        let pr = item(
            "pr_build_failed:demo-api:7",
            AttentionKind::PrBuildFailed,
            Some(2),
            Some(("demo-api", 7)),
            0,
            "build failed",
        );
        assert_eq!(
            click(&pr),
            Click::Pr {
                repo: "demo-api".to_string(),
                id: 7,
                url: Some("https://example.test/demo-api/pull-requests/7".to_string()),
            }
        );
        let slot = item(
            "needs_you:3",
            AttentionKind::NeedsYou,
            Some(3),
            None,
            0,
            "x",
        );
        assert_eq!(click(&slot), Click::Slot(3));
        let mut odd = pr.clone();
        odd.url = Some("file:///tmp/demo".to_string());
        assert!(matches!(click(&odd), Click::Pr { url: None, .. }));
    }

    #[test]
    fn badge_label_is_empty_at_zero() {
        assert_eq!(badge_label(0), None);
        assert_eq!(badge_label(3), Some("3".to_string()));
    }
}
