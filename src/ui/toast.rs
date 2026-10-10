//! Self-owned toast notifications, replacing gpui-component's
//! `NotificationList`.
//!
//! All BoxPilot toasts go through `toast::show`. There is a single slot: a
//! new toast replaces the one on screen instead of stacking. The view is an
//! overlay at the bottom centre of the content panel, owned by `RootView`;
//! `show` reaches it from any call site through an app global.
//!
//! The card hugs its message (between [`MIN_WIDTH_PX`] and
//! [`MAX_WIDTH_PX`]): the level's glyph (✓ i ! ×) in a tinted disc, the
//! message, and a close button. Warnings and errors also tint the border,
//! and stay up longer ([`autohide_ms`]). The pointer over the card holds it: a toast
//! whose time runs out meanwhile closes [`LINGER_MS`] after the pointer
//! leaves. A click anywhere on it dismisses it.
//!
//! Owning the whole lifecycle also neutralizes the upstream gpui
//! dropped-present bug (macOS, pinned rev: the frame presented right after
//! an element is removed mid-exit-animation can be dropped, freezing a
//! translucent ghost on screen). Here the exit animation runs all the way
//! to full transparency and the element is only removed afterwards — if the
//! removal frame is dropped, the frozen frame is invisible. The old
//! forced-refresh workaround is gone.

use crate::core::settings::StatusLevel;
use crate::i18n::s;
use crate::ui::theme::{self, CARD_RADIUS};
use crate::ui::widgets::{Control, ControlSize};
use gpui::{
    div, prelude::FluentBuilder as _, px, Animation, AnimationExt, App, AppContext, Context,
    ElementId, Entity, Global, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, WeakEntity, Window,
};
use gpui_component::{
    animation::cubic_bezier,
    button::{Button, ButtonVariants},
    ActiveTheme, Icon, IconName,
};
use std::time::Duration;

const MIN_WIDTH_PX: f32 = 240.0;
const MAX_WIDTH_PX: f32 = 440.0;
/// Gap between the card and the content panel's bottom edge.
const BOTTOM_MARGIN_PX: f32 = 20.0;
/// The disc behind the level icon.
const BADGE_PX: f32 = 22.0;
const GLYPH_PX: f32 = 12.0;
const ENTER_SLIDE_PX: f32 = 12.0;
const EXIT_SLIDE_PX: f32 = 6.0;
const ENTER_MS: u64 = 220;
const EXIT_MS: u64 = 160;
/// How long an expired toast stays once the pointer leaves it.
const LINGER_MS: u64 = 1500;
/// Removal lags the exit animation so the fully transparent final frame is
/// what's on screen if the present after removal gets dropped.
const REMOVE_LAG_MS: u64 = 50;

/// How long a toast stays up: news briefly, problems long enough to read.
fn autohide_ms(level: StatusLevel) -> u64 {
    match level {
        StatusLevel::Info | StatusLevel::Success => 4000,
        StatusLevel::Warning => 6000,
        StatusLevel::Error => 8000,
    }
}

/// Single-slot toast state machine. No gpui context (only `SharedString` as
/// a value type) so the timer races are unit-testable: every timer holds the
/// generation token of the toast it was armed for, and a token from a
/// superseded toast is a no-op.
#[derive(Default)]
struct Slot {
    current: Option<(StatusLevel, SharedString)>,
    closing: bool,
    generation: u64,
    /// The pointer is over the card: autohide waits.
    hovered: bool,
    /// The autohide time ran out while hovered; closes once the pointer
    /// leaves.
    expired: bool,
}

impl Slot {
    /// Put a toast in the slot (replacing any current one, even mid-exit).
    /// Returns the token the autohide timer must present to close it.
    fn show(&mut self, level: StatusLevel, message: SharedString) -> u64 {
        self.generation += 1;
        self.current = Some((level, message));
        self.closing = false;
        self.expired = false;
        self.generation
    }

    /// The autohide timer for `token` fired: close, unless the pointer is
    /// holding the toast (then it's only marked expired). Returns the
    /// removal timer's token when closing starts.
    fn expire(&mut self, token: u64) -> Option<u64> {
        if token != self.generation || self.current.is_none() || self.closing {
            return None;
        }
        if self.hovered {
            self.expired = true;
            return None;
        }
        self.begin_close(token)
    }

    /// The pointer entered or left the card. Returns whether an expired
    /// toast was let go and needs a (short) autohide timer again.
    fn set_hovered(&mut self, hovered: bool) -> bool {
        self.hovered = hovered;
        if hovered || !self.expired || self.current.is_none() || self.closing {
            return false;
        }
        self.expired = false;
        true
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
        // The card is gone without a hover-leave event.
        self.hovered = false;
        self.expired = false;
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
        self.arm_autohide(token, autohide_ms(level), cx);
    }

