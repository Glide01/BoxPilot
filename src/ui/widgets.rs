//! Shared chrome builders beside `card_frame`: empty states,
//! labeled setting rows, grouped lists, segmented controls, setting
//! dropdowns, quiet status labels, the subscription usage meter and a
//! profile's update button and source line (Home and Profiles). Element
//! builders (only [`choice_select`] keeps state, in the window) — one place
//! to restyle what every page repeats.
//!
//! House style: related facts sit side by side, set apart by space (see
//! [`meta_row`]) or on lines of their own — never strung together with
//! " · ". State reads as coloured text or a small dot beside it
//! ([`status_label`]), not as filled badges.
//!
//! Sizes: every control (button, field, segmented control, dropdown) takes
//! its height from where it sits, never from the component's own default,
//! so controls side by side always line up. [`ControlSize`] has the
//! scale:
//!
//! | Size      | Height | Where                                              |
//! |-----------|--------|----------------------------------------------------|
//! | `Mini`    | 20px   | icon buttons in a table row or a dense panel       |
//! | `Inline`  | 24px   | controls inside a line of content (a list row, a   |
//! |           |        | profile's name line, a group's header)             |
//! | `Regular` | 28px   | page header actions, toolbars, card and section    |
//! |           |        | headers, setting rows, dialog footers              |
//! | `Field`   | 32px   | a dialog's form: its fields and the buttons and    |
//! |           |        | switches set among them; an empty state's action   |
//!
//! Buttons size with [`Control::control`] (labelled) or
//! [`Control::icon_control`] (icon only, square), fields with
//! [`control_input`], segmented controls with [`segmented`]'s size. All
//! but `Mini` set their text at 14px; an icon-only button's icon is 16px
//! from `Regular` up.
//!
//! Buttons: `primary` marks the one action a page or card is there for
//! (Add a profile, Start a test, Sign in); every other labelled action is
//! `outline` (Test all, Close all, Clear). `ghost` is for icon-only buttons
//! and for quiet actions inside list rows. An empty state's call to action
//! is [`empty_state_button`]: primary, `Field` tall. A label goes on with
//! [`TextLabel::text_label`], not `.label(..)`, so its letters sit in the
//! middle of the button; an icon goes in front of it with
//! [`IconLabel::icon_label`], not `.icon(..).label(..)`, and in front of
//! other text with [`text_centered`]: both centre it on the letters.
//!
//! Words: a label says what a control is; a hint, when one is needed at
//! all, is one short line. No paragraphs of explanation on a page.
//!
//! Choices: a setting with two options is a switch or a [`segmented`] control;
//! one with three or more is a [`choice_select`] dropdown. A dropdown in a
//! setting row is a [`plain_select`]: no box, the current choice and a
//! caret, as wide as that text.

use crate::actions::KEY_CONTEXT;
use crate::core::presentation::{ConnectionStatus, Freshness, FreshnessState, ProfileRowInfo};
use crate::core::sub_usage::{expiry_date_utc, SubscriptionUsage, UsageLevel};
use crate::core::timefmt::{format_relative_time, from_unix_secs, to_unix_secs};
use crate::i18n::s;
use crate::ui::card_frame;
use crate::ui::pages::ActivePage;
use crate::ui::theme::{FORM_MAX_WIDTH, PANEL_PADDING_X, PANEL_PADDING_Y};
use gpui::{
    canvas, div, linear_color_stop, linear_gradient, point, prelude::FluentBuilder, px, rems,
    Action, AnyElement, App, Bounds, BoxShadow, ClickEvent, Context, Div, ElementId, Entity,
    FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement, Pixels, Point, RenderOnce,
    ScrollHandle, SharedString, Stateful, StatefulInteractiveElement, Styled, Task, TextStyle,
    Window,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    input::{Input, InputState},
    progress::Progress,
    scroll::{Scrollbar, ScrollbarHandle},
    searchable_list::{SearchableListDelegate, SearchableListItem},
    select::{Select, SelectEvent, SelectState},
    spinner::Spinner,
    theme::Theme,
    tooltip::Tooltip,
    Icon, IconName, IndexPath, Sizable, Size, StyledExt,
};
use std::rc::Rc;
use std::time::{Duration, SystemTime};

/// Where to put the top of a `lead_height` tall icon beside one line of
/// `text` set in `style`, from the top of the text's line box, so the icon
/// is centred on the letters (half the cap height above the baseline)
/// rather than on the line box.
///
/// gpui centres the line box, and puts the baseline wherever the ascent
/// and descent of every font the line uses take it: CJK text gets those of
/// its fallback font, Latin text those of the UI font. So an icon centred
/// on the box sits a pixel or two above or below the letters, by an
/// amount that differs per font, size, language and platform. This does
/// gpui's arithmetic (`paint_line`) on the line as it will be shaped.
fn lead_top(
    text: &SharedString,
    style: &TextStyle,
    lead_height: Pixels,
    window: &Window,
) -> Pixels {
    window.pixel_snap(letters_middle(text, style, window) - lead_height / 2.)
}

/// How far below the top of the line box the middle of one line of
/// `text`'s letters lies (half the cap height above the baseline), as
/// gpui will paint it (see [`lead_top`]).
fn letters_middle(text: &SharedString, style: &TextStyle, window: &Window) -> Pixels {
    let font_size = style.font_size.to_pixels(window.rem_size());
    let line_height = line_height(style, window);
    if text.is_empty() {
        return line_height / 2.;
    }
    let text_system = window.text_system();
    // The same shaping the text's own layout does (and caches).
    let line = text_system.shape_line(text.clone(), font_size, &[style.to_run(text.len())], None);
    let baseline = window.pixel_snap((line_height - line.ascent - line.descent) / 2. + line.ascent);
    let font_id = text_system.resolve_font(&style.font());
    // Fonts with an old OS/2 table (DejaVu Sans) report none; UI fonts'
    // cap heights are all close to this.
    let cap_height = Some(text_system.cap_height(font_id, font_size))
        .filter(|height| *height > Pixels::ZERO)
        .unwrap_or(font_size * 0.72);
    baseline - cap_height / 2.
}

/// How far to move an element `lead_height` tall that its row centres in
/// a `box_height` tall box (as gpui-component's sidebar items centre their
/// icon) so it sits like [`TextCentered`] beside one line of `text`, set
/// in `style` and centred in the same box. For leads a component places
/// itself; elsewhere use [`TextCentered`].
pub fn lead_offset(
    text: &SharedString,
    style: &TextStyle,
    lead_height: Pixels,
    box_height: Pixels,
    window: &Window,
) -> Pixels {
    let line_top = window.pixel_snap((box_height - line_height(style, window)) / 2.);
    let centred = window.pixel_snap((box_height - lead_height) / 2.);
    line_top + lead_top(text, style, lead_height, window) - centred
}

/// The height of a line of text in `style`, as gpui's text element
/// computes it (not `TextStyle::line_height_in_pixels`, which rounds
/// differently).
fn line_height(style: &TextStyle, window: &Window) -> Pixels {
    let rem_size = window.rem_size();
    let font_size = style.font_size.to_pixels(rem_size);
    window.pixel_snap(style.line_height.to_pixels(font_size.into(), rem_size))
}

