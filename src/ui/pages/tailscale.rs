//! Tailscale 页:仅当运行中的配置带 Tailscale endpoint 时出现在侧边栏
//! (见 `TailscaleState::has_endpoints`)。每个 endpoint 一组卡片:概况
//! (状态、登录/登出、tailnet、本机)、出口节点选择、Ping 面板、Taildrop
//! 收件箱、证书、按用户分组的设备列表。所有 API 调用都在 `TailscaleState`
//! 里;这里只负责展示、确认框和文件对话框。

use crate::core::settings::StatusLevel;
use crate::core::singbox_api::{
    TaildropFile, TaildropInbox, TaildropReceivingFile, TailscaleCertificate,
    TailscaleEndpointStatus, TailscalePeer,
};
use crate::core::tailscale::{
    current_exit_node_label, default_save_dir, dns_name_display, exit_node_choices, format_bytes,
    peer_name, peer_presence_label, ping_address, ping_summary, receiving_fraction,
    receiving_progress_label, safe_file_name, save_certificate_pair, sorted_user_groups,
    traffic_label, user_label, BACKEND_RUNNING,
};
use crate::core::timefmt::{format_relative_time, from_unix_secs};
use crate::i18n::s;
use crate::state::tailscale::{CertificateFetched, PingSession, TailscaleAction};
use crate::state::{AppState, TailscaleState};
use crate::ui::card_frame;
use crate::ui::toast;
use crate::ui::widgets::{dialog_button, empty_state, meta_row, status_label, TextLabel};
use gpui::{prelude::FluentBuilder, *};
use gpui_component::{
    button::{Button, ButtonVariants},
    dialog::{DialogClose, DialogFooter},
    menu::{DropdownMenu, PopupMenuItem},
    progress::Progress,
    scroll::ScrollableElement,
    theme::Theme,
    ActiveTheme, Disableable, IconName, Sizable, StyledExt, WindowExt,
};
use std::rc::Rc;
use std::time::SystemTime;

pub struct TailscalePage {
    tailscale: Entity<TailscaleState>,
    /// The minute (unix secs / 60) the relative times ("Last seen 5 min
    /// ago") were last rendered for.
    rendered_minute: u64,
}

impl TailscalePage {
    pub fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let tailscale = app_state.read(cx).tailscale.clone();
        cx.observe(&tailscale, |_, _, cx| cx.notify()).detach();
        // The page is a cached view: re-render for the relative times when
        // the minute turns. The status samples (once a second while
        // connected — the only time this page has content) are the clock.
        let traffic = app_state.read(cx).traffic.clone();
        cx.observe(&traffic, |this: &mut Self, _, cx| {
            if current_minute() != this.rendered_minute {
                cx.notify();
            }
        })
        .detach();
        // 证书取回后弹窗展示/保存;证书只活在弹窗闭包里,不进状态、不写日志。
        cx.subscribe_in(
            &tailscale,
            window,
            |_, _, fetched: &CertificateFetched, window, cx| {
                show_certificate(
                    fetched.domain.clone(),
                    Rc::new(fetched.certificate.clone()),
                    window,
                    cx,
                );
            },
        )
        .detach();
        Self {
            tailscale,
            rendered_minute: 0,
        }
    }
}

fn current_minute() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / 60)
}

impl Render for TailscalePage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = self.tailscale.clone();
        self.rendered_minute = current_minute();
        let state = self.tailscale.read(cx);
        let theme = cx.theme();
        let now = SystemTime::now();

        let body = if state.endpoints.is_empty() {
            empty_state(
                theme,
                IconName::Frame,
                s().tailscale.empty_title,
                s().tailscale.empty_hint,
            )
            .into_any_element()
        } else {
            let sections =
                div()
                    .v_flex()
                    .gap_3()
                    .children(state.endpoints.iter().enumerate().flat_map(|(ei, status)| {
                        endpoint_sections(ei, status, state, &entity, theme, now)
                    }));
            div()
                .v_flex()
                .flex_1()
                .min_h_0()
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .child(sections.overflow_y_scrollbar()),
                )
                .into_any_element()
        };

        div().v_flex().size_full().child(body)
    }
}

