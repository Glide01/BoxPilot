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
use crate::ui::pages::ActivePage;
use crate::ui::widgets::{
    choice_select_with, connect_button, empty_state, form_column, grouped_card, page_header,
    page_layout, plain_select, section_heading, setting_row, small_input, stat, status_label,
    text_centered, TextLabel,
};
use crate::ui::{card_frame, locale};
use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{
    button::{Button, ButtonVariants},
    input::InputState,
    progress::Progress,
    scroll::ScrollableElement,
    select::{SearchableVec, Select, SelectItem, SelectState},
    spinner::Spinner,
    switch::Switch,
    tab::{Tab, TabBar},
    theme::Theme,
    ActiveTheme, Disableable, Icon, IndexPath, Sizable, StyledExt,
};

/// Width of the text fields.
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

    /// Start, or Cancel while the test runs.
    fn run_button(
        &self,
        id: &'static str,
        running: bool,
        start: fn(&mut Self, &mut Context<Self>),
        cancel: fn(&mut NetworkTools, &mut Context<NetworkTools>),
        cx: &mut Context<Self>,
    ) -> Button {
        let t = s();
        let button = Button::new(id).small();
        if running {
            button
                .outline()
                .text_label(t.common.cancel)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.tools.update(cx, cancel);
                }))
        } else {
            button
                .primary()
                .text_label(t.common.start)
                .on_click(cx.listener(move |this, _, _, cx| start(this, cx)))
        }
    }

    fn quality_section(
        &self,
        run: Option<&QualityRun>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let running = run.is_some_and(|r| r.status.is_running());
        let t = s();
        let page = cx.entity().downgrade();
        let runtime = choice_select_with(
            "nq-runtime",
            MAX_RUNTIME_CHOICES.map(|secs| (secs, (t.tools.seconds)(secs))),
            self.max_runtime,
            running,
            move |secs, _, cx| {
                let _ = page.update(cx, |this, cx| {
                    this.max_runtime = secs;
                    cx.notify();
                });
            },
            window,
            cx,
        );
        let button = self.run_button(
            "nq-run",
            running,
            Self::start_quality,
            NetworkTools::cancel_quality,
            cx,
        );
        let serial = self.serial;
        let http3 = self.http3;
        let theme = cx.theme();

        let options = grouped_card(
            theme,
            [
                setting_row(theme, t.tools.outbound, None)
                    .child(outbound_picker(
                        &self.quality_outbound,
                        &self.outbounds,
                        running,
                        window,
                        cx,
                    ))
                    .into_any_element(),
                setting_row(theme, t.tools.mode, None)
                    .child(
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
                    )
                    .into_any_element(),
                setting_row(theme, t.tools.max_runtime, None)
                    .child(runtime)
                    .into_any_element(),
                setting_row(theme, "HTTP/3", None)
                    .child(
                        Switch::new("nq-http3")
                            .checked(http3)
                            .disabled(running)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.http3 = *checked;
                                cx.notify();
                            })),
                    )
                    .into_any_element(),
                setting_row(theme, t.tools.config_url, None)
                    .child(text_field(&self.config_url, running))
                    .into_any_element(),
            ],
        );

        // A run that ended before measuring anything shows only why it
        // failed, if it did.
        let shows_results =
            |run: &&QualityRun| running || run.latest.is_some() || failure(&run.status).is_some();
        let results = run.filter(shows_results).map(|run| {
            let measured = running || run.latest.is_some();
            let metrics = run.metrics();
            // Accuracy arrives with the final result only.
            let accuracy = |capacity: Option<&str>, rpm: Option<&str>| match (capacity, rpm) {
                (Some(capacity), Some(rpm)) => vec![(t.tools.accuracy)(capacity, rpm)],
                _ => Vec::new(),
            };
            let progress = running.then(|| {
                let progress = Progress::new("nq-progress").small();
                match run.percent() {
                    Some(percent) => progress.value(percent),
                    None => progress.loading(true),
                }
            });
            card_frame(theme)
                .gap_4()
                .children(progress)
                .when(measured, |this| {
                    this.child(
                        div()
                            .h_flex()
                            .items_start()
                            .gap_4()
                            .w_full()
                            .child(metric(
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
                            .child(metric(
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
                            .child(metric(
                                theme,
                                t.tools.idle_latency,
                                metrics.idle_latency,
                                Vec::new(),
                            )),
                    )
                })
                .when_some(failure(&run.status), |this, message| {
                    this.child(error_text(theme, message))
                })
        });

        section(
            theme,
            t.tools.quality_section,
            run.map(|r| (r.status.clone(), r.status_label())),
            button,
        )
        .child(options)
        .children(results)
    }

    fn stun_section(
        &self,
        run: Option<&StunRun>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let running = run.is_some_and(|r| r.status.is_running());
        let t = s();
        let button = self.run_button(
            "stun-run",
            running,
            Self::start_stun,
            NetworkTools::cancel_stun,
            cx,
        );
        let theme = cx.theme();

        let options = grouped_card(
            theme,
            [
                setting_row(theme, t.tools.outbound, None)
                    .child(outbound_picker(
                        &self.stun_outbound,
                        &self.outbounds,
                        running,
                        window,
                        cx,
                    ))
                    .into_any_element(),
                setting_row(theme, t.tools.stun_server, None)
                    .child(text_field(&self.stun_server, running))
                    .into_any_element(),
            ],
        );

        let results = run.and_then(|run| {
            let unsupported = run.nat_type_supported == Some(false);
            // A run that ended before measuring anything shows only why.
            let measured = running || !run.external_addr.is_empty();
            let mut rows = Vec::new();
            // The verdict first: what the mapping and filtering add up to.
            if let Some(summary) = run.summary() {
                rows.push(match summary.classic {
                    Some(classic) => setting_row(theme, classic, Some(summary.explanation)),
                    None => setting_row(theme, summary.explanation, None),
                });
            }
            if measured {
                rows.push(value_row(
                    theme,
                    t.tools.external_address,
                    run.external_addr_label(),
                ));
                rows.push(value_row(theme, t.tools.latency, run.latency_label()));
            }
            if measured && unsupported {
                rows.push(div().child(hint(theme, t.tools.nat_unsupported)));
            } else if measured {
                rows.push(value_row(
                    theme,
                    t.tools.nat_mapping,
                    run.mapping_label().to_string(),
                ));
                rows.push(value_row(
                    theme,
                    t.tools.nat_filtering,
                    run.filtering_label().to_string(),
                ));
            }
            if let Some(message) = failure(&run.status) {
                rows.push(error_text(theme, message));
            }
            (!rows.is_empty())
                .then(|| grouped_card(theme, rows.into_iter().map(IntoElement::into_any_element)))
        });

        section(
            theme,
            t.tools.stun_section,
            run.map(|r| (r.status.clone(), r.status_label().to_string())),
            button,
        )
        .child(options)
        .children(results)
    }
}

impl Render for ToolsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tools = self.tools.read(cx);
        let active = tools.active;
        let quality = tools.quality.clone();
        let stun = tools.stun.clone();

        let body = if !active {
            empty_state(
                cx.theme(),
                Icon::empty().path("icons/gauge.svg"),
                s().tools.not_running_title,
                s().tools.not_running_hint,
            )
            .action(connect_button("tools-connect"))
            .into_any_element()
        } else {
            let cards = div()
                .v_flex()
                .gap_6()
                .pb_2()
                .child(self.quality_section(quality.as_ref(), window, cx))
                .child(self.stun_section(stun.as_ref(), window, cx));
            div()
                .flex_1()
                .min_h_0()
                .child(
                    div()
                        .w_full()
                        .child(form_column(cards))
                        .overflow_y_scrollbar(),
                )
                .into_any_element()
        };

        page_layout(
            page_header(cx.theme(), ActivePage::Tools),
            div().v_flex().size_full().child(body),
        )
    }
}

