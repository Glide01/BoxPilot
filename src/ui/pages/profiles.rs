//! Profiles 页:订阅 profile 单列列表。每行 = 单选圈 + 名称,同一行右侧是
//! 更新按钮(⟳ + 多久前更新,即 freshness 与更新动作合一)与 ✎ 编辑;第二行
//! 来源(订阅域名 / 文件名,悬停看完整),第三行用量条。点别的行即切换。
//! 增删改全走弹窗(草稿存弹窗 InputState,Save 才写回),删除入口在编辑
//! 弹窗左下角。

use crate::core::presentation::{auto_update_choices, profile_freshness, profile_row_info};
use crate::core::profile_draft::{is_json_config, DraftKind, ProfileDraft};
use crate::core::settings::{Profile, StatusLevel};
use crate::i18n::s;
use crate::state::app_state::FetchOrigin;
use crate::state::AppState;
use crate::ui::pages::ActivePage;
use crate::ui::theme::CARD_RADIUS;
use crate::ui::toast;
use crate::ui::widgets::{
    choice_select, dialog_button, empty_state, empty_state_button, form_button, form_input,
    freshness_button, full_text_tooltip, grouped_card, minute_ticker, page_header, page_layout,
    profile_source_line, row_hover_bg, section_heading, segmented, setting_row, usage_meter,
    Control, ControlSize, IconLabel, Segment, TextLabel, CONTROL_LINE_HEIGHT, DIALOG_BODY_BOTTOM,
};
use gpui::{prelude::FluentBuilder, *};
use gpui_component::{
    button::{Button, ButtonVariants},
    dialog::{DialogAction, DialogClose, DialogFooter},
    input::InputState,
    scroll::ScrollableElement,
    switch::Switch,
    ActiveTheme, Disableable, Icon, IconName, StyledExt, WindowExt,
};
use std::time::SystemTime;

/// Letters of a profile's name its line shows whole beside the update and
/// edit buttons in the narrowest window; longer ones get a tooltip.
const PROFILE_NAME_ROOM: usize = 28;

pub struct ProfilesPage {
    app_state: Entity<AppState>,
    /// Re-renders once a minute: "25 min ago" and the usage line's
    /// expiry countdown move with the clock, not with any entity.
    _ticker: Task<()>,
}

