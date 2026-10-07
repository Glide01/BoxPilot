//! Shared chrome builders beside `card_frame`: page titles, empty states,
//! labeled setting rows, grouped lists, segmented controls, quiet status
//! labels and the subscription usage meter. Pure element builders — one
//! place to restyle what every page repeats.
//!
//! House style: related facts sit side by side, set apart by space (see
//! [`meta_row`]) or on lines of their own — never strung together with
//! " · ". State reads as coloured text or a small dot beside it
//! ([`status_label`]), not as filled badges.

use crate::actions::ToggleProcess;
use crate::core::sub_usage::{expiry_date_utc, SubscriptionUsage, UsageLevel};
use crate::core::timefmt::{format_relative_time, from_unix_secs, to_unix_secs};
use crate::i18n::s;
use crate::ui::card_frame;
use gpui::{
    div, prelude::FluentBuilder, px, AnyElement, App, Context, Div, ElementId, FontWeight, Hsla,
    InteractiveElement, IntoElement, ParentElement, SharedString, Stateful,
    StatefulInteractiveElement, Styled, Task, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    progress::Progress,
    theme::Theme,
    tooltip::Tooltip,
    Icon, IconName, Sizable, StyledExt,
};
use std::rc::Rc;
use std::time::{Duration, SystemTime};

/// Page title ("Groups", "Logs", …). Pages compose it into their own header
/// row (some add counts or buttons beside it).
pub fn page_header(theme: &Theme, title: &'static str) -> Div {
    div()
        .text_xl()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.foreground)
        .child(title)
}

/// Small heading above a group of cards or rows ("General", "Network").
pub fn section_heading(theme: &Theme, title: &'static str) -> Div {
    div()
        .px_1()
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme.muted_foreground)
        .child(title)
}

/// Centered empty state: the icon in a soft disc, a title and a hint. Fills
/// the remaining page height (`flex_1`) without a frame of its own — an
/// empty page needs no box around its emptiness. Callers may append an
/// action button.
pub fn empty_state(
    theme: &Theme,
    icon: impl Into<Icon>,
    title: &'static str,
    subtitle: &'static str,
) -> Div {
    div()
        .v_flex()
        .flex_1()
        .w_full()
        .items_center()
        .justify_center()
        .gap_2()
        .pb_8()
        .child(
            div()
                .size(px(56.))
                .mb_2()
                .rounded_full()
                .bg(theme.muted)
                .flex()
                .items_center()
                .justify_center()
                .child(icon.into().size(px(24.)).text_color(theme.muted_foreground)),
        )
        .child(
            div()
                .text_base()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.foreground)
                .child(title),
        )
        .child(
            div()
                .max_w(px(360.))
                .text_center()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(subtitle),
        )
}

