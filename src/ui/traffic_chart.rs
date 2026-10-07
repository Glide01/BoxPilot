//! Home page live traffic chart: the last two minutes of upload and download
//! rates as two areas under a legend with the current values.
//!
//! Its own view so it can be embedded `.cached()`: it re-renders once per
//! status sample (it observes `Traffic`), and the chart's hover crosshair —
//! which notifies the view painting it on every mouse move — repaints only
//! this view, not all of Home.

use crate::core::bytefmt::format_speed;
use crate::state::traffic::{RatePoint, HISTORY_LEN};
use crate::state::Traffic;
use gpui::{
    div, linear_color_stop, linear_gradient, px, rgb, Context, Div, Entity, FontWeight, Hsla,
    IntoElement, ParentElement, Render, SharedString, Styled, Window,
};
use gpui_component::{
    chart::AreaChart, plot::AxisLabelPlacement, theme::Theme, ActiveTheme, Icon, Sizable, StyledExt,
};
use std::collections::VecDeque;

/// Total height of the view: legend row, gap, plot.
pub const HEIGHT: f32 = LEGEND_HEIGHT + GAP + PLOT_HEIGHT;
const LEGEND_HEIGHT: f32 = 16.;
const GAP: f32 = 8.;
/// The download reading's least width ("Download 999.9 KB/s"), so the
/// upload reading beside it holds still while the rates tick.
const LEGEND_ITEM_MIN_WIDTH: f32 = 150.;
const PLOT_HEIGHT: f32 = 72.;
/// How far the plot's clip reaches past its top and sides (see `render`).
const CLIP_BLEED: f32 = 8.;

/// The y axis never tops out below this (bytes/sec), so an idle connection's
/// keep-alive trickle stays a flat line instead of filling the plot.
const MIN_AXIS_MAX: u64 = 10 * 1024;

/// The series colours. Download follows the accent (`primary`, which
/// `ui::theme` steps per mode); upload is an orange stepped for each mode's
/// surface. Checked as a pair against white and against the dark background
/// for lightness, chroma, colour-blind separation and 3:1 contrast — gpui-
/// component's theme has no orange, and its yellow is too light on dark.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SeriesColors {
    pub download: Hsla,
    pub upload: Hsla,
}

pub fn series_colors(theme: &Theme) -> SeriesColors {
    SeriesColors {
        download: theme.primary,
        upload: upload_color(theme.is_dark()),
    }
}

fn upload_color(dark: bool) -> Hsla {
    if dark {
        rgb(0xD95926).into()
    } else {
        rgb(0xEB6834).into()
    }
}

/// The y-axis top for a window whose highest rate is `peak` bytes/sec: the
/// first 1-2-5 step (in B, KB, MB, … of 1024) at least 5% above it, never
/// below [`MIN_AXIS_MAX`]. Round tops keep the tick labels short ("500 KB/s",
/// "2 MB/s", halves "250 KB/s", "1 MB/s").
pub fn axis_max(peak: u64) -> u64 {
    let target = peak.saturating_add(peak / 20).max(MIN_AXIS_MAX);
    let mut unit: u64 = 1;
    for _ in 0..5 {
        for step in [1, 2, 5, 10, 20, 50, 100, 200, 500] {
            if step * unit >= target {
                return step * unit;
            }
        }
        unit *= 1024;
    }
    target
}

/// A y-axis tick label: `format_speed` without a trailing ".0".
fn tick_label(value: f64) -> SharedString {
    format_speed(value.max(0.).round() as u64)
        .replace(".0 ", " ")
        .into()
}

/// "Now", "45 s ago", "1 min 5 s ago".
fn age_label(seconds: u16) -> SharedString {
    let t = &crate::i18n::s().chart;
    match seconds {
        0 => t.now.into(),
        s if s < 60 => (t.secs_ago)(s).into(),
        s if s % 60 == 0 => (t.mins_ago)(s / 60).into(),
        s => (t.mins_secs_ago)(s / 60, s % 60).into(),
    }
}

#[derive(Clone, Copy)]
struct ChartPoint {
    up: f64,
    down: f64,
    /// Seconds before the newest sample (samples come once a second).
    age: u16,
    /// False for the empty slots left of the first sample of this run.
    sampled: bool,
}

/// One point per slot of the full window, newest at the right edge: the
/// history right-aligned, zero-height unsampled slots before it. Keeping the
/// width fixed makes the chart scroll like a monitor from the first second
/// instead of stretching while the history fills.
fn chart_points(history: &VecDeque<RatePoint>) -> Vec<ChartPoint> {
    let empty = HISTORY_LEN.saturating_sub(history.len());
    let unsampled = (0..empty).map(|_| (RatePoint::default(), false));
    let sampled = history.iter().map(|point| (*point, true));
    unsampled
        .chain(sampled)
        .enumerate()
        .map(|(i, (point, sampled))| ChartPoint {
            up: point.up as f64,
            down: point.down as f64,
            age: (HISTORY_LEN.max(history.len()) - 1 - i) as u16,
            sampled,
        })
        .collect()
}

fn legend_item(
    theme: &Theme,
    icon: &'static str,
    color: Hsla,
    label: &'static str,
    value: String,
) -> Div {
    div()
        .h_flex()
        .items_center()
        .gap_1()
        .whitespace_nowrap()
        .child(Icon::default().path(icon).small().text_color(color))
        .child(div().text_color(theme.muted_foreground).child(label))
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.foreground)
                .child(value),
        )
}