/// Something set in front of one line of text (an icon, a spinner, a
/// status dot), centred on the text's letters rather than on its line box
/// (see [`lead_top`]). It takes the text style of the row it sits in, so
/// that row, not the text beside it, must set the text's size and weight.
/// Rows must centre their items (`items_center`): this is a box one line
/// of text tall, aligned like the text's own.
#[derive(IntoElement)]
pub struct TextCentered {
    lead: Lead,
    text: SharedString,
}

/// What [`TextCentered`] holds.
pub enum Lead {
    /// An icon at the text's size, as `Button` sizes its icon.
    Icon(Icon),
    Spinner(Spinner),
    /// Any element `height` tall (and as wide).
    Sized(AnyElement, Pixels),
}

impl From<Icon> for Lead {
    fn from(icon: Icon) -> Self {
        Lead::Icon(icon)
    }
}

impl From<IconName> for Lead {
    fn from(icon: IconName) -> Self {
        Lead::Icon(Icon::new(icon))
    }
}

impl From<Spinner> for Lead {
    fn from(spinner: Spinner) -> Self {
        Lead::Spinner(spinner)
    }
}

pub fn text_centered(lead: impl Into<Lead>, text: impl Into<SharedString>) -> TextCentered {
    TextCentered {
        lead: lead.into(),
        text: text.into(),
    }
}

impl RenderOnce for TextCentered {
    fn render(self, window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let style = window.text_style();
        let rem_size = window.rem_size();
        let font_size = style.font_size.to_pixels(rem_size);
        let (lead, size) = match self.lead {
            Lead::Icon(icon) => (icon.size(font_size).into_any_element(), font_size),
            Lead::Spinner(spinner) => (spinner.with_size(font_size).into_any_element(), font_size),
            Lead::Sized(element, size) => (element, size),
        };
        let top = lead_top(&self.text, &style, size, window);
        div()
            .relative()
            .flex_none()
            .w(size)
            .h(line_height(&style, window))
            .child(div().absolute().left_0().top(top).child(lead))
    }
}

/// One line of `text`, or something drawn with it, moved up or down so
/// the letters sit in the middle of their line box instead of wherever the
/// fonts' ascent and descent put them. Chinese takes its fallback font's,
/// which (Noto Sans CJK on Linux) sets it a pixel or two low: invisible in
/// running text, plain to see inside a button. Takes the text style of the
/// element it sits in, as [`TextCentered`] does.
#[derive(IntoElement)]
pub struct OnLetters {
    text: SharedString,
    child: AnyElement,
}

impl RenderOnce for OnLetters {
    fn render(self, window: &mut Window, _: &mut App) -> impl IntoElement {
        let style = window.text_style();
        let shift = line_height(&style, window) / 2. - letters_middle(&self.text, &style, window);
        div()
            .relative()
            .top(window.pixel_snap(shift))
            .min_w_0()
            .child(self.child)
    }
}

/// A button label's own wrapper (`Button::label`'s), centred on its letters.
fn button_text(label: SharedString) -> OnLetters {
    OnLetters {
        text: label.clone(),
        child: div()
            .min_w_0()
            .whitespace_nowrap()
            .text_ellipsis()
            .child(label)
            .into_any_element(),
    }
}

/// `Button::label`, with the letters centred in the button ([`OnLetters`]).
pub trait TextLabel {
    fn text_label(self, label: impl Into<SharedString>) -> Self;
}

impl TextLabel for Button {
    fn text_label(self, label: impl Into<SharedString>) -> Self {
        let label = label.into();
        self.accessibility_label(label.clone())
            .child(button_text(label))
    }
}

/// `Button::icon` + `Button::label`, with the icon centred on the label's
/// letters ([`TextCentered`]) and both on the button ([`OnLetters`]). The icon takes the label's size, as a
/// small or extra-small button's own does.
pub trait IconLabel {
    fn icon_label(self, icon: impl Into<Lead>, label: impl Into<SharedString>) -> Self;
}

impl IconLabel for Button {
    fn icon_label(self, icon: impl Into<Lead>, label: impl Into<SharedString>) -> Self {
        let label = label.into();
        self.accessibility_label(label.clone())
            .child(OnLetters {
                text: label.clone(),
                child: text_centered(icon, label.clone()).into_any_element(),
            })
            .child(button_text(label))
    }
}

/// How tall a control is, by the place it sits in (see the module docs).
/// Controls in one place take one size, whatever their kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlSize {
    /// Icon buttons in a table row or a dense panel: 20px.
    Mini,
    /// Inside a line of content: 24px.
    Inline,
    /// Page header actions, toolbars, card headers, setting rows, dialog
    /// footers: 28px.
    Regular,
    /// A dialog's form fields and what sits among them: 32px.
    Field,
}

impl ControlSize {
    pub const fn height(self) -> Pixels {
        px(match self {
            ControlSize::Mini => 20.,
            ControlSize::Inline => 24.,
            ControlSize::Regular => 28.,
            ControlSize::Field => 32.,
        })
    }

    /// A labelled button's side padding.
    fn padding_x(self) -> Pixels {
        px(match self {
            ControlSize::Mini => 4.,
            ControlSize::Inline => 8.,
            ControlSize::Regular => 10.,
            ControlSize::Field => 12.,
        })
    }

    /// The component size that sets a labelled control's text (and its
    /// icon, caret and gaps): 12px text at `Mini`, 14px above it.
    /// gpui-component's medium jumps to 16px for buttons, louder than
    /// every label and field around them.
    fn text_size(self) -> Size {
        match self {
            ControlSize::Mini => Size::XSmall,
            _ => Size::Small,
        }
    }

    /// The component size that sets an icon-only button's icon: 16px from
    /// `Regular` up, where the 14px of a label's icon looks lost in the
    /// square.
    fn icon_size(self) -> Size {
        match self {
            ControlSize::Mini => Size::XSmall,
            ControlSize::Inline => Size::Small,
            ControlSize::Regular | ControlSize::Field => Size::Medium,
        }
    }
}

/// [`ControlSize`] for buttons.
pub trait Control {
    /// A labelled button (text, icon and text, or a dropdown with its
    /// caret) `size` tall.
    fn control(self, size: ControlSize) -> Self;
    /// An icon-only button: a square `size` on a side.
    fn icon_control(self, size: ControlSize) -> Self;
}

impl Control for Button {
    fn control(self, size: ControlSize) -> Self {
        self.with_size(size.text_size())
            .h(size.height())
            .px(size.padding_x())
    }

    fn icon_control(self, size: ControlSize) -> Self {
        self.with_size(size.icon_size()).size(size.height())
    }
}

/// A form page's column (Settings, Tools): the panel's full width up to
/// [`FORM_MAX_WIDTH`], centred in it past that, inside the page's scrolled
/// area.
pub fn form_column(content: impl IntoElement) -> Div {
    div()
        .w_full()
        .flex()
        .flex_row()
        .justify_center()
        .child(div().w_full().max_w(px(FORM_MAX_WIDTH)).child(content))
}

