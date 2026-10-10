//! Pure decisions for the cross-entity orchestration in `state/`. The gpui
//! observers there execute whatever these functions return, so the branching
//! that used to be untestable (edge detection, exactly-once semantics) is
//! unit-tested here without a gpui context. No gpui dependency — keep it that
//! way so the core test shim keeps working.

/// Where a sing-box run stands, as far as the API consumers are concerned.
/// `AppState` reads it off `ProcessSession` and its own API probe.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RunPhase {
    /// No sing-box process.
    #[default]
    Stopped,
    /// sing-box runs, but its API hasn't answered yet: it is still starting
    /// (rule sets, TUN, inbounds). Shown as Starting.
    Launched,
    /// The API answered (`AppState`'s probe): sing-box is up. Shown as
    /// Connected; every API consumer streams from here on.
    Ready,
}

/// What the app must do when the run moves from one `RunPhase` to another.
/// `AppState`'s observers map these onto the `ProxyGroups`, `Traffic`,
/// `LogBuffer`, `ClashMode`, `Connections`, `NetworkTools`,
/// `TailscaleState` and `VpnStatus` entities, and its own API probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunEffect {
    /// Launched: a new log run; stream the sing-box API's log. Starts before
    /// the API is up so the run's early pipe lines belong to it; the stream
    /// itself re-subscribes until the API answers.
    StartLogs,
    /// Launched: ask the API until it answers (→ Ready), warning if it takes
    /// too long.
    ProbeApi,
    /// Ready: subscribe to live groups.
    StartGroups,
    /// Ready: stream traffic.
    StartTraffic,
    /// Ready: load the clash mode list and follow the current mode.
    StartClashMode,
    /// Ready: subscribe to the live connection list.
    StartConnections,
    /// Ready: load the outbound list the Tools page tests through.
    StartNetworkTools,
    /// Ready: stream Tailscale endpoint status + Taildrop inboxes.
    StartTailscale,
    /// Ready: stream the OpenConnect / OpenVPN / USB/IP endpoints.
    StartVpn,
    /// Stopped: groups are shown only while connected; ends the group
    /// stream.
    ClearGroups,
    /// Stopped: stop the stream and zero the readout.
    StopTraffic,
    /// Stopped: end the mode stream; the switcher hides.
    ClearClashMode,
    /// Stopped: end the connection stream and empty the list — connections
    /// belong to one sing-box run.
    StopConnections,
    /// Stopped: cancel any running test and clear the Tools page.
    StopNetworkTools,
    /// Stopped: forget the endpoints (hides the Tailscale page).
    ClearTailscale,
    /// Stopped: forget the endpoints (hides the VPN page).
    ClearVpn,
    /// Stopped: end the log stream; stderr after exit still shows.
    StopLogs,
    /// Stopped: drop the probe of a run that never got Ready.
    StopApiProbe,
}

const LAUNCHED: &[RunEffect] = &[RunEffect::StartLogs, RunEffect::ProbeApi];
const READY: &[RunEffect] = &[
    RunEffect::StartGroups,
    RunEffect::StartTraffic,
    RunEffect::StartClashMode,
    RunEffect::StartConnections,
    RunEffect::StartNetworkTools,
    RunEffect::StartTailscale,
    RunEffect::StartVpn,
];

/// Decide the effects of an observed `RunPhase` change. gpui observers fire
/// on every `notify`, not just on transitions, so the caller passes the
/// last phase it acted on (`prev`) and stores `now` back only when the
/// returned slice is non-empty — that is what makes each transition fire
/// exactly once. A run only moves forward (Stopped → Launched → Ready) until
/// it stops; a skipped step (both edges seen in one observation) gets both
/// steps' effects, in order.
pub fn run_phase_effects(prev: RunPhase, now: RunPhase) -> &'static [RunEffect] {
    use RunPhase::*;
    match (prev, now) {
        (Stopped, Launched) => LAUNCHED,
        (Launched, Ready) => READY,
        (Stopped, Ready) => &[
            RunEffect::StartLogs,
            RunEffect::ProbeApi,
            RunEffect::StartGroups,
            RunEffect::StartTraffic,
            RunEffect::StartClashMode,
            RunEffect::StartConnections,
            RunEffect::StartNetworkTools,
            RunEffect::StartTailscale,
            RunEffect::StartVpn,
        ],
        (Launched, Stopped) => &[RunEffect::StopApiProbe, RunEffect::StopLogs],
        (Ready, Stopped) => &[
            RunEffect::ClearGroups,
            RunEffect::StopTraffic,
            RunEffect::ClearClashMode,
            RunEffect::StopConnections,
            RunEffect::StopNetworkTools,
            RunEffect::ClearTailscale,
            RunEffect::ClearVpn,
            RunEffect::StopLogs,
            RunEffect::StopApiProbe,
        ],
        // Ready never goes back to Launched while the run lives.
        (Ready, Launched) | (Stopped, Stopped) | (Launched, Launched) | (Ready, Ready) => &[],
    }
}

