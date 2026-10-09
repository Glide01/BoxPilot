//! Settings › Troubleshooting › Running config: a large dialog showing the
//! config sing-box runs with (`core::config_view`) in a read-only, selectable,
//! searchable editor (set up like the Logs viewer), with "Hide credentials"
//! (on by default), Copy (what is shown) and Open folder.
//!
//! While sing-box is stopped it shows a preview built from the active
//! profile instead. The file read and JSON work run on the background
//! executor; the dialog reloads when sing-box starts or stops, or when the
//! profile or a setting the preview depends on changes.

use crate::core::config_view::{load, ConfigRequest, ConfigSource, ConfigView, ConfigViewError};
use crate::core::paths::profile_config_path;
use crate::core::privileged_helper::{
    process_is_elevated, start_route, StartRoute, HELPER_PLATFORM,
};
use crate::i18n::s;
use crate::state::AppState;
use crate::ui::widgets::{status_label, IconLabel};
use gpui::{prelude::FluentBuilder, *};
use gpui_component::{
    button::{Button, ButtonVariants},
    input::{Editor, EditorState, Search},
    switch::Switch,
    ActiveTheme, Icon, IconName, Sizable, StyledExt, WindowExt,
};
use std::time::Duration;

/// The dialog's preferred width; a smaller window shrinks it.
const DIALOG_WIDTH: f32 = 960.;
/// Gap above the dialog (instead of the default tenth of the window).
const DIALOG_TOP: f32 = 24.;
/// Height the dialog's own chrome (title, paddings, edge margins) takes
/// from the window, besides `DIALOG_TOP`; the viewer gets the rest.
const DIALOG_CHROME: f32 = 88.;
/// How long the Copy button reads "Copied".
const COPIED_FOR: Duration = Duration::from_millis(1500);
/// The viewer never gets shorter than this; a tinier window scrolls.
const MIN_VIEWER_HEIGHT: f32 = 240.;
/// Nor taller than this on a huge screen.
const MAX_VIEWER_HEIGHT: f32 = 1000.;

/// Open the Running config dialog.
pub fn open(app_state: Entity<AppState>, window: &mut Window, cx: &mut App) {
    let viewer = cx.new(|cx| ConfigViewer::new(app_state, window, cx));
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .title(s().config_viewer.title)
            .w(px(DIALOG_WIDTH))
            .margin_top(px(DIALOG_TOP))
            .child(viewer.clone())
    });
}

/// What the dialog shows.
enum Content {
    Loading,
    Loaded(ConfigView),
    Empty(ConfigViewError),
}

/// Everything a load depends on; a change means the shown config may be
/// out of date.
#[derive(Clone, PartialEq)]
struct LoadKey {
    running: bool,
    profile_id: String,
    /// Bumped when the profile's config content changes (fetch / import).
    profile_updated: Option<u64>,
    /// The settings `RuntimeOptions::new` reads, which shape the preview.
    proxy_mode: bool,
    set_system_proxy: bool,
    proxy_port: u16,
    tun_ipv6: bool,
    allow_lan: bool,
}

impl LoadKey {
    fn of(app_state: &Entity<AppState>, cx: &App) -> Self {
        let state = app_state.read(cx);
        let settings = &state.settings;
        Self {
            running: !state.process.read(cx).is_stopped(),
            profile_id: settings.active_profile_id.clone(),
            profile_updated: settings
                .active_profile()
                .and_then(|profile| profile.last_updated_secs),
            proxy_mode: settings.proxy_mode,
            set_system_proxy: settings.set_system_proxy,
            proxy_port: settings.proxy_port,
            tun_ipv6: settings.tun_ipv6,
            allow_lan: settings.allow_lan,
        }
    }
}