/// Title line of a [`page_header`]: its height and the title's size, at
/// the top of the page and once the header has collapsed (see
/// [`scroll_page`]) — as tall as the header's `Regular` actions.
const PAGE_TITLE_HEIGHT: f32 = 32.;
const PAGE_TITLE_SIZE: f32 = 26.;
const COLLAPSED_TITLE_HEIGHT: f32 = 28.;
const COLLAPSED_TITLE_SIZE: f32 = 17.;
/// Context line of a [`page_header`], and the gap above it.
const PAGE_CONTEXT_HEIGHT: f32 = 20.;
const PAGE_CONTEXT_GAP: f32 = 4.;
/// Room under a [`page_header`] before the body, open and collapsed.
const PAGE_HEADER_SPACE: f32 = 20.;
const COLLAPSED_HEADER_SPACE: f32 = 12.;
/// A [`page_header`]'s height, open and collapsed: how far a
/// [`scroll_page`]'s body scrolls while the header collapses.
const PAGE_HEADER_HEIGHT: f32 =
    PAGE_TITLE_HEIGHT + PAGE_CONTEXT_GAP + PAGE_CONTEXT_HEIGHT + PAGE_HEADER_SPACE;
const COLLAPSED_HEADER_HEIGHT: f32 = COLLAPSED_TITLE_HEIGHT + COLLAPSED_HEADER_SPACE;
/// How far a collapsed [`scroll_page`] header rises into the panel's top
/// padding: its title then sits as far below the panel's edge as above the
/// line under it, in a bar as tall as a window's toolbar.
const COLLAPSED_HEADER_LIFT: f32 = PANEL_PADDING_Y - COLLAPSED_HEADER_SPACE;
/// How far the body scrolls under a collapsed [`scroll_page`] header while
/// the line under the header fades in.
const HEADER_DIVIDER_FADE: f32 = 8.;

/// Every page's head, the same on each: the page's title, one line of
/// context under it — live figures when the page has them, else what the
/// page is for — and the page's own actions on the right. A fixed height,
/// so the body starts at the same place on every page. On a
/// [`scroll_page`] it collapses as the body scrolls.
pub fn page_header(theme: &Theme, page: ActivePage) -> PageHeader {
    PageHeader {
        title: page.label(),
        context: page.hint().into_any_element(),
        muted: theme.muted_foreground,
        background: theme.background,
        divider: theme.border,
        actions: Vec::new(),
        collapsed: 0.,
    }
}

/// See [`page_header`].
#[derive(IntoElement)]
pub struct PageHeader {
    title: &'static str,
    context: AnyElement,
    muted: Hsla,
    /// The panel's colour, so a [`scroll_page`]'s body passes under it.
    background: Hsla,
    /// The line under it while a [`scroll_page`]'s body is under it.
    divider: Hsla,
    actions: Vec<AnyElement>,
    /// How far it has collapsed: 0 open, 1 down to its title line.
    collapsed: f32,
}

impl PageHeader {
    /// Live figures in place of the page's hint. One line: what doesn't fit
    /// is clipped.
    pub fn context(mut self, context: impl IntoElement) -> Self {
        self.context = context.into_any_element();
        self
    }

    /// An action on the header's right, after those added before it:
    /// [`ControlSize::Regular`] buttons, as every header's are.
    pub fn action(mut self, action: impl IntoElement) -> Self {
        self.actions.push(action.into_any_element());
        self
    }
}

impl RenderOnce for PageHeader {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let collapsed = self.collapsed;
        let between = |open: f32, closed: f32| px(open + (closed - open) * collapsed);
        let title_height = between(PAGE_TITLE_HEIGHT, COLLAPSED_TITLE_HEIGHT);
        let text = div()
            .v_flex()
            .flex_1()
            .min_w_0()
            .gap(between(PAGE_CONTEXT_GAP, 0.))
            .child(
                div()
                    .h(title_height)
                    .text_size(between(PAGE_TITLE_SIZE, COLLAPSED_TITLE_SIZE))
                    .line_height(title_height)
                    .font_weight(FontWeight::BOLD)
                    .truncate()
                    .child(self.title),
            )
            .when(collapsed < 1., |text| {
                text.child(
                    div()
                        .h(between(PAGE_CONTEXT_HEIGHT, 0.))
                        // Gone by halfway, before the title closes up on it.
                        .opacity((1. - 2. * collapsed).max(0.))
                        .h_flex()
                        .items_center()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_sm()
                        .text_color(self.muted)
                        .child(self.context),
                )
            });
        div()
            .flex_none()
            .w_full()
            .h_flex()
            .items_center()
            .gap_4()
            .pb(between(PAGE_HEADER_SPACE, COLLAPSED_HEADER_SPACE))
            .bg(self.background)
            .child(text)
            .when(!self.actions.is_empty(), |header| {
                header.child(
                    div()
                        .flex_none()
                        .h_flex()
                        .items_center()
                        .gap_2()
                        .children(self.actions),
                )
            })
    }
}

/// A page's root: its [`page_header`] over its body, which takes the
/// height left.
pub fn page_layout(header: PageHeader, body: impl IntoElement) -> Div {
    div()
        .v_flex()
        .size_full()
        .child(header)
        .child(div().flex_1().min_h_0().w_full().child(body))
}

/// A page whose body scrolls as one column (Home, Profiles, Settings, …):
/// [`page_layout`], except the body scrolls up under the header, and the
/// header collapses as it does — the context line fades out and the title
/// shrinks to a line as tall as the header's actions — then stays at the
/// top, small, while the body goes on scrolling under it. Scrolling back to
/// the top opens it again. While the body is under it, a hairline across
/// the whole panel marks the header's edge, as a window's toolbar has.
///
/// The header follows the scroll position rather than a timer, as large
/// titles on macOS and iOS do: the body's top edge pushes the header's
/// bottom edge up, so the two move together at any scroll speed and in
/// either direction, and there is no state to keep.
pub fn scroll_page(header: PageHeader, body: impl IntoElement) -> ScrollPage {
    ScrollPage {
        header,
        body: body.into_any_element(),
    }
}

/// See [`scroll_page`].
#[derive(IntoElement)]
pub struct ScrollPage {
    header: PageHeader,
    body: AnyElement,
}

