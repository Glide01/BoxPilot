//! Groups page: toolbar (search box, Sort Default / Delay, Test all),
//! then every group as a card — title line (chevron + name + node count +
//! current node + test icon button) over a grid of node cards (name + delay
//! badge / protocol type).
//!
//! - Clicking a title line folds / unfolds the group; sing-box stores that
//!   (`ProxyGroups::set_expanded`), so it survives restarts. While a search
//!   is active every group with a match is shown open, without storing it,
//!   and groups without a match are hidden.
//! - Clicking a node card switches a selector group to it; urltest groups
//!   pick by latency and are read-only.
//! - Clicking a node's delay badge (or the zap icon that shows on hover
//!   while it has none) tests just that node; its badge spins meanwhile.
//! - sing-box stopped (or its API unreachable): no groups, empty state.
//!
//! The cards are rows of a `v_virtual_list`, so only what is on screen is
//! built — thousands of nodes stay smooth. Each card is drawn row by row
//! (title row with top border, node rows with side borders, last row closing
//! it). The row list comes from `core::groups_view::flatten_rows`, cached by
//! (groups revision, query, sort, column count); rows read the live state
//! by index when drawn, and nothing is cloned per render.

use crate::core::groups_view::{flatten_rows, normalize_query, GroupsLayout, NodeSort, Row};
use crate::core::singbox_api::{classify_delay, DelayLevel, GroupKind, ProxyGroup};
use crate::i18n::s;
use crate::state::{AppState, DelayState, GroupSource, ProxyGroups};
use crate::ui::locale;
use crate::ui::pages::ActivePage;
use crate::ui::theme::CARD_RADIUS;
use crate::ui::widgets::{
    control_input, empty_state, full_text_tooltip, page_header, page_layout, page_scrollbar,
    segmented, text_centered, toolbar_search, Control, ControlSize, IconLabel, Segment,
};
use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{
    button::{Button, ButtonVariants},
    input::{InputEvent, InputState},
    spinner::Spinner,
    theme::Theme,
    tooltip::Tooltip,
    v_virtual_list, ActiveTheme, Disableable, Icon, IconName, Sizable, StyledExt,
    VirtualListScrollHandle,
};
use std::rc::Rc;

/// Height of a group's title row.
const HEADER_HEIGHT: f32 = 46.;
/// Height of one node card.
const CARD_HEIGHT: f32 = 56.;
/// Space between node cards, both ways.
const CARD_GAP: f32 = 8.;
/// Inner padding of a group card (sides, and below its last node row).
const CARD_PADDING: f32 = 16.;
/// Space between two group cards.
const GROUP_GAP: f32 = 12.;
/// Node cards get at least this wide; the column count follows the width.
const MIN_CARD_WIDTH: f32 = 200.;
const MAX_COLUMNS: usize = 4;
/// Columns before the list has been measured once.
const DEFAULT_COLUMNS: usize = 2;
/// `group_hover` name of a node card (shows the test icon).
const NODE_CARD_GROUP: &str = "node-card";
/// Letters a node name has before the narrowest card may cut it (its
/// name, beside the delay, in `MIN_CARD_WIDTH`); longer ones get a
/// tooltip with the whole name.
const NODE_NAME_ROOM: usize = 16;
/// The same for a group's name in its header, which may take half of it.
const GROUP_NAME_ROOM: usize = 24;
/// Group of a group header's fold area: hovering it lights up the chevron.
const GROUP_TOGGLE_GROUP: &str = "group-toggle";

/// What the cached rows were derived from.
#[derive(Clone, PartialEq)]
struct RowsKey {
    revision: u64,
    query: String,
    sort: NodeSort,
    columns: usize,
}

pub struct GroupsPage {
    proxy_groups: Entity<ProxyGroups>,
    search: Entity<InputState>,
    sort: NodeSort,
    scroll: VirtualListScrollHandle,
    /// The list's width at the last paint; picks the column count.
    list_width: Pixels,
    layout: Rc<GroupsLayout>,
    /// Height of each row in `layout.rows`, for the virtual list.
    sizes: Rc<Vec<Size<Pixels>>>,
    rows_key: Option<RowsKey>,
}