/// Every card for one endpoint, top to bottom.
fn endpoint_sections(
    ei: usize,
    status: &TailscaleEndpointStatus,
    state: &TailscaleState,
    entity: &Entity<TailscaleState>,
    theme: &Theme,
    now: SystemTime,
) -> Vec<AnyElement> {
    let running = status.backend_state == BACKEND_RUNNING;
    let mut sections = vec![overview_card(ei, status, state, entity, theme).into_any_element()];
    if running {
        sections.push(exit_node_card(ei, status, state, entity, theme).into_any_element());
    }
    if let Some(ping) = state
        .ping
        .as_ref()
        .filter(|p| p.endpoint_tag == status.endpoint_tag)
    {
        sections.push(ping_card(ei, ping, entity, theme).into_any_element());
    }
    let inbox = state.inboxes.get(&status.endpoint_tag);
    let has_files = inbox.is_some_and(|i| !i.files.is_empty() || !i.receiving.is_empty());
    if status.can_share_files || has_files {
        sections
            .push(taildrop_card(ei, status, inbox, state, entity, theme, now).into_any_element());
    }
    if running && !status.cert_domains.is_empty() {
        sections.push(certificate_card(ei, status, state, entity, theme).into_any_element());
    }
    if running {
        sections.push(devices_card(ei, status, entity, theme, now).into_any_element());
    }
    sections
}

fn card_title(theme: &Theme, title: impl Into<SharedString>) -> Div {
    div()
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.foreground)
        .child(title.into())
}

fn muted(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(text.into())
}

/// "Label   value" line in the overview card.
fn info_row(theme: &Theme, label: &'static str, value: impl Into<SharedString>) -> Div {
    div()
        .h_flex()
        .gap_3()
        .child(
            div()
                .w(px(96.))
                .flex_shrink_0()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_sm()
                .text_color(theme.foreground)
                .child(value.into()),
        )
}

fn dot(color: Hsla) -> Div {
    div().w_2().h_2().flex_shrink_0().rounded_full().bg(color)
}

fn id(ei: usize, what: impl std::fmt::Display) -> SharedString {
    SharedString::from(format!("ts-{}-{}", ei, what))
}

fn overview_card(
    ei: usize,
    status: &TailscaleEndpointStatus,
    state: &TailscaleState,
    entity: &Entity<TailscaleState>,
    theme: &Theme,
) -> Div {
    let tag = status.endpoint_tag.clone();
    let state_color = match status.backend_state.as_str() {
        BACKEND_RUNNING => theme.success,
        "NeedsLogin" | "NeedsMachineAuth" => theme.warning,
        "Starting" => theme.primary,
        _ => theme.muted_foreground,
    };
    let state_text = if status.state_text.is_empty() {
        status.backend_state.clone()
    } else {
        status.state_text.clone()
    };

    let login = (!status.auth_url.is_empty()).then(|| {
        let url = status.auth_url.clone();
        Button::new(id(ei, "login"))
            .primary()
            .small()
            .text_label(s().tailscale.log_in)
            .tooltip(s().tailscale.log_in_tooltip)
            .on_click(move |_, _, cx| cx.open_url(&url))
    });
    let can_logout = !matches!(status.backend_state.as_str(), "NeedsLogin" | "NoState" | "");
    let logout = can_logout.then(|| {
        let entity = entity.clone();
        let tag = tag.clone();
        let key_auth = status.key_auth;
        let busy = state.is_busy(&TailscaleAction::Logout { tag: tag.clone() });
        Button::new(id(ei, "logout"))
            .outline()
            .small()
            .text_label(s().tailscale.log_out)
            .loading(busy)
            .disabled(busy)
            .on_click(move |_, window, cx| {
                confirm_logout(entity.clone(), tag.clone(), key_auth, window, cx)
            })
    });

    let hint = match status.backend_state.as_str() {
        "NeedsLogin" if status.auth_url.is_empty() => Some(s().tailscale.waiting_login_link),
        "NeedsLogin" => Some(s().tailscale.log_in_hint),
        "NeedsMachineAuth" => Some(s().tailscale.waiting_approval),
        _ => None,
    };

    let header = div()
        .h_flex()
        .items_center()
        .gap_2()
        .child(card_title(theme, tag.clone()))
        .child(dot(state_color))
        .child(div().text_xs().text_color(state_color).child(state_text))
        .child(div().flex_1())
        .children(login)
        .children(logout);

    let mut card = card_frame(theme)
        .child(header)
        .children(hint.map(|h| muted(theme, h)));
    if !status.network_name.is_empty() {
        card = card.child(info_row(
            theme,
            s().tailscale.tailnet,
            status.network_name.clone(),
        ));
    }
    if let Some(me) = &status.self_peer {
        card = card.child(info_row(theme, s().tailscale.this_device, peer_name(me)));
        if !me.dns_name.is_empty() {
            card = card.child(info_row(
                theme,
                s().tailscale.dns_name,
                dns_name_display(&me.dns_name).to_string(),
            ));
        }
        if !me.tailscale_ips.is_empty() {
            card = card.child(info_row(
                theme,
                s().tailscale.addresses,
                me.tailscale_ips.join(", "),
            ));
        }
    }
    card
}