pub struct ConfigViewer {
    app_state: Entity<AppState>,
    /// Read-only editor holding the shown text.
    editor: Entity<EditorState>,
    hide_credentials: bool,
    content: Content,
    /// What the shown content (or the load in flight) was loaded for.
    loaded_for: Option<LoadKey>,
    /// The load in flight; dropping it cancels it.
    _load: Option<Task<()>>,
    /// The Copy button reads "Copied"; `_copied_reset` clears it.
    copied: bool,
    _copied_reset: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ConfigViewer {
    fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .line_number(true)
                .folding(false)
                .soft_wrap(false)
        });
        let process = app_state.read(cx).process.clone();
        let subscriptions = vec![
            cx.observe_in(&app_state, window, |this, _, window, cx| {
                this.reload_if_stale(window, cx)
            }),
            cx.observe_in(&process, window, |this, _, window, cx| {
                this.reload_if_stale(window, cx)
            }),
        ];
        let mut viewer = Self {
            app_state,
            editor,
            hide_credentials: true,
            content: Content::Loading,
            loaded_for: None,
            _load: None,
            copied: false,
            _copied_reset: None,
            _subscriptions: subscriptions,
        };
        viewer.reload_if_stale(window, cx);
        viewer
    }

    fn reload_if_stale(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = LoadKey::of(&self.app_state, cx);
        if self.loaded_for.as_ref() == Some(&key) {
            return;
        }
        let request = {
            let state = self.app_state.read(cx);
            ConfigRequest {
                app_dir: state.app_dir.clone(),
                running: key.running,
                profile_config: state
                    .settings
                    .active_profile()
                    .map(|profile| profile_config_path(&state.app_dir, &profile.id)),
                settings: state.settings.clone(),
                through_helper: start_route(
                    HELPER_PLATFORM,
                    state.settings.proxy_mode,
                    process_is_elevated(),
                ) == StartRoute::Helper,
            }
        };
        self.loaded_for = Some(key);
        // Whatever is shown stays up until the new load lands.
        self._load = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { load(&request) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| this.show(result, window, cx));
        }));
    }

    fn show(
        &mut self,
        result: Result<ConfigView, ConfigViewError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self._load = None;
        let text = match &result {
            Ok(view) => view.text(self.hide_credentials).to_string(),
            Err(_) => String::new(),
        };
        let unchanged = matches!(
            (&self.content, &result),
            (Content::Loaded(old), Ok(new)) if old == new
        );
        if !unchanged {
            self.editor
                .update(cx, |editor, cx| editor.set_value(text, window, cx));
        }
        self.content = match result {
            Ok(view) => Content::Loaded(view),
            Err(e) => Content::Empty(e),
        };
        cx.notify();
    }

    /// Switch between the masked and the full text, keeping the scroll
    /// position (both have the same lines).
    fn set_hide_credentials(&mut self, hide: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.hide_credentials == hide {
            return;
        }
        self.hide_credentials = hide;
        if let Content::Loaded(view) = &self.content {
            let text = view.text(hide).to_string();
            self.editor.update(cx, |editor, cx| {
                let offset = editor.scroll_offset();
                editor.set_value(text, window, cx);
                editor.set_scroll_offset(offset, cx);
            });
        }
        cx.notify();
    }

    /// Copy what is shown. Confirmed on the button itself ("Copied" for a
    /// moment): a toast would sit under the dialog.
    fn copy(&mut self, cx: &mut Context<Self>) {
        let Content::Loaded(view) = &self.content else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(
            view.text(self.hide_credentials).to_string(),
        ));
        self.copied = true;
        self._copied_reset = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COPIED_FOR).await;
            let _ = this.update(cx, |this, cx| {
                this.copied = false;
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Source status (dot + word) + hint, and the Hide credentials switch. Only with a
    /// config to show.
    fn header(&self, cx: &mut Context<Self>) -> Option<Div> {
        let Content::Loaded(view) = &self.content else {
            return None;
        };
        let theme = cx.theme();
        let t = &s().config_viewer;
        let (badge, hint) = match view.source {
            ConfigSource::Running => (status_label(theme.success, t.running), t.running_hint),
            ConfigSource::Preview => (
                status_label(theme.muted_foreground, t.preview),
                t.preview_hint,
            ),
        };
        let row = div()
            .h_flex()
            .items_center()
            .justify_between()
            .gap_4()
            .w_full()
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .flex_1()
                    .min_w_0()
                    .child(badge)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(hint),
                    ),
            )
            .child(
                div().flex_shrink_0().child(
                    Switch::new("config-hide-credentials")
                        .checked(self.hide_credentials)
                        .label(t.hide_credentials)
                        .tooltip(t.hide_credentials_tooltip)
                        .on_click(cx.listener(|this, checked: &bool, window, cx| {
                            this.set_hide_credentials(*checked, window, cx)
                        })),
                ),
            );
        Some(row)
    }

    fn body(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let t = s();
        let frame = || {
            div()
                .flex_1()
                .min_h_0()
                .w_full()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.background)
                .overflow_hidden()
        };
        let empty = |icon: IconName, title: &'static str, detail: SharedString| {
            frame()
                .v_flex()
                .items_center()
                .justify_center()
                .gap_2()
                .p_6()
                .child(Icon::new(icon).large().text_color(theme.muted_foreground))
                .child(div().text_sm().text_color(theme.foreground).child(title))
                .child(
                    div()
                        .max_w(px(480.))
                        .text_xs()
                        .text_center()
                        .text_color(theme.muted_foreground)
                        .child(detail),
                )
                .into_any_element()
        };
        match &self.content {
            Content::Loaded(_) => frame()
                .child(
                    Editor::new(&self.editor)
                        .appearance(false)
                        .readonly(true)
                        .h_full()
                        .text_sm(),
                )
                .into_any_element(),
            Content::Loading => empty(IconName::Loader, t.common.loading, SharedString::default()),
            Content::Empty(ConfigViewError::NoProfile) => empty(
                IconName::Inbox,
                t.config_viewer.no_profile_title,
                t.config_viewer.no_profile_hint.into(),
            ),
            Content::Empty(ConfigViewError::NotDownloaded) => empty(
                IconName::Inbox,
                t.config_viewer.no_config_title,
                t.config_viewer.no_config_hint.into(),
            ),
            Content::Empty(ConfigViewError::Failed(message)) => empty(
                IconName::TriangleAlert,
                t.config_viewer.load_failed_title,
                message.clone().into(),
            ),
        }
    }

    /// The file's name, and Search / Open folder / Copy. Only with a config
    /// to show.
    fn footer(&self, cx: &mut Context<Self>) -> Option<Div> {
        let Content::Loaded(view) = &self.content else {
            return None;
        };
        let file = view.file.clone();
        let file_name: SharedString = file
            .file_name()
            .map(|name| name.to_string_lossy().into_owned().into())
            .unwrap_or_default();
        let theme = cx.theme();
        let t = s();
        let row = div()
            .h_flex()
            .items_center()
            .justify_between()
            .gap_2()
            .w_full()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .font_family(theme.mono_font_family.clone())
                    .text_color(theme.muted_foreground)
                    .child(file_name),
            )
            .child(
                div()
                    .h_flex()
                    .flex_shrink_0()
                    .gap_2()
                    .child(
                        Button::new("config-search")
                            .ghost()
                            .small()
                            .icon_label(IconName::Search, t.common.search)
                            // The keys come from the editor's own binding:
                            // ⌘F on macOS, Ctrl+F elsewhere.
                            .tooltip_with_action(t.common.search, &Search, Some("Input"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.editor
                                    .update(cx, |editor, cx| editor.open_search(false, cx));
                            })),
                    )
                    .child(
                        Button::new("config-open-folder")
                            .outline()
                            .small()
                            .icon_label(IconName::FolderOpen, t.config_viewer.open_folder)
                            .tooltip(t.config_viewer.open_folder_tooltip)
                            .on_click(move |_, _, cx| cx.reveal_path(&file)),
                    )
                    .child(
                        Button::new("config-copy")
                            .primary()
                            .small()
                            .min_w(px(84.))
                            .when_else(
                                self.copied,
                                |button| button.icon_label(IconName::Check, t.common.copied),
                                |button| button.icon_label(IconName::Copy, t.common.copy),
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.copy(cx))),
                    ),
            );
        Some(row)
    }
}

impl Render for ConfigViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let height = (window.viewport_size().height - px(DIALOG_TOP + DIALOG_CHROME))
            .clamp(px(MIN_VIEWER_HEIGHT), px(MAX_VIEWER_HEIGHT));
        div()
            .v_flex()
            .gap_3()
            .w_full()
            .h(height)
            .children(self.header(cx))
            .child(self.body(cx))
            .children(self.footer(cx))
    }
}
