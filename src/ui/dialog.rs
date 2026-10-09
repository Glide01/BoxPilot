//! Dialogs placed a little above the window's middle.
//!
//! gpui-component puts a dialog's top edge a tenth of the way down the
//! window, which leaves a short confirmation hanging near the top. Here a
//! dialog sits a little above the centre instead (`ABOVE` of the free
//! height over it), and stays where it opened: content that grows later
//! (an error line, a field that appears) grows the dialog downward, and it
//! only moves up when it would otherwise run off the window.
//!
//! gpui-component places a dialog by its top edge and doesn't say how tall
//! it came out, so the dialog measures itself: its title and its last part
//! (the footer) are wrapped in probes, and the
//! chrome around them is added back. The first frame opens at
//! gpui-component's spot, fully transparent (its fade starts at 0); from the
//! second the dialog is where it belongs. The extra offset is a top margin
//! on the surface, so gpui-component's own short slide-in is kept.

use std::cell::Cell;
use std::rc::Rc;

use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::dialog::{AlertDialog, Dialog, DialogAction, DialogClose, DialogFooter};
use gpui_component::{window_paddings, ActiveTheme, StyledExt, WindowExt};

use crate::i18n::s;
use crate::ui::widgets::{dialog_button, TextLabel};

/// Share of the free height above the dialog (the rest is below it): a
/// touch above the geometric centre, which reads as centred.
const ABOVE: f32 = 0.4;
/// gpui-component's dialog chrome above the title: border and top padding.
const TOP_CHROME: f32 = 17.;
/// Below a footer: bottom padding and border.
const FOOTER_TAIL: f32 = 17.;

/// Open a dialog that places itself (see the module docs). `build` wraps the
/// title in [`Centered::title`] and the footer in [`Centered::footer`].
pub fn open_dialog(
    window: &mut Window,
    cx: &mut App,
    build: impl Fn(Dialog, &Centered, &mut Window, &mut App) -> Dialog + 'static,
) {
    let centered = Centered::default();
    window.open_dialog(cx, move |dialog, window, cx| {
        let dialog = build(dialog, &centered, window, cx);
        centered.place(dialog, window, cx)
    });
}

/// Open an alert that places itself. `build` wraps the title in
/// [`Centered::title`] and gives it a [`Centered::confirm_footer`].
pub fn open_alert(
    window: &mut Window,
    cx: &mut App,
    build: impl Fn(AlertDialog, &Centered, &mut Window, &mut App) -> AlertDialog + 'static,
) {
    let centered = Centered::default();
    window.open_alert_dialog(cx, move |alert, window, cx| {
        let alert = build(alert, &centered, window, cx);
        centered.place(alert, window, cx)
    });
}

/// One open dialog's measurements and the spot it settled on.
#[derive(Clone, Default)]
pub struct Centered(Rc<Probes>);

#[derive(Default)]
pub struct Probes {
    /// Window y of the title's top edge, as last painted.
    top: Cell<Option<Pixels>>,
    /// Window y of the last part's bottom edge, and the chrome under it.
    bottom: Cell<Option<(Pixels, f32)>>,
    /// The top edge chosen when the dialog first measured, in layer
    /// coordinates, with the layer height it was chosen for.
    settled: Cell<Option<(Pixels, Pixels)>>,
}

impl Centered {
    /// The dialog's title.
    pub fn title(&self, title: impl IntoElement) -> Div {
        let probes = self.0.clone();
        probe(title, move |bounds| {
            probes.top.replace(Some(bounds.top())) != Some(bounds.top())
        })
    }

    /// A dialog's footer, its last part.
    pub fn footer(&self, footer: impl IntoElement) -> Div {
        let probes = self.0.clone();
        probe(footer, move |bounds| {
            let bottom = Some((bounds.bottom(), FOOTER_TAIL));
            probes.bottom.replace(bottom) != bottom
        })
        .w_full()
    }

