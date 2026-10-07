//! VPN page: the running config's OpenConnect / OpenVPN client endpoints
//! (state, tunnel details, errors) and USB/IP servers (shared devices,
//! read-only). Shown only while the running config has any of them (the
//! sidebar hides it otherwise).
//!
//! Sign-in: when an endpoint needs the user (a login form, a one-time code,
//! a web sign-in), `VpnStatus` announces the challenge once and this page
//! pops a dialog for it. Closing the dialog leaves the challenge pending —
//! the endpoint card's "Sign in" button reopens it. A dialog whose challenge
//! ended meanwhile (answered elsewhere, timed out) says so and only offers
//! Close; the next challenge's dialog opens on top of it.

use crate::core::settings::StatusLevel;
use crate::core::singbox_api::{
    openconnect_callback_result, openconnect_form_values, openvpn_answer, usb_speed_label,
    OpenConnectBrowserMode, OpenConnectBrowserRequest, OpenConnectChallenge, OpenConnectFieldKind,
    OpenConnectPrompt, OpenVpnChallenge, OpenVpnChallengeKind, UsbSharedDevice, VpnState,
};
use crate::core::vpn::{
    deadline_label, openconnect_browser_limitation, openconnect_tunnel_rows, openvpn_tunnel_rows,
    vpn_state_label, vpn_state_tone, ChallengeKey, InfoRow, VpnProtocol, VpnTone,
};
use crate::i18n::s;
use crate::state::vpn::VpnStream;
use crate::state::{AppState, ChallengeRequested, VpnStatus};
use crate::ui::card_frame;
use crate::ui::toast;
use crate::ui::widgets::{empty_state, form_input, meta_row, page_header, IconLabel, Lead};
use gpui::{prelude::FluentBuilder, *};
use gpui_component::{
    button::{Button, ButtonVariants},
    dialog::{DialogAction, DialogClose, DialogFooter},
    input::InputState,
    scroll::ScrollableElement,
    select::{Select, SelectState},
    theme::Theme,
    ActiveTheme, Disableable, Icon, IconName, IndexPath, Sizable, StyledExt, WindowExt,
};
use std::time::{SystemTime, UNIX_EPOCH};

const DIALOG_WIDTH: f32 = 440.;

pub struct VpnPage {
    vpn: Entity<VpnStatus>,
    /// The sign-in dialogs this page has open, bottom to top.
    open_dialogs: Vec<ChallengeKey>,
}

impl VpnPage {
    pub fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let vpn = app_state.read(cx).vpn.clone();
        cx.observe(&vpn, |_, _, cx| cx.notify()).detach();
        // The page is a cached view: the tunnels' "Uptime" rows tick with
        // the status samples (once a second while connected).
        let traffic = app_state.read(cx).traffic.clone();
        cx.observe(&traffic, |this: &mut Self, _, cx| {
            if this.vpn.read(cx).is_visible() {
                cx.notify();
            }
        })
        .detach();
        cx.subscribe_in(
            &vpn,
            window,
            |this, _, event: &ChallengeRequested, window, cx| {
                this.prompt(event.0.clone(), window, cx);
            },
        )
        .detach();
        Self {
            vpn,
            open_dialogs: Vec::new(),
        }
    }

    /// Open the sign-in dialog for `key`, unless it is already open or no
    /// longer pending.
    fn prompt(&mut self, key: ChallengeKey, window: &mut Window, cx: &mut Context<Self>) {
        if self.open_dialogs.contains(&key) {
            return;
        }
        // A dialog whose challenge ended stays up showing "this request has
        // ended" until the user closes it; the new one opens on top. Closing
        // it here would be unsafe: `close_dialog` pops whatever is topmost,
        // which may be another page's dialog (e.g. an Import link prompt).
        let dialog = DialogHandle {
            vpn: self.vpn.clone(),
            page: cx.entity().downgrade(),
            key: key.clone(),
        };
        let vpn = self.vpn.read(cx);
        if let Some(challenge) = vpn.openconnect_challenge(&key).cloned() {
            open_openconnect_dialog(dialog, challenge, window, cx);
        } else if let Some(challenge) = vpn.openvpn_challenge(&key).cloned() {
            open_openvpn_dialog(dialog, challenge, window, cx);
        } else {
            return;
        }
        self.open_dialogs.push(key);
    }
}