impl RenderOnce for ScrollPage {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        // Kept while the page shows, as `overflow_y_scrollbar`'s is. A
        // scroll (wheel or scrollbar) re-renders the page, so the header
        // collapses in step.
        let scroll = window
            .use_keyed_state(ElementId::from(self.header.title), cx, |_, _| {
                ScrollHandle::new()
            })
            .read(cx)
            .clone();
        // The header's bottom edge rises by what it loses and what it lifts.
        let travel = PAGE_HEADER_HEIGHT - COLLAPSED_HEADER_HEIGHT + COLLAPSED_HEADER_LIFT;
        let scrolled = settled_scroll(&scroll);
        let collapsed = (scrolled / travel).min(1.);
        let header_bottom = px(PAGE_HEADER_HEIGHT - travel * collapsed);
        // Nothing is under the header until it has collapsed: the body's
        // top edge holds its bottom edge up till then.
        let under = ((scrolled - travel) / HEADER_DIVIDER_FADE).clamp(0., 1.);
        let divider = self.header.divider.opacity(under);
        let scrollbar = page_scrollbar(
            "page-scrollbar",
            &BelowHeader {
                scroll: scroll.clone(),
                header: header_bottom,
            },
        )
        .top(header_bottom);
        // Layout clamps the offset after this render has read it (to a body
        // that got shorter, say): if it settles elsewhere, draw again with
        // it, or the header would stay collapsed over a body at the top.
        let handle = scroll.clone();
        let settle = canvas(
            move |_, window, _| {
                if (settled_scroll(&handle) - scrolled).abs() > 0.5 {
                    let view = window.current_view();
                    window.on_next_frame(move |_, cx| cx.notify(view));
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_0();
        div()
            .relative()
            .size_full()
            .child(
                div()
                    .id("page-scroll")
                    .size_full()
                    .flex()
                    .flex_col()
                    .overflow_y_scroll()
                    .restrict_scroll_to_axis()
                    .track_scroll(&scroll)
                    // The open header's room: the body starts under it.
                    .child(div().flex_none().h(px(PAGE_HEADER_HEIGHT)))
                    .child(div().flex_none().w_full().child(self.body)),
            )
            .child(
                div()
                    .absolute()
                    .top(px(-COLLAPSED_HEADER_LIFT * collapsed))
                    .left_0()
                    .w_full()
                    .child(PageHeader {
                        collapsed,
                        ..self.header
                    }),
            )
            // On the header's last pixel, out to the panel's sides (the
            // panel clips it at its border).
            .when(under > 0., |page| {
                page.child(
                    div()
                        .absolute()
                        .top(header_bottom - px(1.))
                        .left(px(-PANEL_PADDING_X))
                        .right(px(-PANEL_PADDING_X))
                        .h(px(1.))
                        .bg(divider),
                )
            })
            // The scrollbar runs from the header down, not under it.
            .child(scrollbar)
            .child(settle)
    }
}

/// How far a [`scroll_page`]'s body is scrolled down, as layout will let
/// it be: a wheel moves the offset as far as it turns, and only layout
/// clamps it to the body's height — on a body that fits, a turn must not
/// collapse the header over a body that never moved.
fn settled_scroll(scroll: &ScrollHandle) -> f32 {
    f32::from((-scroll.offset().y).min(scroll.max_offset().y)).max(0.)
}

/// How far a [`page_scrollbar`] reaches past the page's right edge into
/// the panel's padding: its 16 px track ends 6 px short of the panel's
/// edge, clear of the cards and rows beside it.
const SCROLLBAR_OUTSET: f32 = PANEL_PADDING_X - 6.;

/// A page's vertical scrollbar for `scroll`: in the panel's right padding,
/// beside the page's cards and rows rather than over their right edge
/// (where it covered a card's border, a row's close button). An absolute
/// layer over its parent, reaching out into the padding; set its `top` to
/// start it below a header.
pub fn page_scrollbar<H: ScrollbarHandle + Clone>(id: impl Into<ElementId>, scroll: &H) -> Div {
    div()
        .absolute()
        .top_0()
        .bottom_0()
        .left_0()
        .right(px(-SCROLLBAR_OUTSET))
        .child(Scrollbar::vertical(scroll).id(id).viewport_from_layout())
}

/// A [`scroll_page`]'s scroll handle as its scrollbar sees it: the part
/// under the header — always as tall as the scrolled area less the
/// header — is the scrollbar's whole track, and its content as much shorter.
#[derive(Clone)]
struct BelowHeader {
    scroll: ScrollHandle,
    header: Pixels,
}

impl ScrollbarHandle for BelowHeader {
    fn viewport_bounds(&self) -> Bounds<Pixels> {
        let mut bounds = self.scroll.bounds();
        bounds.origin.y += self.header;
        bounds.size.height -= self.header;
        bounds
    }

    fn offset(&self) -> Point<Pixels> {
        self.scroll.offset()
    }

    fn set_offset(&self, offset: Point<Pixels>) {
        self.scroll.set_offset(offset);
    }

    fn content_size(&self) -> gpui::Size<Pixels> {
        let mut size = ScrollbarHandle::content_size(&self.scroll);
        size.height -= self.header;
        size
    }
}

/// Small heading above a group of cards or rows ("General", "Network").
pub fn section_heading(theme: &Theme, title: &'static str) -> Div {
    div()
        .px_1()
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme.muted_foreground)
        .child(title)
}

/// Centred empty state: the icon in a soft disc, a title and a hint, and
/// optionally an [`EmptyState::action`] under them. No frame of its own —
/// an empty page needs no box around its emptiness.
///
/// It covers its parent, which must be the body's root under the
/// [`page_header`]: centred on the whole body rather than on the room under
/// a toolbar, it sits at the same height on every page, whatever sits
/// above it. Only the icon, title and
/// hint are centred; the
/// action hangs under them, so a page with one puts its message where a
/// page without one does.
pub fn empty_state(
    theme: &Theme,
    icon: impl Into<Icon>,
    title: &'static str,
    subtitle: &'static str,
) -> EmptyState {
    EmptyState {
        icon: icon.into().size(px(24.)).text_color(theme.muted_foreground),
        disc: theme.muted,
        title,
        title_color: theme.foreground,
        subtitle,
        subtitle_color: theme.muted_foreground,
        action: None,
    }
}

/// See [`empty_state`].
#[derive(IntoElement)]
pub struct EmptyState {
    icon: Icon,
    disc: Hsla,
    title: &'static str,
    title_color: Hsla,
    subtitle: &'static str,
    subtitle_color: Hsla,
    action: Option<AnyElement>,
}

impl EmptyState {
    /// The page's way out of being empty: a button
    /// ([`empty_state_button`]).
    pub fn action(mut self, action: impl IntoElement) -> Self {
        self.action = Some(action.into_any_element());
        self
    }
}

impl RenderOnce for EmptyState {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let message = div()
            .v_flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .size(px(56.))
                    .mb_2()
                    .rounded_full()
                    .bg(self.disc)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(self.icon),
            )
            .child(
                div()
                    .text_base()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(self.title_color)
                    .child(self.title),
            )
            .child(
                div()
                    .max_w(px(360.))
                    .text_center()
                    .text_sm()
                    .text_color(self.subtitle_color)
                    .child(self.subtitle),
            );
        // Zero height, so it leaves the message centred: the action
        // overflows downward from it.
        let action = div()
            .h_0()
            .w_full()
            .flex()
            .justify_center()
            .items_start()
            .children(self.action.map(|action| div().mt_5().child(action)));
        div()
            .absolute()
            .inset_0()
            .v_flex()
            .items_center()
            .justify_center()
            // A little above the middle, where the eye puts the centre.
            .pb_8()
            .child(message)
            .child(action)
    }
}