    /// A confirmation's footer: Cancel, then `ok` (the alert's `on_ok`),
    /// as [`dialog_button`]s like every other dialog's — not
    /// gpui-component's default pair, which is a size up with louder text.
    pub fn confirm_footer(&self, ok: impl Into<SharedString>) -> Div {
        // In a row of their own: `DialogClose` and `DialogAction` are as
        // wide as what holds them, and side by side in the footer itself
        // they would share its whole width.
        self.footer(
            DialogFooter::new().child(
                div()
                    .h_flex()
                    .gap_2()
                    .child(DialogClose::new().child(dialog_button(
                        Button::new("alert-cancel")
                            .outline()
                            .text_label(s().common.cancel),
                    )))
                    .child(DialogAction::new().child(dialog_button(
                        Button::new("alert-ok").primary().text_label(ok),
                    ))),
            ),
        )
    }

    /// The surface's height, once both probes have painted.
    fn height(&self) -> Option<Pixels> {
        let top = self.0.top.get()?;
        let (bottom, tail) = self.0.bottom.get()?;
        Some(bottom - top + px(TOP_CHROME + tail))
    }

    fn place<D: Styled>(&self, dialog: D, window: &Window, cx: &App) -> D {
        let Some(height) = self.height() else {
            return dialog;
        };
        let paddings = window_paddings(window);
        let layer = window.viewport_size().height - paddings.top - paddings.bottom;
        let edge = cx.theme().spacing_tokens().lg;
        let settled = match self.0.settled.get() {
            Some((for_layer, top)) if for_layer == layer => top,
            // First measured, or the window was resized.
            _ => {
                let top = settled_top(layer, height, edge);
                self.0.settled.set(Some((layer, top)));
                top
            }
        };
        dialog.mt(top_margin(layer, height, edge, settled))
    }
}

/// `child`, reporting its bounds each time it paints. `report` says
/// whether they moved, and then the dialog is placed again on the next
/// frame (gpui ignores a notify sent while it draws).
fn probe(child: impl IntoElement, report: impl Fn(Bounds<Pixels>) -> bool + 'static) -> Div {
    div().relative().child(child).child(
        canvas(
            move |bounds, window, _| {
                if report(bounds) {
                    window.on_next_frame(|window, _| window.refresh());
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full(),
    )
}

/// Where a dialog `height` tall goes in a layer `layer` tall: `ABOVE` of
/// the free height over it, and at least `edge` from the top.
fn settled_top(layer: Pixels, height: Pixels, edge: Pixels) -> Pixels {
    ((layer - height) * ABOVE).max(edge)
}

/// The top margin that moves the surface from gpui-component's spot to
/// `settled`, or up only as far as needed to keep it `edge` clear of the
/// layer's bottom (never above `edge` from its top).
///
/// gpui-component's spot is a tenth of the layer down, moved up (by its
/// positioner) when the dialog wouldn't fit there; the margin isn't part of
/// what that positioner fits.
fn top_margin(layer: Pixels, height: Pixels, edge: Pixels, settled: Pixels) -> Pixels {
    let lowest = (layer - edge - height).max(edge);
    let base = (layer / 10.).min(lowest).max(edge);
    settled.min(lowest).max(edge) - base
}

#[cfg(test)]
mod tests {
    use super::{settled_top, top_margin};
    use gpui::{px, Pixels};

    const EDGE: Pixels = px(16.);

    #[test]
    fn a_short_dialog_sits_a_little_above_the_middle() {
        let top = settled_top(px(700.), px(200.), EDGE);
        assert_eq!(top, px(200.));
        // 200 over it, 300 under it.
        assert_eq!(top_margin(px(700.), px(200.), EDGE, top), px(130.));
    }

    #[test]
    fn a_dialog_taller_than_the_layer_keeps_the_edge() {
        let top = settled_top(px(700.), px(690.), EDGE);
        assert_eq!(top, EDGE);
        // gpui-component's positioner already pushed it up to the edge.
        assert_eq!(top_margin(px(700.), px(690.), EDGE, top), px(0.));
    }

    #[test]
    fn a_grown_dialog_keeps_its_top_until_it_would_run_off() {
        let top = settled_top(px(700.), px(200.), EDGE);
        // Grown by 100: same top.
        assert_eq!(top_margin(px(700.), px(300.), EDGE, top) + px(70.), top);
        // Grown to 600: moved up just enough to clear the bottom edge.
        let margin = top_margin(px(700.), px(600.), EDGE, top);
        assert_eq!(px(70.) + margin + px(600.), px(700.) - EDGE);
    }
}