/// What every sign-in dialog needs to act on its challenge.
#[derive(Clone)]
struct DialogHandle {
    vpn: Entity<VpnStatus>,
    page: WeakEntity<VpnPage>,
    key: ChallengeKey,
}

impl DialogHandle {
    fn is_pending(&self, cx: &App) -> bool {
        self.vpn.read(cx).is_pending(&self.key)
    }

    /// The dialog is going away: stop tracking it.
    fn forget(&self, cx: &mut App) {
        let key = self.key.clone();
        let _ = self.page.update(cx, |page, _| {
            page.open_dialogs.retain(|open| *open != key);
        });
    }

    fn title(&self) -> String {
        (s().vpn.sign_in_title)(self.key.protocol.label(), &self.key.endpoint_tag)
    }

    /// Refuse the challenge and close the dialog. `label` says what that
    /// means for this protocol.
    fn cancel_button(&self, label: &'static str) -> Button {
        let handle = self.clone();
        Button::new("vpn-dialog-cancel")
            .outline()
            .label(label)
            .on_click(move |_, window, cx| {
                let key = handle.key.clone();
                handle
                    .vpn
                    .update(cx, |vpn, cx| vpn.cancel_challenge(key, cx));
                handle.forget(cx);
                window.close_dialog(cx);
            })
    }

    /// Footer: the cancel button on the left; Close (keeps the challenge
    /// pending) and, when there is something to submit, the submit button on
    /// the right. An ended challenge only gets Close.
    fn footer(
        &self,
        pending: bool,
        cancel_label: &'static str,
        submit_label: Option<&'static str>,
    ) -> DialogFooter {
        let right = div()
            .h_flex()
            .gap_2()
            .child(
                DialogClose::new().child(Button::new("vpn-dialog-close").outline().label(
                    if pending && submit_label.is_some() {
                        s().vpn.later
                    } else {
                        s().common.close
                    },
                )),
            )
            .when_some(submit_label.filter(|_| pending), |this, label| {
                this.child(
                    DialogAction::new()
                        .child(Button::new("vpn-dialog-submit").primary().label(label)),
                )
            });
        DialogFooter::new()
            .justify_between()
            .child(div().when(pending, |this| this.child(self.cancel_button(cancel_label))))
            .child(right)
    }
}

/// One input of an OpenConnect form.
#[derive(Clone)]
enum FieldInput {
    Text(Entity<InputState>),
    /// The select state and the option values, by index.
    Choice(Entity<SelectState<Vec<SharedString>>>, Vec<String>),
}

impl FieldInput {
    fn answer(&self, cx: &App) -> String {
        match self {
            FieldInput::Text(input) => input.read(cx).value().to_string(),
            FieldInput::Choice(select, values) => select
                .read(cx)
                .selected_index(cx)
                .and_then(|ix| values.get(ix.row).cloned())
                .unwrap_or_default(),
        }
    }

    fn element(&self) -> AnyElement {
        match self {
            FieldInput::Text(input) => form_input(input).cleanable(false).into_any_element(),
            FieldInput::Choice(select, _) => Select::new(select).into_any_element(),
        }
    }
}

fn labeled(theme: &Theme, label: String, input: AnyElement) -> Div {
    div()
        .v_flex()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(label),
        )
        .child(input)
}

fn text_input(
    window: &mut Window,
    cx: &mut App,
    value: &str,
    masked: bool,
    placeholder: &'static str,
) -> Entity<InputState> {
    let value = value.to_string();
    cx.new(|cx| {
        InputState::new(window, cx)
            .masked(masked)
            .placeholder(placeholder)
            .default_value(value)
    })
}

/// The challenge's own words: banner, prompt, previous error.
fn challenge_text(theme: &Theme, banner: &str, message: &str, error: &str) -> Div {
    div()
        .v_flex()
        .gap_2()
        .when(!banner.trim().is_empty(), |this| {
            this.child(
                div()
                    .p_2()
                    .rounded_md()
                    .bg(theme.muted)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(banner.trim().to_string()),
            )
        })
        .when(!message.trim().is_empty(), |this| {
            this.child(div().text_sm().child(message.trim().to_string()))
        })
        .when(!error.trim().is_empty(), |this| {
            this.child(
                div()
                    .text_sm()
                    .text_color(theme.danger)
                    .child(error.trim().to_string()),
            )
        })
}

