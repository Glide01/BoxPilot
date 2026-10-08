//! Settings › TUN on macOS: the privileged helper's row (ADR 0006 rule 7).
//! Its state (`AppState::helper_status`, looked at again whenever this
//! page is shown), then Install, or Reinstall and Remove. Each button asks
//! first (`RootView::prompt_helper_install` / `prompt_helper_remove`), as
//! Linux's TUN grant does; macOS's own administrator prompt follows, and
//! the password never reaches BoxPilot. Nothing on Windows, whose helper
//! comes and goes with the MSI, or on Linux, which has none.

use super::SettingsPage;
use crate::core::privileged_helper::macos_status::HelperStatus;
use crate::core::privileged_helper::{process_is_elevated, HELPER_INSTALLED_BY_APP};
use crate::i18n::s;
use crate::state::{AppState, HelperChange};
use crate::ui::widgets::{setting_row, IconLabel, TextLabel};
use crate::ui::RootView;
use gpui::{
    div, prelude::FluentBuilder, AnyElement, Context, Entity, IntoElement, ParentElement,
    SharedString, Styled, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    spinner::Spinner,
    ActiveTheme, Disableable, Sizable, StyledExt,
};

/// This slot's rows, in display order; empty = nothing to show.
pub(super) fn rows(
    app_state: &Entity<AppState>,
    _window: &mut Window,
    cx: &mut Context<SettingsPage>,
) -> Vec<AnyElement> {
    if !HELPER_INSTALLED_BY_APP {
        return Vec::new();
    }
    let t = &s().settings;
    let theme = cx.theme();
    // Privilege the user brought runs sing-box directly: no helper to show.
    if process_is_elevated() {
        return vec![setting_row(theme, t.helper, Some(t.helper_as_root)).into_any_element()];
    }

    let state = app_state.read(cx);
    let status = state.helper_status.clone();
    let bundled = state.app_bundle.is_some();
    let can_change = state.can_change_helper(cx);
    let changing = state.changing_helper();

    let looked = status != HelperStatus::Unknown;
    let mut text = status.message();
    if looked && !bundled {
        text = format!("{text} {}", t.helper_no_bundle);
    }
    // A state that asks for something reads at full contrast.
    let settled = matches!(status, HelperStatus::Ready | HelperStatus::Unknown);
    let text_color = if settled {
        theme.muted_foreground
    } else {
        theme.foreground
    };

    let button = |id: &'static str, label: &'static str, busy: bool| {
        Button::new(id)
            .small()
            .map(|button| {
                if busy {
                    button.icon_label(Spinner::new(), label)
                } else {
                    button.text_label(label)
                }
            })
            .loading(busy)
            .disabled(!can_change)
    };
    let mut actions = div().h_flex().flex_shrink_0().gap_2();
    if looked && bundled {
        let installing = changing == Some(HelperChange::Install);
        let install = if status.installed() {
            button("helper-reinstall", t.reinstall_helper, installing)
        } else {
            button("helper-install", t.install_helper, installing)
        };
        let app_state_install = app_state.clone();
        actions = actions.child(
            install
                // What needs doing gets the primary button; a reinstall of
                // a helper that is fine stays quiet.
                .map(|button| {
                    if settled {
                        button.outline()
                    } else {
                        button.primary()
                    }
                })
                .on_click(move |_, window, cx| {
                    RootView::prompt_helper_install(app_state_install.clone(), false, window, cx);
                }),
        );
        if status.installed() {
            let app_state_remove = app_state.clone();
            actions = actions.child(
                button(
                    "helper-remove",
                    t.remove_helper,
                    changing == Some(HelperChange::Remove),
                )
                .outline()
                .on_click(move |_, window, cx| {
                    RootView::prompt_helper_remove(app_state_remove.clone(), window, cx);
                }),
            );
        }
    }

    // `widgets::setting_row`'s layout, with an owned state line.
    let row = div()
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
                .child(div().text_sm().text_color(theme.foreground).child(t.helper))
                .child(
                    div()
                        .text_xs()
                        .text_color(text_color)
                        .child(SharedString::from(text)),
                ),
        )
        .child(actions);

    vec![row.into_any_element()]
}
