//! The Today panel on the board: one bar per session, drawn on a shared
//! time axis, with the uncovered stretches in red. The daemon works the day
//! out; the app only lays it out, shows the hover text and opens a slot on
//! a click.

use gpui::{
    div, prelude::*, px, relative, rgb, Context, ElementId, FontWeight, Render, SharedString,
    Window,
};
use overseer_proto::{local_hhmm, slot_name, DayStretch, DayView, SlotId, Span, SpanKind};

use crate::app::{colors, OverseerApp};

/// One span of a row, placed on the shared axis.
#[derive(Debug, Clone, PartialEq)]
pub struct Bar {
    /// Index of the stretch in the row, and of the span in the stretch;
    /// the hover text looks the span up by these.
    pub stretch: usize,
    pub span: usize,
    pub x: f32,
    pub w: f32,
    pub kind: SpanKind,
}

/// The bars of one slot's row.
#[derive(Debug, Clone, PartialEq)]
pub struct RowBars {
    pub slot: SlotId,
    pub bars: Vec<Bar>,
}

/// Where the shared axis starts: the earliest stretch start of the day, or
/// the day's start when nothing ran.
pub fn axis_start(v: &DayView) -> i64 {
    v.rows
        .iter()
        .flat_map(|r| r.stretches.iter())
        .map(|s| s.first)
        .min()
        .unwrap_or(v.start)
}

/// True when there is something to draw: at least one stretch.
pub fn has_stretches(v: &DayView) -> bool {
    v.rows.iter().any(|r| !r.stretches.is_empty())
}

/// The folded line: `Today: all logged`, `Today: 1 gap`, `Today: 2 gaps`.
pub fn fold_label(v: &DayView) -> String {
    let gaps: usize = v.rows.iter().map(|r| v.gaps(r.slot).len()).sum();
    match gaps {
        0 => "Today: all logged".to_string(),
        1 => "Today: 1 gap".to_string(),
        n => format!("Today: {n} gaps"),
    }
}

/// Every span of every row as `x` and `w` on one axis from `axis_start` to
/// `v.now`, scaled to `width` and clamped into `[0, width]`. A stretch with
/// no spans draws nothing.
pub fn bars(v: &DayView, width: f32) -> Vec<RowBars> {
    let start = axis_start(v);
    let span_secs = ((v.now - start).max(1)) as f32;
    let at = |t: i64| ((t - start) as f32 / span_secs * width).clamp(0., width);
    v.rows
        .iter()
        .map(|row| {
            let mut out = Vec::new();
            for (si, st) in row.stretches.iter().enumerate() {
                for (pi, sp) in st.spans.iter().enumerate() {
                    let x = at(sp.from);
                    let w = (at(sp.to) - x).max(0.);
                    out.push(Bar {
                        stretch: si,
                        span: pi,
                        x,
                        w,
                        kind: sp.kind,
                    });
                }
            }
            RowBars {
                slot: row.slot,
                bars: out,
            }
        })
        .collect()
}

/// The hover text for one span: its times, then what covers it.
pub fn hover(s: &DayStretch, span: &Span, clock: impl Fn(i64) -> String) -> String {
    let range = format!("{}-{}", clock(span.from), clock(span.to));
    let what = match span.kind {
        SpanKind::Logged if s.logs.is_empty() => "logged".to_string(),
        SpanKind::Logged => s.logs.join(", "),
        SpanKind::Gap => "no log covers this".to_string(),
        SpanKind::Unlogged => "not logged yet".to_string(),
        SpanKind::Short => "short gap, not flagged".to_string(),
        SpanKind::Wait => "waiting".to_string(),
    };
    format!("{range} {what}")
}

fn fill(kind: SpanKind) -> u32 {
    match kind {
        SpanKind::Logged | SpanKind::Short => colors::GREEN,
        SpanKind::Gap => colors::RED,
        SpanKind::Unlogged => colors::EDGE_IDLE,
        SpanKind::Wait => colors::GREY,
    }
}

