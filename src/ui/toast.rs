//! Self-owned toast notifications, replacing gpui-component's
//! `NotificationList`.
//!
//! All BoxPilot toasts go through `toast::show`. There is a single slot: a
//! new toast replaces the one on screen instead of stacking. The view is a
//! bottom-center overlay owned by `RootView`; `show` reaches it from any
//! call site through an app global.
//!
//! Owning the whole lifecycle also neutralizes the upstream gpui
//! dropped-present bug (macOS, pinned rev: the frame presented right after
//! an element is removed mid-exit-animation can be dropped, freezing a
//! translucent ghost on screen). Here the exit animation runs all the way
//! to full transparency and the element is only removed afterwards — if the
//! removal frame is dropped, the frozen frame is invisible. The old
//! forced-refresh workaround is gone.

use crate::core::settings::StatusLevel;
use gpui::{
    div, prelude::FluentBuilder as _, px, Animation, AnimationExt, App, AppContext, Context,
    ElementId, Entity, Global, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, WeakEntity, Window,
};
use gpui_component::{animation::cubic_bezier, ActiveTheme, Icon, IconName};
use std::time::Duration;

const TOAST_WIDTH_PX: f32 = 320.0;
const BOTTOM_MARGIN_PX: f32 = 16.0;
const SLIDE_PX: f32 = 24.0;
const AUTOHIDE_MS: u64 = 5000;
const ANIM_MS: u64 = 250;
/// Removal lags the exit animation so the fully transparent final frame is
/// what's on screen if the present after removal gets dropped.
const REMOVE_LAG_MS: u64 = 50;

/// Single-slot toast state machine. No gpui context (only `SharedString` as
/// a value type) so the timer races are unit-testable: every timer holds the
/// generation token of the toast it was armed for, and a token from a
/// superseded toast is a no-op.
#[derive(Default)]
struct Slot {
    current: Option<(StatusLevel, SharedString)>,
    closing: bool,
    generation: u64,
}

impl Slot {
    /// Put a toast in the slot (replacing any current one, even mid-exit).
    /// Returns the token the autohide timer must present to close it.
    fn show(&mut self, level: StatusLevel, message: SharedString) -> u64 {
        self.generation += 1;
        self.current = Some((level, message));
        self.closing = false;
        self.generation
    }

    /// Start the exit animation. Returns the token for the removal timer,
    /// or `None` if the toast was superseded or is already closing.
    fn begin_close(&mut self, token: u64) -> Option<u64> {
        if token != self.generation || self.current.is_none() || self.closing {
            return None;
        }
        self.closing = true;
        Some(self.generation)
    }

    /// Empty the slot once the exit animation finished. Stale tokens are
    /// ignored; returns whether anything changed.
    fn finish_close(&mut self, token: u64) -> bool {
        if token != self.generation || !self.closing {
            return false;
        }
        self.current = None;
        self.closing = false;
        true
    }
}

/// The toast overlay view. Created once via [`init`] and rendered by
/// `RootView`; renders nothing while the slot is empty.
pub struct Toasts {
    slot: Slot,
}

impl Toasts {
    fn show(&mut self, level: StatusLevel, message: SharedString, cx: &mut Context<Self>) {
        let token = self.slot.show(level, message);
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(AUTOHIDE_MS))
                .await;
            this.update(cx, |this, cx| this.begin_close(token, cx)).ok();
        })
        .detach();
    }

    fn begin_close(&mut self, token: u64, cx: &mut Context<Self>) {
        let Some(token) = self.slot.begin_close(token) else {
            return;
        };
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(ANIM_MS + REMOVE_LAG_MS))
                .await;
            this.update(cx, |this, cx| {
                if this.slot.finish_close(token) {
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        self.begin_close(self.slot.generation, cx);
    }
}

impl Render for Toasts {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some((level, message)) = self.slot.current.clone() else {
            return div().into_any_element();
        };
        let closing = self.slot.closing;
        let theme = cx.theme();
        let (icon, icon_color) = match level {
            StatusLevel::Info => (IconName::Info, theme.info),
            StatusLevel::Success => (IconName::CircleCheck, theme.success),
            StatusLevel::Warning => (IconName::TriangleAlert, theme.warning),
            StatusLevel::Error => (IconName::CircleX, theme.danger),
        };

        let card = div()
            .id("toast")
            // 退场期间卡片已(半)透明,不再拦截鼠标——否则一张看不见的卡
            // 会在退场+移除滞后的 ~300ms 里挡住底部区域的点击。
            .when(!closing, |this| {
                this.occlude()
                    .on_click(cx.listener(|this, _, _, cx| this.dismiss(cx)))
            })
            .relative()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .w(px(TOAST_WIDTH_PX))
            .py_3p5()
            .px_4()
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .rounded(theme.radius_lg)
            .shadow_md()
            .text_sm()
            .child(Icon::new(icon).text_color(icon_color))
            .child(div().flex_1().min_w_0().child(message))
            .with_animation(
                // Keyed on (generation, closing) so a replacement re-enters
                // and the closing flip restarts the timeline as an exit.
                ElementId::NamedInteger(
                    "toast-anim".into(),
                    self.slot.generation * 2 + closing as u64,
                ),
                Animation::new(Duration::from_millis(ANIM_MS))
                    .with_easing(cubic_bezier(0.4, 0., 0.2, 1.)),
                move |this, delta| {
                    if closing {
                        // Slide down and fade to fully transparent — see the
                        // module docs for why it must end at opacity 0.
                        this.opacity(1. - delta).top(px(delta * SLIDE_PX))
                    } else {
                        this.opacity(delta).top(px((1. - delta) * SLIDE_PX))
                    }
                },
            );

        div()
            .absolute()
            .left_0()
            .right_0()
            .bottom(px(BOTTOM_MARGIN_PX))
            .flex()
            .justify_center()
            .child(card)
            .into_any_element()
    }
}