fn confirm_logout(
    entity: Entity<TailscaleState>,
    tag: String,
    key_auth: bool,
    window: &mut Window,
    cx: &mut App,
) {
    window.open_alert_dialog(cx, move |alert, _, _| {
        let entity = entity.clone();
        let tag = tag.clone();
        let mut description = (s().tailscale.logout_body)(&tag);
        if key_auth {
            description.push_str(s().tailscale.logout_key_auth);
        }
        alert
            .title(s().tailscale.logout_title)
            .description(description)
            .confirm()
            .on_ok(move |_, _, cx| {
                entity.update(cx, |state, cx| state.logout(tag.clone(), cx));
                true
            })
    });
}

fn exit_node_card(
    ei: usize,
    status: &TailscaleEndpointStatus,
    state: &TailscaleState,
    entity: &Entity<TailscaleState>,
    theme: &Theme,
) -> Div {
    let tag = status.endpoint_tag.clone();
    let choices = exit_node_choices(status);
    let current = current_exit_node_label(status);
    let busy = state.is_busy(&TailscaleAction::ExitNode { tag: tag.clone() });

    let header = div().h_flex().items_center().gap_2().child(
        div()
            .v_flex()
            .flex_1()
            .min_w_0()
            .gap_1()
            .child(card_title(theme, s().tailscale.exit_node))
            .child(muted(
                theme,
                if current.is_some() {
                    s().tailscale.exit_node_on
                } else {
                    s().tailscale.exit_node_off
                },
            )),
    );

    if choices.is_empty() && current.is_none() {
        return card_frame(theme)
            .child(header)
            .child(muted(theme, s().tailscale.no_exit_nodes));
    }

    let has_current = current.is_some();
    let picker = Button::new(id(ei, "exit-node"))
        .outline()
        .small()
        .text_label(
            current
                .clone()
                .unwrap_or_else(|| s().tailscale.exit_node_none.to_string()),
        )
        .loading(busy)
        .disabled(busy)
        .dropdown_menu({
            let entity = entity.clone();
            move |menu, _, _| {
                let none = {
                    let entity = entity.clone();
                    let tag = tag.clone();
                    PopupMenuItem::new(s().tailscale.exit_node_none)
                        .checked(!has_current)
                        .on_click(move |_, _, cx| {
                            entity.update(cx, |state, cx| {
                                state.set_exit_node(tag.clone(), String::new(), cx)
                            });
                        })
                };
                choices.iter().fold(menu.item(none), |menu, choice| {
                    let entity = entity.clone();
                    let tag = tag.clone();
                    let stable_id = choice.stable_id.clone();
                    let label = if choice.online {
                        choice.label.clone()
                    } else {
                        (s().tailscale.offline_choice)(&choice.label)
                    };
                    menu.item(PopupMenuItem::new(label).checked(choice.selected).on_click(
                        move |_, _, cx| {
                            entity.update(cx, |state, cx| {
                                state.set_exit_node(tag.clone(), stable_id.clone(), cx)
                            });
                        },
                    ))
                })
            }
        });

    card_frame(theme).child(header.child(picker))
}