fn ended_notice(theme: &Theme) -> Div {
    div()
        .text_sm()
        .text_color(theme.muted_foreground)
        .child(s().vpn.ended)
}

fn open_browser_button(id: &'static str, url: String) -> Button {
    Button::new(id)
        .outline()
        // A medium button's icon is 16px, a size up from its text.
        .icon_label(
            Lead::Sized(
                Icon::new(IconName::ExternalLink)
                    .size_4()
                    .into_any_element(),
                px(16.),
            ),
            s().vpn.open_sign_in_page,
        )
        .on_click(move |_, _, cx| cx.open_url(&url))
}

fn open_openconnect_dialog(
    handle: DialogHandle,
    challenge: OpenConnectChallenge,
    window: &mut Window,
    cx: &mut App,
) {
    match challenge.prompt.clone() {
        OpenConnectPrompt::Form(fields) => {
            let inputs: Vec<FieldInput> = fields
                .iter()
                .map(|field| match field.kind {
                    OpenConnectFieldKind::Select => {
                        let labels: Vec<SharedString> = field
                            .options
                            .iter()
                            .map(|choice| SharedString::from(choice.display_label().to_string()))
                            .collect();
                        let values = field.options.iter().map(|c| c.value.clone()).collect();
                        let selected = field.initial_choice().map(IndexPath::new);
                        let state = cx.new(|cx| SelectState::new(labels, selected, window, cx));
                        FieldInput::Choice(state, values)
                    }
                    OpenConnectFieldKind::Password => {
                        FieldInput::Text(text_input(window, cx, &field.value, true, ""))
                    }
                    OpenConnectFieldKind::Text | OpenConnectFieldKind::Other(_) => {
                        FieldInput::Text(text_input(window, cx, &field.value, false, ""))
                    }
                })
                .collect();
            window.open_dialog(cx, move |dialog, _, cx| {
                let pending = handle.is_pending(cx);
                let theme = cx.theme();
                let mut body = div().v_flex().gap_3().child(challenge_text(
                    theme,
                    &challenge.banner,
                    &challenge.message,
                    &challenge.error,
                ));
                if pending {
                    for (field, input) in fields.iter().zip(&inputs) {
                        let element = match (&field.kind, input) {
                            (OpenConnectFieldKind::Password, FieldInput::Text(state)) => {
                                form_input(state)
                                    .cleanable(false)
                                    .mask_toggle()
                                    .into_any_element()
                            }
                            _ => input.element(),
                        };
                        body = body.child(labeled(theme, field.display_label(), element));
                    }
                } else {
                    body = body.child(ended_notice(theme));
                }
                dialog
                    .title(handle.title())
                    .w(px(DIALOG_WIDTH))
                    .child(body)
                    // A form without fields is a "click to continue" step.
                    .footer(handle.footer(
                        pending,
                        s().vpn.cancel_sign_in,
                        Some(if fields.is_empty() {
                            s().vpn.continue_
                        } else {
                            s().vpn.sign_in
                        }),
                    ))
                    .on_ok({
                        let handle = handle.clone();
                        let fields = fields.clone();
                        let inputs = inputs.clone();
                        move |_, _, cx| {
                            if !handle.is_pending(cx) {
                                return true;
                            }
                            let answers: Vec<String> =
                                inputs.iter().map(|input| input.answer(cx)).collect();
                            match openconnect_form_values(&fields, &answers) {
                                Ok(values) => {
                                    let key = handle.key.clone();
                                    handle.vpn.update(cx, |vpn, cx| {
                                        vpn.submit_openconnect_form(key, values, cx)
                                    });
                                    true
                                }
                                Err(message) => {
                                    toast::show(StatusLevel::Warning, message, cx);
                                    false
                                }
                            }
                        }
                    })
                    .on_close({
                        let handle = handle.clone();
                        move |_, _, cx| handle.forget(cx)
                    })
            });
        }
        OpenConnectPrompt::Browser(request) => {
            let pasted = text_input(window, cx, "", false, "http://127.0.0.1:…");
            window.open_dialog(cx, move |dialog, _, cx| {
                let pending = handle.is_pending(cx);
                let theme = cx.theme();
                let limitation = openconnect_browser_limitation(&request);
                let mut body = div().v_flex().gap_3().child(challenge_text(
                    theme,
                    &challenge.banner,
                    &challenge.message,
                    &challenge.error,
                ));
                if !pending {
                    body = body.child(ended_notice(theme));
                } else if let Some(limitation) = limitation.clone() {
                    body = body.child(
                        div()
                            .h_flex()
                            .items_start()
                            .gap_2()
                            .child(
                                Icon::new(IconName::TriangleAlert)
                                    .small()
                                    .text_color(theme.warning),
                            )
                            .child(div().flex_1().text_sm().child(limitation)),
                    );
                } else {
                    body = body.child(callback_instructions(theme, &request)).child(
                        div()
                            .v_flex()
                            .gap_2()
                            .child(div().h_flex().child(open_browser_button(
                                "vpn-open-browser",
                                request.url.clone(),
                            )))
                            .child(labeled(
                                theme,
                                s().vpn.callback_address.to_string(),
                                form_input(&pasted).cleanable(true).into_any_element(),
                            )),
                    );
                }
                let can_submit = limitation.is_none();
                dialog
                    .title(handle.title())
                    .w(px(DIALOG_WIDTH))
                    .child(body)
                    .footer(handle.footer(
                        pending,
                        s().vpn.cancel_sign_in,
                        can_submit.then_some(s().vpn.sign_in),
                    ))
                    .on_ok({
                        let handle = handle.clone();
                        let request = request.clone();
                        let pasted = pasted.clone();
                        move |_, _, cx| {
                            if !can_submit || !handle.is_pending(cx) {
                                return true;
                            }
                            let address = pasted.read(cx).value().to_string();
                            match openconnect_callback_result(&request, &address) {
                                Ok(result) => {
                                    let key = handle.key.clone();
                                    handle.vpn.update(cx, |vpn, cx| {
                                        vpn.submit_openconnect_browser(key, result, cx)
                                    });
                                    true
                                }
                                Err(message) => {
                                    toast::show(StatusLevel::Warning, message, cx);
                                    false
                                }
                            }
                        }
                    })
                    .on_close({
                        let handle = handle.clone();
                        move |_, _, cx| handle.forget(cx)
                    })
            });
        }
        OpenConnectPrompt::Unknown => {
            window.open_dialog(cx, move |dialog, _, cx| {
                let pending = handle.is_pending(cx);
                let theme = cx.theme();
                let body = div()
                    .v_flex()
                    .gap_3()
                    .child(challenge_text(
                        theme,
                        &challenge.banner,
                        &challenge.message,
                        &challenge.error,
                    ))
                    .child(if pending {
                        div().text_sm().child(s().vpn.step_too_new)
                    } else {
                        ended_notice(theme)
                    });
                dialog
                    .title(handle.title())
                    .w(px(DIALOG_WIDTH))
                    .child(body)
                    .footer(handle.footer(pending, s().vpn.cancel_sign_in, None))
                    .on_close({
                        let handle = handle.clone();
                        move |_, _, cx| handle.forget(cx)
                    })
            });
        }
    }
}

