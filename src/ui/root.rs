use crate::actions::{
    FocusNext, FocusPrevious, ShowConnections, ShowGroups, ShowHome, ShowLogs, ShowProfiles,
    ShowSettings, ShowTools, ToggleProcess, UpdateSubscription, KEY_CONTEXT,
};
use crate::core::bytefmt::format_speed;
use crate::core::presentation::{redact_url, ConnectionStatus};
use crate::core::settings::StatusEvent;
use crate::i18n::s;
use crate::state::{AppState, HelperInstallRequested, ImportRequested};
#[cfg(target_os = "linux")]
use crate::state::TunGrantRequested;
use crate::ui::pages::{
    ActivePage, ConnectionsPage, GroupsPage, HomePage, LogsPage, ProfilesPage, SettingsPage,
    TailscalePage, ToolsPage, VpnPage,
};
use crate::ui::sidebar::{brand, sidebar, Badges, OptionalPages, SidebarColors, StatusDetail};
use crate::ui::theme::{PANEL_INSET, PANEL_RADIUS};
use crate::ui::title_bar;
use crate::ui::toast::{self, Toasts};
use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{ActiveTheme, StyledExt, WindowExt};

/// Top-level view: sidebar navigation + the active page, owns the
/// keyboard-shortcut action handlers and the toast routing. All page
/// entities stay alive across switches (so input state survives); only the
/// active one is rendered.
pub struct RootView {
    app_state: Entity<AppState>,
    /// Keeps keyboard focus inside the `KEY_CONTEXT` subtree. With nothing
    /// focused gpui dispatches keys from the window's root element, above
    /// this view, so the shortcuts would never fire. Focused at creation;
    /// a click on anything not focusable itself lands focus back here.
    focus_handle: FocusHandle,
    /// Last `AppState::is_starting`, so the sidebar status re-renders when a
    /// TUN gate (Linux's grant, macOS's helper; `ProcessSession` doesn't see
    /// either) opens or closes.
    starting: bool,
    /// Last `AppState::update_available().is_some()` — the Settings
    /// sidebar dot; re-rendered on its edges only, like `starting`.
    update_badge: bool,
    /// Last active profile name — the status tile's second line while
    /// disconnected; re-rendered when it changes, like `starting`.
    profile_name: Option<String>,
    active_page: ActivePage,
    home: Entity<HomePage>,
    groups: Entity<GroupsPage>,
    connections: Entity<ConnectionsPage>,
    profiles: Entity<ProfilesPage>,
    logs: Entity<LogsPage>,
    tools: Entity<ToolsPage>,
    settings: Entity<SettingsPage>,
    tailscale: Entity<TailscalePage>,
    /// Whether the sidebar currently offers the Tailscale page (the running
    /// config has Tailscale endpoints).
    tailscale_visible: bool,
    vpn: Entity<VpnPage>,
    /// Whether the sidebar currently offers the VPN page (the running config
    /// has OpenConnect / OpenVPN endpoints or USB/IP servers).
    vpn_visible: bool,
    toasts: Entity<Toasts>,
}

impl RootView {
    pub fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let home = cx.new(|cx| HomePage::new(app_state.clone(), cx));
        let groups = cx.new(|cx| GroupsPage::new(app_state.clone(), window, cx));
        let connections = cx.new(|cx| ConnectionsPage::new(app_state.clone(), window, cx));
        let profiles = cx.new(|cx| ProfilesPage::new(app_state.clone(), cx));
        let logs = cx.new(|cx| LogsPage::new(app_state.clone(), window, cx));
        let tools = cx.new(|cx| ToolsPage::new(app_state.clone(), window, cx));
        let settings = cx.new(|cx| SettingsPage::new(app_state.clone(), window, cx));
        let tailscale = cx.new(|cx| TailscalePage::new(app_state.clone(), window, cx));
        let vpn = cx.new(|cx| VpnPage::new(app_state.clone(), window, cx));
        let toasts = toast::init(cx);