fn ping_card(ei: usize, ping: &PingSession, entity: &Entity<TailscaleState>, theme: &Theme) -> Div {
    let action = if ping.active {
        let entity = entity.clone();
        Button::new(id(ei, "ping-stop"))
            .outline()
            .small()
            .text_label(s().common.stop)
            .on_click(move |_, _, cx| entity.update(cx, |state, cx| state.stop_ping(cx)))
    } else {
        let entity = entity.clone();
        Button::new(id(ei, "ping-close"))
            .outline()
            .small()
            .text_label(s().common.close)
            .on_click(move |_, _, cx| entity.update(cx, |state, cx| state.dismiss_ping(cx)))
    };
    let header = div()
        .h_flex()
        .items_center()
        .gap_2()
        .child(card_title(
            theme,
            (s().tailscale.ping_title)(&ping.peer_name, &ping.peer_ip),
        ))
        .when(ping.active, |this| {
            this.child(status_label(theme.success, s().tailscale.running))
        })
        .child(div().flex_1())
        .child(action);

    let mut card = card_frame(theme).child(header);
    if ping.results.is_empty() && ping.active {
        card = card.child(muted(theme, s().tailscale.waiting_reply));
    }
    card = card.children(ping.results.iter().enumerate().map(|(i, result)| {
        let color = if result.error.is_some() {
            theme.danger
        } else if i == 0 {
            theme.foreground
        } else {
            theme.muted_foreground
        };
        let summary = ping_summary(result);
        div()
            .h_flex()
            .items_baseline()
            .gap_3()
            .text_sm()
            .text_color(color)
            .children(summary.latency.map(|latency| {
                div()
                    .flex_none()
                    .font_weight(FontWeight::MEDIUM)
                    .child(latency)
            }))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .when(result.error.is_none(), |this| {
                        this.text_color(theme.muted_foreground)
                    })
                    .child(summary.path),
            )
    }));
    if let Some(error) = &ping.error {
        card = card.child(
            div()
                .text_xs()
                .text_color(theme.danger)
                .child(error.clone()),
        );
    }
    card
}

#[allow(clippy::too_many_arguments)]
fn taildrop_card(
    ei: usize,
    status: &TailscaleEndpointStatus,
    inbox: Option<&TaildropInbox>,
    state: &TailscaleState,
    entity: &Entity<TailscaleState>,
    theme: &Theme,
    now: SystemTime,
) -> Div {
    let tag = status.endpoint_tag.clone();
    let unread = status.unread_file_count;
    let mark_read = (unread > 0).then(|| {
        let entity = entity.clone();
        let tag = tag.clone();
        let busy = state.is_busy(&TailscaleAction::MarkRead { tag: tag.clone() });
        Button::new(id(ei, "mark-read"))
            .outline()
            .small()
            .text_label(s().tailscale.mark_read)
            .loading(busy)
            .disabled(busy)
            .on_click(move |_, _, cx| {
                entity.update(cx, |state, cx| state.mark_inbox_read(tag.clone(), cx))
            })
    });
    let unread_badge = (unread > 0).then(|| {
        div()
            .flex_shrink_0()
            .text_xs()
            .px_2()
            .rounded_full()
            .bg(theme.primary)
            .text_color(theme.primary_foreground)
            .child((s().tailscale.new_files)(unread as u64))
    });
    let header = div()
        .h_flex()
        .items_center()
        .gap_2()
        .child(card_title(theme, s().tailscale.taildrop))
        .children(unread_badge)
        .child(div().flex_1())
        .children(mark_read);

    let mut card = card_frame(theme).child(header);
    let empty = TaildropInbox::default();
    let inbox = inbox.unwrap_or(&empty);
    if inbox.files.is_empty() && inbox.receiving.is_empty() {
        card = card.child(muted(
            theme,
            if status.can_share_files {
                s().tailscale.no_files_share
            } else {
                s().tailscale.no_files
            },
        ));
    }
    card = card.children(
        inbox
            .receiving
            .iter()
            .enumerate()
            .map(|(i, file)| receiving_row(ei, i, &tag, file, state, entity, theme)),
    );
    card.children(
        inbox
            .files
            .iter()
            .enumerate()
            .map(|(i, file)| file_row(ei, i, &tag, file, state, entity, theme, now)),
    )
}

/// "from laptop", or nothing for an unknown sender.
fn sender_label(sender: &str) -> Option<String> {
    (!sender.is_empty()).then(|| (s().tailscale.from_sender)(sender))
}