/// How to finish a callback-mode sign-in from the system browser.
fn callback_instructions(theme: &Theme, request: &OpenConnectBrowserRequest) -> Div {
    debug_assert_eq!(request.mode(), OpenConnectBrowserMode::Callback);
    let prefixes = request.callback_url_prefixes.join(s().vpn.or);
    div()
        .v_flex()
        .gap_1()
        .text_sm()
        .child(s().vpn.callback_intro)
        .child((s().vpn.callback_body)(&prefixes))
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(request.url.clone()),
        )
}

fn open_openvpn_dialog(
    handle: DialogHandle,
    challenge: OpenVpnChallenge,
    window: &mut Window,
    cx: &mut App,
) {
    let prompt = challenge.prompt();
    let username = text_input(window, cx, &challenge.username, false, "");
    let password = text_input(window, cx, "", true, "");
    let secret_masked = prompt.secret.as_ref().is_some_and(|secret| !secret.echo);
    let secret = text_input(window, cx, "", secret_masked, "");

    window.open_dialog(cx, move |dialog, _, cx| {
        let pending = handle.is_pending(cx);
        let theme = cx.theme();
        let mut body = div().v_flex().gap_3();
        if !challenge.previous_error.trim().is_empty() {
            body = body.child(div().text_sm().text_color(theme.danger).child(
                (s().vpn.last_attempt_failed)(challenge.previous_error.trim()),
            ));
        }
        if !pending {
            body = body.child(ended_notice(theme));
        } else {
            match &challenge.kind {
                OpenVpnChallengeKind::Message => {
                    body = body.child(div().text_sm().child(challenge.message.clone()));
                }
                OpenVpnChallengeKind::OpenUrl => {
                    body = body
                        .child(div().text_sm().child(s().vpn.open_url_body))
                        .children(prompt.open_url.clone().map(|url| {
                            div()
                                .v_flex()
                                .gap_2()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(url.clone()),
                                )
                                .child(
                                    div()
                                        .h_flex()
                                        .child(open_browser_button("vpn-open-browser", url)),
                                )
                        }));
                }
                OpenVpnChallengeKind::Other(kind) => {
                    body = body.child(div().text_sm().child((s().vpn.unknown_step)(kind)));
                }
                OpenVpnChallengeKind::Credentials | OpenVpnChallengeKind::Secret => {}
            }
            if prompt.credentials {
                body = body
                    .child(labeled(
                        theme,
                        s().vpn.username.to_string(),
                        form_input(&username).cleanable(false).into_any_element(),
                    ))
                    .child(labeled(
                        theme,
                        s().vpn.password.to_string(),
                        form_input(&password)
                            .cleanable(false)
                            .mask_toggle()
                            .into_any_element(),
                    ));
            } else if !challenge.username.is_empty() && prompt.secret.is_some() {
                body = body.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child((s().vpn.account)(&challenge.username)),
                );
            }
            if let Some(secret_prompt) = &prompt.secret {
                let input = form_input(&secret).cleanable(false);
                let input = if secret_prompt.echo {
                    input
                } else {
                    input.mask_toggle()
                };
                let label = if secret_prompt.label.is_empty() {
                    s().vpn.response.to_string()
                } else {
                    secret_prompt.label.clone()
                };
                body = body.child(labeled(theme, label, input.into_any_element()));
            }
            if let Some(deadline) = challenge.deadline {
                body = body.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(deadline_label(deadline, unix_now())),
                );
            }
        }
        dialog
            .title(handle.title())
            .w(px(DIALOG_WIDTH))
            .child(body)
            .footer(handle.footer(
                pending,
                s().vpn.disconnect,
                prompt.answerable().then_some(s().vpn.sign_in),
            ))
            .on_ok({
                let handle = handle.clone();
                let challenge = challenge.clone();
                let username = username.clone();
                let password = password.clone();
                let secret = secret.clone();
                move |_, _, cx| {
                    if !challenge.prompt().answerable() || !handle.is_pending(cx) {
                        return true;
                    }
                    match openvpn_answer(
                        &challenge,
                        &username.read(cx).value(),
                        &password.read(cx).value(),
                        &secret.read(cx).value(),
                    ) {
                        Ok(answer) => {
                            let key = handle.key.clone();
                            handle
                                .vpn
                                .update(cx, |vpn, cx| vpn.submit_openvpn(key, answer, cx));
                            true
                        }
                        Err(message) => {
                            toast::show(StatusLevel::Warning, message, cx);
                            false
                        }
                    }
                }
            })
            .on_close({
                let handle = handle.clone();
                move |_, _, cx| handle.forget(cx)
            })
    });
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn tone_color(tone: VpnTone, theme: &Theme) -> Hsla {
    match tone {
        VpnTone::Success => theme.success,
        VpnTone::Warning => theme.warning,
        VpnTone::Danger => theme.danger,
        VpnTone::Muted => theme.muted_foreground,
    }
}

