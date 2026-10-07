//! The Connections page's details panel: one connection's fields in
//! sections (Overview, Route, Source, Process, Traffic — the field list is
//! `core::connection_details`), each value with its own copy button, and
//! Close connection at the bottom while it is open.
//!
//! Its own entity, drawn by the page as a cached view: the page re-renders
//! on every connection-table change, the panel only when what it shows
//! changed (it observes `Connections` itself and compares). It follows the
//! connection live — rates, totals, duration, then "closed" — and says so
//! when sing-box no longer has it at all.

use super::connections::unix_millis_now;
use crate::core::connection_details::{connection_details, DetailField, DetailGroup};
use crate::core::connections_view::host_label;
use crate::core::timefmt::format_local_datetime;
use crate::i18n::s;
use crate::state::Connections;
use crate::ui::widgets::{status_label, TextLabel};
use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{
    button::{Button, ButtonVariants},
    scroll::ScrollableElement,
    ActiveTheme, Icon, IconName, Sizable, StyledExt,
};
use std::time::Duration;

/// How long a copy button reads "Copied".
const COPIED_FOR: Duration = Duration::from_millis(1500);
/// Width of the label column.
const LABEL_WIDTH: f32 = 84.;

/// The panel asks the page to close it (its ✕).
pub struct DetailsDismissed;

/// What the panel shows. Compared on every table change, so the panel only
/// re-renders when this moved.
#[derive(Clone, PartialEq)]
enum Shown {
    Details {
        title: String,
        closed: bool,
        groups: Vec<DetailGroup>,
    },
    /// Selected, but sing-box no longer has it (evicted from the closed list).
    Gone { title: String },
}

pub struct ConnectionDetailsPanel {
    connections: Entity<Connections>,
    /// Id of the connection shown; `None` while the panel is closed.
    selected: Option<String>,
    shown: Option<Shown>,
    /// The field whose copy button reads "Copied" right now.
    copied: Option<DetailField>,
    _copied_reset: Option<Task<()>>,
    scroll: ScrollHandle,
}

impl EventEmitter<DetailsDismissed> for ConnectionDetailsPanel {}

impl ConnectionDetailsPanel {
    pub fn new(connections: Entity<Connections>, cx: &mut Context<Self>) -> Self {
        cx.observe(&connections, |this: &mut Self, _, cx| {
            let next = this.compute(cx);
            if next != this.shown {
                this.shown = next;
                cx.notify();
            }
        })
        .detach();
        Self {
            connections,
            selected: None,
            shown: None,
            copied: None,
            _copied_reset: None,
            scroll: ScrollHandle::new(),
        }
    }

    /// Show `id` (or nothing). A different connection starts at the top.
    pub fn set_selected(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        if self.selected == id {
            return;
        }
        self.selected = id;
        self.copied = None;
        self._copied_reset = None;
        self.scroll.set_offset(point(px(0.), px(0.)));
        self.shown = self.compute(cx);
        cx.notify();
    }

    fn compute(&self, cx: &App) -> Option<Shown> {
        let id = self.selected.as_ref()?;
        let shown = match self.connections.read(cx).table.get(id) {
            Some(connection) => Shown::Details {
                title: host_label(connection),
                closed: connection.is_closed(),
                groups: connection_details(connection, unix_millis_now(), format_local_datetime),
            },
            None => Shown::Gone {
                title: match &self.shown {
                    Some(Shown::Details { title, .. } | Shown::Gone { title }) => title.clone(),
                    None => String::new(),
                },
            },
        };
        Some(shown)
    }

