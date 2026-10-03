use crate::core::groups_view::test_cover;
use crate::core::settings::{StatusEvent, StatusLevel};
use crate::i18n::s;
use crate::core::singbox_api::{
    apply_expand_overrides, delay_states, merge_groups, parse_groups_from_config,
    parse_node_types_from_config, url_test_done, GroupKind, GroupsSnapshot, ProxyGroup, SingBoxApi,
    UrlTestHistory,
};
use crate::state::drain::{next_batch_or, Wake};
use futures_channel::mpsc::{self, UnboundedSender};
use gpui::{Context, EventEmitter, Task};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub use crate::core::singbox_api::DelayState;

/// Once a snapshot arrives, how long the drain task waits for a newer one
/// before applying. sing-box already throttles group pushes to one per
/// 250ms, so this is short: it only folds a backlog into one render.
const COALESCE: Duration = Duration::from_millis(50);
/// While a user-started URL test is in flight, how often the drain task
/// checks whether it is over (`url_test_done`) — tests can end without a
/// snapshot. No clock runs otherwise.
const SETTLE_TICK: Duration = Duration::from_millis(250);
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
    /// From `test_delay` / `test_node` / `test_all`: a test started, so the
    /// drain task must run its settle clock.
    TestStarted,
}

/// What a user-started URL test was started on: a group's Test button (or
/// Test all), or one node's delay badge.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum TestTarget {
    Group(String),
    Node(String),
}