impl GroupsPage {
    pub fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let proxy_groups = app_state.read(cx).proxy_groups.clone();
        cx.observe(&proxy_groups, |_, _, cx| cx.notify()).detach();

        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder(s().groups.search_placeholder));
        locale::observe(window, cx, |this: &mut Self, window, cx| {
            this.search.update(cx, |search, cx| {
                search.set_placeholder(s().groups.search_placeholder, window, cx)
            });
        })
        .detach();
        cx.subscribe_in(&search, window, |this, _, ev: &InputEvent, _, cx| {
            if matches!(ev, InputEvent::Change) {
                this.scroll.scroll_to_item(0, ScrollStrategy::Top);
                cx.notify();
            }
        })
        .detach();

        Self {
            proxy_groups,
            search,
            sort: NodeSort::default(),
            scroll: VirtualListScrollHandle::new(),
            list_width: px(0.),
            layout: Rc::new(GroupsLayout::default()),
            sizes: Rc::new(Vec::new()),
            rows_key: None,
        }
    }

    fn set_sort(&mut self, sort: NodeSort, cx: &mut Context<Self>) {
        if self.sort != sort {
            self.sort = sort;
            self.scroll.scroll_to_item(0, ScrollStrategy::Top);
            cx.notify();
        }
    }

    fn searching(&self) -> bool {
        self.rows_key.as_ref().is_some_and(|k| !k.query.is_empty())
    }

    /// Recompute the rows only when the groups, the query, the sort or the
    /// column count moved.
    fn refresh_rows(&mut self, cx: &App) {
        let state = self.proxy_groups.read(cx);
        let key = RowsKey {
            revision: state.revision,
            query: normalize_query(&self.search.read(cx).value()),
            sort: self.sort,
            columns: columns_for(self.list_width),
        };
        if self.rows_key.as_ref() == Some(&key) {
            return;
        }
        let layout = flatten_rows(
            &state.groups,
            &state.node_types,
            &state.delays,
            &key.query,
            key.sort,
            key.columns,
        );
        let sizes = layout
            .rows
            .iter()
            .map(|row| size(px(0.), px(row_height(row))))
            .collect();
        self.layout = Rc::new(layout);
        self.sizes = Rc::new(sizes);
        self.rows_key = Some(key);
    }

    /// One row of the virtual list, read from the live state.
    fn render_row(&self, ix: usize, cx: &App) -> AnyElement {
        let Some(&row) = self.layout.rows.get(ix) else {
            return div().into_any_element();
        };
        let state = self.proxy_groups.read(cx);
        let theme = cx.theme();
        match row {
            Row::Gap => div().h(px(GROUP_GAP)).into_any_element(),
            Row::Header {
                group,
                expanded,
                shown,
            } => match state.groups.get(group) {
                Some(entry) => self
                    .group_header(group, entry, expanded, shown, state, theme)
                    .into_any_element(),
                None => div().h(px(HEADER_HEIGHT)).into_any_element(),
            },
            Row::Nodes {
                group,
                start,
                end,
                last,
            } => {
                let Some(entry) = state.groups.get(group) else {
                    return div().h(px(row_height(&row))).into_any_element();
                };
                let columns = self.rows_key.as_ref().map_or(1, |k| k.columns);
                let live = state.source == GroupSource::Api;
                let selectable = live && entry.kind == GroupKind::Selector;
                let nodes = &self.layout.nodes[start..end];
                let mut line = div()
                    .h(px(row_height(&row)))
                    .w_full()
                    .px(px(CARD_PADDING))
                    .border_l_1()
                    .border_r_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .when(last, |line| line.border_b_1().rounded_b(px(CARD_RADIUS)))
                    .h_flex()
                    .items_start()
                    .gap(px(CARD_GAP))
                    .children(nodes.iter().filter_map(|&ni| {
                        let node = entry.all.get(ni)?;
                        Some(node_card(
                            NodeCard {
                                group,
                                index: ni,
                                group_name: &entry.name,
                                node,
                                node_type: state.node_types.get(node).map_or("", String::as_str),
                                selected: *node == entry.now,
                                live,
                                selectable,
                                delay: if live {
                                    state.delays.get(node).copied()
                                } else {
                                    None
                                },
                                testing: state.testing_nodes.contains(node),
                            },
                            &self.proxy_groups,
                            theme,
                        ))
                    }));
                // Keep a short last line's cards as wide as the full ones
                // above: the fillers carry the same padding + border, which
                // `flex_1`'s basis doesn't go below.
                for _ in nodes.len()..columns {
                    line = line.child(div().flex_1().px_3().border_1());
                }
                line.into_any_element()
            }
        }
    }

    fn group_header(
        &self,
        gi: usize,
        group: &ProxyGroup,
        expanded: bool,
        shown: usize,
        state: &ProxyGroups,
        theme: &Theme,
    ) -> Div {
        let live = state.source == GroupSource::Api;
        let is_testing = state.testing.contains(&group.name);
        let searching = self.searching();
        let count = if searching {
            format!("{}/{}", shown, group.all.len())
        } else {
            group.all.len().to_string()
        };
        let now = SharedString::from(group.now.clone());

        // A quiet icon button: one per group, so a labelled button would
        // stack down the page; the header's Test all carries the words.
        let test_btn = {
            let proxy_groups = self.proxy_groups.clone();
            let name = group.name.clone();
            Button::new(("group-test", gi))
                .ghost()
                .icon_control(ControlSize::Inline)
                .map(|button| {
                    if is_testing {
                        button.icon(Spinner::new())
                    } else {
                        button.icon(Icon::empty().path("icons/zap.svg"))
                    }
                })
                .tooltip(s().groups.test_group)
                // `loading` keeps the button inert while the spinner turns.
                .loading(is_testing)
                .disabled(!live)
                .on_click(move |_, _, cx| {
                    proxy_groups.update(cx, |state, cx| state.test_delay(name.clone(), cx));
                })
        };

        // The left part (chevron + name + current node) folds the card; the
        // Test button sits outside it so a test doesn't fold too. While
        // searching the card is forced open, so folding is off.
        let toggle_area = div()
            .id(("group-toggle", gi))
            .h_flex()
            .items_center()
            .gap_2()
            .flex_1()
            .min_w_0()
            .h_full()
            .when(!searching, |area| {
                let proxy_groups = self.proxy_groups.clone();
                let name = group.name.clone();
                area.group(GROUP_TOGGLE_GROUP)
                    .cursor_pointer()
                    .on_click(move |_, _, cx| {
                        proxy_groups.update(cx, |state, cx| {
                            state.set_expanded(name.clone(), !expanded, cx)
                        });
                    })
            })
            // The chevron sits on a soft square while the fold area is
            // hovered, like a ghost icon button: the header says it folds.
            .child(
                div()
                    .flex_none()
                    .size(px(20.))
                    .ml(px(-3.))
                    .mr(px(-3.))
                    .rounded(theme.radius)
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(!searching, |chevron| {
                        let hover_bg = theme.secondary_hover;
                        chevron.group_hover(GROUP_TOGGLE_GROUP, move |s| s.bg(hover_bg))
                    })
                    .child(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .small()
                        .text_color(theme.muted_foreground),
                    ),
            )
            .child(full_text_tooltip(
                div()
                    .flex_shrink_0()
                    .max_w(relative(0.5))
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.foreground),
                ("group-name", gi),
                group.name.clone(),
                GROUP_NAME_ROOM,
            ))
            // urltest groups pick their node by latency themselves, which a
            // gauge beside the group's name says (with a tooltip) instead
            // of a badge.
            .when(group.kind == GroupKind::UrlTest, |row| {
                row.child(
                    div()
                        .id(("group-auto", gi))
                        .flex_none()
                        .child(
                            Icon::empty()
                                .path("icons/gauge.svg")
                                .xsmall()
                                .text_color(theme.primary),
                        )
                        .tooltip(|window, cx| Tooltip::new(s().groups.auto).build(window, cx)),
                )
            })
            .child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(count),
            )
            // The node in use, after an arrow.
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_1p5()
                    .flex_1()
                    .min_w_0()
                    .ml_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(text_centered(
                        Icon::new(IconName::ArrowRight)
                            .text_color(theme.muted_foreground.opacity(0.7)),
                        now.clone(),
                    ))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(now),
                    ),
            );

        div()
            .h(px(HEADER_HEIGHT))
            .w_full()
            .px(px(CARD_PADDING))
            .h_flex()
            .items_center()
            .gap_2()
            .border_t_1()
            .border_l_1()
            .border_r_1()
            .border_color(theme.border)
            .bg(theme.background)
            .map(|row| {
                if expanded {
                    row.rounded_t(px(CARD_RADIUS))
                } else {
                    row.border_b_1().rounded(px(CARD_RADIUS))
                }
            })
            .child(toggle_area)
            .child(test_btn)
    }
}