/// sing-box's power button, Home's and (smaller) the sidebar's: outlined
/// while disconnected, a spinner in a faint accent ring while starting (a
/// click cancels the start), a filled accent disc while connected. The
/// same size in every state, so it never jumps under the pointer that just
/// clicked it. Keyed by status: a new state is a new element, so a tooltip
/// shown while the pointer stays on it (built once, from the old status)
/// goes away instead of still offering "Connect" after the click
/// connected. The caller adds the click, the tooltip and any focus.
pub fn power_button(
    id: &'static str,
    status: ConnectionStatus,
    diameter: f32,
    icon_size: f32,
    theme: &Theme,
) -> Stateful<Div> {
    // The big button's 2 px ring and lift, thinner and closer when small.
    let large = diameter >= 48.;
    let base = div()
        .id((id, status as usize))
        .flex_none()
        .size(px(diameter))
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .map(|button| {
            if status == ConnectionStatus::Connected {
                button
            } else if large {
                button.border_2()
            } else {
                button.border_1()
            }
        });
    let power = || {
        Icon::default()
            .path("icons/power.svg")
            .with_size(px(icon_size))
    };
    match status {
        ConnectionStatus::Starting => base
            // A faint accent wash: blue-50 / blue-200 on white, a dim navy
            // on the dark background.
            .bg(theme.primary.opacity(0.08))
            .border_color(theme.primary.opacity(0.35))
            .hover(|style| style.border_color(theme.primary))
            .child(Spinner::new().with_size(px(icon_size)).color(theme.primary)),
        ConnectionStatus::Connected => base
            .bg(linear_gradient(
                180.,
                // 上浅下深:顶部比 primary 亮一档(浅色下约 blue-500)。
                linear_color_stop(lighter(theme.primary, 0.08), 0.),
                linear_color_stop(theme.primary, 1.),
            ))
            // Hover: the same gradient in the theme's hover accent, like a
            // primary button.
            .hover(|style| {
                style.bg(linear_gradient(
                    180.,
                    linear_color_stop(lighter(theme.primary_hover, 0.08), 0.),
                    linear_color_stop(theme.primary_hover, 1.),
                ))
            })
            // A soft lift, not a glow: on the dark background a wide accent
            // shadow reads as a halo, so it stays faint there.
            .shadow(vec![BoxShadow {
                color: theme
                    .primary
                    .opacity(if theme.is_dark() { 0.2 } else { 0.28 }),
                offset: point(px(0.), px(if large { 3. } else { 1. })),
                blur_radius: px(if large { 10. } else { 4. }),
                spread_radius: px(0.),
                inset: false,
            }])
            .child(power().text_color(theme.primary_foreground)),
        ConnectionStatus::Disconnected => base
            .bg(theme.background)
            .border_color(theme.border)
            .map(|button| {
                if large {
                    button.shadow_sm()
                } else {
                    button.shadow_xs()
                }
            })
            .hover(|s| s.border_color(theme.primary).text_color(theme.primary))
            .text_color(theme.muted_foreground)
            .child(power()),
    }
}

/// `color` raised `amount` in lightness (HSL), for the top of a gradient.
fn lighter(color: Hsla, amount: f32) -> Hsla {
    Hsla {
        l: (color.l + amount).min(1.),
        ..color
    }
}

/// An empty state's call to action, unlabelled: a primary
/// [`ControlSize::Field`] button, a step up from a header's, with the same
/// text size. The caller adds the label, icon and handler and passes it to
/// [`EmptyState::action`].
pub fn empty_state_button(id: impl Into<ElementId>) -> Button {
    Button::new(id).primary().control(ControlSize::Field)
}

/// A labeled settings row: label (+ optional hint) on the left, the caller's
/// control appended as the right-hand child. The text column takes the
/// leftover width and wraps, so a long hint never pushes the control out of
/// its card.
pub fn setting_row(theme: &Theme, label: &'static str, description: Option<&'static str>) -> Div {
    div()
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
                .gap_0p5()
                .child(div().text_sm().text_color(theme.foreground).child(label))
                .children(description.map(|text| {
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(text)
                })),
        )
}

/// Rows stacked in one card, a hairline between neighbours (a grouped
/// list). Each row gets the card's side padding and its own vertical
/// padding; the card itself has none, so the dividers run edge to edge.
pub fn grouped_card(theme: &Theme, rows: impl IntoIterator<Item = AnyElement>) -> Div {
    let border = theme.border;
    card_frame(theme)
        .p_0()
        .gap_0()
        .children(rows.into_iter().enumerate().map(move |(ix, row)| {
            div()
                .w_full()
                .px_4()
                .py_3()
                .when(ix > 0, |this| this.border_t_1().border_color(border))
                .child(row)
                .into_any_element()
        }))
}

/// Rough width of `text` in Latin letters: a wide (CJK) character counts
/// as two.
fn text_units(text: &str) -> usize {
    text.chars()
        .map(|c| if (c as u32) < 0x1100 { 1 } else { 2 })
        .sum()
}

/// Whether `text` is long enough to risk an ellipsis in a column with
/// `room` Latin letters of space (see [`full_text_tooltip`]).
pub fn may_truncate(text: &str, room: usize) -> bool {
    text_units(text) > room
}

/// `text` cut to about `room` Latin letters with an ellipsis, for places
/// that clip instead of ellipsizing (popup menu items).
pub fn shorten(text: &str, room: usize) -> String {
    if !may_truncate(text, room) {
        return text.to_string();
    }
    let mut used = 0;
    let mut out = String::new();
    for c in text.chars() {
        used += text_units(c.encode_utf8(&mut [0; 4]));
        if used > room.saturating_sub(1) {
            break;
        }
        out.push(c);
    }
    out.truncate(out.trim_end().len());
    out.push('…');
    out
}

/// `label`, a line that ellipsizes when its column runs short (a node,
/// profile or host name), with the whole text in a tooltip — only when the
/// text is long enough to be at risk of the ellipsis (more than `room`
/// Latin letters), so a short name never repeats itself on hover.
pub fn full_text_tooltip(
    label: Div,
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    room: usize,
) -> Stateful<Div> {
    let text: SharedString = text.into();
    let long = may_truncate(&text, room);
    label.id(id).child(text.clone()).when(long, |label| {
        label.tooltip(move |window, cx| Tooltip::new(text.clone()).build(window, cx))
    })
}

/// Background of a clickable row under the pointer (a profile, a
/// connection): a wash of the text colour, a little stronger on the dark
/// theme, where the panel's muted tone barely shows.
pub fn row_hover_bg(theme: &Theme) -> Hsla {
    theme
        .foreground
        .opacity(if theme.is_dark() { 0.06 } else { 0.035 })
}

/// Secondary facts in one line, set apart by space rather than separators:
/// `12.0 GB    from laptop    5 min ago`. Each item truncates on its own
/// when the line runs short.
pub fn meta_row<T: Into<SharedString>>(theme: &Theme, items: impl IntoIterator<Item = T>) -> Div {
    div()
        .h_flex()
        .items_center()
        .gap_3()
        .min_w_0()
        .text_xs()
        .text_color(theme.muted_foreground)
        .children(items.into_iter().map(|item| {
            div()
                .min_w_0()
                .flex_shrink(1.)
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .child(item.into())
        }))
}

/// A state in words with a small dot of its colour in front ("Active",
/// "Closed", "Running") — the quiet replacement for a filled badge.
pub fn status_label(color: Hsla, text: impl Into<SharedString>) -> Div {
    let text = text.into();
    let dot = div()
        .size(px(6.))
        .rounded_full()
        .bg(color)
        .into_any_element();
    div()
        .h_flex()
        .flex_shrink_0()
        .items_center()
        .gap_1p5()
        .text_xs()
        .text_color(color)
        .child(text_centered(Lead::Sized(dot, px(6.)), text.clone()))
        .child(text)
}

/// Warnings (and UDP beside TCP): an orange that holds up beside the
/// accent blue in both themes — the theme's own warning is a yellow.
pub fn warn_orange(theme: &Theme) -> Hsla {
    if theme.is_dark() {
        gpui::rgb(0xFB923C).into() // orange-400
    } else {
        gpui::rgb(0xEA580C).into() // orange-600
    }
}