        // Every StatusEvent emitter routes to the same single toast slot.
        Self::route_status_toasts(&app_state, window, cx);
        let process_session = app_state.read(cx).process.clone();
        Self::route_status_toasts(&process_session, window, cx);
        let proxy_groups = app_state.read(cx).proxy_groups.clone();
        Self::route_status_toasts(&proxy_groups, window, cx);
        let clash_mode = app_state.read(cx).clash_mode.clone();
        Self::route_status_toasts(&clash_mode, window, cx);
        let connection_list = app_state.read(cx).connections.clone();
        Self::route_status_toasts(&connection_list, window, cx);
        let tailscale_state = app_state.read(cx).tailscale.clone();
        Self::route_status_toasts(&tailscale_state, window, cx);
        // Show/hide the Tailscale sidebar item; leave the page if it goes
        // away under the user (sing-box stopped). Only visibility changes
        // re-render — status pushes are frequent.
        cx.observe_in(
            &tailscale_state,
            window,
            |this: &mut Self, state, window, cx| {
                let visible = state.read(cx).has_endpoints();
                if visible != this.tailscale_visible {
                    this.tailscale_visible = visible;
                    if !visible && this.active_page == ActivePage::Tailscale {
                        this.show_page(ActivePage::Home, window, cx);
                    }
                    cx.notify();
                }
            },
        )
        .detach();
        let vpn_status = app_state.read(cx).vpn.clone();
        Self::route_status_toasts(&vpn_status, window, cx);
        // The VPN sidebar entry comes and goes with the running config;
        // leave the page if it goes away under the user. Only visibility
        // changes re-render, like Tailscale above.
        let vpn_visible = vpn_status.read(cx).is_visible();
        cx.observe_in(&vpn_status, window, |this: &mut Self, state, window, cx| {
            let visible = state.read(cx).is_visible();
            if visible != this.vpn_visible {
                this.vpn_visible = visible;
                if !visible && this.active_page == ActivePage::Vpn {
                    this.show_page(ActivePage::Home, window, cx);
                }
                cx.notify();
            }
        })
        .detach();

        // Sidebar footer 的状态点跟随进程状态。
        cx.observe(&process_session, |_, _, cx| cx.notify()).detach();
        // …and the Linux TUN gate, which only `AppState` knows about; plus
        // the Settings update dot. Only their edges re-render: `AppState`
        // notifies often.
        cx.observe(&app_state, |this: &mut Self, state, cx| {
            let state = state.read(cx);
            let starting = state.is_starting(cx);
            let update_badge = state.update_available().is_some();
            let profile_name = state.settings.active_profile().map(|p| p.name.clone());
            if starting != this.starting
                || update_badge != this.update_badge
                || profile_name != this.profile_name
            {
                this.starting = starting;
                this.update_badge = update_badge;
                this.profile_name = profile_name;
                cx.notify();
            }
        })
        .detach();
        // Sidebar footer 网速行随 traffic 实体实时刷新(~1/sec)。
        let traffic = app_state.read(cx).traffic.clone();
        cx.observe(&traffic, |_, _, cx| cx.notify()).detach();

        // Deep-link imports need a user confirmation dialog. No startup
        // special case: `AppState` holds every launch attempt back until
        // `view_attached()` below, so this subscriber cannot miss one.
        cx.subscribe_in(
            &app_state,
            window,
            |_, app_state, _: &ImportRequested, window, cx| {
                Self::prompt_import(app_state.clone(), window, cx);
            },
        )
        .detach();

        // Surfacing the window for every launch attempt (ADR 0001) is
        // app-level now (`ui::app_window`): it must also reopen a window
        // that was closed to the tray, when no `RootView` exists.

        // A Linux TUN-mode start without CAP_NET_ADMIN stops short and asks
        // for the one-time grant here.
        #[cfg(target_os = "linux")]
        cx.subscribe_in(
            &app_state,
            window,
            |_, app_state, _: &TunGrantRequested, window, cx| {
                Self::prompt_tun_grant(app_state.clone(), window, cx);
            },
        )
        .detach();