/// Column count for a list `width` wide (0 = not measured yet).
fn columns_for(width: Pixels) -> usize {
    let width = f32::from(width);
    if width <= 0. {
        return DEFAULT_COLUMNS;
    }
    // Inside the card's side padding and borders.
    let inner = width - 2. * CARD_PADDING - 2.;
    let fit = ((inner + CARD_GAP) / (MIN_CARD_WIDTH + CARD_GAP)).floor() as usize;
    fit.clamp(1, MAX_COLUMNS)
}

fn row_height(row: &Row) -> f32 {
    match row {
        Row::Header { .. } => HEADER_HEIGHT,
        Row::Nodes { last: false, .. } => CARD_HEIGHT + CARD_GAP,
        Row::Nodes { last: true, .. } => CARD_HEIGHT + CARD_PADDING,
        Row::Gap => GROUP_GAP,
    }
}

fn delay_color(state: DelayState, theme: &Theme) -> Hsla {
    match state {
        DelayState::Ok(ms) => match classify_delay(ms) {
            DelayLevel::Fast => theme.success,
            DelayLevel::Medium => theme.warning,
            DelayLevel::Slow => theme.danger,
        },
        DelayState::Timeout => theme.danger,
    }
}

fn delay_label(state: DelayState) -> SharedString {
    match state {
        DelayState::Ok(ms) => format!("{}ms", ms).into(),
        DelayState::Timeout => s().groups.timeout.into(),
    }
}