pub struct TrafficChart {
    traffic: Entity<Traffic>,
}

impl TrafficChart {
    pub fn new(traffic: Entity<Traffic>, cx: &mut Context<Self>) -> Self {
        cx.observe(&traffic, |_, _, cx| cx.notify()).detach();
        Self { traffic }
    }
}

impl Render for TrafficChart {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let traffic = self.traffic.read(cx);
        let theme = cx.theme();
        let colors = series_colors(theme);
        let history = traffic.history();
        let peak = history.iter().map(|p| p.up.max(p.down)).max().unwrap_or(0);
        let top = axis_max(peak) as f64;
        let data = chart_points(history);

        // Soft fill fading to the baseline, so the overlapping upload area
        // leaves the download line readable underneath.
        let fill = |color: Hsla| {
            linear_gradient(
                180.,
                linear_color_stop(color.opacity(0.35), 0.),
                linear_color_stop(color.opacity(0.05), 1.),
            )
        };

        let chart = AreaChart::new(data)
            .x(|_: &ChartPoint| SharedString::default())
            .x_axis(false)
            .y(|p: &ChartPoint| p.down)
            .stroke(colors.download)
            .fill(fill(colors.download))
            .name(crate::i18n::s().common.download)
            .y(|p: &ChartPoint| p.up)
            .stroke(colors.upload)
            .fill(fill(colors.upload))
            .name(crate::i18n::s().common.upload)
            .y_domain(0., top)
            .y_padding(0., 0.)
            .y_axis(true)
            .y_axis_label_placement(AxisLabelPlacement::Outside)
            .y_tick_count(3)
            .y_tick_format(tick_label)
            .tooltip_title(|p: &ChartPoint| age_label(p.age))
            .tooltip_value(|p: &ChartPoint, _, value| {
                if p.sampled {
                    format_speed(value as u64).into()
                } else {
                    "—".into()
                }
            });

        div()
            .v_flex()
            .w_full()
            .h(px(HEIGHT))
            .gap(px(GAP))
            .child(
                div()
                    .h(px(LEGEND_HEIGHT))
                    .h_flex()
                    .items_center()
                    .gap_4()
                    .text_xs()
                    .child(
                        legend_item(
                            theme,
                            "icons/arrow-down.svg",
                            colors.download,
                            crate::i18n::s().common.download,
                            format_speed(traffic.down),
                        )
                        .min_w(px(LEGEND_ITEM_MIN_WIDTH)),
                    )
                    .child(legend_item(
                        theme,
                        "icons/arrow-up.svg",
                        colors.upload,
                        crate::i18n::s().common.upload,
                        format_speed(traffic.up),
                    ))
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_color(theme.muted_foreground)
                            .whitespace_nowrap()
                            .child(crate::i18n::s().chart.last_two_minutes),
                    ),
            )
            // Clip at the baseline only: the smoothed curve dips a few pixels
            // below zero right after a spike, which the chart's own mask lets
            // through. The clip box reaches past the plot's top and sides so
            // hover dots on those edges stay whole (the tooltip box is
            // deferred and unaffected).
            .child(
                div()
                    .mt(px(-CLIP_BLEED))
                    .mx(px(-CLIP_BLEED))
                    .pt(px(CLIP_BLEED))
                    .px(px(CLIP_BLEED))
                    .overflow_hidden()
                    .child(div().w_full().h(px(PLOT_HEIGHT)).child(chart)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;

    #[test]
    fn axis_max_steps_through_round_values_with_headroom() {
        assert_eq!(axis_max(0), 10 * KB);
        assert_eq!(axis_max(9 * KB), 10 * KB);
        assert_eq!(axis_max(10 * KB), 20 * KB); // 5% headroom
        assert_eq!(axis_max(150 * KB), 200 * KB);
        assert_eq!(axis_max(470 * KB), 500 * KB);
        assert_eq!(axis_max(480 * KB), MB); // 504 KB with headroom
        assert_eq!(axis_max(600 * KB), MB);
        assert_eq!(axis_max(3 * MB), 5 * MB);
        assert_eq!(axis_max(u64::MAX), u64::MAX);
    }

    #[test]
    fn tick_labels_drop_a_trailing_zero_decimal() {
        assert_eq!(tick_label(0.), "0 B/s");
        assert_eq!(tick_label((MB / 2) as f64), "512 KB/s");
        assert_eq!(tick_label(MB as f64), "1 MB/s");
        assert_eq!(tick_label((5 * KB / 2) as f64), "2.5 KB/s");
    }

    #[test]
    fn age_labels() {
        assert_eq!(age_label(0), "Now");
        assert_eq!(age_label(45), "45 s ago");
        assert_eq!(age_label(60), "1 min ago");
        assert_eq!(age_label(65), "1 min 5 s ago");
    }

    #[test]
    fn chart_points_right_align_the_history() {
        let history: VecDeque<_> = [RatePoint { up: 1, down: 2 }, RatePoint { up: 3, down: 4 }]
            .into_iter()
            .collect();
        let points = chart_points(&history);
        assert_eq!(points.len(), HISTORY_LEN);
        assert!(points[..HISTORY_LEN - 2]
            .iter()
            .all(|p| !p.sampled && p.up == 0.));
        let last = points[HISTORY_LEN - 1];
        assert!(last.sampled);
        assert_eq!((last.up, last.down, last.age), (3., 4., 0));
        assert_eq!(points[0].age as usize, HISTORY_LEN - 1);
    }
}