    fn copy(&mut self, field: DetailField, value: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(value));
        self.copied = Some(field);
        self._copied_reset = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COPIED_FOR).await;
            let _ = this.update(cx, |this, cx| {
                this.copied = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn close_connection(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.selected.clone() {
            self.connections.update(cx, |state, cx| state.close(id, cx));
        }
    }

    fn header(&self, title: &str, closed: Option<bool>, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let t = &s().connection_details;
        let state = closed.map(|closed| {
            if closed {
                status_label(theme.muted_foreground, t.closed)
            } else {
                status_label(theme.success, t.active)
            }
        });
        div()
            .h_flex()
            .items_start()
            .gap_2()
            .pl_4()
            .pr_2()
            .py_3()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .pt_0p5()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.foreground)
                    .child(title.to_string()),
            )
            .children(state.map(|state| state.mt_1()))
            .child(
                Button::new("conn-details-dismiss")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .tooltip(t.close_panel)
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DetailsDismissed))),
            )
    }

    fn section(&self, group: &DetailGroup, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let heading = div()
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme.muted_foreground)
            .child(group.section.label());
        let mut section = div().v_flex().gap_1().child(heading);
        for row in &group.rows {
            section = section.child(self.field_row(row.field, &row.value, cx));
        }
        section
    }

    /// Label, value (wrapping, never clipped) and the value's copy button.
    fn field_row(&self, field: DetailField, value: &str, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let copied = self.copied == Some(field);
        let common = &s().common;
        let copy_value = value.to_string();
        let button = Button::new(SharedString::from(format!("conn-copy-{field:?}")))
            .ghost()
            .xsmall()
            .icon(
                Icon::new(if copied {
                    IconName::Check
                } else {
                    IconName::Copy
                })
                .text_color(if copied {
                    theme.success
                } else {
                    theme.muted_foreground
                }),
            )
            .when(!copied, |button| button.tooltip(common.copy))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.copy(field, copy_value.clone(), cx);
            }));
        // "Copied" sits beside the button, over the value's end, so nothing
        // reflows for the moment it shows.
        let feedback = copied.then(|| {
            div()
                .absolute()
                .right(px(26.))
                .top(px(1.))
                .px_1p5()
                .rounded_sm()
                .whitespace_nowrap()
                .text_xs()
                // Inverted, like a tooltip: legible in light and dark.
                .bg(theme.foreground)
                .text_color(theme.background)
                .child(common.copied)
        });
        div()
            .h_flex()
            .items_start()
            .gap_2()
            .min_h(px(24.))
            .child(
                div()
                    .flex_shrink_0()
                    .w(px(LABEL_WIDTH))
                    .pt(px(3.))
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(field.label()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .pt(px(2.))
                    .text_sm()
                    .text_color(theme.foreground)
                    .child(value.to_string()),
            )
            .child(
                div()
                    .relative()
                    .flex_shrink_0()
                    .child(button)
                    .children(feedback),
            )
    }
}

impl Render for ConnectionDetailsPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // What is drawn is what later table changes are compared against.
        self.shown = self.compute(cx);
        let theme = cx.theme();
        let frame = div()
            .size_full()
            .v_flex()
            .overflow_hidden()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .shadow_lg();
        let Some(shown) = self.shown.clone() else {
            return frame;
        };
        let t = &s().connection_details;

        match shown {
            Shown::Gone { title } => frame.child(self.header(&title, None, cx)).child(
                div()
                    .flex_1()
                    .v_flex()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .px_6()
                    .child(
                        Icon::new(IconName::Info)
                            .large()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().foreground)
                            .child(t.gone_title),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_center()
                            .text_color(cx.theme().muted_foreground)
                            .child(t.gone_hint),
                    ),
            ),
            Shown::Details {
                title,
                closed,
                groups,
            } => {
                let header = self.header(&title, Some(closed), cx);
                let mut content = div().v_flex().gap_4().px_4().py_3();
                for group in &groups {
                    content = content.child(self.section(group, cx));
                }
                let body = div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("conn-details-body")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .child(content),
                    )
                    .vertical_scrollbar(&self.scroll);
                let theme = cx.theme();
                let footer = (!closed).then(|| {
                    div()
                        .h_flex()
                        .justify_end()
                        .px_4()
                        .py_2p5()
                        .border_t_1()
                        .border_color(theme.border)
                        .child(
                            Button::new("conn-details-close-connection")
                                .outline()
                                .danger()
                                .small()
                                .text_label(s().connections.close_connection)
                                .on_click(cx.listener(|this, _, _, cx| this.close_connection(cx))),
                        )
                });
                frame.child(header).child(body).children(footer)
            }
        }
    }
}