fn receiving_row(
    ei: usize,
    i: usize,
    tag: &str,
    file: &TaildropReceivingFile,
    state: &TailscaleState,
    entity: &Entity<TailscaleState>,
    theme: &Theme,
) -> Div {
    let action = TailscaleAction::CancelReceiving {
        tag: tag.to_string(),
        sender_id: file.sender_id.clone(),
        name: file.name.clone(),
    };
    let busy = state.is_busy(&action);
    let cancel = {
        let entity = entity.clone();
        let (tag, sender_id, name) = (tag.to_string(), file.sender_id.clone(), file.name.clone());
        Button::new(id(ei, format!("recv-cancel-{}", i)))
            .ghost()
            .small()
            .text_label(s().common.cancel)
            .loading(busy)
            .disabled(busy)
            .on_click(move |_, _, cx| {
                entity.update(cx, |state, cx| {
                    state.cancel_receiving(tag.clone(), sender_id.clone(), name.clone(), cx)
                })
            })
    };
    let progress = Progress::new(id(ei, format!("recv-progress-{}", i)))
        .loading(receiving_fraction(file).is_none())
        .value(receiving_fraction(file).unwrap_or(0.0) * 100.0);
    div()
        .v_flex()
        .gap_1()
        .child(
            div()
                .h_flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .text_sm()
                        .text_color(theme.foreground)
                        .child(file.name.clone()),
                )
                .child(cancel),
        )
        .child(progress)
        .child(meta_row(
            theme,
            std::iter::once((s().tailscale.receiving)(&receiving_progress_label(file)))
                .chain(sender_label(&file.sender_name)),
        ))
}

#[allow(clippy::too_many_arguments)]
fn file_row(
    ei: usize,
    i: usize,
    tag: &str,
    file: &TaildropFile,
    state: &TailscaleState,
    entity: &Entity<TailscaleState>,
    theme: &Theme,
    now: SystemTime,
) -> Div {
    let downloading = state.is_busy(&TailscaleAction::Download {
        tag: tag.to_string(),
        name: file.name.clone(),
    });
    let deleting = state.is_busy(&TailscaleAction::Delete {
        tag: tag.to_string(),
        name: file.name.clone(),
    });
    let save = {
        let entity = entity.clone();
        let (tag, name) = (tag.to_string(), file.name.clone());
        Button::new(id(ei, format!("file-save-{}", i)))
            .outline()
            .small()
            .text_label(s().common.save_as)
            .loading(downloading)
            .disabled(downloading)
            .on_click(move |_, window, cx| {
                save_file(entity.clone(), tag.clone(), name.clone(), window, cx)
            })
    };
    let delete = {
        let entity = entity.clone();
        let (tag, name) = (tag.to_string(), file.name.clone());
        Button::new(id(ei, format!("file-delete-{}", i)))
            .ghost()
            .small()
            .text_label(s().common.delete)
            .loading(deleting)
            .disabled(deleting || downloading)
            .on_click(move |_, window, cx| {
                confirm_delete(entity.clone(), tag.clone(), name.clone(), window, cx)
            })
    };
    let when = file
        .modified_at
        .map(|secs| format_relative_time(from_unix_secs(secs.max(0) as u64), now));
    div()
        .h_flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(
                    div()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .text_sm()
                        .text_color(theme.foreground)
                        .child(file.name.clone()),
                )
                .child(meta_row(
                    theme,
                    std::iter::once(format_bytes(file.size))
                        .chain(sender_label(&file.sender_name))
                        .chain(when),
                )),
        )
        .child(save)
        .child(delete)
}

/// Save-as dialog (opening in Downloads, suggesting the received name made
/// safe for Windows), then download there.
fn save_file(
    entity: Entity<TailscaleState>,
    tag: String,
    name: String,
    window: &mut Window,
    cx: &mut App,
) {
    let rx = cx.prompt_for_new_path(&default_save_dir(), Some(&safe_file_name(&name)));
    window
        .spawn(cx, async move |cx| match rx.await {
            Ok(Ok(Some(path))) => {
                let _ = cx.update(|_, cx| {
                    entity.update(cx, |state, cx| state.download_file(tag, name, path, cx))
                });
            }
            Ok(Err(e)) => {
                let _ = cx.update(|_, cx| {
                    toast::show(
                        StatusLevel::Error,
                        (s().tailscale.save_dialog_failed)(&e.to_string()),
                        cx,
                    )
                });
            }
            _ => {}
        })
        .detach();
}

fn confirm_delete(
    entity: Entity<TailscaleState>,
    tag: String,
    name: String,
    window: &mut Window,
    cx: &mut App,
) {
    window.open_alert_dialog(cx, move |alert, _, _| {
        let entity = entity.clone();
        let tag = tag.clone();
        let name = name.clone();
        alert
            .title((s().tailscale.delete_title)(&name))
            .description(s().tailscale.delete_body)
            .confirm()
            .on_ok(move |_, _, cx| {
                entity.update(cx, |state, cx| {
                    state.delete_file(tag.clone(), name.clone(), cx)
                });
                true
            })
    });
}