impl ProfilesPage {
    pub fn new(app_state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&app_state, |_, _, cx| cx.notify()).detach();
        Self {
            app_state,
            _ticker: minute_ticker(cx),
        }
    }

    /// 打开编辑/新增弹窗。`profile = None` 即新增。顶部 Subscription/Local file
    /// 切换(仅 Add 显示;Edit 锁定原类型)。Save(按钮或 Enter)一次性写回;
    /// Cancel/Esc/遮罩点击丢弃草稿。
    /// `pub(crate)` so `HomePage`'s empty-state "Add subscription" button can
    /// open the same dialog.
    pub(crate) fn open_profile_dialog(
        app_state: Entity<AppState>,
        profile: Option<Profile>,
        can_delete: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        let editing_id = profile.as_ref().map(|p| p.id.clone());
        let delete_name = profile.as_ref().map(|p| p.name.clone()).unwrap_or_default();
        // 草稿模型(core/profile_draft)播种字段;Add 默认 Remote,Edit 锁定原类型。
        let draft = ProfileDraft::from_profile(profile.as_ref());
        let t = s();
        let title: &'static str = if editing_id.is_some() {
            t.profiles.edit_title
        } else {
            t.profiles.add_title
        };

        let name_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t.profiles.name_placeholder)
                .default_value(draft.name.clone())
        });
        let url_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t.profiles.url_placeholder)
                .default_value(draft.url.clone())
        });
        let path_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t.profiles.no_file_selected)
                .default_value(draft.path.clone())
        });
        let kind_cell = cx.new(|_| draft.kind.index());
        let interval_cell = cx.new(|_| draft.interval_minutes);
        let via_sing_box_cell = cx.new(|_| draft.update_via_sing_box);

        crate::ui::dialog::open_dialog(window, cx, move |dialog, centered, window, cx| {
            let t = s();
            let kind = *kind_cell.read(cx);
            let interval = *interval_cell.read(cx);
            let via_sing_box = *via_sing_box_cell.read(cx);
            let is_edit = editing_id.is_some();

            // Built before `theme` borrows `cx`: the dropdown keeps its state
            // in the window.
            let interval_select = {
                let interval_cell = interval_cell.clone();
                choice_select(
                    "profile-auto-update",
                    auto_update_choices(),
                    interval,
                    move |minutes, window, cx| {
                        interval_cell.update(cx, |cell, _| *cell = minutes);
                        window.refresh();
                    },
                    window,
                    cx,
                )
            };
            let theme = cx.theme();

            // A form field: its label over the input, both full width.
            let field = |label: &'static str, input: AnyElement| {
                div()
                    .v_flex()
                    .gap_1p5()
                    .child(div().text_sm().text_color(theme.foreground).child(label))
                    .child(input)
            };

            // 左下删除(仅编辑态):先弹确认框,确认后连编辑弹窗一并关闭。
            let delete_button = editing_id.clone().map(|id| {
                let app_state = app_state.clone();
                let name = delete_name.clone();
                dialog_button(Button::new("profile-dialog-delete"))
                    .outline()
                    .text_label(t.common.delete)
                    .text_color(theme.danger)
                    .border_color(theme.danger.opacity(0.5))
                    .disabled(!can_delete)
                    .on_click(move |_, window, cx| {
                        let app_state = app_state.clone();
                        let id = id.clone();
                        let name = name.clone();
                        crate::ui::dialog::open_alert(window, cx, move |alert, centered, _, _| {
                            let app_state = app_state.clone();
                            let id = id.clone();
                            alert
                                .title(centered.title((s().profiles.delete_title)(&name)))
                                .description(s().profiles.delete_body)
                                .footer(centered.confirm_footer(s().common.ok))
                                .on_ok(move |_, window, cx| {
                                    app_state.update(cx, |state, cx| {
                                        state.delete_profile(id.clone(), cx);
                                    });
                                    // 把底下的编辑弹窗一并关掉;确认框自身
                                    // 随后的关闭落在空栈上,是安全的 no-op。
                                    window.close_all_dialogs(cx);
                                    true
                                })
                        });
                    })
            });

            // 类型切换 —— 仅 Add。Edit 锁定类型(切换会孤立另一来源的数据)。
            let kind_toggle = (!is_edit).then(|| {
                let kind_cell = kind_cell.clone();
                // 包一层 h_flex:分段控件本身无显式宽度,直接放进外层 v_flex 会被
                // 拉伸成整行;放进 row 里则按内容收窄并左对齐。
                div().h_flex().child(segmented(
                    theme,
                    "profile-kind",
                    ControlSize::Field,
                    vec![
                        Segment::new(t.profiles.kind_subscription),
                        Segment::new(t.profiles.kind_local),
                    ],
                    Some(kind),
                    move |ix, window, cx| {
                        kind_cell.update(cx, |k, _| *k = ix);
                        // builder 每帧重跑,refresh 强制重渲以切换下方字段。
                        window.refresh();
                    },
                ))
            });

            let name_field = field(
                t.profiles.name,
                form_input(&name_input).cleanable(false).into_any_element(),
            );

            // 订阅:链接一栏;更新选项(自动更新、经 sing-box)放进与设置页同款的
            // 分组卡片。本地文件没有更新选项,不显示这一节。
            let url_field = field(
                t.profiles.subscription_url,
                form_input(&url_input).cleanable(true).into_any_element(),
            );
            let update_options = div()
                .v_flex()
                .gap_2()
                .child(section_heading(theme, t.profiles.update_section))
                .child(grouped_card(
                    theme,
                    [
                        setting_row(theme, t.profiles.auto_update, None)
                            .child(interval_select)
                            .into_any_element(),
                        setting_row(
                            theme,
                            t.profiles.update_via_sing_box,
                            Some(t.profiles.update_via_sing_box_hint),
                        )
                        .child({
                            let via_sing_box_cell = via_sing_box_cell.clone();
                            Switch::new("profile-update-via-sing-box")
                                .checked(via_sing_box)
                                .on_click(move |checked: &bool, window, cx| {
                                    let checked = *checked;
                                    via_sing_box_cell.update(cx, |via, _| *via = checked);
                                    window.refresh();
                                })
                        })
                        .into_any_element(),
                    ],
                ));

            let choose_file = {
                let path_input = path_input.clone();
                let name_input = name_input.clone();
                form_button(Button::new("profile-choose-file"))
                    .outline()
                    .text_label(t.profiles.browse)
                    .on_click(move |_, window, cx| {
                        let rx = cx.prompt_for_paths(PathPromptOptions {
                            files: true,
                            directories: false,
                            multiple: false,
                            prompt: None,
                        });
                        let path_input = path_input.clone();
                        let name_input = name_input.clone();
                        window
                            .spawn(cx, async move |cx| {
                                if let Ok(Ok(Some(paths))) = rx.await {
                                    if let Some(p) = paths.first() {
                                        // gpui 原生对话框无扩展名过滤,选后校验:只接受 .json。
                                        if !is_json_config(p) {
                                            let _ = cx.update(|_, cx| {
                                                toast::show(
                                                    StatusLevel::Warning,
                                                    s().profiles.choose_json,
                                                    cx,
                                                );
                                            });
                                            return;
                                        }
                                        let display = p.display().to_string();
                                        let stem = p
                                            .file_stem()
                                            .and_then(|s| s.to_str())
                                            .map(|s| s.to_string());
                                        let _ = cx.update(|window, cx| {
                                            path_input.update(cx, |st, cx| {
                                                st.set_value(display.clone(), window, cx)
                                            });
                                            // 名称留空则用文件名兜底。
                                            if let Some(stem) = stem {
                                                let empty =
                                                    name_input.read(cx).value().trim().is_empty();
                                                if empty {
                                                    name_input.update(cx, |st, cx| {
                                                        st.set_value(stem, window, cx)
                                                    });
                                                }
                                            }
                                        });
                                    }
                                }
                            })
                            .detach();
                    })
            };
            let local_field = field(
                t.profiles.config_file,
                div()
                    .h_flex()
                    .gap_2()
                    .w_full()
                    .child(
                        div()
                            .flex_1()
                            .child(form_input(&path_input).cleanable(true)),
                    )
                    .child(choose_file)
                    .into_any_element(),
            );

            dialog
                .title(centered.title(title))
                .w(px(460.))
                .child(
                    div()
                        .v_flex()
                        .gap_5()
                        .pt_2()
                        .pb(DIALOG_BODY_BOTTOM)
                        .children(kind_toggle)
                        .child(
                            div()
                                .v_flex()
                                .gap_3()
                                .child(name_field)
                                .when(kind == 0, move |this| this.child(url_field))
                                .when(kind == 1, move |this| this.child(local_field)),
                        )
                        .when(kind == 0, move |this| this.child(update_options)),
                )
                .footer(
                    centered.footer(
                        DialogFooter::new()
                            .justify_between()
                            .child(div().children(delete_button))
                            .child(
                                div()
                                    .h_flex()
                                    .gap_2()
                                    .child(
                                        DialogClose::new().child(dialog_button(
                                            Button::new("profile-dialog-cancel")
                                                .outline()
                                                .text_label(t.common.cancel),
                                        )),
                                    )
                                    .child(
                                        DialogAction::new().child(dialog_button(
                                            Button::new("profile-dialog-save")
                                                .primary()
                                                .text_label(t.common.save),
                                        )),
                                    ),
                            ),
                    ),
                )
                .on_ok({
                    let app_state = app_state.clone();
                    let editing_id = editing_id.clone();
                    let kind_cell = kind_cell.clone();
                    let name_input = name_input.clone();
                    let url_input = url_input.clone();
                    let interval_cell = interval_cell.clone();
                    let via_sing_box_cell = via_sing_box_cell.clone();
                    let path_input = path_input.clone();
                    move |_, _, cx| {
                        // 字段 → 草稿 → 模型;解析/裁剪/has_content 规则都在
                        // `ProfileDraft::build`(core,带单测),这里只搬运。
                        let output = ProfileDraft {
                            name: name_input.read(cx).value().to_string(),
                            kind: DraftKind::from_index(*kind_cell.read(cx)),
                            url: url_input.read(cx).value().to_string(),
                            interval_minutes: *interval_cell.read(cx),
                            update_via_sing_box: *via_sing_box_cell.read(cx),
                            path: path_input.read(cx).value().to_string(),
                        }
                        .build();
                        app_state.update(cx, |state, cx| match editing_id.clone() {
                            Some(id) => {
                                state.update_profile_fields(id, output.name, output.source, cx);
                            }
                            None => {
                                let id = state.create_profile(output.name, output.source, cx);
                                if output.has_content {
                                    state.update_profile(id, FetchOrigin::Manual, cx);
                                }
                            }
                        });
                        true
                    }
                })
        });
    }

    fn profile_row(
        &self,
        ix: usize,
        profile: &Profile,
        is_active: bool,
        can_delete: bool,
        state: &AppState,
        theme: &gpui_component::theme::Theme,
    ) -> impl IntoElement {
        let now = SystemTime::now();
        let freshness = profile_freshness(
            profile,
            state.updating_profile_id() == Some(profile.id.as_str()),
            state.fetch_error(&profile.id),
            now,
        );
        let t = s();

        let app_state_refresh = self.app_state.clone();
        let app_state_edit = self.app_state.clone();
        let app_state_use = self.app_state.clone();
        let refresh_id = profile.id.clone();
        let use_id = profile.id.clone();
        let edit_profile = profile.clone();

        let primary = theme.primary;
        let hover_bg = row_hover_bg(theme);
        // A radio mark: which profile sing-box runs with. Clicking anywhere
        // on another row (outside its buttons) switches to it. Centred on
        // the name's line, not on the whole row.
        let radio = div()
            .flex_none()
            .mt((CONTROL_LINE_HEIGHT - px(18.)) / 2.)
            .size(px(18.))
            .rounded_full()
            .map(|this| {
                if is_active {
                    this.border(px(5.))
                        .border_color(primary)
                        .bg(theme.background)
                } else {
                    this.border(px(1.5)).border_color(theme.input)
                }
            });

        // Name on the left, the update button and edit on the right of the
        // same line: every row's controls end in one column, level with
        // its name.
        let title_line = div()
            .h_flex()
            .items_center()
            .gap_2()
            .h(CONTROL_LINE_HEIGHT)
            .child(full_text_tooltip(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.foreground)
                    .truncate(),
                ("profile-name", ix),
                profile.name.clone(),
                PROFILE_NAME_ROOM,
            ))
            .child(
                div()
                    .h_flex()
                    .flex_none()
                    .items_center()
                    .gap_1()
                    .children(freshness.map(|freshness| {
                        freshness_button(
                            theme,
                            ("profile-refresh", ix),
                            freshness,
                            None,
                            move |_, _, cx| {
                                // Not a click on the row: that would switch.
                                cx.stop_propagation();
                                app_state_refresh.update(cx, |state, cx| {
                                    state.update_profile(
                                        refresh_id.clone(),
                                        FetchOrigin::Manual,
                                        cx,
                                    );
                                });
                            },
                        )
                    }))
                    .child(
                        Button::new(("profile-edit", ix))
                            .ghost()
                            .icon_control(ControlSize::Inline)
                            .icon(Icon::default().path("icons/pencil.svg"))
                            .tooltip(t.profiles.edit_title)
                            .on_click(move |_, window, cx| {
                                cx.stop_propagation();
                                Self::open_profile_dialog(
                                    app_state_edit.clone(),
                                    Some(edit_profile.clone()),
                                    can_delete,
                                    window,
                                    cx,
                                );
                            }),
                    ),
            );

        div()
            .id(("profile-row", ix))
            .px_4()
            .py_3()
            .rounded(px(CARD_RADIUS))
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .map(|this| {
                // The radio says which row is in use; a faint tint backs it
                // up. No accent border on top: one signal is enough.
                if is_active {
                    this.bg(primary.opacity(0.04))
                } else {
                    this.cursor_pointer()
                        .hover(move |s| s.bg(hover_bg))
                        .on_click(move |_, _, cx| {
                            app_state_use.update(cx, |state, cx| {
                                state.set_active_profile(use_id.clone(), cx);
                            });
                        })
                }
            })
            .h_flex()
            .items_start()
            .gap_3()
            .w_full()
            .child(radio)
            .child(
                div()
                    .v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(title_line)
                    .child(profile_source_line(
                        theme,
                        ("profile-source", ix),
                        profile_row_info(&profile.source),
                    ))
                    // Traffic / expiry the subscription server reported,
                    // across the row.
                    .children(profile.usage.as_ref().map(|usage| {
                        div().pt_2().w_full().child(usage_meter(
                            theme,
                            ("profile-usage", ix),
                            usage,
                            now,
                        ))
                    })),
            )
    }
}

