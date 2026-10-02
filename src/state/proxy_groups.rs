use crate::core::settings::{StatusEvent, StatusLevel};
use crate::core::singbox_api::{
    delay_states, merge_groups, parse_groups_from_config, parse_node_types_from_config,
    url_test_done, GroupKind, GroupsSnapshot, ProxyGroup, SingBoxApi, UrlTestHistory,
};
use gpui::{Context, EventEmitter, Task};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub use crate::core::singbox_api::DelayState;

/// How often the UI-thread drain task applies the newest queued snapshot.
/// sing-box throttles group pushes to one per 250ms, so this matches it.
const DRAIN_INTERVAL: Duration = Duration::from_millis(250);
/// Delay before the reader thread re-subscribes after the stream ends while
/// still running — covers the window before the sing-box API is listening, and
/// the routine idle read timeout. Bounded by the `running` flag.
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
/// If no snapshot has arrived this long after start, warn once. The reader
/// keeps retrying regardless, so groups still appear if the API comes up late.
const FIRST_SNAPSHOT_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq)]
pub enum GroupSource {
    /// Live data from the sing-box API; switching is enabled.
    Api,
    /// No live data — sing-box is stopped or its API hasn't answered yet.
    /// The group list is empty in this state.
    Inactive,
}

/// What the reader thread hands the UI thread.
enum StreamEvent {
    Snapshot(GroupsSnapshot),
    /// Why the last subscription attempt ended; surfaced only if no snapshot
    /// ever arrives.
    Error(String),
}

/// A user-started URL test still in flight.
struct PendingTest {
    /// Unix seconds; results at least this recent count as this test's.
    started_at: i64,
    started: Instant,
}

/// Selector groups + their live state. Owned by `AppState`; streamed while
/// sing-box runs (see the observer in `AppState::new`), the same way
/// `Traffic` is driven.
pub struct ProxyGroups {
    pub groups: Vec<ProxyGroup>,
    pub source: GroupSource,
    /// 节点名 → 协议类型(lowercase),跨组共享。运行中来自 API 快照(叠在
    /// config 之上);停止时为空(随 groups 一起清空)。
    pub node_types: HashMap<String, String>,
    /// 节点名 → 延迟徽标,由 `history` + `tested` 每次快照重算。会话级:进程
    /// 停止时清空,不持久化。
    pub delays: HashMap<String, DelayState>,
    /// 测速进行中的组名(Test 按钮 loading)。
    pub testing: HashSet<String>,
    /// sing-box API 句柄(端口 Settings 可配)。stream/select/test 都走它;改端口
    /// 经 `set_api` 换新句柄,运行中由 AppState 重启 sing-box 才生效。
    api: SingBoxApi,
    config_path: PathBuf,
    /// Latest URL-test results from the stream (sing-box's own history, which
    /// also covers urltest groups' periodic checks).
    history: HashMap<String, UrlTestHistory>,
    /// Members of groups whose user-started test has concluded. One without
    /// history failed — sing-box deletes history on failure — so it shows
    /// `Timeout`.
    tested: HashSet<String>,
    pending_tests: HashMap<String, PendingTest>,
    /// When the last snapshot arrived — the clock for `URL_TEST_QUIET`.
    last_snapshot: Option<Instant>,
    /// Liveness flag for the current streaming session. Cleared by `clear()`
    /// and `Drop` so the detached reader thread self-terminates.
    running: Arc<AtomicBool>,
    /// UI-thread task draining snapshots — dropped (= cancelled) by `clear()`.
    /// `select`/`test_delay` are the other lifetime policy on purpose:
    /// fire-and-forget `.detach()`, bounded by their own timeouts.
    _task: Option<Task<()>>,
}

impl EventEmitter<StatusEvent> for ProxyGroups {}

impl ProxyGroups {
    /// Runs once at startup. sing-box is never running at this point, so the
    /// node list starts empty — groups only appear while connected.
    pub fn new(config_path: PathBuf, api: SingBoxApi) -> Self {
        Self {
            groups: Vec::new(),
            source: GroupSource::Inactive,
            node_types: HashMap::new(),
            delays: HashMap::new(),
            testing: HashSet::new(),
            api,
            config_path,
            history: HashMap::new(),
            tested: HashSet::new(),
            pending_tests: HashMap::new(),
            last_snapshot: None,
            running: Arc::new(AtomicBool::new(false)),
            _task: None,
        }
    }

