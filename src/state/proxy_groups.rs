use crate::core::clash_api::{
    merge_groups, parse_groups_from_config, parse_node_types_from_config, ClashApi, GroupKind,
    ProxyGroup,
};
use crate::core::settings::{StatusEvent, StatusLevel};
use gpui::{Context, EventEmitter, Task};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

/// The Clash API needs a moment after process start; poll up to
/// REFRESH_ATTEMPTS with a fixed interval before giving up and leaving the
/// node list empty.
const REFRESH_ATTEMPTS: usize = 5;
const REFRESH_RETRY_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, PartialEq)]
pub enum GroupSource {
    /// Live data from the Clash API; switching is enabled.
    Api,
    /// No live data — sing-box is stopped or the Clash API couldn't be reached
    /// after start. The group list is empty in this state.
    Inactive,
}

/// 单个节点最近一次测速的结果。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DelayState {
    Ok(u32),
    /// 测过但不在响应里——失败/超时。
    Timeout,
}

/// Selector groups + where they came from. Owned by `AppState`; refreshed on
/// process Running/Stopped edges (see the observer in `AppState::new`).
pub struct ProxyGroups {
    pub groups: Vec<ProxyGroup>,
    pub source: GroupSource,
    /// 节点名 → 协议类型(lowercase),跨组共享。运行中来自 API;
    /// 停止时为空(随 groups 一起清空)。
    pub node_types: HashMap<String, String>,
    /// 节点名 → 最近测速结果。会话级:进程停止时清空,不持久化。
    pub delays: HashMap<String, DelayState>,
    /// 测速进行中的组名(Test 按钮 loading)。
    pub testing: HashSet<String>,
    /// Clash API 句柄(端口 Settings 可配)。refresh/select/test 都走它;改端口
    /// 经 `set_api` 换新句柄,运行中由 AppState 重启 sing-box 才生效。
    api: ClashApi,
    config_path: PathBuf,
    /// In-flight refresh/reload — replaced (= cancelled) by each new request,
    /// and dropped by `clear()` so a stop cancels it. `select`/`test_delay`
    /// are the other lifetime policy on purpose: fire-and-forget `.detach()`,
    /// bounded by their own request timeouts (see `test_delay`).
    _task: Option<Task<()>>,
}

impl EventEmitter<StatusEvent> for ProxyGroups {}

impl ProxyGroups {
    /// Runs once at startup. sing-box is never running at this point, so the
    /// node list starts empty — groups only appear while connected.
    pub fn new(config_path: PathBuf, api: ClashApi) -> Self {
        Self {
            groups: Vec::new(),
            source: GroupSource::Inactive,
            node_types: HashMap::new(),
            delays: HashMap::new(),
            testing: HashSet::new(),
            api,
            config_path,
            _task: None,
        }
    }

    /// Swap the Clash API handle after a Settings port change. The next
    /// refresh/select/test uses it; AppState restarts sing-box when running so
    /// a live session actually moves to the new port.
    pub fn set_api(&mut self, api: ClashApi) {
        self.api = api;
    }

    /// Point at another profile's config. The node list is empty while
    /// stopped, so just retarget and clear; while running, the switch restarts
    /// sing-box and the Running/Stopped edges drive the refresh (with the path
    /// already updated here).
    pub fn set_config_path(&mut self, config_path: PathBuf, cx: &mut Context<Self>) {
        if self.config_path == config_path {
            return;
        }
        self.config_path = config_path;
        self.clear(cx);
    }