        // A macOS TUN start that needs the privileged helper installed (or
        // reinstalled) stops short and asks here. Never emitted elsewhere.
        cx.subscribe_in(
            &app_state,
            window,
            |_, app_state, _: &HelperInstallRequested, window, cx| {
                Self::prompt_helper_install(app_state.clone(), true, window, cx);
            },
        )
        .detach();

        if let Some((level, message)) = app_state.update(cx, |state, _| state.pending_status.take())
        {
            cx.on_next_frame(window, move |_, _, cx| {
                toast::show(level, message, cx);
            });
        }
        // Subscribers are wired — release the launch attempts queued during
        // startup (argv link, or one the pipe forwarded while the window was
        // still opening).
        app_state.update(cx, |state, _| state.view_attached());
        // A window (re)opened *for* an import link — closed to the tray when
        // the link arrived — is built while that link's `ImportRequested` is
        // already queued, and gpui only activates the subscription above
        // after it: ask from here instead. `prompt_import` takes the request,
        // so a second call is a no-op.
        if app_state.read(cx).pending_import.is_some() {
            let app_state = app_state.clone();
            cx.on_next_frame(window, move |_, window, cx| {
                Self::prompt_import(app_state, window, cx);
            });
        }

        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        let starting = app_state.read(cx).is_starting(cx);
        let update_badge = app_state.read(cx).update_available().is_some();
        let profile_name = app_state
            .read(cx)
            .settings
            .active_profile()
            .map(|p| p.name.clone());

