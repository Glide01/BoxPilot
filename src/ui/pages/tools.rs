//! Tools page: network diagnostics sing-box runs through one of its
//! outbounds — a network quality test (throughput + responsiveness) and a
//! STUN test (external address + NAT behaviour). Only while sing-box is
//! running; otherwise an empty state. Test options are page-local and not
//! persisted; the runs themselves live in `NetworkTools`.

use crate::core::network_tools::{
    OutboundChoice, QualityRun, RunStatus, StunRun, DEFAULT_MAX_RUNTIME, DEFAULT_STUN_SERVER,
    MAX_RUNTIME_CHOICES,
};
use crate::core::singbox_api::{NetworkQualityRequest, StunRequest};
use crate::i18n::s;
use crate::state::{AppState, NetworkTools};
use crate::ui::widgets::{empty_card, page_header, setting_row};
use crate::ui::{card_frame, locale};
use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{
    button::{Button, ButtonVariants},
    input::{Input, InputState},
    progress::Progress,
    scroll::ScrollableElement,
    select::{SearchableVec, Select, SelectItem, SelectState},
    spinner::Spinner,
    switch::Switch,
    tab::{Tab, TabBar},
    theme::Theme,
    ActiveTheme, Disableable, IconName, IndexPath, Sizable, StyledExt,
};

/// Width of the right-hand controls (pickers and text fields).
const FIELD_WIDTH: f32 = 260.;

/// Outbound picker row. Value = the outbound tag, "" = default outbound.
#[derive(Clone)]
struct OutboundRow(OutboundChoice);

impl SelectItem for OutboundRow {
    type Value = String;

    fn title(&self) -> SharedString {
        self.0.label().to_string().into()
    }

    fn render(&self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        div()
            .h_flex()
            .w_full()
            .justify_between()
            .gap_2()
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(self.title()),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(self.0.outbound_type.clone()),
            )
    }

    fn value(&self) -> &String {
        &self.0.tag
    }
}

type OutboundSelect = Entity<SelectState<SearchableVec<OutboundRow>>>;

pub struct ToolsPage {
    tools: Entity<NetworkTools>,
    /// The picker list currently loaded into both selects.
    outbounds: Vec<OutboundChoice>,
    quality_outbound: OutboundSelect,
    stun_outbound: OutboundSelect,
    config_url: Entity<InputState>,
    stun_server: Entity<InputState>,
    serial: bool,
    http3: bool,
    max_runtime: u32,
}

impl ToolsPage {
    pub fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let tools = app_state.read(cx).network_tools.clone();
        let outbounds = tools.read(cx).outbounds.clone();
        let quality_outbound = Self::outbound_select(&outbounds, window, cx);
        let stun_outbound = Self::outbound_select(&outbounds, window, cx);
        let config_url =
            cx.new(|cx| InputState::new(window, cx).placeholder(s().tools.config_url_placeholder));
        locale::observe(window, cx, |this: &mut Self, window, cx| {
            this.config_url.update(cx, |input, cx| {
                input.set_placeholder(s().tools.config_url_placeholder, window, cx)
            });
        })
        .detach();
        let stun_server = cx.new(|cx| InputState::new(window, cx).placeholder(DEFAULT_STUN_SERVER));

        cx.observe_in(&tools, window, |this: &mut Self, tools, window, cx| {
            let outbounds = tools.read(cx).outbounds.clone();
            if outbounds != this.outbounds {
                for select in [&this.quality_outbound, &this.stun_outbound] {
                    Self::reload_outbounds(select, &outbounds, window, cx);
                }
                this.outbounds = outbounds;
            }
            cx.notify();
        })
        .detach();