/// A test's heading, its run's status and its Start / Cancel button on
/// one line above its cards.
fn section(
    theme: &Theme,
    title: &'static str,
    status: Option<(RunStatus, String)>,
    button: Button,
) -> Div {
    let status = status.map(|(status, label)| {
        let color = match status {
            RunStatus::Running | RunStatus::Cancelled => theme.muted_foreground,
            RunStatus::Done => theme.success,
            RunStatus::Failed(_) => theme.danger,
        };
        if status.is_running() {
            div()
                .h_flex()
                .items_center()
                .gap_1p5()
                .text_xs()
                .text_color(color)
                .child(text_centered(Spinner::new(), label.clone()))
                .child(label)
        } else {
            status_label(color, label)
        }
    });
    div().v_flex().gap_2().child(
        div()
            .h_flex()
            .items_center()
            .justify_between()
            .gap_3()
            .child(section_heading(theme, title))
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_3()
                    .children(status)
                    .child(button),
            ),
    )
}

/// The outbound a test runs through ([`plain_select`]); its menu has
/// room for the protocol beside each name.
fn outbound_picker(
    select: &OutboundSelect,
    outbounds: &[OutboundChoice],
    disabled: bool,
    window: &Window,
    cx: &App,
) -> Div {
    let picked = ToolsPage::picked_outbound(select, cx);
    let current = outbounds
        .iter()
        .find(|o| o.tag == picked)
        .map(|o| SharedString::from(o.label().to_string()))
        .unwrap_or_default();
    let rows: Vec<SharedString> = outbounds
        .iter()
        .map(|o| format!("{}    {}", o.label(), o.outbound_type).into())
        .collect();
    plain_select(
        Select::new(select).search_placeholder(s().tools.search_outbounds),
        &current,
        &rows,
        disabled,
        window,
    )
}

fn text_field(input: &Entity<InputState>, disabled: bool) -> Div {
    div()
        .w(px(FIELD_WIDTH))
        .on_mouse_down_out(|_, window, cx| window.blur(cx))
        .child(small_input(input).disabled(disabled))
}

fn hint(theme: &Theme, text: &'static str) -> Div {
    div()
        .text_sm()
        .text_color(theme.muted_foreground)
        .child(text)
}

fn error_text(theme: &Theme, message: String) -> Div {
    div().text_sm().text_color(theme.danger).child(message)
}

/// A result: caption, value, then its finer points in small print.
fn metric(theme: &Theme, label: &'static str, value: String, subs: Vec<String>) -> Div {
    stat(theme, label, value).children(subs.into_iter().map(|sub| {
        div()
            .text_xs()
            .text_color(theme.muted_foreground)
            .whitespace_nowrap()
            .overflow_hidden()
            .text_ellipsis()
            .child(sub)
    }))
}

/// A result row: what was measured on the left, the value on the right.
fn value_row(theme: &Theme, label: &'static str, value: String) -> Div {
    setting_row(theme, label, None).child(
        div()
            .text_sm()
            .text_color(theme.muted_foreground)
            .child(value),
    )
}

fn failure(status: &RunStatus) -> Option<String> {
    match status {
        RunStatus::Failed(message) => Some(message.clone()),
        _ => None,
    }
}