/// A short upper-case tag in a tinted box, in a table's cell: a log
/// level (`ERROR`), a network (`TCP`).
pub fn tag_badge(theme: &Theme, text: impl Into<SharedString>, color: Hsla) -> Div {
    div()
        .flex_none()
        .px_1p5()
        .rounded(px(4.))
        .bg(color.opacity(0.14))
        .text_color(color)
        .text_xs()
        .font_family(theme.mono_font_family.clone())
        .font_weight(FontWeight::SEMIBOLD)
        .child(text.into())
}

/// One choice of a [`segmented`] control.
pub struct Segment {
    pub label: SharedString,
    pub tooltip: Option<&'static str>,
    /// A count shown after the label in muted text ("Active  12"); `None`
    /// (or zero, see [`Segment::count`]) shows the label alone.
    pub count: Option<usize>,
    /// Greyed out and closed to clicks (a choice this system can't have,
    /// or one a running test holds).
    pub disabled: bool,
}

impl Segment {
    pub fn new(label: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
            tooltip: None,
            count: None,
            disabled: false,
        }
    }

    pub fn tooltip(mut self, tooltip: &'static str) -> Self {
        self.tooltip = Some(tooltip);
        self
    }

    /// How many items the choice holds. Zero shows nothing: an empty
    /// choice needs no number.
    pub fn count(mut self, count: usize) -> Self {
        self.count = (count > 0).then_some(count);
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

/// The app's one segmented control (a mode, a view, a sort order, a log
/// level): a muted track, the selected choice raised on it. `size` tall,
/// as every control beside it ([`ControlSize`]): the choices sit 2px in
/// from the track's edge, their text the size of a button's beside them.
pub fn segmented(
    theme: &Theme,
    id: impl Into<ElementId>,
    size: ControlSize,
    segments: Vec<Segment>,
    selected: Option<usize>,
    on_select: impl Fn(usize, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let on_select = Rc::new(on_select);
    let (fg, muted_fg, raised) = (theme.foreground, theme.muted_foreground, theme.background);
    let inset = px(2.);
    div()
        .id(id)
        .h_flex()
        .flex_shrink_0()
        .items_center()
        .h(size.height())
        .gap(inset)
        .p(inset)
        .rounded(theme.radius)
        .bg(theme.tab_bar_segmented)
        .map(|track| match size.text_size() {
            Size::XSmall => track.text_xs(),
            _ => track.text_sm(),
        })
        .children(segments.into_iter().enumerate().map(|(ix, segment)| {
            let active = selected == Some(ix);
            let on_select = on_select.clone();
            let label = segment.label.clone();
            let text = div()
                .h_flex()
                .items_center()
                .gap_1p5()
                .whitespace_nowrap()
                .child(segment.label)
                .children(
                    segment
                        .count
                        .map(|count| div().text_color(muted_fg).child(count.to_string())),
                );
            div()
                .id(ix)
                .h_full()
                .px(size.padding_x())
                .flex()
                .items_center()
                .rounded(theme.radius - inset)
                .map(|this| {
                    if active {
                        this.bg(raised).text_color(fg).shadow_xs()
                    } else if segment.disabled {
                        this.text_color(muted_fg.opacity(0.5))
                    } else {
                        this.text_color(muted_fg)
                            .cursor_pointer()
                            .hover(move |s| s.text_color(fg))
                    }
                })
                .when_some(segment.tooltip, |this, tip| {
                    this.tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
                })
                .when(!segment.disabled, |this| {
                    this.on_click(move |_, window, cx| on_select(ix, window, cx))
                })
                .child(OnLetters {
                    text: label,
                    child: text.into_any_element(),
                })
        }))
}

/// Widest a dropdown grows to fit its text; a longer one ellipsizes.
pub const SELECT_MAX_WIDTH: f32 = 280.;

/// A toolbar's search field: room for a host name or a few words, not
/// the whole row. It gives way to a narrow window, down to [`SEARCH_MIN_WIDTH`].
const SEARCH_WIDTH: f32 = 300.;
const SEARCH_MIN_WIDTH: f32 = 160.;

/// The search field at the head of a page's toolbar (Groups, Connections,
/// Logs), at [`SEARCH_WIDTH`]; the toolbar's switches follow it, pushed to
/// the right under the header's actions.
pub fn toolbar_search(field: impl IntoElement) -> Div {
    div()
        .flex_shrink(1.)
        .min_w(px(SEARCH_MIN_WIDTH))
        .w(px(SEARCH_WIDTH))
        .child(field)
}

/// What a boxed dropdown's trigger adds around its text at `size`:
/// border, padding, the gap and the caret (and a pixel each way for
/// rounding).
fn select_trigger_chrome(size: Size) -> Pixels {
    match size {
        Size::Small | Size::XSmall => px(2. + 16. + 4. + 14. + 2.),
        _ => px(2. + 20. + 4. + 16. + 2.),
    }
}

/// What a [`plain_select`]'s trigger adds to its text: the (transparent)
/// border, the gap and the small caret, and a pixel each way for rounding.
const PLAIN_SELECT_CHROME: f32 = 2. + 4. + 14. + 2.;

/// What the open menu adds around a choice: border, list padding, item
/// padding, the gap and the check mark.
const SELECT_MENU_CHROME: f32 = 2. + 8. + 16. + 4. + 12. + 6.;

/// The width of the longest of `labels` in the window's UI font at a
/// dropdown's text size (`text_sm` for small and medium).
fn widest_text<'a>(labels: impl IntoIterator<Item = &'a SharedString>, window: &Window) -> Pixels {
    let style = window.text_style();
    let font_size = rems(0.875).to_pixels(window.rem_size());
    let text_system = window.text_system();
    labels
        .into_iter()
        .filter(|label| !label.is_empty())
        .map(|label| {
            text_system
                .shape_line(label.clone(), font_size, &[style.to_run(label.len())], None)
                .width
        })
        .fold(Pixels::ZERO, Pixels::max)
}

/// The open menu's width: the longest of `labels` with room for the check
/// mark, at least `trigger` wide.
fn select_menu_width<'a>(
    labels: impl IntoIterator<Item = &'a SharedString>,
    trigger: Pixels,
    window: &Window,
) -> Pixels {
    (widest_text(labels, window) + px(SELECT_MENU_CHROME))
        .ceil()
        .min(px(SELECT_MAX_WIDTH))
        .max(trigger)
}

/// Widths for a boxed dropdown (a form field): the box as wide as the
/// longest of `labels`, up to [`SELECT_MAX_WIDTH`], so it doesn't change
/// width as the choice does; its menu at least as wide.
pub fn select_widths<'a>(
    labels: impl IntoIterator<Item = &'a SharedString> + Clone,
    size: Size,
    window: &Window,
) -> (Pixels, Pixels) {
    let trigger = (widest_text(labels.clone(), window) + select_trigger_chrome(size))
        .ceil()
        .min(px(SELECT_MAX_WIDTH));
    (trigger, select_menu_width(labels, trigger, window))
}