/// How far a sing-box start has got, as far as a settings or profile change
/// is concerned. `AppState::start_phase` reads it off `ProcessSession` and
/// the Linux TUN gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartPhase {
    /// Nothing running or starting; the next start reads the settings.
    Idle,
    /// A Linux TUN-mode start waiting on its plan probe or the pkexec grant.
    /// The runtime config isn't written yet: `launch` reads the settings
    /// when the gate resolves.
    Gated,
    /// `ProcessSession::Preparing`: the runtime config is already written,
    /// sing-box not yet spawned.
    Preparing,
    Running,
}

/// What a change that ends up in the runtime config (proxy mode, ports,
/// TUN IPv6, system proxy, the active profile or its content) must do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigChangeAction {
    /// Nothing runs with the old values (or will): the next start, or the
    /// gated one, reads the new ones.
    Nothing,
    /// Stop sing-box, or the start in progress, which was written with the
    /// old values, and start again. A start stopped before it is up starts
    /// nothing, and the new one waits for it to finish, with the previous
    /// run's cleanup it owns (`ProcessSession::stop`).
    Restart,
}

pub fn config_change_action(phase: StartPhase) -> ConfigChangeAction {
    match phase {
        StartPhase::Idle | StartPhase::Gated => ConfigChangeAction::Nothing,
        StartPhase::Preparing | StartPhase::Running => ConfigChangeAction::Restart,
    }
}

/// Whether a finished profile fetch may land its config. The fetch runs on
/// the background executor, so meanwhile its profile may have been deleted
/// (or rolled back), or a newer fetch of the same profile started: an
/// import taking over, or a refresh queued behind it. `latest_fetch` is the
/// newest fetch started for this profile; anything older is stale, and its
/// staged config is discarded instead of written.
pub fn fetch_result_applies(latest_fetch: Option<u64>, fetch: u64, profile_exists: bool) -> bool {
    profile_exists && latest_fetch == Some(fetch)
}

/// The one automatic redo of a start whose BoxPilot `api` service lost its
/// picked port before sing-box bound it (`subscription::pick_api_port`).
/// `AppState` holds one; `ProcessSession` reports the loss, and the process
/// observer starts again, with a fresh port, once the run has stopped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ApiPortRetry {
    /// The current run may still be redone once.
    #[default]
    Armed,
    /// The current run lost its port: start again once it has stopped.
    Pending,
    /// That start is under way; its launch uses the retry up.
    Redoing,
    /// The current run is the redo: a second loss stands, and its error
    /// (the exit toast, the line in Logs) is what the user sees.
    Used,
}

impl ApiPortRetry {
    /// A runtime config was written for a new run. Only the redo's own
    /// launch keeps the retry used up; any other start gets a fresh one.
    pub fn launched(self) -> Self {
        match self {
            ApiPortRetry::Redoing => ApiPortRetry::Used,
            _ => ApiPortRetry::Armed,
        }
    }

    /// sing-box could not listen on `lost_port`. Only a loss of the current
    /// run's port counts: a late report from an earlier run is ignored.
    pub fn port_lost(self, lost_port: u16, run_port: u16) -> Self {
        match self {
            ApiPortRetry::Armed if lost_port == run_port => ApiPortRetry::Pending,
            other => other,
        }
    }