    fn arm_autohide(&mut self, token: u64, ms: u64, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(ms))
                .await;
            this.update(cx, |this, cx| {
                if let Some(token) = this.slot.expire(token) {
                    this.closing(token, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn set_hovered(&mut self, hovered: bool, cx: &mut Context<Self>) {
        if self.slot.set_hovered(hovered) {
            self.arm_autohide(self.slot.generation, LINGER_MS, cx);
        }
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        if let Some(token) = self.slot.begin_close(self.slot.generation) {
            self.closing(token, cx);
        }
    }

    /// The exit animation has started: re-render, and empty the slot once
    /// it's over.
    fn closing(&mut self, token: u64, cx: &mut Context<Self>) {
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(EXIT_MS + REMOVE_LAG_MS))
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
}

impl Render for Toasts {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some((level, message)) = self.slot.current.clone() else {
            return div().into_any_element();
        };
        let closing = self.slot.closing;
        let theme = cx.theme();
        let dark = theme.is_dark();
        let (glyph, accent) = match level {
            StatusLevel::Info => ("icons/toast-info.svg", theme.primary),
            StatusLevel::Success => ("icons/toast-success.svg", theme.success),
            StatusLevel::Warning => ("icons/toast-warning.svg", theme.warning),
            StatusLevel::Error => ("icons/toast-error.svg", theme.danger),
        };
        let surface = theme::toast_surface(dark);
        let border = match level {
            StatusLevel::Warning | StatusLevel::Error => {
                accent.opacity(if dark { 0.45 } else { 0.35 })
            }
            StatusLevel::Info | StatusLevel::Success => surface.border,
        };

        let badge = div()
            .flex_none()
            .size(px(BADGE_PX))
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(accent.opacity(if dark { 0.2 } else { 0.12 }))
            .child(
                Icon::default()
                    .path(glyph)
                    .size(px(GLYPH_PX))
                    .text_color(accent),
            );

        let close = Button::new("toast-close")
            .ghost()
            .icon_control(ControlSize::Mini)
            .icon(IconName::Close)
            .text_color(theme.muted_foreground)
            .accessibility_label(s().common.close)
            .on_click(cx.listener(|this, _, _, cx| this.dismiss(cx)));

        let card = div()
            .id("toast")
            // 退场期间卡片已(半)透明,不再拦截鼠标——否则一张看不见的卡
            // 会在退场+移除滞后的 ~200ms 里挡住底部区域的点击。
            .when(!closing, |this| {
                this.occlude()
                    .on_hover(
                        cx.listener(|this, hovered: &bool, _, cx| this.set_hovered(*hovered, cx)),
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.dismiss(cx)))
            })
            .relative()
            .flex()
            .flex_row()
            .items_center()
            .gap_2p5()
            .min_w(px(MIN_WIDTH_PX))
            .max_w(px(MAX_WIDTH_PX))
            .py_2()
            .pl_3()
            .pr_1p5()
            .border_1()
            .border_color(border)
            .bg(surface.background)
            .text_color(theme.foreground)
            .rounded(px(CARD_RADIUS))
            .shadow_lg()
            .text_sm()
            .child(badge)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .py_0p5()
                    .line_height(px(20.))
                    .child(message),
            )
            .when(!closing, |this| this.child(close))
            .with_animation(
                // Keyed on (generation, closing) so a replacement re-enters
                // and the closing flip restarts the timeline as an exit.
                ElementId::NamedInteger(
                    "toast-anim".into(),
                    self.slot.generation * 2 + closing as u64,
                ),
                Animation::new(Duration::from_millis(if closing {
                    EXIT_MS
                } else {
                    ENTER_MS
                }))
                .with_easing(cubic_bezier(0.2, 0., 0., 1.)),
                move |this, delta| {
                    if closing {
                        // Sink and fade to fully transparent — see the
                        // module docs for why it must end at opacity 0.
                        this.opacity(1. - delta).top(px(delta * EXIT_SLIDE_PX))
                    } else {
                        this.opacity(delta).top(px((1. - delta) * ENTER_SLIDE_PX))
                    }
                },
            );

        div()
            .absolute()
            .left_0()
            .right_0()
            .bottom(px(BOTTOM_MARGIN_PX))
            .px_4()
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
        let close_token = slot.expire(token).expect("should start closing");
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
        assert_eq!(slot.expire(first), None);
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
    fn hover_holds_an_expired_toast_until_the_pointer_leaves() {
        let mut slot = Slot::default();
        let token = slot.show(StatusLevel::Error, msg("a"));
        assert!(!slot.set_hovered(true));
        assert_eq!(slot.expire(token), None, "hovered: no close yet");
        assert!(slot.current.is_some() && !slot.closing);
        assert!(slot.set_hovered(false), "leaving re-arms the timer");
        assert!(!slot.set_hovered(false), "only once");
        assert!(slot.expire(slot.generation).is_some());
    }

    #[test]
    fn hover_without_expiry_needs_no_timer() {
        let mut slot = Slot::default();
        let token = slot.show(StatusLevel::Info, msg("a"));
        slot.set_hovered(true);
        assert!(
            !slot.set_hovered(false),
            "the original timer is still armed"
        );
        assert!(slot.expire(token).is_some());
    }

    #[test]
    fn replacement_clears_an_expiry_held_by_hover() {
        let mut slot = Slot::default();
        let first = slot.show(StatusLevel::Info, msg("a"));
        slot.set_hovered(true);
        assert_eq!(slot.expire(first), None);
        let _second = slot.show(StatusLevel::Info, msg("b"));
        assert!(
            !slot.set_hovered(false),
            "the new toast has its own timer; leaving mustn't cut it short"
        );
    }

    #[test]
    fn removal_forgets_the_hover() {
        let mut slot = Slot::default();
        let token = slot.show(StatusLevel::Info, msg("a"));
        slot.set_hovered(true);
        let removal = slot.begin_close(token).unwrap();
        assert!(slot.finish_close(removal));
        let next = slot.show(StatusLevel::Info, msg("b"));
        assert!(slot.expire(next).is_some(), "a fresh card isn't hovered");
    }

    #[test]
    fn problems_stay_up_longer() {
        assert!(autohide_ms(StatusLevel::Error) > autohide_ms(StatusLevel::Warning));
        assert!(autohide_ms(StatusLevel::Warning) > autohide_ms(StatusLevel::Info));
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