        Self {
            app_state,
            focus_handle,
            starting,
            update_badge,
            profile_name,
            active_page: ActivePage::Home,
            home,
            groups,
            connections,
            profiles,
            logs,
            tools,
            settings,
            tailscale,
            tailscale_visible: false,
            vpn,
            vpn_visible,
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

    /// Switch pages. Focus comes back to the root: a control focused on the
    /// page being left is no longer drawn, and with the focus on nothing the
    /// window's keys (Tab, the shortcuts) would stop reaching this view.
    fn show_page(&mut self, page: ActivePage, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_page != page {
            self.active_page = page;
            self.focus_handle.focus(window, cx);
            cx.notify();
            // Settings › TUN shows the macOS helper as it is now (it may
            // have been turned off or on in Login Items meanwhile).
            if page == ActivePage::Settings {
                self.app_state
                    .update(cx, |state, cx| state.refresh_helper_status(cx));
            }
        }
    }

    /// Ctrl+1..7: the pages always in the sidebar, in its order.
    fn register_page_shortcuts(root: Div, cx: &mut Context<Self>) -> Div {
        root.on_action(cx.listener(|this, _: &ShowHome, window, cx| {
            this.show_page(ActivePage::Home, window, cx)
        }))
        .on_action(cx.listener(|this, _: &ShowGroups, window, cx| {
            this.show_page(ActivePage::Groups, window, cx)
        }))
        .on_action(cx.listener(|this, _: &ShowConnections, window, cx| {
            this.show_page(ActivePage::Connections, window, cx)
        }))
        .on_action(cx.listener(|this, _: &ShowProfiles, window, cx| {
            this.show_page(ActivePage::Profiles, window, cx)
        }))
        .on_action(cx.listener(|this, _: &ShowLogs, window, cx| {
            this.show_page(ActivePage::Logs, window, cx)
        }))
        .on_action(cx.listener(|this, _: &ShowTools, window, cx| {
            this.show_page(ActivePage::Tools, window, cx)
        }))
        .on_action(cx.listener(|this, _: &ShowSettings, window, cx| {
            this.show_page(ActivePage::Settings, window, cx)
        }))
    }

    /// Take the parked deep-link import and confirm it with the user —
    /// links come from arbitrary web pages, never import silently. The URL
    /// shows redacted: the host and path are enough to recognise it, and the
    /// token it may carry shouldn't be on screen.
    fn prompt_import(app_state: Entity<AppState>, window: &mut Window, cx: &mut App) {
        let Some(request) = app_state.update(cx, |state, _| state.pending_import.take()) else {
            return;
        };
        // No activate_window() here: the app-level `ActivateRequested`
        // handler (`ui::app_window`) already ran for this attempt (emitted
        // first, and gpui dispatches effects in emit order), so the window
        // is up before the dialog.
        window.open_alert_dialog(cx, move |alert, _, _| {
            let app_state = app_state.clone();
            let request = request.clone();
            let name = request.name.clone().unwrap_or_default();
            alert
                .title(s().dialogs.import_title)
                .description(
                    div()
                        .v_flex()
                        .gap_1()
                        .children(
                            (!name.is_empty()).then(|| {
                                div().font_weight(FontWeight::SEMIBOLD).child(name)
                            }),
                        )
                        .child(div().text_sm().child(redact_url(&request.url))),
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

    /// Offer the one-time TUN grant (`core::privilege`). Cancel leaves
    /// sing-box stopped; OK runs pkexec, which shows its own password prompt.
    #[cfg(target_os = "linux")]
    pub(crate) fn prompt_tun_grant(
        app_state: Entity<AppState>,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.open_alert_dialog(cx, move |alert, _, _| {
            let app_state = app_state.clone();
            alert
                .title(s().dialogs.tun_grant_title)
                .description(s().dialogs.tun_grant_body)
                .confirm()
                .ok_text(s().dialogs.grant)
                .on_ok(move |_, _, cx| {
                    app_state.update(cx, |state, cx| state.grant_tun_permission(cx));
                    true
                })
        });
    }

    /// Offer to install (or reinstall) the macOS privileged helper, worded
    /// by its state (`HelperStatus::install_prompt`), as Linux offers its
    /// TUN grant. Cancel changes nothing; OK runs the install, whose
    /// administrator prompt is macOS's own. `then_start`: asked by a TUN
    /// start, which goes on once the helper is ready.
    pub(crate) fn prompt_helper_install(
        app_state: Entity<AppState>,
        then_start: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        let prompt = app_state.read(cx).helper_status.install_prompt();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let app_state = app_state.clone();
            alert
                .title(prompt.title)
                .description(prompt.body)
                .confirm()
                .ok_text(prompt.ok)
                .on_ok(move |_, _, cx| {
                    app_state.update(cx, |state, cx| state.install_helper(then_start, cx));
                    true
                })
        });
    }

    /// Confirm removing the macOS privileged helper (Settings › TUN). OK
    /// runs the removal, behind macOS's administrator prompt.
    pub(crate) fn prompt_helper_remove(
        app_state: Entity<AppState>,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.open_alert_dialog(cx, move |alert, _, _| {
            let app_state = app_state.clone();
            alert
                .title(s().dialogs.helper_remove_title)
                .description(s().dialogs.helper_remove_body)
                .confirm()
                .ok_text(s().settings.remove_helper)
                .on_ok(move |_, _, cx| {
                    app_state.update(cx, |state, cx| state.remove_helper(cx));
                    true
                })
        });
    }
}

impl Render for RootView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_starting = self.app_state.read(cx).is_starting(cx);
        let is_running = self.app_state.read(cx).process.read(cx).is_running();
        let theme = cx.theme();
        let status = ConnectionStatus::from_flags(is_starting, is_running);
        let dot_color = match status {
            ConnectionStatus::Starting => theme.warning,
            ConnectionStatus::Connected => theme.success,
            ConnectionStatus::Disconnected => theme.muted_foreground,
        };
        let status_label = status.label();
        let chrome = theme.sidebar;
        let fg = theme.foreground;
        let colors = SidebarColors {
            accent: theme.primary,
            badge: theme.primary,
            muted: theme.muted_foreground,
            tile: theme.background,
            tile_border: theme.border,
        };
        let (panel_bg, panel_border) = (theme.background, theme.border);
        // 状态卡第二行:已连接时显示 ↓/↑ 实时网速,否则显示当前 profile 名。
        let detail = if is_running {
            let traffic = self.app_state.read(cx).traffic.read(cx);
            StatusDetail::Speed(format_speed(traffic.down), format_speed(traffic.up))
        } else {
            match &self.profile_name {
                Some(name) if !is_starting => StatusDetail::Profile(name.clone()),
                _ => StatusDetail::None,
            }
        };

        let view = cx.entity().downgrade();
        let on_nav = move |page: ActivePage, window: &mut Window, cx: &mut App| {
            view.update(cx, |this, cx| this.show_page(page, window, cx))
                .ok();
        };

        let page: AnyView = match self.active_page {
            ActivePage::Home => self.home.clone().into(),
            ActivePage::Groups => self.groups.clone().into(),
            ActivePage::Connections => self.connections.clone().into(),
            ActivePage::Tailscale => self.tailscale.clone().into(),
            ActivePage::Vpn => self.vpn.clone().into(),
            ActivePage::Profiles => self.profiles.clone().into(),
            ActivePage::Logs => self.logs.clone().into(),
            ActivePage::Tools => self.tools.clone().into(),
            ActivePage::Settings => self.settings.clone().into(),
        };

        // Our own title bar (Windows; Linux without server-side
        // decorations): the chrome runs up to the window's top edge, the
        // name moves from the sidebar into the bar, and the panel starts
        // below it. Elsewhere the name heads the sidebar — on macOS under
        // the traffic lights, beside a panel that runs up to the top.
        let strip = title_bar::draws_strip(window);
        let title_bar = strip.then(|| title_bar::title_bar(brand(true), window, cx));
        let header = (!strip).then(|| {
            // The padding `SidebarHeader` gave it, not its hover: the name
            // isn't clickable.
            let name = div().p_2().child(brand(false).px_1().py_1());
            if cfg!(target_os = "macos") {
                title_bar::sidebar_top(name, window, cx).into_any_element()
            } else {
                name.into_any_element()
            }
        });
        let top_edge = title_bar::top_edge(px(PANEL_INSET), window, cx);

        // 注意:不要用 gpui-component 的 `.h_flex()` —— 它附带
        // `items_center`,会把整列内容垂直居中而不是拉伸到全高。
        let body = div()
            .flex()
            .flex_row()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(sidebar(
                header,
                self.active_page,
                dot_color,
                status_label,
                detail,
                colors,
                OptionalPages {
                    tailscale: self.tailscale_visible,
                    vpn: self.vpn_visible,
                },
                Badges {
                    settings: self.update_badge,
                },
                on_nav,
                window,
            ))
            // Cached: the page re-renders only when it notifies (each page
            // observes the entities it reads), not on every root re-render —
            // the sidebar's speed line alone re-renders the root once a
            // second while connected.
            // The page sits in a raised panel inset from the window chrome.
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .when(strip, |panel| panel.mt_1().mb(px(PANEL_INSET)))
                    .when(!strip, |panel| panel.my(px(PANEL_INSET)))
                    .mr(px(PANEL_INSET))
                    .v_flex()
                    .rounded(px(PANEL_RADIUS))
                    .border_1()
                    .border_color(panel_border)
                    .bg(panel_bg)
                    .shadow_xs()
                    .overflow_hidden()
                    .px_6()
                    .pt_5()
                    .pb_6()
                    // Toasts float at the panel's bottom centre.
                    .relative()
                    .child(page.cached(StyleRefinement::default().size_full()))
                    .child(self.toasts.clone()),
            )
            .relative()
            .children(top_edge);

        let root = div()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_update_sub))
            .on_action(cx.listener(Self::on_toggle_process))
            .on_action(|_: &FocusNext, window, cx| window.focus_next(cx))
            .on_action(|_: &FocusPrevious, window, cx| window.focus_prev(cx));
        Self::register_page_shortcuts(root, cx)
            .flex()
            .flex_col()
            .size_full()
            .bg(chrome)
            .text_color(fg)
            .children(title_bar)
            .child(body)
    }
}