    /// Empty the node list: entered on the Running→Stopped edge. Groups are
    /// shown only while sing-box is running (live Clash API data), so once it
    /// stops there are no nodes to display. Synchronous — also cancels any
    /// in-flight API refresh by dropping `_task`.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.groups.clear();
        self.source = GroupSource::Inactive;
        self.node_types.clear();
        // 延迟数据只属于一次运行会话
        self.delays.clear();
        self.testing.clear();
        self._task = None;
        cx.notify();
    }

    /// Fetch live groups from the Clash API (Stopped→Running edge), ordering
    /// them by config position. Retries while the API comes up; on exhaustion
    /// emits a warning toast and leaves the node list empty.
    pub fn refresh_from_api(&mut self, cx: &mut Context<Self>) {
        let config_path = self.config_path.clone();
        let api = self.api;
        let task = cx.spawn(async move |this, cx| {
            let (config_groups, config_types) = cx
                .background_executor()
                .spawn(async move {
                    fs::read_to_string(&config_path)
                        .map(|data| {
                            (
                                parse_groups_from_config(&data),
                                parse_node_types_from_config(&data),
                            )
                        })
                        .unwrap_or_default()
                })
                .await;

            let mut last_error = String::new();
            for attempt in 0..REFRESH_ATTEMPTS {
                if attempt > 0 {
                    cx.background_executor().timer(REFRESH_RETRY_INTERVAL).await;
                }
                let result = cx
                    .background_executor()
                    .spawn(async move { api.fetch_proxies() })
                    .await;
                match result {
                    Ok((api_groups, api_types)) => {
                        let merged = merge_groups(&config_groups, api_groups);
                        // API types 叠在 config types 之上;同会话内重拉
                        // 分组不清 delays。
                        let mut node_types = config_types.clone();
                        node_types.extend(api_types);
                        let _ = this.update(cx, |state, cx| {
                            state.groups = merged;
                            state.source = GroupSource::Api;
                            state.node_types = node_types;
                            state._task = None;
                            cx.notify();
                        });
                        return;
                    }
                    Err(e) => last_error = e,
                }
            }
            let _ = this.update(cx, |state, cx| {
                // No read-only config fallback: if the API never came up the
                // node list stays empty (same as stopped).
                state.groups.clear();
                state.source = GroupSource::Inactive;
                state.node_types.clear();
                state._task = None;
                cx.emit(StatusEvent {
                    level: StatusLevel::Warning,
                    message: format!("Failed to load proxy groups: {}", last_error),
                });
                cx.notify();
            });
        });
        self._task = Some(task);
    }

    /// Optimistically switch `group` to `node`, then confirm via the API.
    /// On failure: error toast + re-fetch to snap the UI back to server truth.
    pub fn select(&mut self, group: String, node: String, cx: &mut Context<Self>) {
        if self.source != GroupSource::Api {
            return;
        }
        let Some(entry) = self.groups.iter_mut().find(|g| g.name == group) else {
            return;
        };
        // URLTest groups auto-select by latency; the API rejects manual PUTs,
        // so ignore the request (the UI also leaves these cards non-clickable).
        if entry.kind != GroupKind::Selector {
            return;
        }
        if entry.now == node {
            return;
        }
        entry.now = node.clone();
        cx.notify();

        let api = self.api;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { api.select_proxy(&group, &node) })
                .await;
            if let Err(message) = result {
                let _ = this.update(cx, |state, cx| {
                    cx.emit(StatusEvent {
                        level: StatusLevel::Error,
                        message,
                    });
                    state.refresh_from_api(cx);
                });
            }
        })
        .detach();
    }

    /// 整组延迟测速(Test 按钮)。结果写入 `delays`;组内在响应中缺席的
    /// 节点标 `Timeout`。请求失败:Warning toast,delays 不动。
    /// detach 不存句柄:请求自带 30s 超时,不会泄漏(与 `select()` 同模式)。
    pub fn test_delay(&mut self, group: String, cx: &mut Context<Self>) {
        if self.source != GroupSource::Api || self.testing.contains(&group) {
            return;
        }
        self.testing.insert(group.clone());
        cx.notify();

        let api = self.api;
        cx.spawn(async move |this, cx| {
            let request_group = group.clone();
            let result = cx
                .background_executor()
                .spawn(async move { api.test_group_delay(&request_group) })
                .await;
            let _ = this.update(cx, |state, cx| {
                state.testing.remove(&group);
                match result {
                    Ok(delays) => {
                        let mut is_urltest = false;
                        if let Some(entry) = state.groups.iter().find(|g| g.name == group) {
                            is_urltest = entry.kind == GroupKind::UrlTest;
                            for node in &entry.all {
                                let value = delays
                                    .get(node)
                                    .copied()
                                    .map(DelayState::Ok)
                                    .unwrap_or(DelayState::Timeout);
                                state.delays.insert(node.clone(), value);
                            }
                        }
                        // URLTest 测速后会按新延迟重选最快节点;重拉一次 /proxies
                        // 把 `now`(标题当前节点 + 卡片高亮)同步到服务端真值。
                        // Selector 的 now 是手动选择、测速不变,无需重拉。
                        // refresh_from_api 不清 `delays`,刚写入的结果保留。
                        if is_urltest {
                            state.refresh_from_api(cx);
                        }
                    }
                    Err(message) => {
                        cx.emit(StatusEvent {
                            level: StatusLevel::Warning,
                            message,
                        });
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}