/// A user-started URL test still in flight.
struct PendingTest {
    /// Unix seconds; results at least this recent count as this test's.
    started_at: i64,
    started: Instant,
    /// The tags whose results end it: the group's nodes, or the one node
    /// (a node that is itself a group: that group's nodes).
    members: Vec<String>,
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
    /// Nodes with a per-node test in flight (spinner on their delay badge).
    pub testing_nodes: HashSet<String>,
    /// Bumped on every change the Groups page shows, so it can cache what it
    /// derives (filtered / sorted rows) until the next one.
    pub revision: u64,
    /// sing-box API 句柄(端口 + 本次运行的 secret)。stream/select/test 都走它;
    /// AppState 每次启动 sing-box 前经 `set_api` 换新句柄。
    api: SingBoxApi,
    config_path: PathBuf,
    /// Latest URL-test results from the stream (sing-box's own history, which
    /// also covers urltest groups' periodic checks).
    history: HashMap<String, UrlTestHistory>,
    /// Members of groups whose user-started test has concluded. One without
    /// history failed — sing-box deletes history on failure — so it shows
    /// `Timeout`.
    tested: HashSet<String>,
    pending_tests: HashMap<TestTarget, PendingTest>,
    /// Expand toggles made this run (group → expanded), laid over every
    /// snapshot: `SetGroupExpand` doesn't push, so a snapshot can predate it.
    /// sing-box stores them in `cache_file`, so the next run starts from
    /// what the snapshot says.
    expand_overrides: HashMap<String, bool>,
    /// When the last snapshot arrived — the clock for `URL_TEST_QUIET`.
    last_snapshot: Option<Instant>,
    /// Liveness flag for the current streaming session. Cleared by `clear()`
    /// and `Drop` so the detached reader thread self-terminates.
    running: Arc<AtomicBool>,
    /// Sender into the drain task's channel, for `StreamEvent::TestStarted`.
    /// Dropped with the task.
    wake: Option<UnboundedSender<StreamEvent>>,
    /// UI-thread task applying snapshots as they arrive — dropped
    /// (= cancelled) by `clear()`.
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
            testing_nodes: HashSet::new(),
            revision: 0,
            api,
            config_path,
            history: HashMap::new(),
            tested: HashSet::new(),
            pending_tests: HashMap::new(),
            expand_overrides: HashMap::new(),
            last_snapshot: None,
            running: Arc::new(AtomicBool::new(false)),
            wake: None,
            _task: None,
        }
    }

    /// Swap in the API handle (port + secret) of the sing-box run about to
    /// start; AppState calls this before every start. The next
    /// start/select/test uses it.
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
        self.wake = None;
        self.groups.clear();
        self.source = GroupSource::Inactive;
        self.node_types.clear();
        // 延迟数据只属于一次运行会话
        self.delays.clear();
        self.testing.clear();
        self.testing_nodes.clear();
        self.history.clear();
        self.tested.clear();
        self.pending_tests.clear();
        self.expand_overrides.clear();
        self.last_snapshot = None;
        self.changed(cx);
    }

    /// Something the page shows moved: new revision, re-render.
    fn changed(&mut self, cx: &mut Context<Self>) {
        self.revision = self.revision.wrapping_add(1);
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

        let (tx, mut rx) = mpsc::unbounded::<StreamEvent>();
        self.wake = Some(tx.clone());
        let api = self.api;
        thread::spawn(move || {
            while running.load(Ordering::SeqCst) {
                let result = api.stream_groups(|snapshot| {
                    running.load(Ordering::SeqCst)
                        && tx.unbounded_send(StreamEvent::Snapshot(snapshot)).is_ok()
                });
                if !running.load(Ordering::SeqCst) {
                    break;
                }
                if let Err(e) = result {
                    if tx
                        .unbounded_send(StreamEvent::Error(e.to_string()))
                        .is_err()
                    {
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

            let executor = cx.background_executor().clone();
            let started = Instant::now();
            let mut received = false;
            let mut warned = false;
            let mut last_error = String::new();
            // A user-started test is in flight.
            let mut testing = false;
            loop {
                // A clock only while there is time-based work: settling
                // tests, and the first-snapshot deadline.
                let deadline = (!received && !warned)
                    .then(|| FIRST_SNAPSHOT_DEADLINE.saturating_sub(started.elapsed()));
                let wait = match (testing.then_some(SETTLE_TICK), deadline) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
                let clock = wait.map(|wait| executor.timer(wait));
                let events = match next_batch_or(&mut rx, clock, || executor.timer(COALESCE)).await
                {
                    Wake::Batch(events) => events,
                    Wake::Timer => Vec::new(),
                    Wake::Closed => return,
                };

                let mut latest = None;
                for event in events {
                    match event {
                        StreamEvent::Snapshot(snapshot) => latest = Some(snapshot),
                        StreamEvent::Error(e) => last_error = e,
                        StreamEvent::TestStarted => {}
                    }
                }
                received |= latest.is_some();

                let alive = this.update(cx, |state, cx| {
                    if let Some(snapshot) = latest {
                        state.apply_snapshot(snapshot, &config_groups, &config_types);
                        state.changed(cx);
                    }
                    // Tests can end without a snapshot (quiet stream / cap),
                    // so check on every wake, not just on arrival.
                    if state.settle_tests() {
                        state.changed(cx);
                    }
                    !state.pending_tests.is_empty()
                });
                match alive {
                    Ok(pending) => testing = pending,
                    Err(_) => return,
                }

                if !received && !warned && started.elapsed() >= FIRST_SNAPSHOT_DEADLINE {
                    warned = true;
                    let reason = if last_error.is_empty() {
                        s().messages.api_no_response.to_string()
                    } else {
                        last_error.clone()
                    };
                    let _ = this.update(cx, |_, cx| {
                        cx.emit(StatusEvent {
                            level: StatusLevel::Warning,
                            message: (s().messages.groups_failed)(&reason),
                        });
                    });
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
        apply_expand_overrides(&mut self.groups, &self.expand_overrides);
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
        let done: Vec<TestTarget> = self
            .pending_tests
            .iter()
            .filter(|(_, test)| {
                let quiet_since = self
                    .last_snapshot
                    .map_or(test.started, |last| last.max(test.started));
                url_test_done(
                    &test.members,
                    &self.history,
                    test.started_at,
                    now - test.started,
                    now - quiet_since,
                )
            })
            .map(|(target, _)| target.clone())
            .collect();
        if done.is_empty() {
            return false;
        }
        for target in done {
            self.finish_test(&target);
        }
        self.delays = delay_states(&self.history, &self.tested);
        true
    }

    /// Conclude a user-started test: its members now count as tested, so any
    /// without a result show `Timeout`.
    fn finish_test(&mut self, target: &TestTarget) {
        let Some(test) = self.pending_tests.remove(target) else {
            return;
        };
        self.forget_target(target);
        self.tested.extend(test.members);
    }

    /// Drop `target` from the spinner sets.
    fn forget_target(&mut self, target: &TestTarget) {
        match target {
            TestTarget::Group(group) => self.testing.remove(group),
            TestTarget::Node(node) => self.testing_nodes.remove(node),
        };
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
        self.changed(cx);

        let api = self.api;
        cx.spawn(async move |this, cx| {
            let request_group = group.clone();
            let request_node = node.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    api.select_outbound(&request_group, &request_node)
                        .map_err(|e| (s().messages.switch_node_failed)(&e.to_string()))
                })
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
                    state.changed(cx);
                });
            }
        })
        .detach();
    }

    /// Expand or fold a group card. Applied locally at once, then stored by
    /// sing-box (`SetGroupExpand` → `cache_file`) so the card comes back the
    /// same way on the next run. A failed store only warns: the card stays
    /// as the user left it for this run.
    pub fn set_expanded(&mut self, group: String, expanded: bool, cx: &mut Context<Self>) {
        if self.source != GroupSource::Api {
            return;
        }
        let Some(entry) = self.groups.iter_mut().find(|g| g.name == group) else {
            return;
        };
        if entry.expanded == expanded {
            return;
        }
        entry.expanded = expanded;
        self.expand_overrides.insert(group.clone(), expanded);
        self.changed(cx);

        let api = self.api;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { api.set_group_expand(&group, expanded) })
                .await;
            if let Err(e) = result {
                let _ = this.update(cx, |_, cx| {
                    cx.emit(StatusEvent {
                        level: StatusLevel::Warning,
                        message: (s().messages.save_group_state_failed)(&e.to_string()),
                    });
                });
            }
        })
        .detach();
    }

    /// 整组延迟测速(Test 按钮)。`URLTest` 只是让 sing-box 在后台开测,结果
    /// 随组快照推送回来;何时算测完由 drain 任务 `settle_tests` 判定(测速期间
    /// 每 `SETTLE_TICK` 一次,见 `url_test_done`),此时仍无结果的节点标 `Timeout`。请求失败:
    /// Warning toast。detach 不存句柄:请求自带 2s 超时,不会泄漏。
    pub fn test_delay(&mut self, group: String, cx: &mut Context<Self>) {
        if self.source != GroupSource::Api {
            return;
        }
        let target = TestTarget::Group(group.clone());
        if self.begin_test(&target) {
            self.tests_started(cx);
            self.send_url_tests(vec![(group, target)], cx);
        }
    }

    /// Test one node (its delay badge): `URLTest` on a plain outbound tag
    /// probes just that outbound and records or clears its history, like a
    /// group test does per member. Settles by the same rules; only this
    /// node's badge spins.
    pub fn test_node(&mut self, node: String, cx: &mut Context<Self>) {
        if self.source != GroupSource::Api {
            return;
        }
        let target = TestTarget::Node(node.clone());
        if self.begin_test(&target) {
            self.tests_started(cx);
            self.send_url_tests(vec![(node, target)], cx);
        }
    }

    /// Test every group (the page's Test all). Every group's button spins
    /// until its own nodes have answered, but sing-box is only asked to test
    /// enough groups to reach every node once (`test_cover`): one node sits
    /// in several groups, and each `URLTest` probes all of a group's members.
    pub fn test_all(&mut self, cx: &mut Context<Self>) {
        if self.source != GroupSource::Api {
            return;
        }
        let cover = test_cover(&self.groups, |name| self.testing.contains(name));
        let names: Vec<String> = self
            .groups
            .iter()
            .filter(|g| !self.testing.contains(&g.name))
            .map(|g| g.name.clone())
            .collect();
        let mut requests: Vec<(String, TestTarget)> = cover
            .iter()
            .map(|&gi| {
                let name = self.groups[gi].name.clone();
                (name.clone(), TestTarget::Group(name))
            })
            .collect();
        let mut started = false;
        for name in names {
            started |= self.begin_test(&TestTarget::Group(name));
        }
        requests.retain(|(_, target)| self.pending_tests.contains_key(target));
        if started {
            self.tests_started(cx);
            self.send_url_tests(requests, cx);
        }
    }

    /// Whether any group test is in flight (Test all's spinner).
    pub fn testing_any_group(&self) -> bool {
        !self.testing.is_empty()
    }

    /// Register a test of `target` (without asking sing-box yet). `false`
    /// if one is already running or the group is gone.
    fn begin_test(&mut self, target: &TestTarget) -> bool {
        if self.pending_tests.contains_key(target) {
            return false;
        }
        let group_members = |name: &str| {
            self.groups
                .iter()
                .find(|g| g.name == name)
                .map(|g| g.all.clone())
        };
        let members = match target {
            TestTarget::Group(group) => match group_members(group) {
                Some(members) => members,
                None => return false,
            },
            // A member that is itself a group: sing-box tests its nodes, and
            // records history under their tags.
            TestTarget::Node(node) => group_members(node).unwrap_or_else(|| vec![node.clone()]),
        };
        // A re-test starts clean: don't keep showing last run's Timeouts while
        // this one is in flight (recorded results stay until replaced).
        for node in &members {
            self.tested.remove(node);
        }
        if let TestTarget::Node(node) = target {
            self.tested.remove(node);
        }
        match target {
            TestTarget::Group(group) => self.testing.insert(group.clone()),
            TestTarget::Node(node) => self.testing_nodes.insert(node.clone()),
        };
        self.pending_tests.insert(
            target.clone(),
            PendingTest {
                started_at: unix_now(),
                started: Instant::now(),
                members,
            },
        );
        true
    }

    /// After `begin_test`: show it, and start the drain task's settle clock —
    /// it may be asleep on the stream.
    fn tests_started(&mut self, cx: &mut Context<Self>) {
        self.delays = delay_states(&self.history, &self.tested);
        self.changed(cx);
        if let Some(wake) = &self.wake {
            let _ = wake.unbounded_send(StreamEvent::TestStarted);
        }
    }

    /// Ask sing-box to run `URLTest` on each tag, in order, off the UI
    /// thread. A rejected request ends its test and warns once (the first
    /// error) — a dead API would otherwise toast once per group on Test all.
    /// Detached: every request carries its own 2s timeout.
    fn send_url_tests(&mut self, requests: Vec<(String, TestTarget)>, cx: &mut Context<Self>) {
        if requests.is_empty() {
            return;
        }
        let api = self.api;
        cx.spawn(async move |this, cx| {
            let failed = cx
                .background_executor()
                .spawn(async move {
                    let mut failed = Vec::new();
                    for (tag, target) in requests {
                        if let Err(e) = api.url_test(&tag) {
                            failed.push((target, e.to_string()));
                        }
                    }
                    failed
                })
                .await;
            let Some((_, first_error)) = failed.first() else {
                return;
            };
            let message = (s().messages.delay_test_failed)(first_error);
            let _ = this.update(cx, |state, cx| {
                for (target, _) in &failed {
                    state.pending_tests.remove(target);
                    state.forget_target(target);
                }
                cx.emit(StatusEvent {
                    level: StatusLevel::Warning,
                    message,
                });
                state.changed(cx);
            });
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