    /// Swap the sing-box API handle after a Settings port change. The next
    /// start/select/test uses it; AppState restarts sing-box when running so
    /// a live session actually moves to the new port.
    pub fn set_api(&mut self, api: SingBoxApi) {
        self.api = api;
    }

    /// Point at another profile's config. The node list is empty while
    /// stopped, so just retarget and clear; while running, the switch restarts
    /// sing-box and the Running/Stopped edges drive the stream (with the path
    /// already updated here).
    pub fn set_config_path(&mut self, config_path: PathBuf, cx: &mut Context<Self>) {
        if self.config_path == config_path {
            return;
        }
        self.config_path = config_path;
        self.clear(cx);
    }

    /// Empty the node list and end the stream: entered on the Running→Stopped
    /// edge. Groups are shown only while sing-box is running.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.running.store(false, Ordering::SeqCst);
        self._task = None;
        self.groups.clear();
        self.source = GroupSource::Inactive;
        self.node_types.clear();
        // 延迟数据只属于一次运行会话
        self.delays.clear();
        self.testing.clear();
        self.history.clear();
        self.tested.clear();
        self.pending_tests.clear();
        self.last_snapshot = None;
        cx.notify();
    }

    /// Subscribe to live groups (Stopped→Running edge). A dedicated reader
    /// thread holds the `SubscribeGroups` stream and re-subscribes while
    /// running; a UI-thread task applies the newest snapshot, ordered by
    /// config position. If nothing arrives within `FIRST_SNAPSHOT_DEADLINE`,
    /// warns once and keeps the list empty until something does.
    pub fn start(&mut self, cx: &mut Context<Self>) {
        // A prior session's thread reads the *old* Arc, so flipping it and
        // replacing `self.running` cleanly separates the two sessions.
        self.running.store(false, Ordering::SeqCst);
        let running = Arc::new(AtomicBool::new(true));
        self.running = running.clone();

        let (tx, rx) = mpsc::channel::<StreamEvent>();
        let api = self.api;
        thread::spawn(move || {
            while running.load(Ordering::SeqCst) {
                let result = api.stream_groups(|snapshot| {
                    running.load(Ordering::SeqCst)
                        && tx.send(StreamEvent::Snapshot(snapshot)).is_ok()
                });
                if !running.load(Ordering::SeqCst) {
                    break;
                }
                if let Err(e) = result {
                    if tx.send(StreamEvent::Error(e)).is_err() {
                        break;
                    }
                }
                thread::sleep(RECONNECT_DELAY);
            }
        });

        let config_path = self.config_path.clone();
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

            let started = Instant::now();
            let mut received = false;
            let mut warned = false;
            let mut last_error = String::new();
            loop {
                cx.background_executor().timer(DRAIN_INTERVAL).await;

                let mut latest = None;
                let mut disconnected = false;
                loop {
                    match rx.try_recv() {
                        Ok(StreamEvent::Snapshot(snapshot)) => latest = Some(snapshot),
                        Ok(StreamEvent::Error(e)) => last_error = e,
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => {
                            disconnected = true;
                            break;
                        }
                    }
                }
                received |= latest.is_some();

                let alive = this.update(cx, |state, cx| {
                    if let Some(snapshot) = latest {
                        state.apply_snapshot(snapshot, &config_groups, &config_types);
                        cx.notify();
                    }
                    // Tests can end without a snapshot (quiet stream / cap),
                    // so check every tick, not just on arrival.
                    if state.settle_tests() {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    return;
                }

                if !received && !warned && started.elapsed() >= FIRST_SNAPSHOT_DEADLINE {
                    warned = true;
                    let reason = if last_error.is_empty() {
                        "sing-box API did not respond".to_string()
                    } else {
                        last_error.clone()
                    };
                    let _ = this.update(cx, |_, cx| {
                        cx.emit(StatusEvent {
                            level: StatusLevel::Warning,
                            message: format!("Failed to load proxy groups: {}", reason),
                        });
                    });
                }

                if disconnected {
                    return;
                }
            }
        });
        self._task = Some(task);
    }

    fn apply_snapshot(
        &mut self,
        snapshot: GroupsSnapshot,
        config_groups: &[ProxyGroup],
        config_types: &HashMap<String, String>,
    ) {
        let mut node_types = config_types.clone();
        node_types.extend(snapshot.node_types);
        self.groups = merge_groups(config_groups, snapshot.groups);
        self.source = GroupSource::Api;
        self.node_types = node_types;
        self.history = snapshot.history;
        self.last_snapshot = Some(Instant::now());
        self.delays = delay_states(&self.history, &self.tested);
    }

    /// End every pending test that `url_test_done` says is over. Returns
    /// whether anything changed.
    fn settle_tests(&mut self) -> bool {
        let now = Instant::now();
        let done: Vec<String> = self
            .pending_tests
            .iter()
            .filter(|(name, test)| {
                let Some(group) = self.groups.iter().find(|g| &g.name == *name) else {
                    return true;
                };
                let quiet_since = self
                    .last_snapshot
                    .map_or(test.started, |last| last.max(test.started));
                url_test_done(
                    group,
                    &self.history,
                    test.started_at,
                    now - test.started,
                    now - quiet_since,
                )
            })
            .map(|(name, _)| name.clone())
            .collect();
        if done.is_empty() {
            return false;
        }
        for name in done {
            self.finish_test(&name);
        }
        self.delays = delay_states(&self.history, &self.tested);
        true
    }

    /// Conclude a user-started test: its members now count as tested, so any
    /// without a result show `Timeout`.
    fn finish_test(&mut self, group: &str) {
        self.pending_tests.remove(group);
        self.testing.remove(group);
        if let Some(entry) = self.groups.iter().find(|g| g.name == group) {
            self.tested.extend(entry.all.iter().cloned());
        }
    }

    /// Optimistically switch `group` to `node`, then confirm via the API.
    /// On failure: error toast + revert to the previous node (sing-box
    /// rejected the switch, so its selection is unchanged).
    pub fn select(&mut self, group: String, node: String, cx: &mut Context<Self>) {
        if self.source != GroupSource::Api {
            return;
        }
        let Some(entry) = self.groups.iter_mut().find(|g| g.name == group) else {
            return;
        };
        // URLTest groups auto-select by latency; the API rejects manual
        // selection, so ignore the request (the UI also leaves these cards
        // non-clickable).
        if entry.kind != GroupKind::Selector {
            return;
        }
        if entry.now == node {
            return;
        }
        let previous = std::mem::replace(&mut entry.now, node.clone());
        cx.notify();

        let api = self.api;
        cx.spawn(async move |this, cx| {
            let request_group = group.clone();
            let request_node = node.clone();
            let result = cx
                .background_executor()
                .spawn(async move { api.select_outbound(&request_group, &request_node) })
                .await;
            if let Err(message) = result {
                let _ = this.update(cx, |state, cx| {
                    if let Some(entry) = state.groups.iter_mut().find(|g| g.name == group) {
                        if entry.now == node {
                            entry.now = previous;
                        }
                    }
                    cx.emit(StatusEvent {
                        level: StatusLevel::Error,
                        message,
                    });
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 整组延迟测速(Test 按钮)。`URLTest` 只是让 sing-box 在后台开测,结果
    /// 随组快照推送回来;何时算测完由 drain 任务每拍 `settle_tests` 判定
    /// (见 `url_test_done`),此时仍无结果的节点标 `Timeout`。请求失败:
    /// Warning toast。detach 不存句柄:请求自带 2s 超时,不会泄漏。
    pub fn test_delay(&mut self, group: String, cx: &mut Context<Self>) {
        if self.source != GroupSource::Api || self.testing.contains(&group) {
            return;
        }
        self.pending_tests.insert(
            group.clone(),
            PendingTest {
                started_at: unix_now(),
                started: Instant::now(),
            },
        );
        self.testing.insert(group.clone());
        // A re-test starts clean: don't keep showing last run's Timeouts while
        // this one is in flight (recorded results stay until replaced).
        if let Some(entry) = self.groups.iter().find(|g| g.name == group) {
            for node in &entry.all {
                self.tested.remove(node);
            }
        }
        self.delays = delay_states(&self.history, &self.tested);
        cx.notify();

        let api = self.api;
        cx.spawn(async move |this, cx| {
            let request_group = group.clone();
            let result = cx
                .background_executor()
                .spawn(async move { api.url_test(&request_group) })
                .await;
            if let Err(message) = result {
                let _ = this.update(cx, |state, cx| {
                    state.pending_tests.remove(&group);
                    state.testing.remove(&group);
                    cx.emit(StatusEvent {
                        level: StatusLevel::Warning,
                        message,
                    });
                    cx.notify();
                });
            }
        })
        .detach();
    }
}

impl Drop for ProxyGroups {
    fn drop(&mut self) {
        // Let the detached reader thread exit at its next snapshot or
        // reconnect check once the entity is gone (e.g. on app quit).
        self.running.store(false, Ordering::SeqCst);
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