impl Render for ProfilesPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.app_state.read(cx);
        let profiles = state.settings.profiles.clone();
        let active_id = state.settings.active_profile_id.clone();
        let can_delete = true;
        let app_state_add = self.app_state.clone();
        let theme = cx.theme();

        let rows: Vec<AnyElement> = profiles
            .iter()
            .enumerate()
            .map(|(ix, profile)| {
                self.profile_row(
                    ix,
                    profile,
                    profile.id == active_id,
                    can_delete,
                    state,
                    theme,
                )
                .into_any_element()
            })
            .collect();

        let add = move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            Self::open_profile_dialog(app_state_add.clone(), None, false, window, cx);
        };
        if profiles.is_empty() {
            let empty = empty_state(
                theme,
                Icon::new(IconName::GalleryVerticalEnd),
                s().profiles.empty,
                s().home.no_subscription_hint,
            )
            .action(
                empty_state_button("profile-add")
                    .icon_label(IconName::Plus, s().profiles.add_title)
                    .on_click(add),
            );
            return page_layout(
                page_header(theme, ActivePage::Profiles),
                div().size_full().child(empty),
            );
        }

        let list = div().v_flex().gap_2().w_full().children(rows);
        page_layout(
            page_header(theme, ActivePage::Profiles).action(
                Button::new("profile-add")
                    .primary()
                    .control(ControlSize::Regular)
                    .icon_label(IconName::Plus, s().profiles.add)
                    .on_click(add),
            ),
            div().size_full().child(list.overflow_y_scrollbar()),
        )
    }
}