fn info_rows(theme: &Theme, rows: Vec<InfoRow>) -> Div {
    div().v_flex().gap_1().children(rows.into_iter().map(|row| {
        div()
            .h_flex()
            .items_start()
            .gap_3()
            .child(
                div()
                    .w(px(80.))
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(row.label),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .text_color(theme.foreground)
                    .child(row.value),
            )
    }))
}

/// Everything an endpoint card shows, protocol-independent.
struct EndpointCard {
    key_prefix: &'static str,
    protocol: VpnProtocol,
    tag: String,
    /// `None` until the first snapshot.
    state: Option<(VpnState, String)>,
    error: String,
    rows: Vec<InfoRow>,
    challenge_id: Option<String>,
}

impl VpnPage {
    fn endpoint_card(
        &self,
        ix: usize,
        card: EndpointCard,
        busy: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let (state_label, state_color) = match &card.state {
            Some((state, text)) => (
                vpn_state_label(state, text),
                tone_color(vpn_state_tone(state), theme),
            ),
            None => (
                s().vpn.waiting_for_sing_box.to_string(),
                theme.muted_foreground,
            ),
        };
        let sign_in = card.challenge_id.clone().map(|challenge_id| {
            let key = ChallengeKey {
                protocol: card.protocol,
                endpoint_tag: card.tag.clone(),
                challenge_id,
            };
            Button::new(SharedString::from(format!(
                "{}-sign-in-{}",
                card.key_prefix, ix
            )))
            .primary()
            .small()
            .label(s().vpn.sign_in)
            .loading(busy)
            .disabled(busy)
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.prompt(key.clone(), window, cx)
            }))
        });
        let header = div()
            .h_flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.foreground)
                    .child(card.tag.clone()),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(card.protocol.label()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .text_color(state_color)
                    .child(state_label),
            )
            .children(sign_in);
        card_frame(theme)
            .child(header)
            .when(!card.error.trim().is_empty(), |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(theme.danger)
                        .child(card.error.trim().to_string()),
                )
            })
            .when(!card.rows.is_empty(), |this| {
                this.child(info_rows(theme, card.rows))
            })
    }
}