        Self {
            tools,
            outbounds,
            quality_outbound,
            stun_outbound,
            config_url,
            stun_server,
            serial: false,
            http3: false,
            max_runtime: DEFAULT_MAX_RUNTIME,
        }
    }

    fn outbound_select(
        outbounds: &[OutboundChoice],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> OutboundSelect {
        let rows = SearchableVec::new(
            outbounds
                .iter()
                .cloned()
                .map(OutboundRow)
                .collect::<Vec<_>>(),
        );
        cx.new(|cx| SelectState::new(rows, Some(IndexPath::default()), window, cx).searchable(true))
    }

    /// Swap in a new outbound list, keeping the picked outbound when it is
    /// still there and falling back to the default outbound otherwise.
    fn reload_outbounds(
        select: &OutboundSelect,
        outbounds: &[OutboundChoice],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        select.update(cx, |state, cx| {
            let picked = state.selected_value().cloned().unwrap_or_default();
            let keep = outbounds.iter().any(|o| o.tag == picked);
            let rows = SearchableVec::new(
                outbounds
                    .iter()
                    .cloned()
                    .map(OutboundRow)
                    .collect::<Vec<_>>(),
            );
            state.set_items(rows, window, cx);
            state.set_selected_value(&if keep { picked } else { String::new() }, window, cx);
        });
    }

    fn picked_outbound(select: &OutboundSelect, cx: &App) -> String {
        select
            .read(cx)
            .selected_value()
            .cloned()
            .unwrap_or_default()
    }

    fn start_quality(&mut self, cx: &mut Context<Self>) {
        let request = NetworkQualityRequest {
            config_url: self.config_url.read(cx).value().trim().to_string(),
            outbound_tag: Self::picked_outbound(&self.quality_outbound, cx),
            serial: self.serial,
            max_runtime_seconds: self.max_runtime,
            http3: self.http3,
        };
        self.tools
            .update(cx, |tools, cx| tools.start_quality(request, cx));
    }

    fn start_stun(&mut self, cx: &mut Context<Self>) {
        let request = StunRequest {
            server: self.stun_server.read(cx).value().trim().to_string(),
            outbound_tag: Self::picked_outbound(&self.stun_outbound, cx),
        };
        self.tools
            .update(cx, |tools, cx| tools.start_stun(request, cx));
    }

    fn quality_card(&self, run: Option<&QualityRun>, cx: &mut Context<Self>) -> Div {
        let running = run.is_some_and(|r| r.status.is_running());
        let serial = self.serial;
        let http3 = self.http3;
        let runtime_ix = MAX_RUNTIME_CHOICES
            .iter()
            .position(|&s| s == self.max_runtime)
            .unwrap_or(1);
        let theme = cx.theme();
        let t = s();

        let start = if running {
            Button::new("nq-cancel")
                .outline()
                .small()
                .label(t.common.cancel)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.tools.update(cx, |tools, cx| tools.cancel_quality(cx));
                }))
        } else {
            Button::new("nq-start")
                .primary()
                .small()
                .label(t.common.start)
                .on_click(cx.listener(|this, _, _, cx| this.start_quality(cx)))
        };

        let mut card = card_frame(theme)
            .child(section_label(theme, t.tools.quality_section))
            .child(hint(theme, t.tools.quality_hint))
            .child(
                setting_row(theme, t.tools.outbound, None).child(
                    div().w(px(FIELD_WIDTH)).child(
                        Select::new(&self.quality_outbound)
                            .small()
                            .search_placeholder(t.tools.search_outbounds)
                            .disabled(running),
                    ),
                ),
            )
            .child(
                setting_row(theme, t.tools.mode, Some(t.tools.mode_hint)).child(
                    TabBar::new("nq-mode")
                        .segmented()
                        .selected_index(if serial { 1 } else { 0 })
                        .on_click(cx.listener(|this, ix: &usize, _, cx| {
                            this.serial = *ix == 1;
                            cx.notify();
                        }))
                        .children(
                            [t.tools.parallel, t.tools.serial]
                                .map(|label| Tab::new().label(label).disabled(running)),
                        ),
                ),
            )
            .child(
                setting_row(theme, t.tools.max_runtime, None).child(
                    TabBar::new("nq-runtime")
                        .segmented()
                        .selected_index(runtime_ix)
                        .on_click(cx.listener(|this, ix: &usize, _, cx| {
                            if let Some(&secs) = MAX_RUNTIME_CHOICES.get(*ix) {
                                this.max_runtime = secs;
                                cx.notify();
                            }
                        }))
                        .children(MAX_RUNTIME_CHOICES.map(|secs| {
                            Tab::new().label((t.tools.seconds)(secs)).disabled(running)
                        })),
                ),
            )
            .child(
                setting_row(theme, "HTTP/3", Some(t.tools.http3_hint)).child(
                    Switch::new("nq-http3")
                        .checked(http3)
                        .disabled(running)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.http3 = *checked;
                            cx.notify();
                        })),
                ),
            )
            .child(
                setting_row(theme, t.tools.config_url, Some(t.tools.config_url_hint)).child(
                    div()
                        .w(px(FIELD_WIDTH))
                        .on_mouse_down_out(|_, window, cx| window.blur(cx))
                        .child(Input::new(&self.config_url).small().disabled(running)),
                ),
            )
            .child(action_row(
                theme,
                start,
                run.map(|r| (r.status.clone(), r.status_label())),
            ));

        let Some(run) = run else {
            return card;
        };
        if running {
            let progress = Progress::new("nq-progress").small();
            card = card.child(match run.percent() {
                Some(percent) => progress.value(percent),
                None => progress.loading(true),
            });
        }
        let metrics = run.metrics();
        // Accuracy arrives with the final result only.
        let accuracy = |capacity: Option<&str>, rpm: Option<&str>| match (capacity, rpm) {
            (Some(capacity), Some(rpm)) => {
                vec![(s().tools.accuracy)(capacity, rpm)]
            }
            _ => Vec::new(),
        };
        card.child(
            div()
                .flex()
                .flex_row()
                .gap_3()
                .w_full()
                .child(metric_tile(
                    theme,
                    t.common.download,
                    metrics.download,
                    std::iter::once(metrics.download_rpm)
                        .chain(accuracy(
                            metrics.download_accuracy,
                            metrics.download_rpm_accuracy,
                        ))
                        .collect(),
                ))
                .child(metric_tile(
                    theme,
                    t.common.upload,
                    metrics.upload,
                    std::iter::once(metrics.upload_rpm)
                        .chain(accuracy(
                            metrics.upload_accuracy,
                            metrics.upload_rpm_accuracy,
                        ))
                        .collect(),
                ))
                .child(metric_tile(
                    theme,
                    t.tools.idle_latency,
                    metrics.idle_latency,
                    Vec::new(),
                )),
        )
        .when(run.status == RunStatus::Done, |this| {
            this.child(hint(theme, t.tools.rpm_hint))
        })
        .when_some(failure(&run.status), |this, message| {
            this.child(error_text(theme, message))
        })
    }

    fn stun_card(&self, run: Option<&StunRun>, cx: &mut Context<Self>) -> Div {
        let running = run.is_some_and(|r| r.status.is_running());
        let theme = cx.theme();
        let t = s();

        let start = if running {
            Button::new("stun-cancel")
                .outline()
                .small()
                .label(t.common.cancel)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.tools.update(cx, |tools, cx| tools.cancel_stun(cx));
                }))
        } else {
            Button::new("stun-start")
                .primary()
                .small()
                .label(t.common.start)
                .on_click(cx.listener(|this, _, _, cx| this.start_stun(cx)))
        };

        let card = card_frame(theme)
            .child(section_label(theme, t.tools.stun_section))
            .child(hint(theme, t.tools.stun_hint))
            .child(
                setting_row(theme, t.tools.outbound, None).child(
                    div().w(px(FIELD_WIDTH)).child(
                        Select::new(&self.stun_outbound)
                            .small()
                            .search_placeholder(t.tools.search_outbounds)
                            .disabled(running),
                    ),
                ),
            )
            .child(
                setting_row(theme, t.tools.stun_server, Some(t.tools.stun_server_hint)).child(
                    div()
                        .w(px(FIELD_WIDTH))
                        .on_mouse_down_out(|_, window, cx| window.blur(cx))
                        .child(Input::new(&self.stun_server).small().disabled(running)),
                ),
            )
            .child(action_row(
                theme,
                start,
                run.map(|r| (r.status.clone(), r.status_label().to_string())),
            ));

        let Some(run) = run else {
            return card;
        };
        let unsupported = run.nat_type_supported == Some(false);
        card.child(
            div()
                .v_flex()
                .gap_2()
                .w_full()
                .child(result_row(
                    theme,
                    t.tools.external_address,
                    run.external_addr_label(),
                ))
                .child(result_row(theme, t.tools.latency, run.latency_label()))
                .when(!unsupported, |this| {
                    this.child(result_row(
                        theme,
                        t.tools.nat_mapping,
                        run.mapping_label().to_string(),
                    ))
                    .child(result_row(
                        theme,
                        t.tools.nat_filtering,
                        run.filtering_label().to_string(),
                    ))
                }),
        )
        .when(unsupported, |this| {
            this.child(hint(theme, t.tools.nat_unsupported))
        })
        .when_some(run.summary(), |this, summary| {
            this.child(
                div()
                    .v_flex()
                    .gap_1()
                    .p_3()
                    .rounded_md()
                    .bg(theme.muted)
                    .when_some(summary.classic, |this, classic| {
                        this.child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme.foreground)
                                .child(classic),
                        )
                    })
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(summary.explanation),
                    ),
            )
        })
        .when_some(failure(&run.status), |this, message| {
            this.child(error_text(theme, message))
        })
    }
}