/// The primary "Connect" action an empty state offers when what it lacks
/// only exists while sing-box runs. Dispatches `ToggleProcess`, the same
/// action as the Home power button and its shortcut.
pub fn connect_button(id: &'static str) -> Div {
    div().mt_3().child(
        Button::new(id)
            .primary()
            .icon(Icon::default().path("icons/power.svg"))
            .label(s().status.connect)
            .on_click(|_, window, cx| window.dispatch_action(Box::new(ToggleProcess), cx)),
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
                .gap_0p5()
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

/// Rows stacked in one card, a hairline between neighbours (a grouped
/// list). Each row gets the card's side padding and its own vertical
/// padding; the card itself has none, so the dividers run edge to edge.
pub fn grouped_card(theme: &Theme, rows: impl IntoIterator<Item = AnyElement>) -> Div {
    let border = theme.border;
    card_frame(theme)
        .p_0()
        .gap_0()
        .children(rows.into_iter().enumerate().map(move |(ix, row)| {
            div()
                .w_full()
                .px_4()
                .py_3()
                .when(ix > 0, |this| this.border_t_1().border_color(border))
                .child(row)
                .into_any_element()
        }))
}

/// Secondary facts in one line, set apart by space rather than separators:
/// `12.0 GB    from laptop    5 min ago`. Each item truncates on its own
/// when the line runs short.
pub fn meta_row<T: Into<SharedString>>(theme: &Theme, items: impl IntoIterator<Item = T>) -> Div {
    div()
        .h_flex()
        .items_center()
        .gap_3()
        .min_w_0()
        .text_xs()
        .text_color(theme.muted_foreground)
        .children(items.into_iter().map(|item| {
            div()
                .min_w_0()
                .flex_shrink(1.)
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .child(item.into())
        }))
}

/// A state in words with a small dot of its colour in front ("Active",
/// "Closed", "Running") — the quiet replacement for a filled badge.
pub fn status_label(color: Hsla, text: impl Into<SharedString>) -> Div {
    div()
        .h_flex()
        .flex_shrink_0()
        .items_center()
        .gap_1p5()
        .text_xs()
        .text_color(color)
        .child(div().size(px(6.)).rounded_full().bg(color))
        .child(text.into())
}

/// One choice of a [`segmented`] control.
pub struct Segment {
    pub label: SharedString,
    pub tooltip: Option<&'static str>,
}

impl Segment {
    pub fn new(label: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
            tooltip: None,
        }
    }

    pub fn tooltip(mut self, tooltip: &'static str) -> Self {
        self.tooltip = Some(tooltip);
        self
    }
}

/// A compact segmented control (sort orders, views, log levels): a muted
/// track, the selected choice raised on it. Same look as gpui-component's
/// segmented `TabBar`, sized for toolbars, and with per-choice tooltips.
pub fn segmented(
    theme: &Theme,
    id: impl Into<ElementId>,
    segments: Vec<Segment>,
    selected: Option<usize>,
    on_select: impl Fn(usize, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let on_select = Rc::new(on_select);
    let (fg, muted_fg, raised) = (theme.foreground, theme.muted_foreground, theme.background);
    div()
        .id(id)
        .h_flex()
        .flex_shrink_0()
        .items_center()
        .gap_0p5()
        .p_0p5()
        .rounded(theme.radius)
        .bg(theme.tab_bar_segmented)
        .children(segments.into_iter().enumerate().map(|(ix, segment)| {
            let active = selected == Some(ix);
            let on_select = on_select.clone();
            div()
                .id(ix)
                .h(px(24.))
                .px_2p5()
                .flex()
                .items_center()
                .rounded(theme.radius - px(2.))
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .whitespace_nowrap()
                .cursor_pointer()
                .map(|this| {
                    if active {
                        this.bg(raised).text_color(fg).shadow_xs()
                    } else {
                        this.text_color(muted_fg).hover(move |s| s.text_color(fg))
                    }
                })
                .when_some(segment.tooltip, |this, tip| {
                    this.tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
                })
                .on_click(move |_, window, cx| on_select(ix, window, cx))
                .child(segment.label)
        }))
}

/// A short stat: caption over a larger value ("Memory / 48.2 MB").
pub fn stat(theme: &Theme, label: &'static str, value: impl Into<SharedString>) -> Div {
    div()
        .v_flex()
        .flex_1()
        .min_w_0()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .whitespace_nowrap()
                .child(label),
        )
        .child(
            div()
                .text_lg()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.foreground)
                .whitespace_nowrap()
                .child(value.into()),
        )
}

/// `text` with its first letter capitalized, for a phrase made to sit
/// inside a sentence ("expires in 3 days") shown on its own. No-op for
/// scripts without case.
pub fn capitalize_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
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

/// Subscription usage: "44.7 GB / 100.0 GB" with the expiry ("Expires in
/// 22 days") at the other end of the line, over a thin bar of the
/// allowance used (only with a known allowance). Coloured by level, with a
/// level icon when it needs attention; fills its container's width. Hover
/// shows the expiry date and how old the reading is.
pub fn usage_meter(
    theme: &Theme,
    id: impl Into<ElementId>,
    usage: &SubscriptionUsage,
    now: SystemTime,
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
    let t = s();
    let mut tooltip = Vec::new();
    if let Some(expire) = usage.expire {
        tooltip.push((t.usage.expires_on)(&expiry_date_utc(expire)));
    }
    tooltip.push((t.usage.as_of)(&format_relative_time(
        from_unix_secs(usage.fetched_at),
        now,
    )));
    let tooltip: SharedString = tooltip.join("\n").into();
    let expiry = usage
        .expiry_label(now_secs)
        .map(|label| capitalize_first(&label));

    div()
        .id(id.clone())
        .v_flex()
        .gap_1p5()
        .w_full()
        .min_w_0()
        .child(
            div()
                .h_flex()
                .items_center()
                .gap_3()
                .w_full()
                .text_xs()
                .child(
                    div()
                        .h_flex()
                        .items_center()
                        .gap_1()
                        .flex_1()
                        .min_w_0()
                        .text_color(label_color)
                        .children(
                            icon.map(|icon| Icon::new(icon).xsmall().flex_none().text_color(color)),
                        )
                        .child(div().min_w_0().truncate().child(usage.traffic_label())),
                )
                .children(expiry.map(|expiry| {
                    div()
                        .flex_shrink_0()
                        .whitespace_nowrap()
                        .text_color(label_color)
                        .child(expiry)
                })),
        )
        .children(usage.fraction_used().map(|fraction| {
            Progress::new(id)
                .small()
                .w_full()
                .color(color)
                .value(fraction * 100.)
        }))
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
}

#[cfg(test)]
mod tests {
    use super::capitalize_first;

    #[test]
    fn capitalize_first_touches_only_the_first_letter() {
        assert_eq!(capitalize_first("expires in 3 days"), "Expires in 3 days");
        assert_eq!(capitalize_first("12 天后到期"), "12 天后到期");
        assert_eq!(capitalize_first(""), "");
    }
}