/// Element id unique per (group, node index) — a node can sit in several
/// groups, and several cards are on screen at once.
fn node_id(prefix: &'static str, group: usize, index: usize) -> ElementId {
    ElementId::NamedInteger(prefix.into(), ((group as u64) << 32) | index as u64)
}

/// Everything one node card shows.
struct NodeCard<'a> {
    group: usize,
    index: usize,
    group_name: &'a str,
    node: &'a str,
    node_type: &'a str,
    selected: bool,
    /// sing-box is running (foreground text, delay badge).
    live: bool,
    /// Clicking switches to it (selector groups only; urltest is read-only).
    selectable: bool,
    delay: Option<DelayState>,
    /// A per-node test is in flight.
    testing: bool,
}

fn node_card(card: NodeCard, proxy_groups: &Entity<ProxyGroups>, theme: &Theme) -> Stateful<Div> {
    let primary = theme.primary;
    let name_row = div()
        .h_flex()
        .items_center()
        .justify_between()
        .gap_2()
        .child(full_text_tooltip(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .text_sm()
                .text_color(if card.live {
                    theme.foreground
                } else {
                    theme.muted_foreground
                }),
            node_id("node-name", card.group, card.index),
            card.node.to_string(),
            NODE_NAME_ROOM,
        ))
        .when(card.live, |row| {
            row.child(delay_badge(&card, proxy_groups, theme))
        });

    let mut element = div()
        .id(node_id("node", card.group, card.index))
        .group(NODE_CARD_GROUP)
        .flex_1()
        .min_w_0()
        .h(px(CARD_HEIGHT))
        .px_3()
        .rounded(px(8.))
        .border_1()
        .border_color(if card.selected {
            theme.primary.opacity(0.6)
        } else {
            theme.border
        })
        .bg(if card.selected {
            theme.primary.opacity(0.06)
        } else {
            theme.background
        })
        .v_flex()
        .justify_center()
        .gap_1()
        .child(name_row)
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(SharedString::from(card.node_type.to_string())),
        );

    if card.selectable {
        let proxy_groups = proxy_groups.clone();
        let group = card.group_name.to_string();
        let node = card.node.to_string();
        element = element
            .cursor_pointer()
            .hover(move |s| s.border_color(primary))
            .on_click(move |_, _, cx| {
                proxy_groups.update(cx, |state, cx| {
                    state.select(group.clone(), node.clone(), cx)
                });
            });
    }
    element
}

