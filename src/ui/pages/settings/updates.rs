//! Settings › About update-check rows: the "Check for updates automatically"
//! switch, then the status line with its actions (Download / Skip this
//! version / Check now). State lives in `AppState::update_check`; the page
//! observes `AppState`, so a finished check re-renders it.

use super::SettingsPage;
use crate::i18n::s;
use crate::state::{app_state::UpdateCheck, AppState};
use crate::ui::widgets::{setting_row, IconLabel};
use gpui::{
    div, prelude::FluentBuilder, AnyElement, Context, Entity, Hsla, IntoElement, ParentElement,
    SharedString, Styled, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    spinner::Spinner,
    switch::Switch,
    ActiveTheme, Sizable, StyledExt,
};

/// This slot's rows, in display order; empty = nothing to show.
pub(super) fn rows(
    app_state: &Entity<AppState>,
    _window: &mut Window,
    cx: &mut Context<SettingsPage>,
) -> Vec<AnyElement> {
    let state = app_state.read(cx);
    let enabled = state.settings.check_updates;
    let check = state.update_check.clone();
    // The release to act on, and whether it's still offered (not skipped).
    let (release, offered) = match &check {
        UpdateCheck::Available(info) => (Some(info.clone()), state.update_available().is_some()),
        _ => (None, false),
    };

    let theme = cx.theme();
    let t = &s().updates;
    let (status, status_color): (SharedString, Hsla) = match &check {
        UpdateCheck::Idle => (t.not_checked.into(), theme.muted_foreground),
        UpdateCheck::Checking => (t.checking.into(), theme.muted_foreground),
        UpdateCheck::UpToDate => (t.up_to_date.into(), theme.muted_foreground),
        UpdateCheck::Available(info) if offered => {
            ((t.available)(&info.version).into(), theme.primary)
        }
        UpdateCheck::Available(info) => (
            (t.available_skipped)(&info.version).into(),
            theme.muted_foreground,
        ),
        UpdateCheck::Failed(reason) => ((t.failed)(reason).into(), theme.muted_foreground),
    };

    let auto_row = setting_row(
        theme,
        t.check_automatically,
        Some(t.check_automatically_hint),
    )
    .child(Switch::new("check-updates").checked(enabled).on_click({
        let app_state = app_state.clone();
        move |checked: &bool, _, cx| {
            let value = *checked;
            app_state.update(cx, |state, cx| state.set_check_updates(value, cx));
        }
    }));

    let mut actions = div().h_flex().flex_shrink_0().gap_2();
    if let Some(info) = release {
        let url = info.url.clone();
        let download = Button::new("update-download")
            .small()
            .label(t.download)
            .tooltip(url.clone())
            .on_click(move |_, _, cx| cx.open_url(&url));
        // The offered release gets the primary button; a skipped one stays
        // reachable, quietly.
        actions = actions.child(if offered {
            download.primary()
        } else {
            download.outline()
        });
        if offered {
            let app_state = app_state.clone();
            let version = info.version;
            actions = actions.child(
                Button::new("update-skip")
                    .outline()
                    .small()
                    .label(t.skip)
                    .on_click(move |_, _, cx| {
                        let version = version.clone();
                        app_state
                            .update(cx, |state, cx| state.skip_update_version(Some(version), cx));
                    }),
            );
        }
    }
    let checking = check == UpdateCheck::Checking;
    actions = actions.child(
        Button::new("update-check-now")
            .outline()
            .small()
            // A loading button is inert; the spinner says why.
            .map(|button| {
                if checking {
                    button.icon_label(Spinner::new(), t.check_now)
                } else {
                    button.label(t.check_now)
                }
            })
            .loading(checking)
            .on_click({
                let app_state = app_state.clone();
                move |_, _, cx| {
                    app_state.update(cx, |state, cx| state.check_for_updates(true, cx));
                }
            }),
    );

    // `widgets::setting_row`'s layout, with an owned, coloured status line.
    let status_row = div()
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
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.foreground)
                        .child(t.updates),
                )
                .child(div().text_xs().text_color(status_color).child(status)),
        )
        .child(actions);

    vec![auto_row.into_any_element(), status_row.into_any_element()]
}
