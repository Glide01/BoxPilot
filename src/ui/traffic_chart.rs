//! Home page live rate charts: the last two minutes of the upload or the
//! download rate, one area each, filling the foot of its rate card.
//!
//! Its own view so it can be embedded `.cached()`: it re-renders once per
//! status sample (it observes `Traffic`), and the chart's hover crosshair —
//! which notifies the view painting it on every mouse move — repaints only
//! this view, not all of Home.

use crate::core::bytefmt::{format_bytes, format_speed};
use crate::state::traffic::{RatePoint, HISTORY_LEN};
use crate::state::Traffic;
use gpui::{
    div, linear_color_stop, linear_gradient, px, rgb, Context, Entity, Hsla, IntoElement,
    ParentElement, Render, SharedString, Styled, Window,
};
use gpui_component::{chart::AreaChart, theme::Theme, ActiveTheme};
use std::collections::VecDeque;

/// The plot's height.
pub const HEIGHT: f32 = 64.;

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

/// A gridline label for an [`axis_max`] amount of bytes (the Home traffic
/// chart's per-minute scale): `format_bytes` without a trailing ".0".
pub fn axis_label(bytes: u64) -> SharedString {
    format_bytes(bytes).replace(".0 ", " ").into()
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
    rate: f64,
    /// Seconds before the newest sample (samples come once a second).
    age: u16,
    /// False for the empty slots left of the first sample of this run.
    sampled: bool,
}

/// Which rate a [`RateChart`] plots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Upload,
    Download,
}

impl Direction {
    pub fn rate(self, point: &RatePoint) -> u64 {
        match self {
            Direction::Upload => point.up,
            Direction::Download => point.down,
        }
    }

    pub fn color(self, colors: SeriesColors) -> Hsla {
        match self {
            Direction::Upload => colors.upload,
            Direction::Download => colors.download,
        }
    }
}

/// One point per slot of the full window, newest at the right edge: the
/// history right-aligned, zero-height unsampled slots before it. Keeping the
/// width fixed makes the chart scroll like a monitor from the first second
/// instead of stretching while the history fills.
fn chart_points(history: &VecDeque<RatePoint>, direction: Direction) -> Vec<ChartPoint> {
    let empty = HISTORY_LEN.saturating_sub(history.len());
    let unsampled = (0..empty).map(|_| (0, false));
    let sampled = history.iter().map(|point| (direction.rate(point), true));
    unsampled
        .chain(sampled)
        .enumerate()
        .map(|(i, (rate, sampled))| ChartPoint {
            rate: rate as f64,
            age: (HISTORY_LEN.max(history.len()) - 1 - i) as u16,
            sampled,
        })
        .collect()
}

/// The highest `direction` rate of the window, bytes/sec.
pub fn peak(history: &VecDeque<RatePoint>, direction: Direction) -> u64 {
    history
        .iter()
        .map(|point| direction.rate(point))
        .max()
        .unwrap_or(0)
}

pub struct RateChart {
    traffic: Entity<Traffic>,
    direction: Direction,
}

impl RateChart {
    pub fn new(traffic: Entity<Traffic>, direction: Direction, cx: &mut Context<Self>) -> Self {
        cx.observe(&traffic, |_, _, cx| cx.notify()).detach();
        Self { traffic, direction }
    }
}

impl Render for RateChart {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let history = self.traffic.read(cx).history();
        let color = self.direction.color(series_colors(cx.theme()));
        let top = axis_max(peak(history, self.direction)) as f64;
        let data = chart_points(history, self.direction);

        // A soft fill fading to the card's foot.
        let fill = linear_gradient(
            180.,
            linear_color_stop(color.opacity(0.3), 0.),
            linear_color_stop(color.opacity(0.02), 1.),
        );
        let chart = AreaChart::new(data)
            .x(|_: &ChartPoint| SharedString::default())
            .x_axis(false)
            .grid(false)
            .y(|p: &ChartPoint| p.rate)
            .stroke(color)
            .fill(fill)
            .y_domain(0., top)
            .y_padding(0., 0.)
            .tooltip_title(|p: &ChartPoint| age_label(p.age))
            .tooltip_value(|p: &ChartPoint, _, value| {
                if p.sampled {
                    format_speed(value as u64).into()
                } else {
                    "—".into()
                }
            });
        // Clip at the baseline: the smoothed curve dips a few pixels below
        // zero right after a spike, which the chart's own mask lets through.
        div()
            .w_full()
            .h(px(HEIGHT))
            .overflow_hidden()
            .child(div().size_full().child(chart))
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
    fn axis_labels_drop_a_trailing_zero_decimal() {
        assert_eq!(axis_label(0), "0 B");
        assert_eq!(axis_label(MB / 2), "512 KB");
        assert_eq!(axis_label(MB), "1 MB");
        assert_eq!(axis_label(5 * KB / 2), "2.5 KB");
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
        let points = chart_points(&history, Direction::Download);
        assert_eq!(points.len(), HISTORY_LEN);
        assert!(points[..HISTORY_LEN - 2]
            .iter()
            .all(|p| !p.sampled && p.rate == 0.));
        let last = points[HISTORY_LEN - 1];
        assert!(last.sampled);
        assert_eq!((last.rate, last.age), (4., 0));
        assert_eq!(points[0].age as usize, HISTORY_LEN - 1);
        assert_eq!(
            chart_points(&history, Direction::Upload)[HISTORY_LEN - 1].rate,
            3.
        );
        assert_eq!(peak(&history, Direction::Upload), 3);
        assert_eq!(peak(&VecDeque::new(), Direction::Upload), 0);
    }
}
