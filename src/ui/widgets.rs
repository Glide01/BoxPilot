//! Shared chrome builders beside `card_frame`: page titles, empty states,
//! labeled setting rows, pill badges and the subscription usage meter. Pure
//! element builders — one place to restyle what every page repeats.

use crate::core::sub_usage::{expiry_date_utc, SubscriptionUsage, UsageLevel};
use crate::core::timefmt::{format_relative_time, from_unix_secs, to_unix_secs};
use crate::ui::card_frame;
use gpui::{
    div, Context, Div, ElementId, FontWeight, Hsla, InteractiveElement, ParentElement, Pixels,
    SharedString, Stateful, StatefulInteractiveElement, Styled, Task,
};
use gpui_component::{
    progress::Progress, theme::Theme, tooltip::Tooltip, Icon, IconName, Sizable, StyledExt,
};
use std::time::{Duration, SystemTime};

/// Page title ("Groups", "Logs", …). Pages compose it into their own header
/// row (some add counts or buttons beside it).
pub fn page_header(theme: &Theme, title: &'static str) -> Div {
    div()
        .text_lg()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.foreground)
        .child(title)
}

/// Centered empty-state card: icon + title + hint. Fills the remaining page
/// height (`flex_1`).
pub fn empty_card(
    theme: &Theme,
    icon: IconName,
    title: &'static str,
    subtitle: &'static str,
) -> Div {
    card_frame(theme)
        .flex_1()
        .items_center()
        .justify_center()
        .gap_2()
        .child(Icon::new(icon).large().text_color(theme.muted_foreground))
        .child(
            div()
                .text_sm()
                .text_color(theme.foreground)
                .child(title),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(subtitle),
        )
}

/// A labeled settings row: label (+ optional hint) on the left, the caller's
/// control appended as the right-hand child. The text column takes the
/// leftover width and wraps, so a long hint never pushes the control out of
/// its card.
pub fn setting_row(theme: &Theme, label: &'static str, description: Option<&'static str>) -> Div {
    div()
        .h_flex()
        .items_center()
        .justify_between()
        .gap_4()
        .w_full()
        .child(
            div()
                .v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.foreground)
                        .child(label),
                )
                .children(description.map(|text| {
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(text)
                })),
        )
}

/// Pill badge tone: `Primary` = the blue "Active" badge, `Muted` = the grey
/// "auto" badge.
#[derive(Clone, Copy)]
pub enum PillTone {
    Primary,
    Muted,
}

pub fn pill(theme: &Theme, tone: PillTone, text: &'static str) -> Div {
    let base = div().flex_shrink_0().text_xs().px_2().rounded_full();
    match tone {
        PillTone::Primary => base.bg(theme.primary).text_color(gpui::white()),
        PillTone::Muted => base.bg(theme.muted).text_color(theme.muted_foreground),
    }
    .child(text)
}

/// Notify `cx`'s view once a minute for as long as the returned task is
/// held — for labels that move with the clock rather than with any entity
/// ("updated 5 min ago", "expires in 3 days"), which a cached view would
/// otherwise leave stale.
pub fn minute_ticker<T: 'static>(cx: &mut Context<T>) -> Task<()> {
    cx.spawn(async move |this, cx| loop {
        cx.background_executor().timer(Duration::from_secs(60)).await;
        if this.update(cx, |_, cx| cx.notify()).is_err() {
            break;
        }
    })
}

/// The colour a subscription's usage reads in: accent while fine, the
/// theme's warning / danger colours as it runs out.
pub fn usage_color(theme: &Theme, level: UsageLevel) -> Hsla {
    match level {
        UsageLevel::Normal => theme.primary,
        UsageLevel::Warning => theme.warning,
        UsageLevel::Critical => theme.danger,
    }
}

/// Subscription traffic / expiry on one line: a `bar_width`-wide usage bar
/// (only with a known allowance) + "12.0 GB / 100.0 GB · expires in 12 days",
/// coloured by level and truncated to the space left. Hover shows the expiry
/// date and how old the reading is.
pub fn usage_meter(
    theme: &Theme,
    id: impl Into<ElementId>,
    usage: &SubscriptionUsage,
    now: SystemTime,
    bar_width: Pixels,
) -> Stateful<Div> {
    let id = id.into();
    let now_secs = to_unix_secs(now).unwrap_or(0);
    let level = usage.level(now_secs);
    let color = usage_color(theme, level);
    // Colour alone is easy to miss (and yellow text is faint): a level icon
    // says it too.
    let (label_color, icon) = match level {
        UsageLevel::Normal => (theme.muted_foreground, None),
        UsageLevel::Warning => (color, Some(IconName::TriangleAlert)),
        UsageLevel::Critical => (color, Some(IconName::CircleX)),
    };
    let mut tooltip = Vec::new();
    if let Some(expire) = usage.expire {
        tooltip.push(format!("Expires {} (UTC)", expiry_date_utc(expire)));
    }
    tooltip.push(format!(
        "Usage as of {}",
        format_relative_time(from_unix_secs(usage.fetched_at), now)
    ));
    let tooltip: SharedString = tooltip.join(" · ").into();

    div()
        .id(id.clone())
        .h_flex()
        .items_center()
        .gap_2()
        .w_full()
        .min_w_0()
        .children(usage.fraction_used().map(|fraction| {
            Progress::new(id)
                .small()
                .w(bar_width)
                .flex_none()
                .color(color)
                .value(fraction * 100.)
        }))
        .children(icon.map(|icon| {
            Icon::new(icon)
                .xsmall()
                .flex_none()
                .text_color(color)
        }))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_xs()
                .text_color(label_color)
                .truncate()
                .child(usage.usage_label(now_secs)),
        )
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
}
