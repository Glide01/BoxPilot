//! Clash mode (the `clash_mode` route/DNS rule selector): `GetClashModeStatus`,
//! `SubscribeClashMode`, `SetClashMode`. Available whenever the `api`
//! service runs — no `experimental.clash_api` needed.

use super::transport::{ApiError, IDLE_STREAM_READ_TIMEOUT};
use super::{pb, SingBoxApi};

impl SingBoxApi {
    /// `GetClashModeStatus` — the selectable modes and the current one.
    ///
    /// The list is derived from the config: every `clash_mode` value used in
    /// `route.rules` and `dns.rules` (logical rules included), custom modes
    /// sorted by name first, then whichever of `Rule`, `Global`, `Direct`
    /// appear, in that order. The default mode (`Rule`, unless the config's
    /// own `clash_api.default_mode` names another) is prepended when no rule
    /// mentions it, so a config without `clash_mode` rules yields just
    /// `["Rule"]`. The
    /// current mode is restored from `cache_file` at startup.
    pub fn get_clash_mode_status(&self) -> Result<ClashModeStatus, ApiError> {
        self.unary("GetClashModeStatus", &())
            .map(ClashModeStatus::from_proto)
    }

    /// Stream `SubscribeClashMode`: the current mode on subscribe, then the
    /// new one on every change (from `set_clash_mode` or any other API
    /// client). Idle otherwise: `TimedOut` after `IDLE_STREAM_READ_TIMEOUT`;
    /// re-subscribe.
    pub fn stream_clash_mode(
        &self,
        mut on_mode: impl FnMut(String) -> bool,
    ) -> Result<(), ApiError> {
        self.stream(
            "SubscribeClashMode",
            &(),
            IDLE_STREAM_READ_TIMEOUT,
            |mode: pb::ClashMode| on_mode(mode.mode),
        )
    }

    /// `SetClashMode` — switch mode; matched case-insensitively against the
    /// list. sing-box persists it in `cache_file` and clears its DNS cache.
    /// An unknown mode, or the current one, is silently ignored (still `Ok`,
    /// and nothing is pushed) — confirm through `stream_clash_mode`.
    pub fn set_clash_mode(&self, mode: &str) -> Result<(), ApiError> {
        let request = pb::ClashMode {
            mode: mode.to_string(),
        };
        self.unary("SetClashMode", &request)
    }
}

/// `GetClashModeStatus` result.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClashModeStatus {
    /// Selectable modes, in sing-box's display order.
    pub modes: Vec<String>,
    pub current: String,
}

impl ClashModeStatus {
    /// Whether there is anything to switch between. A config without
    /// `clash_mode` rules yields just `["Rule"]` — no switcher then.
    pub fn is_switchable(&self) -> bool {
        is_switchable(&self.modes)
    }

    fn from_proto(status: pb::ClashModeStatus) -> Self {
        Self {
            modes: status.mode_list,
            current: status.current_mode,
        }
    }
}

/// Whether a mode list offers a choice (two or more modes).
pub fn is_switchable(modes: &[String]) -> bool {
    modes.len() >= 2
}

/// Position of `mode` in `modes`, matched the way `SetClashMode` matches:
/// exact first, then case-insensitively. `None` for an unknown or empty mode
/// (`SubscribeClashMode` sends an empty one while the box isn't started).
pub fn mode_index(modes: &[String], mode: &str) -> Option<usize> {
    if mode.is_empty() {
        return None;
    }
    let folded = mode.to_lowercase();
    modes
        .iter()
        .position(|m| m == mode)
        .or_else(|| modes.iter().position(|m| m.to_lowercase() == folded))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modes(list: &[&str]) -> Vec<String> {
        list.iter().map(|m| m.to_string()).collect()
    }

    #[test]
    fn switchable_needs_two_modes() {
        assert!(!is_switchable(&[]));
        assert!(!is_switchable(&modes(&["Rule"])));
        assert!(is_switchable(&modes(&["Rule", "Global"])));
        let status = ClashModeStatus {
            modes: modes(&["Rule"]),
            current: "Rule".into(),
        };
        assert!(!status.is_switchable());
    }

    #[test]
    fn mode_index_matches_like_set_clash_mode() {
        let list = modes(&["Proxy", "Rule", "Global", "Direct"]);
        assert_eq!(mode_index(&list, "Global"), Some(2));
        assert_eq!(mode_index(&list, "global"), Some(2));
        assert_eq!(mode_index(&list, "DIRECT"), Some(3));
        assert_eq!(mode_index(&list, "Unknown"), None);
        assert_eq!(mode_index(&list, ""), None);
        // Exact spelling wins over an earlier case-insensitive match.
        let mixed = modes(&["global", "Global"]);
        assert_eq!(mode_index(&mixed, "Global"), Some(1));
    }

    #[test]
    fn clash_mode_status_keeps_list_order_and_current() {
        let status = ClashModeStatus::from_proto(pb::ClashModeStatus {
            mode_list: vec!["Rule".into(), "Global".into(), "Direct".into()],
            current_mode: "Global".into(),
        });
        assert_eq!(status.modes, vec!["Rule", "Global", "Direct"]);
        assert_eq!(status.current, "Global");
    }
}
