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
//!
//! In the Route section each selector group of the chain is a quiet button
//! that opens the group's nodes, the current one checked; picking one
//! switches the group (`ProxyGroups::select`, whose failures toast) as the
//! Groups page does. Which hops switch is `chain_switches`, against the
//! live groups of `ProxyGroups` (also observed): while sing-box is stopped,
//! or for a group the running config no longer has, the chain is text.

use super::connections::unix_millis_now;
use crate::core::connection_details::{
    chain_switches, connection_details, ChainHop, DetailField, DetailGroup,
};
use crate::core::connections_view::{host_label, CHAIN_SEPARATOR};
use crate::core::timefmt::format_local_datetime;
use crate::i18n::s;
use crate::state::proxy_groups::{GroupSource, ProxyGroups};
use crate::state::Connections;
use crate::ui::widgets::{shorten, status_label, TextLabel};
use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{
    button::{Button, ButtonVariants},
    menu::{DropdownMenu, PopupMenu, PopupMenuItem},
    scroll::ScrollableElement,
    ActiveTheme, Icon, IconName, Sizable, StyledExt,
};
use std::time::Duration;

/// How long a copy button reads "Copied".
const COPIED_FOR: Duration = Duration::from_millis(1500);
/// Width of the label column.
const LABEL_WIDTH: f32 = 84.;
/// A group's node menu: about this many Latin letters of a node's name,
/// and no taller than this before it scrolls.
const NODE_MENU_NAME_ROOM: usize = 36;
const NODE_MENU_MAX_HEIGHT: f32 = 320.;

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
        /// The chain hop by hop, for the Route section's switchers.
        hops: Vec<ChainHop>,
    },
    /// Selected, but sing-box no longer has it (evicted from the closed list).
    Gone { title: String },
}

pub struct ConnectionDetailsPanel {
    connections: Entity<Connections>,
    proxy_groups: Entity<ProxyGroups>,
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
    pub fn new(
        connections: Entity<Connections>,
        proxy_groups: Entity<ProxyGroups>,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&connections, |this: &mut Self, _, cx| this.refresh(cx))
            .detach();
        // Groups come and go with sing-box: the chain's switchers too.
        cx.observe(&proxy_groups, |this: &mut Self, _, cx| this.refresh(cx))
            .detach();
        Self {
            connections,
            proxy_groups,
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

    /// Re-render only when what is shown moved.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let next = self.compute(cx);
        if next != self.shown {
            self.shown = next;
            cx.notify();
        }
    }

    fn compute(&self, cx: &App) -> Option<Shown> {
        let id = self.selected.as_ref()?;
        let shown = match self.connections.read(cx).table.get(id) {
            Some(connection) => Shown::Details {
                title: host_label(connection),
                closed: connection.is_closed(),
                groups: connection_details(connection, unix_millis_now(), format_local_datetime),
                hops: {
                    let groups = self.proxy_groups.read(cx);
                    let live = match groups.source {
                        GroupSource::Api => groups.groups.as_slice(),
                        GroupSource::Inactive => &[],
                    };
                    chain_switches(connection, live)
                },
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

    fn section(&self, group: &DetailGroup, hops: &[ChainHop], cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let heading = div()
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme.muted_foreground)
            .child(group.section.label());
        let mut section = div().v_flex().gap_1().child(heading);
        for row in &group.rows {
            let switchable = row.field == DetailField::Chain && hops.iter().any(|h| h.switchable);
            let value = if switchable {
                self.chain_value(hops, cx).into_any_element()
            } else {
                row.value.clone().into_any_element()
            };
            section = section.child(self.field_row(row.field, &row.value, value, cx));
        }
        section
    }

    /// The chain with each switchable group as a button that opens its
    /// nodes, the current one checked. The menu is filled when it opens,
    /// from the groups as they are then.
    fn chain_value(&self, hops: &[ChainHop], cx: &mut Context<Self>) -> Div {
        let muted = cx.theme().muted_foreground;
        let t = &s().connection_details;
        // Each later hop keeps its arrow on its line when the chain wraps.
        let mut value = div().h_flex().flex_wrap().items_center().gap_x_1p5();
        for (ix, hop) in hops.iter().enumerate() {
            let name = if hop.switchable {
                let groups = self.proxy_groups.clone();
                let group = hop.tag.clone();
                // Pulled left by its padding, so its name sits where the
                // text would, and a little closer to the next arrow.
                div()
                    .ml(px(-8.))
                    .mr(px(-4.))
                    .child(
                        Button::new(SharedString::from(format!("conn-hop-{ix}")))
                            .ghost()
                            .small()
                            .dropdown_caret(true)
                            .text_label(hop.tag.clone())
                            .tooltip(t.switch_node)
                            .dropdown_menu(move |menu, _, cx| node_menu(menu, &groups, &group, cx)),
                    )
                    .into_any_element()
            } else {
                div().child(hop.tag.clone()).into_any_element()
            };
            value = value.child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_1p5()
                    .when(ix > 0, |hop| {
                        hop.child(div().text_color(muted).child(CHAIN_SEPARATOR.trim()))
                    })
                    .child(name),
            );
        }
        value
    }

    /// Label, value (wrapping, never clipped) and the copy button of the
    /// value's text, `text`.
    fn field_row(
        &self,
        field: DetailField,
        text: &str,
        value: AnyElement,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = cx.theme();
        let copied = self.copied == Some(field);
        let common = &s().common;
        let copy_value = text.to_string();
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
                    .child(value),
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

/// `group`'s nodes, the current one checked; picking one switches the
/// group. Built as the menu opens, from the groups as they are then.
fn node_menu(menu: PopupMenu, groups: &Entity<ProxyGroups>, group: &str, cx: &App) -> PopupMenu {
    let menu = menu
        .min_w(px(160.))
        .max_h(px(NODE_MENU_MAX_HEIGHT))
        .scrollable(true);
    let Some(entry) = groups.read(cx).groups.iter().find(|g| g.name == group) else {
        return menu;
    };
    entry.all.iter().fold(menu, |menu, node| {
        let groups = groups.clone();
        let group = group.to_string();
        let node = node.clone();
        menu.item(
            PopupMenuItem::new(shorten(&node, NODE_MENU_NAME_ROOM))
                .checked(node == entry.now)
                .on_click(move |_, _, cx| {
                    groups.update(cx, |groups, cx| {
                        groups.select(group.clone(), node.clone(), cx)
                    });
                }),
        )
    })
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
                hops,
            } => {
                let header = self.header(&title, Some(closed), cx);
                let mut content = div().v_flex().gap_4().px_4().py_3();
                for group in &groups {
                    content = content.child(self.section(group, &hops, cx));
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