fn certificate_card(
    ei: usize,
    status: &TailscaleEndpointStatus,
    state: &TailscaleState,
    entity: &Entity<TailscaleState>,
    theme: &Theme,
) -> Div {
    let tag = status.endpoint_tag.clone();
    card_frame(theme)
        .child(
            div()
                .v_flex()
                .gap_1()
                .child(card_title(theme, s().tailscale.https_certs))
                .child(muted(theme, s().tailscale.https_hint)),
        )
        .children(status.cert_domains.iter().enumerate().map(|(i, domain)| {
            let busy = state.is_busy(&TailscaleAction::Certificate {
                tag: tag.clone(),
                domain: domain.clone(),
            });
            let entity = entity.clone();
            let (tag, request_domain) = (tag.clone(), domain.clone());
            div()
                .h_flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .text_sm()
                        .text_color(theme.foreground)
                        .child(domain.clone()),
                )
                .child(
                    Button::new(id(ei, format!("cert-{}", i)))
                        .outline()
                        .small()
                        .text_label(s().tailscale.get_certificate)
                        .loading(busy)
                        .disabled(busy)
                        .on_click(move |_, _, cx| {
                            entity.update(cx, |state, cx| {
                                state.fetch_certificate(tag.clone(), request_domain.clone(), cx)
                            })
                        }),
                )
        }))
}

/// Show a fetched certificate (never the key) with Copy / Save….
fn show_certificate(
    domain: String,
    certificate: Rc<TailscaleCertificate>,
    window: &mut Window,
    cx: &mut App,
) {
    let pem: SharedString = String::from_utf8_lossy(&certificate.certificate_pem)
        .into_owned()
        .into();
    window.open_dialog(cx, move |dialog, _, cx| {
        let theme = cx.theme();
        let (cert_name, key_name) = crate::core::tailscale::certificate_file_names(&domain);
        let copy = {
            let pem = pem.clone();
            dialog_button(Button::new("ts-cert-copy"))
                .outline()
                .text_label(s().tailscale.copy_certificate)
                .on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(pem.to_string()));
                    toast::show(StatusLevel::Info, s().tailscale.certificate_copied, cx);
                })
        };
        let save = {
            let domain = domain.clone();
            let certificate = certificate.clone();
            dialog_button(Button::new("ts-cert-save"))
                .primary()
                .text_label(s().common.save_as)
                .on_click(move |_, window, cx| {
                    save_certificate(domain.clone(), certificate.clone(), window, cx)
                })
        };
        dialog
            .title((s().tailscale.certificate_title)(&domain))
            .w(px(560.))
            .child(
                div()
                    .v_flex()
                    .gap_2()
                    .child(muted(
                        theme,
                        (s().tailscale.certificate_body)(&cert_name, &key_name),
                    ))
                    .child(
                        div()
                            .id("ts-cert-pem")
                            .h(px(240.))
                            .p_2()
                            .rounded_md()
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.muted)
                            .font_family(theme.mono_font_family.clone())
                            .text_xs()
                            .text_color(theme.foreground)
                            .child(pem.clone())
                            .overflow_y_scrollbar(),
                    ),
            )
            .footer(
                DialogFooter::new()
                    .child(copy)
                    .child(
                        DialogClose::new().child(
                            dialog_button(Button::new("ts-cert-close"))
                                .outline()
                                .text_label(s().common.close),
                        ),
                    )
                    .child(save),
            )
    });
}

fn save_certificate(
    domain: String,
    certificate: Rc<TailscaleCertificate>,
    window: &mut Window,
    cx: &mut App,
) {
    let rx = cx.prompt_for_paths(PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some(s().tailscale.save_here.into()),
    });
    window
        .spawn(cx, async move |cx| {
            let dir = match rx.await {
                Ok(Ok(Some(paths))) => match paths.into_iter().next() {
                    Some(dir) => dir,
                    None => return,
                },
                Ok(Err(e)) => {
                    let _ = cx.update(|_, cx| {
                        toast::show(
                            StatusLevel::Error,
                            (s().tailscale.folder_dialog_failed)(&e.to_string()),
                            cx,
                        )
                    });
                    return;
                }
                _ => return,
            };
            let _ =
                cx.update(
                    |window, cx| match save_certificate_pair(&dir, &domain, &certificate) {
                        Ok((cert, key)) => {
                            toast::show(
                                StatusLevel::Success,
                                (s().tailscale.saved_pair)(
                                    &cert.display().to_string(),
                                    &key.display().to_string(),
                                ),
                                cx,
                            );
                            window.close_dialog(cx);
                        }
                        Err(e) => toast::show(
                            StatusLevel::Error,
                            (s().tailscale.save_certificate_failed)(&e.to_string()),
                            cx,
                        ),
                    },
                );
        })
        .detach();
}

