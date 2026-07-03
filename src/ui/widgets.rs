//! Shared chrome builders beside `card_frame`: page titles, empty states,
//! labeled setting rows, and pill badges. Pure element builders — one place
//! to restyle what every page repeats.

use crate::ui::card_frame;
use gpui::{div, Div, FontWeight, ParentElement, Styled};
use gpui_component::{theme::Theme, Icon, IconName, Sizable, StyledExt};

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
/// control appended as the right-hand child.
pub fn setting_row(theme: &Theme, label: &'static str, description: Option<&'static str>) -> Div {
    div()
        .h_flex()
        .items_center()
        .justify_between()
        .w_full()
        .child(
            div()
                .v_flex()
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