/// A dropdown in a setting row: no box, just the current choice and a
/// caret after it, as wide as that text (up to [`SELECT_MAX_WIDTH`]); the
/// menu below fits every choice in `labels`. Greyed out while `disabled`
/// (the component only greys its box, which this has none of).
pub fn plain_select<'a, D>(
    select: Select<D>,
    current: &SharedString,
    labels: impl IntoIterator<Item = &'a SharedString>,
    disabled: bool,
    window: &Window,
) -> Div
where
    D: SearchableListDelegate + 'static,
    <D::Item as SearchableListItem>::Value: PartialEq + Clone,
{
    let width = (widest_text([current], window) + px(PLAIN_SELECT_CHROME))
        .ceil()
        .min(px(SELECT_MAX_WIDTH));
    let menu_width = select_menu_width(labels, width, window);
    div()
        .flex_none()
        .w(width)
        .when(disabled, |this| this.opacity(0.5))
        .child(
            select
                .small()
                .appearance(false)
                .px_0()
                .menu_width(menu_width)
                .disabled(disabled),
        )
}

/// One option of a [`choice_select`]: what it shows, what it stands for.
#[derive(Clone)]
pub struct Choice<V> {
    label: SharedString,
    value: V,
}

impl<V: Clone + PartialEq> SearchableListItem for Choice<V> {
    type Value = V;

    fn title(&self) -> SharedString {
        self.label.clone()
    }

    fn value(&self) -> &V {
        &self.value
    }
}

/// A small dropdown for a setting row with three or more choices
/// (Language, Appearance): the current choice and a caret
/// ([`plain_select`]), the rest in a menu below with a check on the
/// current one. Mouse or keyboard (Tab to it, Up/Down/Enter, Esc);
/// `on_select` runs as soon as a choice is picked, re-picking the current
/// one included.
///
/// The dropdown's state lives in the window under `id` for as long as it is
/// drawn every frame, and is rebuilt when its page comes back. Each render
/// brings its labels (the UI language may have changed) and its selection
/// (`selected` may have been changed elsewhere) up to date.
pub fn choice_select<V: Copy + PartialEq + 'static, L: Into<SharedString>>(
    id: &'static str,
    choices: impl IntoIterator<Item = (V, L)>,
    selected: V,
    on_select: impl Fn(V, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    choice_select_with(id, choices, selected, false, on_select, window, cx)
}

/// [`choice_select`], greyed out and closed to input while `disabled`
/// (a test option while the test runs).
pub fn choice_select_with<V: Copy + PartialEq + 'static, L: Into<SharedString>>(
    id: &'static str,
    choices: impl IntoIterator<Item = (V, L)>,
    selected: V,
    disabled: bool,
    on_select: impl Fn(V, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let choices: Vec<Choice<V>> = choices
        .into_iter()
        .map(|(value, label)| Choice {
            label: label.into(),
            value,
        })
        .collect();
    let selected_ix = choices
        .iter()
        .position(|choice| choice.value == selected)
        .map(|ix| IndexPath::default().row(ix));

    let labels: Vec<SharedString> = choices.iter().map(|choice| choice.label.clone()).collect();
    let current = choices
        .iter()
        .find(|choice| choice.value == selected)
        .map(|choice| choice.label.clone())
        .unwrap_or_default();

    let mut created = false;
    let state = window.use_keyed_state(id, cx, |window, cx| {
        created = true;
        SelectState::new(choices.clone(), selected_ix, window, cx)
    });
    if created {
        window
            .subscribe(&state, cx, move |_, event: &SelectEvent<_>, window, cx| {
                let SelectEvent::Confirm(Some(value)) = event else {
                    return;
                };
                on_select(*value, window, cx);
            })
            .detach();
    } else {
        state.update(cx, |state, cx| {
            state.set_items(choices, window, cx);
            if state.selected_value() != Some(&selected) {
                state.set_selected_value(&selected, window, cx);
            }
        });
    }

    plain_select(Select::new(&state), &current, &labels, disabled, window).into_any_element()
}

/// A short stat: caption over a larger value ("Memory / 48.2 MB").
pub fn stat(theme: &Theme, label: &'static str, value: impl Into<SharedString>) -> Div {
    div()
        .v_flex()
        .flex_1()
        .min_w_0()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .whitespace_nowrap()
                .child(label),
        )
        .child(
            div()
                .text_lg()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.foreground)
                .whitespace_nowrap()
                .child(value.into()),
        )
}

/// `text` with its first letter capitalized, for a phrase made to sit
/// inside a sentence ("expires in 3 days") shown on its own. No-op for
/// scripts without case.
pub fn capitalize_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Notify `cx`'s view once a minute for as long as the returned task is
/// held — for labels that move with the clock rather than with any entity
/// ("updated 5 min ago", "expires in 3 days"), which a cached view would
/// otherwise leave stale.
pub fn minute_ticker<T: 'static>(cx: &mut Context<T>) -> Task<()> {
    cx.spawn(async move |this, cx| loop {
        cx.background_executor()
            .timer(Duration::from_secs(60))
            .await;
        if this.update(cx, |_, cx| cx.notify()).is_err() {
            break;
        }
    })
}

/// The colour a subscription's usage reads in: accent while fine, the
/// theme's warning / danger colours as it runs out.
pub fn usage_color(theme: &Theme, level: UsageLevel) -> Hsla {
    match level {
        UsageLevel::Normal => theme.primary,
        UsageLevel::Warning => theme.warning,
        UsageLevel::Critical => theme.danger,
    }
}

/// Subscription usage: "44.7 GB / 100.0 GB" with the expiry ("Expires in
/// 22 days") at the other end of the line, over a thin bar of the
/// allowance used (only with a known allowance). Coloured by level, with a
/// level icon when it needs attention; fills its container's width. Hover
/// shows the expiry date and how old the reading is.
pub fn usage_meter(
    theme: &Theme,
    id: impl Into<ElementId>,
    usage: &SubscriptionUsage,
    now: SystemTime,
) -> Stateful<Div> {
    let id = id.into();
    let now_secs = to_unix_secs(now).unwrap_or(0);
    let level = usage.level(now_secs);
    let color = usage_color(theme, level);
    // Colour alone is easy to miss (and yellow text is faint): a level icon
    // says it too.
    let (label_color, icon) = match level {
        UsageLevel::Normal => (theme.muted_foreground, None),
        UsageLevel::Warning => (color, Some(IconName::TriangleAlert)),
        UsageLevel::Critical => (color, Some(IconName::CircleX)),
    };
    let t = s();
    let mut tooltip = Vec::new();
    if let Some(expire) = usage.expire {
        tooltip.push((t.usage.expires_on)(&expiry_date_utc(expire)));
    }
    tooltip.push((t.usage.as_of)(&format_relative_time(
        from_unix_secs(usage.fetched_at),
        now,
    )));
    let tooltip: SharedString = tooltip.join("\n").into();
    let traffic: SharedString = usage.traffic_label().into();
    let expiry = usage
        .expiry_label(now_secs)
        .map(|label| capitalize_first(&label));

    div()
        .id(id.clone())
        .v_flex()
        .gap_1p5()
        .w_full()
        .min_w_0()
        .child(
            div()
                .h_flex()
                .items_center()
                .gap_3()
                .w_full()
                .text_xs()
                .child(
                    div()
                        .h_flex()
                        .items_center()
                        .gap_1()
                        .flex_1()
                        .min_w_0()
                        .text_color(label_color)
                        .children(icon.map(|icon| {
                            text_centered(Icon::new(icon).text_color(color), traffic.clone())
                        }))
                        .child(div().min_w_0().truncate().child(traffic)),
                )
                .children(expiry.map(|expiry| {
                    div()
                        .flex_shrink_0()
                        .whitespace_nowrap()
                        .text_color(label_color)
                        .child(expiry)
                })),
        )
        .children(usage.fraction_used().map(|fraction| {
            Progress::new(id)
                .small()
                .w_full()
                .color(color)
                .value(fraction * 100.)
        }))
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
}