impl Render for ToolsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tools = self.tools.read(cx);
        let active = tools.active;
        let quality = tools.quality.clone();
        let stun = tools.stun.clone();

        let body = if !active {
            empty_card(
                cx.theme(),
                IconName::Network,
                s().tools.not_running_title,
                s().tools.not_running_hint,
            )
            .into_any_element()
        } else {
            let cards = div()
                .v_flex()
                .gap_4()
                .child(self.quality_card(quality.as_ref(), cx))
                .child(self.stun_card(stun.as_ref(), cx));
            div()
                .flex_1()
                .min_h_0()
                .child(cards.overflow_y_scrollbar())
                .into_any_element()
        };

        div()
            .v_flex()
            .size_full()
            .gap_4()
            .child(page_header(cx.theme(), s().tools.title))
            .child(body)
    }
}

fn section_label(theme: &Theme, text: &'static str) -> Div {
    div()
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(text)
}

fn hint(theme: &Theme, text: &'static str) -> Div {
    div()
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(text)
}

fn error_text(theme: &Theme, message: String) -> Div {
    div().text_sm().text_color(theme.danger).child(message)
}

/// Start/Cancel button with the run's status beside it.
fn action_row(theme: &Theme, button: Button, status: Option<(RunStatus, String)>) -> Div {
    let status = status.map(|(status, label)| {
        let color = match status {
            RunStatus::Running => theme.muted_foreground,
            RunStatus::Done => theme.success,
            RunStatus::Failed(_) => theme.danger,
            RunStatus::Cancelled => theme.muted_foreground,
        };
        div()
            .h_flex()
            .items_center()
            .gap_2()
            .when(status.is_running(), |this| {
                this.child(Spinner::new().small())
            })
            .child(div().text_sm().text_color(color).child(label))
    });
    div()
        .h_flex()
        .items_center()
        .gap_3()
        .child(button)
        .children(status)
}

fn metric_tile(theme: &Theme, label: &'static str, value: String, subs: Vec<String>) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .v_flex()
        .gap_1()
        .px_3()
        .py_2()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(label),
        )
        .child(
            div()
                .text_lg()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.foreground)
                .child(value),
        )
        .children(subs.into_iter().map(|sub| {
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .whitespace_nowrap()
                .overflow_hidden()
                .text_ellipsis()
                .child(sub)
        }))
}

fn result_row(theme: &Theme, label: &'static str, value: String) -> Div {
    div()
        .h_flex()
        .items_center()
        .justify_between()
        .w_full()
        .child(
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(label),
        )
        .child(div().text_sm().text_color(theme.foreground).child(value))
}

fn failure(status: &RunStatus) -> Option<String> {
    match status {
        RunStatus::Failed(message) => Some(message.clone()),
        _ => None,
    }
}