/// Plain text in a small dark box, for `.tooltip`.
struct BarTooltip(SharedString);

impl Render for BarTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded(px(4.))
            .text_xs()
            .bg(rgb(colors::CARD_HOVER))
            .border_1()
            .border_color(rgb(colors::BORDER_COORD))
            .text_color(rgb(colors::TEXT))
            .child(self.0.clone())
    }
}

impl OverseerApp {
    /// The panel, or nothing when no session has run today. It folds to one
    /// line; a click on the line opens the bars.
    pub(crate) fn render_timeline(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let v = &self.timeline;
        if v.rows.is_empty() || !has_stretches(v) {
            return None;
        }
        let gaps = fold_label(v) != "Today: all logged";
        let header = div()
            .id("timeline-header")
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(rgb(if gaps { colors::RED } else { colors::TEXT_DIM }))
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| {
                this.timeline_open = !this.timeline_open;
                cx.notify();
            }))
            .child(fold_label(v));
        let mut panel = div()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .rounded(px(6.))
            .bg(rgb(colors::CARD))
            .border_1()
            .border_color(rgb(if gaps { colors::RED } else { colors::BORDER }))
            .child(header);
        if !self.timeline_open {
            return Some(panel);
        }
        let axis = div()
            .flex()
            .justify_between()
            .text_xs()
            .text_color(rgb(colors::MUTED))
            .child(local_hhmm(axis_start(v)))
            .child("now");
        let rows: Vec<_> = bars(v, 1.0)
            .into_iter()
            .enumerate()
            .map(|(n, row)| {
                let slot = row.slot;
                let strip = row.bars.iter().fold(
                    div()
                        .relative()
                        .flex_1()
                        .h(px(12.))
                        .rounded(px(2.))
                        .bg(rgb(colors::BORDER)),
                    |strip, b| {
                        let st = &v.rows[n].stretches[b.stretch];
                        let tip = hover(st, &st.spans[b.span], local_hhmm);
                        let wait = b.kind == SpanKind::Wait;
                        strip.child(
                            div()
                                .id(ElementId::Name(
                                    format!("timeline-{slot}-{}-{}", b.stretch, b.span).into(),
                                ))
                                .absolute()
                                .left(relative(b.x))
                                .w(relative(b.w))
                                .min_w(px(1.))
                                .when(wait, |d| d.top(px(5.)).h(px(2.)))
                                .when(!wait, |d| d.top(px(0.)).h(px(12.)))
                                .bg(rgb(fill(b.kind)))
                                .tooltip(move |_, cx| {
                                    cx.new(|_| BarTooltip(tip.clone().into())).into()
                                }),
                        )
                    },
                );
                div()
                    .id(("timeline-row", n))
                    .flex()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if !this.modal_open() {
                            this.open_card(slot, window, cx);
                        }
                    }))
                    .child(
                        div()
                            .flex_none()
                            .w(px(28.))
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(colors::ACCENT))
                            .child(slot_name(slot)),
                    )
                    .child(strip)
            })
            .collect();
        panel = panel.child(axis).children(rows);
        Some(panel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use overseer_proto::DayRow;

    fn sp(from: i64, to: i64, kind: SpanKind) -> Span {
        Span { from, to, kind }
    }

    fn stretch(first: i64, last: i64, spans: Vec<Span>, logs: &[&str]) -> DayStretch {
        DayStretch {
            session: "demo-session".to_string(),
            first,
            last,
            running: false,
            spans,
            logs: logs.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn view(rows: Vec<DayRow>) -> DayView {
        DayView {
            day: "2026-11-24".to_string(),
            start: 0,
            now: 1000,
            rows,
        }
    }

    fn row(slot: SlotId, stretches: Vec<DayStretch>) -> DayRow {
        DayRow { slot, stretches }
    }

    #[test]
    fn fold_label_counts_gaps_across_rows() {
        let none = view(vec![row(
            1,
            vec![stretch(0, 100, vec![sp(0, 100, SpanKind::Logged)], &[])],
        )]);
        assert_eq!(fold_label(&none), "Today: all logged");
        let one = view(vec![row(
            1,
            vec![stretch(0, 100, vec![sp(0, 100, SpanKind::Gap)], &[])],
        )]);
        assert_eq!(fold_label(&one), "Today: 1 gap");
        let two = view(vec![
            row(
                1,
                vec![stretch(0, 100, vec![sp(0, 100, SpanKind::Gap)], &[])],
            ),
            row(
                2,
                vec![stretch(0, 100, vec![sp(10, 50, SpanKind::Gap)], &[])],
            ),
        ]);
        assert_eq!(fold_label(&two), "Today: 2 gaps");
    }

    #[test]
    fn bars_share_one_axis_from_the_first_stretch_to_now() {
        let v = view(vec![
            row(
                1,
                vec![stretch(100, 600, vec![sp(100, 600, SpanKind::Logged)], &[])],
            ),
            row(
                2,
                vec![stretch(
                    500,
                    1000,
                    vec![sp(500, 1000, SpanKind::Unlogged)],
                    &[],
                )],
            ),
        ]);
        let b = bars(&v, 90.0);
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].bars[0].x, 0.0);
        let end = b[1].bars[0].x + b[1].bars[0].w;
        assert!((end - 90.0).abs() < 1e-3, "ends at width, got {end}");
        assert!((b[1].bars[0].x - 40.0).abs() < 1e-3);
    }

    #[test]
    fn bars_clamp_spans_outside_the_axis_and_survive_a_zero_width_axis() {
        let mut v = view(vec![row(
            1,
            vec![stretch(100, 600, vec![sp(50, 5000, SpanKind::Logged)], &[])],
        )]);
        let b = bars(&v, 100.0);
        assert_eq!(b[0].bars[0].x, 0.0);
        assert_eq!(b[0].bars[0].x + b[0].bars[0].w, 100.0);
        // now equal to the axis start: no divide by zero, nothing past width.
        v.now = 100;
        let b = bars(&v, 100.0);
        assert!(b[0].bars[0].x.is_finite() && b[0].bars[0].w.is_finite());
        assert!(b[0].bars[0].x + b[0].bars[0].w <= 100.0);
    }

    #[test]
    fn a_stretch_with_no_spans_draws_nothing() {
        let v = view(vec![row(1, vec![stretch(100, 100, vec![], &[])])]);
        let b = bars(&v, 100.0);
        assert_eq!(b.len(), 1);
        assert!(b[0].bars.is_empty());
        assert!(has_stretches(&v));
    }

    #[test]
    fn the_axis_falls_back_to_the_day_start_without_stretches() {
        let v = view(vec![row(1, vec![])]);
        assert_eq!(axis_start(&v), 0);
        assert!(!has_stretches(&v));
        assert!(bars(&v, 10.0)[0].bars.is_empty());
    }

    #[test]
    fn hover_says_what_covers_each_kind_of_span() {
        let clock = |t: i64| format!("{:02}:{:02}", t / 60, t % 60);
        let s = stretch(0, 100, vec![], &["demo-ws-s1", "demo-api-s1"]);
        let at = |kind| hover(&s, &sp(542, 640, kind), clock);
        assert_eq!(at(SpanKind::Logged), "09:02-10:40 demo-ws-s1, demo-api-s1");
        assert_eq!(at(SpanKind::Gap), "09:02-10:40 no log covers this");
        assert_eq!(at(SpanKind::Unlogged), "09:02-10:40 not logged yet");
        assert_eq!(at(SpanKind::Short), "09:02-10:40 short gap, not flagged");
        assert_eq!(at(SpanKind::Wait), "09:02-10:40 waiting");
    }
}