/// Height of a line that holds [`ControlSize::Inline`] buttons beside text
/// (a profile's name with its update and edit buttons): the buttons'
/// height, so the text and the buttons share one centre line.
pub const CONTROL_LINE_HEIGHT: Pixels = ControlSize::Inline.height();

/// A profile's update button: how fresh it is and the action that
/// refreshes it, one quiet control ("⟳ 25 min ago"), the same on Home and
/// on the Profiles page. A ghost button in muted text, so it sits in a row
/// like a status line until hovered; the tooltip says when exactly, how it
/// stays fresh and what a click does (plus `shortcut`'s keys, if given).
/// Updating, it spins; a failure swaps the icon for a
/// warning; a stale time turns the warning colour, the icon unchanged.
pub fn freshness_button(
    theme: &Theme,
    id: impl Into<ElementId>,
    freshness: Freshness,
    shortcut: Option<&dyn Action>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    let refresh = Icon::default().path("icons/refresh-cw.svg");
    let label = capitalize_first(&freshness.label);
    let button = Button::new(id)
        .ghost()
        .control(ControlSize::Inline)
        .text_color(theme.muted_foreground);
    let button = match freshness.state {
        // Not `loading` / `disabled`: either stops the button tracking the
        // pointer, and it would come back from the update still painted as
        // hovered by the click that started it. A click meanwhile is a
        // no-op (`AppState::update_profile` ignores a profile in flight).
        FreshnessState::Updating => button.icon_label(Spinner::new(), label),
        FreshnessState::Failed => button.icon_label(
            Icon::new(IconName::TriangleAlert).text_color(theme.warning),
            label,
        ),
        FreshnessState::Fresh { stale: true } => button
            .accessibility_label(label.clone())
            .child(text_centered(refresh, label.clone()))
            .child(div().text_color(theme.warning).child(label)),
        FreshnessState::Never | FreshnessState::Fresh { stale: false } => {
            button.icon_label(refresh, label)
        }
    };
    let tooltip = freshness.tooltip;
    let button = match (tooltip.is_empty(), shortcut) {
        (true, _) => button,
        (false, Some(action)) => button.tooltip_with_action(tooltip, action, Some(KEY_CONTEXT)),
        (false, None) => button.tooltip(tooltip),
    };
    button.on_click(on_click)
}

/// A profile's source in one quiet line, like [`meta_row`]: where it comes
/// from (the subscription's host or the file's name; hover for the whole
/// URL or path) and, set apart by space, "Auto-update off" or "Local file"
/// when it applies.
pub fn profile_source_line(theme: &Theme, id: impl Into<ElementId>, info: ProfileRowInfo) -> Div {
    let item = || {
        div()
            .min_w_0()
            .flex_shrink(1.)
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
    };
    let source = item().id(id).child(info.source);
    let source = match info.source_full.map(SharedString::from) {
        Some(full) => source
            .tooltip(move |window, cx| Tooltip::new(full.clone()).build(window, cx))
            .into_any_element(),
        None => source.into_any_element(),
    };
    div()
        .h_flex()
        .items_center()
        .gap_3()
        .min_w_0()
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(source)
        .children(info.note.map(|note| item().flex_shrink_0().child(note)))
}

/// The line a single-line input gives its text: gpui-component's 1.25rem.
const INPUT_LINE_HEIGHT: f32 = 20.;

/// Vertical padding that fits [`INPUT_LINE_HEIGHT`] inside an input
/// `height` tall with a 1px border (as much as fits, in a short one).
fn input_py(height: Pixels) -> Pixels {
    ((height - px(2. + INPUT_LINE_HEIGHT)) / 2.).max(Pixels::ZERO)
}

/// A single-line input `size` tall ([`ControlSize`]): `Field` in a
/// dialog's form ([`form_input`]), `Regular` in a toolbar or a setting
/// row. Its text isn't clipped: gpui-component pads a medium input 8px top
/// and bottom inside its 32px, leaving 14px for a 20px line, and clips the
/// text to that box, so Chinese characters lose their bottom edge (Latin
/// letters mostly fit). This pads it to fit the line.
pub fn control_input(state: &Entity<InputState>, size: ControlSize) -> Input {
    let component_size = match size {
        ControlSize::Field => Size::Medium,
        _ => Size::Small,
    };
    // `Styled::h`: `Input::h` sets only a multi-line input's height.
    Styled::h(Input::new(state).with_size(component_size), size.height())
        .py(input_py(size.height()))
}

/// A field in a dialog's form: [`control_input`] at [`ControlSize::Field`].
pub fn form_input(state: &Entity<InputState>) -> Input {
    control_input(state, ControlSize::Field)
}

/// The narrowest a dialog's button gets, so short labels side by side
/// ("Cancel" / "Save", "取消" / "保存") come out one width, as the system's
/// own dialogs do. Longer labels widen their button as usual.
const DIALOG_BUTTON_MIN_WIDTH: Pixels = px(72.);

/// A button in a dialog's footer, unlabelled: [`ControlSize::Regular`], a
/// step under the fields above it so the footer's actions don't outweigh
/// them, and at least [`DIALOG_BUTTON_MIN_WIDTH`] wide. A button beside a
/// field takes [`form_button`] instead.
pub fn dialog_button(button: Button) -> Button {
    button
        .control(ControlSize::Regular)
        .min_w(DIALOG_BUTTON_MIN_WIDTH)
}

/// [`dialog_button`] at [`ControlSize::Field`], for a button in a row with
/// a field ("Browse…"), so their edges line up.
pub fn form_button(button: Button) -> Button {
    button
        .control(ControlSize::Field)
        .min_w(DIALOG_BUTTON_MIN_WIDTH)
}

/// Room a dialog's content leaves below its last control.
/// gpui-component's dialog clips its body to the content's bounds, and a
/// focused input's 3px ring is drawn outside the input: an input at the
/// bottom of the body otherwise loses the bottom of its ring.
pub const DIALOG_BODY_BOTTOM: Pixels = px(4.);

#[cfg(test)]
mod tests {
    use super::capitalize_first;

    #[test]
    fn shorten_cuts_long_text_with_an_ellipsis() {
        assert_eq!(super::shorten("Home backup", 20), "Home backup");
        assert_eq!(super::shorten("Hong Kong Premium 01", 10), "Hong Kong…");
        assert_eq!(super::shorten("香港高级节点一号", 9), "香港高级…");
    }

    #[test]
    fn text_units_count_wide_characters_twice() {
        assert_eq!(super::text_units("Hong Kong 01"), 12);
        assert_eq!(super::text_units("香港 01"), 7);
        assert_eq!(super::text_units(""), 0);
    }

    #[test]
    fn capitalize_first_touches_only_the_first_letter() {
        assert_eq!(capitalize_first("expires in 3 days"), "Expires in 3 days");
        assert_eq!(capitalize_first("12 天后到期"), "12 天后到期");
        assert_eq!(capitalize_first(""), "");
    }
}