struct GlobalToasts(WeakEntity<Toasts>);
impl Global for GlobalToasts {}

/// Create the toast view and register it globally so [`show`] can reach it
/// from any call site. Called once from `RootView::new`; the returned
/// entity must be rendered by the root view (it draws the overlay).
pub fn init(cx: &mut App) -> Entity<Toasts> {
    let toasts = cx.new(|_| Toasts {
        slot: Slot::default(),
    });
    cx.set_global(GlobalToasts(toasts.downgrade()));
    toasts
}

pub fn show(level: StatusLevel, message: impl Into<SharedString>, cx: &mut App) {
    let Some(toasts) = cx
        .try_global::<GlobalToasts>()
        .and_then(|global| global.0.upgrade())
    else {
        return;
    };
    toasts.update(cx, |this, cx| this.show(level, message.into(), cx));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(s: &str) -> SharedString {
        SharedString::from(s.to_string())
    }

    #[test]
    fn autohide_closes_current_toast() {
        let mut slot = Slot::default();
        let token = slot.show(StatusLevel::Info, msg("a"));
        let close_token = slot.begin_close(token).expect("should start closing");
        assert!(slot.closing);
        assert!(slot.finish_close(close_token));
        assert!(slot.current.is_none());
        assert!(!slot.closing);
    }

    #[test]
    fn stale_autohide_token_is_ignored_after_replacement() {
        let mut slot = Slot::default();
        let first = slot.show(StatusLevel::Info, msg("a"));
        let _second = slot.show(StatusLevel::Error, msg("b"));
        assert_eq!(slot.begin_close(first), None);
        assert!(slot.current.is_some());
        assert!(!slot.closing);
    }

    #[test]
    fn replacement_during_exit_revives_slot_and_voids_removal() {
        let mut slot = Slot::default();
        let first = slot.show(StatusLevel::Info, msg("a"));
        let removal = slot.begin_close(first).unwrap();
        let _second = slot.show(StatusLevel::Success, msg("b"));
        assert!(!slot.closing, "new toast must not inherit the exit state");
        assert!(!slot.finish_close(removal), "stale removal must be a no-op");
        assert_eq!(
            slot.current.as_ref().map(|(_, m)| m.as_ref()),
            Some("b"),
            "replacement toast must survive the old toast's removal timer"
        );
    }

    #[test]
    fn double_close_is_idempotent() {
        let mut slot = Slot::default();
        let token = slot.show(StatusLevel::Warning, msg("a"));
        assert!(slot.begin_close(token).is_some());
        assert_eq!(
            slot.begin_close(token),
            None,
            "second close while closing must not rearm the removal timer"
        );
    }

    #[test]
    fn close_on_empty_slot_is_a_noop() {
        let mut slot = Slot::default();
        assert_eq!(slot.begin_close(0), None);
        assert!(!slot.finish_close(0));
    }

    #[test]
    fn finish_close_requires_begin_close() {
        let mut slot = Slot::default();
        let token = slot.show(StatusLevel::Info, msg("a"));
        assert!(
            !slot.finish_close(token),
            "removal without an exit phase must be rejected"
        );
        assert!(slot.current.is_some());
    }
}