fn device_row(theme: &Theme, device: &UsbSharedDevice) -> Div {
    let mut details = vec![device.usb_id(), (s().vpn.bus)(&device.bus_id.to_string())];
    if let Some(speed) = usb_speed_label(device.speed) {
        details.push(speed.to_string());
    }
    if !device.serial.is_empty() {
        details.push((s().vpn.serial)(&device.serial));
    }
    div()
        .h_flex()
        .items_center()
        .gap_3()
        .child(
            div()
                .v_flex()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.foreground)
                        .child(device.display_name().to_string()),
                )
                .child(meta_row(theme, details)),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(device.state.label()),
        )
}

fn usbip_card(theme: &Theme, tag: &str, body: Div) -> Div {
    card_frame(theme)
        .child(
            div()
                .h_flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.foreground)
                        .child(tag.to_string()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(s().vpn.usbip_server),
                ),
        )
        .child(body)
}

fn muted_note(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(text.into())
}

impl Render for VpnPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let vpn = self.vpn.read(cx);
        let presence = vpn.presence.clone();
        let openconnect = vpn.openconnect.clone();
        let openvpn = vpn.openvpn.clone();
        let usbip = vpn.usbip.clone();
        let loaded = vpn.loaded.clone();
        let stream_errors: Vec<String> = vpn.stream_errors.values().cloned().collect();
        let busy = vpn.busy.clone();
        let now = unix_now();
        let theme = cx.theme().clone();
        let theme = &theme;

        let title = page_header(theme, s().vpn.title);
        if presence.is_empty() {
            return div()
                .v_flex()
                .size_full()
                .gap_4()
                .child(title)
                .child(empty_state(
                    theme,
                    IconName::Globe,
                    s().vpn.empty_title,
                    s().vpn.empty_hint,
                ))
                .into_any_element();
        }

        let mut cards: Vec<Div> = Vec::new();
        for error in stream_errors {
            cards.push(
                card_frame(theme).child(
                    div()
                        .h_flex()
                        .items_start()
                        .gap_2()
                        .child(
                            Icon::new(IconName::CircleAlert)
                                .small()
                                .text_color(theme.danger),
                        )
                        .child(div().flex_1().text_sm().child(error)),
                ),
            );
        }

        let is_busy = |protocol, tag: &str, id: Option<&String>| {
            id.is_some_and(|id| {
                busy.contains(&ChallengeKey {
                    protocol,
                    endpoint_tag: tag.to_string(),
                    challenge_id: id.clone(),
                })
            })
        };

        // Endpoints: sing-box's snapshot once it has arrived, the config's
        // tags until then.
        let mut endpoint_cards = Vec::new();
        if loaded.contains(&VpnStream::OpenConnect) {
            for status in &openconnect {
                let challenge_id = status.challenge.as_ref().map(|c| c.id.clone());
                let busy = is_busy(
                    VpnProtocol::OpenConnect,
                    &status.endpoint_tag,
                    challenge_id.as_ref(),
                );
                endpoint_cards.push((
                    EndpointCard {
                        key_prefix: "openconnect",
                        protocol: VpnProtocol::OpenConnect,
                        tag: status.endpoint_tag.clone(),
                        state: Some((status.state.clone(), status.state_text.clone())),
                        error: status.error.clone(),
                        rows: status
                            .tunnel
                            .as_ref()
                            .map(|tunnel| openconnect_tunnel_rows(tunnel, now))
                            .unwrap_or_default(),
                        challenge_id,
                    },
                    busy,
                ));
            }
        } else {
            for tag in &presence.openconnect {
                endpoint_cards.push((
                    placeholder_card("openconnect", VpnProtocol::OpenConnect, tag),
                    false,
                ));
            }
        }
        if loaded.contains(&VpnStream::OpenVpn) {
            for status in &openvpn {
                let challenge_id = status.challenge.as_ref().map(|c| c.id.clone());
                let busy = is_busy(
                    VpnProtocol::OpenVpn,
                    &status.endpoint_tag,
                    challenge_id.as_ref(),
                );
                endpoint_cards.push((
                    EndpointCard {
                        key_prefix: "openvpn",
                        protocol: VpnProtocol::OpenVpn,
                        tag: status.endpoint_tag.clone(),
                        state: Some((status.state.clone(), status.state_text.clone())),
                        error: status.error.clone(),
                        rows: status
                            .tunnel
                            .as_ref()
                            .map(|tunnel| openvpn_tunnel_rows(tunnel, now))
                            .unwrap_or_default(),
                        challenge_id,
                    },
                    busy,
                ));
            }
        } else {
            for tag in &presence.openvpn {
                endpoint_cards.push((
                    placeholder_card("openvpn", VpnProtocol::OpenVpn, tag),
                    false,
                ));
            }
        }
        for (ix, (card, busy)) in endpoint_cards.into_iter().enumerate() {
            cards.push(self.endpoint_card(ix, card, busy, theme, cx));
        }

        // USB/IP: dynamic servers report their devices; default ones don't.
        let usbip_loaded = loaded.contains(&VpnStream::Usbip);
        for tag in &presence.usbip_dynamic {
            let server = usbip.iter().find(|server| &server.server_tag == tag);
            let body = match server {
                Some(server) if !server.devices.is_empty() => div().v_flex().gap_2().children(
                    server
                        .devices
                        .iter()
                        .map(|device| device_row(theme, device)),
                ),
                Some(_) => muted_note(theme, s().vpn.no_devices_shared),
                None if usbip_loaded => muted_note(theme, s().vpn.no_status),
                None => muted_note(theme, s().vpn.waiting_for_sing_box),
            };
            cards.push(usbip_card(theme, tag, body));
        }
        for tag in &presence.usbip_default {
            cards.push(usbip_card(
                theme,
                tag,
                muted_note(theme, s().vpn.default_server_hint),
            ));
        }

        div()
            .v_flex()
            .size_full()
            .gap_4()
            .child(title)
            .child(
                div().flex_1().min_h_0().child(
                    div()
                        .v_flex()
                        .gap_3()
                        .children(cards)
                        .overflow_y_scrollbar(),
                ),
            )
            .into_any_element()
    }
}

fn placeholder_card(key_prefix: &'static str, protocol: VpnProtocol, tag: &str) -> EndpointCard {
    EndpointCard {
        key_prefix,
        protocol,
        tag: tag.to_string(),
        state: None,
        error: String::new(),
        rows: Vec::new(),
        challenge_id: None,
    }
}
