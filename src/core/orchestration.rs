//! Pure decisions for the cross-entity orchestration in `state/`. The gpui
//! observers there execute whatever these functions return, so the branching
//! that used to be untestable (edge detection, exactly-once semantics) is
//! unit-tested here without a gpui context. No gpui dependency — keep it that
//! way so the core test shim keeps working.

/// What the app must do in response to a process Running/Stopped transition.
/// `AppState`'s process observer maps these onto the `ProxyGroups` and
/// `Traffic` entities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessEdgeEffect {
    /// Stopped→Running: pull live groups from the Clash API.
    RefreshGroups,
    /// Stopped→Running: start streaming `/traffic`.
    StartTraffic,
    /// Running→Stopped: groups are shown only while connected.
    ClearGroups,
    /// Running→Stopped: stop the stream and zero the readout.
    StopTraffic,
}

/// Decide the effects of an observed process-state change. gpui observers
/// fire on every `notify`, not just on transitions, so the caller passes the
/// last `is_running` it acted on (`prev_running`) and stores `now_running`
/// back only when the returned slice is non-empty — that is what makes each
/// transition fire exactly once.
pub fn process_edge_effects(prev_running: bool, now_running: bool) -> &'static [ProcessEdgeEffect] {
    match (prev_running, now_running) {
        (false, true) => &[
            ProcessEdgeEffect::RefreshGroups,
            ProcessEdgeEffect::StartTraffic,
        ],
        (true, false) => &[
            ProcessEdgeEffect::ClearGroups,
            ProcessEdgeEffect::StopTraffic,
        ],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn started_edge_refreshes_groups_and_starts_traffic() {
        assert_eq!(
            process_edge_effects(false, true),
            &[
                ProcessEdgeEffect::RefreshGroups,
                ProcessEdgeEffect::StartTraffic
            ]
        );
    }

    #[test]
    fn stopped_edge_clears_groups_and_stops_traffic() {
        assert_eq!(
            process_edge_effects(true, false),
            &[
                ProcessEdgeEffect::ClearGroups,
                ProcessEdgeEffect::StopTraffic
            ]
        );
    }

    /// Observers fire on every notify (e.g. Preparing→Running keeps
    /// `is_running` false through several notifies) — a non-edge must be a
    /// no-op or groups would refresh repeatedly per transition.
    #[test]
    fn no_edge_means_no_effects() {
        assert!(process_edge_effects(false, false).is_empty());
        assert!(process_edge_effects(true, true).is_empty());
    }

    /// The exactly-once contract: acting on an edge and storing the new state
    /// makes an identical follow-up observation a no-op.
    #[test]
    fn acted_on_edge_does_not_fire_twice() {
        let mut prev = false;
        let first = process_edge_effects(prev, true);
        assert!(!first.is_empty());
        prev = true;
        assert!(process_edge_effects(prev, true).is_empty());
    }
}