fn devices_card(
    ei: usize,
    status: &TailscaleEndpointStatus,
    entity: &Entity<TailscaleState>,
    theme: &Theme,
    now: SystemTime,
) -> Div {
    let groups = sorted_user_groups(&status.user_groups);
    let count: usize = groups.iter().map(|g| g.peers.len()).sum();
    let mut card = card_frame(theme).child(
        div()
            .h_flex()
            .items_center()
            .gap_2()
            .child(card_title(theme, s().tailscale.devices))
            .child(muted(theme, count.to_string())),
    );
    if groups.is_empty() {
        return card.child(muted(theme, s().tailscale.no_devices));
    }
    for (gi, group) in groups.iter().enumerate() {
        let heading = div()
            .h_flex()
            .items_center()
            .gap_2()
            .pt_1()
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.foreground)
                    .child(user_label(group)),
            )
            .when(
                !group.display_name.is_empty() && !group.login_name.is_empty(),
                |this| this.child(muted(theme, group.login_name.clone())),
            );
        card = card
            .child(heading)
            .children(group.peers.iter().enumerate().map(|(pi, peer)| {
                peer_row(
                    ei,
                    format!("{}-{}", gi, pi),
                    status,
                    peer,
                    entity,
                    theme,
                    now,
                )
            }));
    }
    card
}

/// A peer's role in words beside its name ("Exit node", "Shared").
fn peer_tag(color: Hsla, text: &'static str) -> Div {
    div()
        .flex_shrink_0()
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .text_color(color)
        .child(text)
}

fn peer_row(
    ei: usize,
    key: String,
    status: &TailscaleEndpointStatus,
    peer: &TailscalePeer,
    entity: &Entity<TailscaleState>,
    theme: &Theme,
    now: SystemTime,
) -> Div {
    let name = peer_name(peer);
    let mut details = Vec::new();
    if !peer.os.is_empty() {
        details.push(peer.os.clone());
    }
    if !peer.tailscale_ips.is_empty() {
        details.push(peer.tailscale_ips.join(", "));
    }
    details.push(peer_presence_label(peer, now));

    let ping = ping_address(peer).map(|ip| {
        let entity = entity.clone();
        let tag = status.endpoint_tag.clone();
        let ip = ip.to_string();
        let name = name.clone();
        Button::new(id(ei, format!("peer-ping-{}", key)))
            .ghost()
            .small()
            .text_label(s().tailscale.ping)
            .on_click(move |_, _, cx| {
                entity.update(cx, |state, cx| {
                    state.start_ping(tag.clone(), ip.clone(), name.clone(), cx)
                })
            })
    });

    div()
        .h_flex()
        .items_center()
        .gap_2()
        .px_2()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(if peer.exit_node {
            theme.primary
        } else {
            theme.border
        })
        .child(dot(if peer.online {
            theme.success
        } else {
            theme.muted_foreground
        }))
        .child(
            div()
                .v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(
                    div()
                        .h_flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .text_sm()
                                .text_color(if peer.online {
                                    theme.foreground
                                } else {
                                    theme.muted_foreground
                                })
                                .child(name),
                        )
                        // Roles as quiet words beside the name: the exit
                        // node in use in the accent, an expired key in the
                        // warning colour, the rest muted.
                        .when(peer.exit_node, |this| {
                            this.child(peer_tag(theme.primary, s().tailscale.badge_exit_node))
                        })
                        .when(!peer.exit_node && peer.exit_node_option, |this| {
                            this.child(peer_tag(
                                theme.muted_foreground,
                                s().tailscale.badge_exit_option,
                            ))
                        })
                        .when(peer.sharee_node, |this| {
                            this.child(peer_tag(theme.muted_foreground, s().tailscale.badge_shared))
                        })
                        .when(peer.expired, |this| {
                            this.child(peer_tag(theme.warning, s().tailscale.badge_key_expired))
                        }),
                )
                .child(meta_row(theme, details)),
        )
        .child(muted(theme, traffic_label(peer)).flex_shrink_0())
        .children(ping)
}