/// The delay result (or, without one, a zap icon shown while the card is
/// hovered); clicking it tests just this node. A spinner while it runs.
fn delay_badge(card: &NodeCard, proxy_groups: &Entity<ProxyGroups>, theme: &Theme) -> AnyElement {
    if card.testing {
        return div()
            .flex_shrink_0()
            .px_1()
            .child(Spinner::new().xsmall().color(theme.muted_foreground))
            .into_any_element();
    }
    let proxy_groups = proxy_groups.clone();
    let node = card.node.to_string();
    let hover_bg = theme.muted;
    let badge = div()
        .id(node_id("node-test", card.group, card.index))
        .flex_shrink_0()
        .px_1()
        .rounded_sm()
        .text_xs()
        .cursor_pointer()
        .hover(move |s| s.bg(hover_bg))
        .tooltip(|window, cx| Tooltip::new(s().groups.test_delay).build(window, cx))
        .on_click(move |_, _, cx| {
            // Don't let the card under it switch to this node as well.
            cx.stop_propagation();
            proxy_groups.update(cx, |state, cx| state.test_node(node.clone(), cx));
        });
    match card.delay {
        Some(delay) => badge
            .text_color(delay_color(delay, theme))
            .child(delay_label(delay))
            .into_any_element(),
        None => badge
            .invisible()
            .group_hover(NODE_CARD_GROUP, |s| s.visible())
            .child(
                Icon::empty()
                    .path("icons/zap.svg")
                    .xsmall()
                    .text_color(theme.muted_foreground),
            )
            .into_any_element(),
    }
}

impl Render for GroupsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh_rows(cx);
        let page = cx.entity();
        let weak_page = page.downgrade();
        let state = self.proxy_groups.read(cx);
        let live = state.source == GroupSource::Api;
        let has_groups = !state.groups.is_empty();
        let testing_all = state.testing_any_group();
        let theme = cx.theme();
        let t = &s().groups;

        // The toolbar, while there are groups to search, sort and test.
        let mut header = None;
        if has_groups {
            let mut toolbar = div()
                .h_flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .w_full()
                .child(toolbar_search(
                    control_input(&self.search, ControlSize::Regular)
                        .cleanable(true)
                        .prefix(
                            Icon::new(IconName::Search)
                                .small()
                                .text_color(theme.muted_foreground),
                        ),
                ))
                // Sorting to the right, under the header's actions.
                .child(div().flex_1())
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(t.sort),
                );
            const SORTS: [NodeSort; 2] = [NodeSort::Default, NodeSort::Delay];
            let page = weak_page.clone();
            toolbar = toolbar.child(segmented(
                theme,
                "groups-sort",
                ControlSize::Regular,
                vec![Segment::new(t.sort_default), Segment::new(t.sort_delay)],
                SORTS.iter().position(|sort| *sort == self.sort),
                move |ix, _, cx| {
                    page.update(cx, |this, cx| this.set_sort(SORTS[ix], cx))
                        .ok();
                },
            ));
            header = Some(toolbar);
        }
        let mut page_head = page_header(theme, ActivePage::Groups);
        if has_groups {
            let proxy_groups = self.proxy_groups.clone();
            page_head = page_head.action(
                Button::new("groups-test-all")
                    .outline()
                    .control(ControlSize::Regular)
                    .map(|button| {
                        if testing_all {
                            button.icon_label(Spinner::new(), t.test_all)
                        } else {
                            button.icon_label(Icon::empty().path("icons/zap.svg"), t.test_all)
                        }
                    })
                    .loading(testing_all)
                    .disabled(!live)
                    .on_click(move |_, _, cx| {
                        proxy_groups.update(cx, |state, cx| state.test_all(cx));
                    }),
            );
        }

        let body = if !has_groups {
            empty_state(theme, IconName::Globe, t.empty_title, t.empty_hint).into_any_element()
        } else if self.layout.rows.is_empty() {
            empty_state(theme, IconName::Search, t.no_match_title, t.no_match_hint)
                .into_any_element()
        } else {
            let list = v_virtual_list(
                page,
                "groups-list",
                self.sizes.clone(),
                |this, range, _, cx| range.map(|ix| this.render_row(ix, cx)).collect::<Vec<_>>(),
            )
            .track_scroll(&self.scroll);
            // Measures the list at paint time: a width that changes the
            // column count re-lays the rows out on the next frame (gpui
            // ignores a notify sent while it draws).
            let measure = canvas(
                move |bounds, window, cx| {
                    let Some(page) = weak_page.upgrade() else {
                        return;
                    };
                    let width = bounds.size.width;
                    let relayout = page.update(cx, |this, _| {
                        let before = columns_for(this.list_width);
                        this.list_width = width;
                        columns_for(width) != before
                    });
                    if relayout {
                        window.on_next_frame(move |_, cx| page.update(cx, |_, cx| cx.notify()));
                    }
                },
                |_, _, _, _| {},
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full();
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(measure)
                .child(list)
                .child(page_scrollbar("groups-scrollbar", &self.scroll))
                .into_any_element()
        };

        page_layout(
            page_head,
            div()
                .v_flex()
                .size_full()
                .gap_4()
                .children(header)
                .child(body),
        )
    }
}