    /// sing-box is stopped: whether to start again now. Moves `Pending` on
    /// to `Redoing`, so the redo happens once.
    pub fn take_redo(&mut self) -> bool {
        if *self == ApiPortRetry::Pending {
            *self = ApiPortRetry::Redoing;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lost port leads to exactly one redo, and the redo's own loss to
    /// none.
    #[test]
    fn api_port_loss_is_retried_once() {
        let mut retry = ApiPortRetry::default().launched();
        assert!(!retry.take_redo(), "nothing lost yet");
        retry = retry.port_lost(41234, 41234);
        assert!(retry.take_redo());
        assert!(!retry.take_redo(), "one redo per loss");
        retry = retry.launched();
        assert_eq!(retry, ApiPortRetry::Used);
        retry = retry.port_lost(41235, 41235);
        assert!(!retry.take_redo(), "the redo is not redone");
        // The user's next start may retry again.
        retry = retry.launched();
        assert_eq!(retry.port_lost(41236, 41236), ApiPortRetry::Pending);
    }

    /// A report from an earlier run names a port the current run doesn't
    /// use.
    #[test]
    fn stale_api_port_loss_is_ignored() {
        let retry = ApiPortRetry::Armed.port_lost(41234, 50000);
        assert_eq!(retry, ApiPortRetry::Armed);
    }

    /// A run that just launched only logs and waits for its API; the API
    /// consumers start once it answers.
    #[test]
    fn launch_waits_for_the_api_before_streaming() {
        assert_eq!(
            run_phase_effects(RunPhase::Stopped, RunPhase::Launched),
            &[RunEffect::StartLogs, RunEffect::ProbeApi]
        );
        assert_eq!(
            run_phase_effects(RunPhase::Launched, RunPhase::Ready),
            &[
                RunEffect::StartGroups,
                RunEffect::StartTraffic,
                RunEffect::StartClashMode,
                RunEffect::StartConnections,
                RunEffect::StartNetworkTools,
                RunEffect::StartTailscale,
                RunEffect::StartVpn,
            ]
        );
    }

    /// Both edges in one observation: everything, logs and probe first.
    #[test]
    fn skipped_launch_gets_both_steps() {
        let effects = run_phase_effects(RunPhase::Stopped, RunPhase::Ready);
        let mut expected = LAUNCHED.to_vec();
        expected.extend_from_slice(READY);
        assert_eq!(effects, expected.as_slice());
    }

    #[test]
    fn stop_ends_every_api_consumer() {
        assert_eq!(
            run_phase_effects(RunPhase::Ready, RunPhase::Stopped),
            &[
                RunEffect::ClearGroups,
                RunEffect::StopTraffic,
                RunEffect::ClearClashMode,
                RunEffect::StopConnections,
                RunEffect::StopNetworkTools,
                RunEffect::ClearTailscale,
                RunEffect::ClearVpn,
                RunEffect::StopLogs,
                RunEffect::StopApiProbe,
            ]
        );
    }

    /// A run that exits before its API answers started only the logs and
    /// the probe.
    #[test]
    fn stop_before_ready_ends_only_logs_and_probe() {
        assert_eq!(
            run_phase_effects(RunPhase::Launched, RunPhase::Stopped),
            &[RunEffect::StopApiProbe, RunEffect::StopLogs]
        );
    }

    /// Observers fire on every notify (e.g. Preparing→Running keeps the
    /// phase Stopped through several notifies) — a non-edge must be a no-op
    /// or the streams would restart repeatedly per transition.
    #[test]
    fn no_edge_means_no_effects() {
        for phase in [RunPhase::Stopped, RunPhase::Launched, RunPhase::Ready] {
            assert!(run_phase_effects(phase, phase).is_empty());
        }
        assert!(run_phase_effects(RunPhase::Ready, RunPhase::Launched).is_empty());
    }

    #[test]
    fn config_change_restarts_running_and_preparing() {
        assert_eq!(
            config_change_action(StartPhase::Running),
            ConfigChangeAction::Restart
        );
        assert_eq!(
            config_change_action(StartPhase::Preparing),
            ConfigChangeAction::Restart
        );
    }

    /// A gated start hasn't written its runtime config yet, and an idle app
    /// has none to fix: neither needs anything beyond the saved setting.
    #[test]
    fn config_change_leaves_idle_and_gated_alone() {
        assert_eq!(
            config_change_action(StartPhase::Idle),
            ConfigChangeAction::Nothing
        );
        assert_eq!(
            config_change_action(StartPhase::Gated),
            ConfigChangeAction::Nothing
        );
    }

    #[test]
    fn current_fetch_of_an_existing_profile_applies() {
        assert!(fetch_result_applies(Some(7), 7, true));
    }

    /// The profile was deleted while its fetch ran: the result must not
    /// recreate `configs/<id>.json`.
    #[test]
    fn fetch_for_a_deleted_profile_is_discarded() {
        assert!(!fetch_result_applies(Some(7), 7, false));
        assert!(!fetch_result_applies(None, 7, false));
    }

    /// A newer fetch of the same profile started (an import took over): the
    /// older result must not land over or after it.
    #[test]
    fn superseded_fetch_is_discarded() {
        assert!(!fetch_result_applies(Some(8), 7, true));
        assert!(!fetch_result_applies(None, 7, true));
    }
}
