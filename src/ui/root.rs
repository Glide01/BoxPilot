use crate::actions::{ToggleProcess, UpdateSubscription};
use crate::core::bytefmt::format_speed;
use crate::core::presentation::ConnectionStatus;
use crate::core::settings::StatusEvent;
use crate::state::{AppState, ImportRequested};
use crate::ui::pages::{ActivePage, GroupsPage, HomePage, LogsPage, ProfilesPage, SettingsPage};
use crate::ui::sidebar::sidebar;
use crate::ui::toast::{self, Toasts};
use gpui::*;
use gpui_component::{ActiveTheme, Root, StyledExt, WindowExt};

/// Top-level view: sidebar navigation + the active page, owns the
/// keyboard-shortcut action handlers and the toast routing. All five page
/// entities stay alive across switches (so input state survives); only the
/// active one is rendered.
pub struct RootView {
    app_state: Entity<AppState>,
    active_page: ActivePage,
    home: Entity<HomePage>,
    groups: Entity<GroupsPage>,
    profiles: Entity<ProfilesPage>,
    logs: Entity<LogsPage>,
    settings: Entity<SettingsPage>,
    toasts: Entity<Toasts>,
}

impl RootView {
    pub fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let home = cx.new(|cx| HomePage::new(app_state.clone(), cx));
        let groups = cx.new(|cx| GroupsPage::new(app_state.clone(), cx));
        let profiles = cx.new(|cx| ProfilesPage::new(app_state.clone(), cx));
        let logs = cx.new(|cx| LogsPage::new(app_state.clone(), window, cx));
        let settings = cx.new(|cx| SettingsPage::new(app_state.clone(), window, cx));
        let toasts = toast::init(cx);

        // Every StatusEvent emitter routes to the same single toast slot.
        Self::route_status_toasts(&app_state, window, cx);
        let process_session = app_state.read(cx).process.clone();
        Self::route_status_toasts(&process_session, window, cx);
        let proxy_groups = app_state.read(cx).proxy_groups.clone();
        Self::route_status_toasts(&proxy_groups, window, cx);

        // Sidebar footer 的状态点跟随进程状态。
        cx.observe(&process_session, |_, _, cx| cx.notify()).detach();
        // Sidebar footer 网速行随 traffic 实体实时刷新(~1/sec)。
        let traffic = app_state.read(cx).traffic.clone();
        cx.observe(&traffic, |_, _, cx| cx.notify()).detach();

        // Deep-link imports need a user confirmation dialog. The event covers
        // links arriving while the app runs; the startup check below covers a
        // link that was processed from argv before this subscriber existed.
        cx.subscribe_in(
            &app_state,
            window,
            |_, app_state, _: &ImportRequested, window, cx| {
                Self::prompt_import(app_state.clone(), window, cx);
            },
        )
        .detach();

        if let Some((level, message)) = app_state.update(cx, |state, _| state.pending_status.take())
        {
            cx.on_next_frame(window, move |_, _, cx| {
                toast::show(level, message, cx);
            });
        }
        if app_state.read(cx).pending_import.is_some() {
            let app_state = app_state.clone();
            cx.on_next_frame(window, move |_, window, cx| {
                Self::prompt_import(app_state, window, cx);
            });
        }

        Self {
            app_state,
            active_page: ActivePage::Home,
            home,
            groups,
            profiles,
            logs,
            settings,
            toasts,
        }
    }

    /// Forward an entity's `StatusEvent`s to the shared toast slot. One
    /// subscription shape for every emitter — a new status source only needs
    /// `impl EventEmitter<StatusEvent>` plus one call here.
    fn route_status_toasts<T: EventEmitter<StatusEvent> + 'static>(
        entity: &Entity<T>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.subscribe_in(entity, window, |_, _, ev: &StatusEvent, _, cx| {
            toast::show(ev.level, ev.message.clone(), cx);
        })
        .detach();
    }

    fn on_update_sub(&mut self, _: &UpdateSubscription, _: &mut Window, cx: &mut Context<Self>) {
        self.app_state
            .update(cx, |state, cx| state.update_subscription(cx));
    }

    fn on_toggle_process(&mut self, _: &ToggleProcess, _: &mut Window, cx: &mut Context<Self>) {
        self.app_state
            .update(cx, |state, cx| state.toggle_process(cx));
    }

    /// Take the parked deep-link import and confirm it with the user —
    /// links come from arbitrary web pages, never import silently.
    fn prompt_import(app_state: Entity<AppState>, window: &mut Window, cx: &mut App) {
        let Some(request) = app_state.update(cx, |state, _| state.pending_import.take()) else {
            return;
        };
        window.open_alert_dialog(cx, move |alert, _, _| {
            let app_state = app_state.clone();
            let request = request.clone();
            let name = request.name.clone().unwrap_or_default();
            alert
                .title("Import subscription profile?")
                .description(
                    div()
                        .v_flex()
                        .gap_1()
                        .children(
                            (!name.is_empty()).then(|| {
                                div().font_weight(FontWeight::SEMIBOLD).child(name)
                            }),
                        )
                        .child(div().text_sm().child(request.url.clone())),
                )
                .confirm()
                .on_ok(move |_, _, cx| {
                    app_state.update(cx, |state, cx| {
                        state.import_profile(request.clone(), cx);
                    });
                    true
                })
        });
    }
}

impl Render for RootView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let process = self.app_state.read(cx).process.clone();
        let process = process.read(cx);
        let is_running = process.is_running();
        let theme = cx.theme();
        let status = ConnectionStatus::from_flags(process.is_starting(), is_running);
        let dot_color = match status {
            ConnectionStatus::Starting => theme.warning,
            ConnectionStatus::Connected => theme.success,
            ConnectionStatus::Disconnected => theme.muted_foreground,
        };
        let status_label = status.label();
        let bg = theme.muted;
        let fg = theme.foreground;
        let speed_color = theme.muted_foreground;
        // 网速行只在已连接时显示;读 traffic 实体格式化 ↓/↑ 速率。
        let speed = is_running.then(|| {
            let traffic = self.app_state.read(cx).traffic.read(cx);
            (format_speed(traffic.down), format_speed(traffic.up))
        });

        let view = cx.entity().downgrade();
        let on_nav = move |page: ActivePage, _: &mut Window, cx: &mut App| {
            view.update(cx, |this, cx| {
                if this.active_page != page {
                    this.active_page = page;
                    cx.notify();
                }
            })
            .ok();
        };

        let page: AnyView = match self.active_page {
            ActivePage::Home => self.home.clone().into(),
            ActivePage::Groups => self.groups.clone().into(),
            ActivePage::Profiles => self.profiles.clone().into(),
            ActivePage::Logs => self.logs.clone().into(),
            ActivePage::Settings => self.settings.clone().into(),
        };

        let dialog_layer = Root::render_dialog_layer(window, cx);

        div()
            .key_context("BoxPilot")
            .on_action(cx.listener(Self::on_update_sub))
            .on_action(cx.listener(Self::on_toggle_process))
            // 注意:不要用 gpui-component 的 `.h_flex()` —— 它附带
            // `items_center`,会把整列内容垂直居中而不是拉伸到全高。
            .flex()
            .flex_row()
            .size_full()
            .bg(bg)
            .text_color(fg)
            .child(sidebar(
                self.active_page,
                dot_color,
                status_label,
                speed,
                speed_color,
                on_nav,
            ))
            .child(div().flex_1().min_w_0().v_flex().p_6().child(page))
            .child(self.toasts.clone())
            .children(dialog_layer)
    }
}
