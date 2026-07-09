//! Profiles 页:订阅 profile 单列列表。每行 = 名称(+Active 徽标)+
//! URL/间隔副标题 + 相对更新时间 + ⟳ 单行立即更新 + ✎ 编辑;非 active
//! 行另有 Use。增删改全走弹窗(草稿存弹窗 InputState,Save 才写回),
//! 删除入口在编辑弹窗左下角。

use crate::core::presentation::{profile_row_info, updated_label};
use crate::core::profile_draft::{is_json_config, DraftKind, ProfileDraft};
use crate::core::settings::{Profile, StatusLevel};
use crate::state::app_state::FetchOrigin;
use crate::state::AppState;
use crate::ui::widgets::{page_header, pill, PillTone};
use crate::ui::toast;
use gpui::{prelude::FluentBuilder, *};
use gpui_component::{
    button::{Button, ButtonVariants},
    dialog::{DialogAction, DialogClose, DialogFooter},
    input::{Input, InputState},
    spinner::Spinner,
    tab::TabBar,
    ActiveTheme, Disableable, Icon, IconName, Sizable, StyledExt, WindowExt,
};
use std::time::SystemTime;

pub struct ProfilesPage {
    app_state: Entity<AppState>,
}

impl ProfilesPage {
    pub fn new(app_state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&app_state, |_, _, cx| cx.notify()).detach();
        Self { app_state }
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
        let delete_name = profile
            .as_ref()
            .map(|p| p.name.clone())
            .unwrap_or_default();
        // 草稿模型(core/profile_draft)播种字段;Add 默认 Remote,Edit 锁定原类型。
        let draft = ProfileDraft::from_profile(profile.as_ref());
        let title: &'static str = if editing_id.is_some() {
            "Edit Profile"
        } else {
            "Add Profile"
        };

        let name_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Profile name…")
                .default_value(draft.name.clone())
        });
        let url_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Enter subscription URL…")
                .default_value(draft.url.clone())
        });
        let interval_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("60")
                .default_value(draft.interval_raw.clone())
        });
        let path_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("No file selected")
                .default_value(draft.path.clone())
        });
        let kind_cell = cx.new(|_| draft.kind.index());

        window.open_dialog(cx, move |dialog, _, cx| {
            let theme = cx.theme();
            let kind = *kind_cell.read(cx);
            let is_edit = editing_id.is_some();

            let field = |label: &'static str, input: AnyElement| {
                div()
                    .v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(label),
                    )
                    .child(input)
            };

            // 左下删除(仅编辑态):先弹确认框,确认后连编辑弹窗一并关闭。
            let delete_button = editing_id.clone().map(|id| {
                let app_state = app_state.clone();
                let name = delete_name.clone();
                Button::new("profile-dialog-delete")
                    .outline()
                    .label("Delete")
                    .text_color(theme.danger)
                    .border_color(theme.danger.opacity(0.5))
                    .disabled(!can_delete)
                    .on_click(move |_, window, cx| {
                        let app_state = app_state.clone();
                        let id = id.clone();
                        let name = name.clone();
                        window.open_alert_dialog(cx, move |alert, _, _| {
                            let app_state = app_state.clone();
                            let id = id.clone();
                            alert
                                .title(format!("Delete profile \"{}\"?", name))
                                .description(
                                    "Its downloaded config is removed too. \
                                     This cannot be undone.",
                                )
                                .confirm()
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
                div().h_flex().child(
                    TabBar::new("profile-kind")
                        .segmented()
                        .selected_index(kind)
                        .on_click(move |ix: &usize, window, cx| {
                            let ix = *ix;
                            kind_cell.update(cx, |k, _| *k = ix);
                            // builder 每帧重跑,refresh 强制重渲以切换下方字段。
                            window.refresh();
                        })
                        .children(vec!["Subscription", "Local file"]),
                )
            });

            let name_field = field(
                "Name",
                Input::new(&name_input).cleanable(false).into_any_element(),
            );

            // 预构建两套字段(避免共享的 `field` 闭包被某个 move 分支独占)。
            let remote_fields = div()
                .v_flex()
                .gap_3()
                .child(field(
                    "Subscription URL",
                    Input::new(&url_input).cleanable(true).into_any_element(),
                ))
                .child(field(
                    "Auto-update interval (minutes, 0 = off)",
                    div()
                        .w(px(120.))
                        .child(Input::new(&interval_input).cleanable(false))
                        .into_any_element(),
                ));

            let choose_file = {
                let path_input = path_input.clone();
                let name_input = name_input.clone();
                Button::new("profile-choose-file")
                    .outline()
                    .label("Browse…")
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
                                                    "Please choose a .json config file.",
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
                                                let empty = name_input
                                                    .read(cx)
                                                    .value()
                                                    .trim()
                                                    .is_empty();
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
                "Config file",
                div()
                    .h_flex()
                    .gap_2()
                    .w_full()
                    .child(div().flex_1().child(Input::new(&path_input).cleanable(true)))
                    .child(choose_file)
                    .into_any_element(),
            );

            dialog
                .title(title)
                .w(px(420.))
                .child(
                    div()
                        .v_flex()
                        .gap_3()
                        .children(kind_toggle)
                        .child(name_field)
                        .when(kind == 0, move |this| this.child(remote_fields))
                        .when(kind == 1, move |this| this.child(local_field)),
                )
                .footer(
                    DialogFooter::new()
                        .justify_between()
                        .child(div().children(delete_button))
                        .child(
                            div()
                                .h_flex()
                                .gap_2()
                                .child(DialogClose::new().child(
                                    Button::new("profile-dialog-cancel")
                                        .outline()
                                        .label("Cancel"),
                                ))
                                .child(DialogAction::new().child(
                                    Button::new("profile-dialog-save")
                                        .primary()
                                        .label("Save"),
                                )),
                        ),
                )
                .on_ok({
                    let app_state = app_state.clone();
                    let editing_id = editing_id.clone();
                    let kind_cell = kind_cell.clone();
                    let name_input = name_input.clone();
                    let url_input = url_input.clone();
                    let interval_input = interval_input.clone();
                    let path_input = path_input.clone();
                    move |_, _, cx| {
                        // 字段 → 草稿 → 模型;解析/裁剪/has_content 规则都在
                        // `ProfileDraft::build`(core,带单测),这里只搬运。
                        let output = ProfileDraft {
                            name: name_input.read(cx).value().to_string(),
                            kind: DraftKind::from_index(*kind_cell.read(cx)),
                            url: url_input.read(cx).value().to_string(),
                            interval_raw: interval_input.read(cx).value().to_string(),
                            path: path_input.read(cx).value().to_string(),
                        }
                        .build();
                        app_state.update(cx, |state, cx| {
                            match editing_id.clone() {
                                Some(id) => {
                                    state.update_profile_fields(id, output.name, output.source, cx);
                                }
                                None => {
                                    let id = state.create_profile(output.name, output.source, cx);
                                    if output.has_content {
                                        state.update_profile(id, FetchOrigin::Manual, cx);
                                    }
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
        updating_id: Option<&str>,
        theme: &gpui_component::theme::Theme,
    ) -> impl IntoElement {
        let this_updating = updating_id == Some(profile.id.as_str());
        let any_updating = updating_id.is_some();
        let row_info = profile_row_info(&profile.source);
        let time_label = updated_label(profile.last_updated_secs, SystemTime::now(), "never updated");

        let app_state_refresh = self.app_state.clone();
        let app_state_edit = self.app_state.clone();
        let app_state_use = self.app_state.clone();
        let refresh_id = profile.id.clone();
        let use_id = profile.id.clone();
        let edit_profile = profile.clone();

        div()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .when(is_active, |this| {
                this.border_color(theme.primary).bg(rgb(0xF5F8FF))
            })
            .h_flex()
            .items_center()
            .justify_between()
            .gap_2()
            .w_full()
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.foreground)
                                    .child(profile.name.clone()),
                            )
                            .when(is_active, |this| {
                                this.child(pill(theme, PillTone::Primary, "Active"))
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .truncate()
                            .child(row_info.subtitle),
                    ),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .flex_none()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(time_label),
                    )
                    .child(
                        Button::new(("profile-refresh", ix))
                            .ghost()
                            .small()
                            .map(|this| {
                                if this_updating {
                                    this.icon(Spinner::new())
                                } else {
                                    this.icon(Icon::default().path("icons/refresh-cw.svg"))
                                }
                            })
                            .disabled(row_info.source_empty || any_updating)
                            .on_click(move |_, _, cx| {
                                app_state_refresh.update(cx, |state, cx| {
                                    state.update_profile(refresh_id.clone(), FetchOrigin::Manual, cx);
                                });
                            }),
                    )
                    .child(
                        Button::new(("profile-edit", ix))
                            .ghost()
                            .small()
                            .icon(Icon::default().path("icons/pencil.svg"))
                            .on_click(move |_, window, cx| {
                                Self::open_profile_dialog(
                                    app_state_edit.clone(),
                                    Some(edit_profile.clone()),
                                    can_delete,
                                    window,
                                    cx,
                                );
                            }),
                    )
                    .when(!is_active, |this| {
                        this.child(
                            Button::new(("profile-use", ix))
                                .outline()
                                .small()
                                .label("Use")
                                .on_click(move |_, _, cx| {
                                    app_state_use.update(cx, |state, cx| {
                                        state.set_active_profile(use_id.clone(), cx);
                                    });
                                }),
                        )
                    }),
            )
    }
}

impl Render for ProfilesPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.app_state.read(cx);
        let profiles = state.settings.profiles.clone();
        let active_id = state.settings.active_profile_id.clone();
        let updating_id = state.updating_profile_id().map(|s| s.to_string());
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
                    updating_id.as_deref(),
                    theme,
                )
                .into_any_element()
            })
            .collect();

        div()
            .v_flex()
            .size_full()
            .gap_4()
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .justify_between()
                    .child(page_header(theme, "Profiles"))
                    .child(
                        Button::new("profile-add")
                            .outline()
                            .small()
                            .icon(Icon::new(IconName::Plus))
                            .label("Add")
                            .on_click(move |_, window, cx| {
                                Self::open_profile_dialog(
                                    app_state_add.clone(),
                                    None,
                                    false,
                                    window,
                                    cx,
                                );
                            }),
                    ),
            )
            .child(if profiles.is_empty() {
                div()
                    .v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("No profiles yet"),
                    )
                    .into_any_element()
            } else {
                div()
                    .v_flex()
                    .gap_2()
                    .w_full()
                    .children(rows)
                    .into_any_element()
            })
    }
}
