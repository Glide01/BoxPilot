//! Facts about the running sing-box itself: `GetVersion`, `GetStartedAt`,
//! `GetDeprecatedWarnings`, `SubscribeServiceStatus`.

use super::transport::{ApiError, IDLE_STREAM_READ_TIMEOUT};
use super::{pb, SingBoxApi};

impl SingBoxApi {
    /// `GetVersion` — the running binary's version (the same string
    /// `sing-box version` prints) and the API revision (4 in sing-box 1.14).
    pub fn get_version(&self) -> Result<SingBoxVersion, ApiError> {
        self.unary("GetVersion", &())
            .map(|version: pb::Version| SingBoxVersion {
                version: version.version,
                api_version: version.api_version,
            })
    }

    /// `GetStartedAt` — when the `api` service came up, as unix milliseconds:
    /// the end of sing-box's own startup, so effectively the process start.
    /// `None` if sing-box reports no start time.
    pub fn get_started_at(&self) -> Result<Option<i64>, ApiError> {
        self.unary("GetStartedAt", &())
            .map(|started: pb::StartedAt| started_at_from_millis(started.started_at))
    }

    /// `GetDeprecatedWarnings`. Under the `api` service this is always empty:
    /// sing-box reports deprecated options on its log (stderr) instead, and
    /// only the sing-box for Desktop daemon collects them for this RPC.
    /// Kept for completeness and future sing-box versions.
    pub fn get_deprecated_warnings(&self) -> Result<Vec<DeprecatedWarning>, ApiError> {
        self.unary("GetDeprecatedWarnings", &())
            .map(|warnings: pb::DeprecatedWarnings| {
                warnings
                    .warnings
                    .into_iter()
                    .map(DeprecatedWarning::from_proto)
                    .collect()
            })
    }

    /// Stream `SubscribeServiceStatus`: the current status on subscribe,
    /// then every change. Under the `api` service the box is already running
    /// when the API comes up and the API dies with it, so this sends
    /// `Started` once and then stays silent (`TimedOut` after
    /// `IDLE_STREAM_READ_TIMEOUT`; re-subscribe) until sing-box exits.
    pub fn stream_service_status(
        &self,
        mut on_status: impl FnMut(ServiceStatus) -> bool,
    ) -> Result<(), ApiError> {
        self.stream(
            "SubscribeServiceStatus",
            &(),
            IDLE_STREAM_READ_TIMEOUT,
            |status: pb::ServiceStatus| on_status(ServiceStatus::from_proto(status)),
        )
    }
}

/// `GetVersion` result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SingBoxVersion {
    /// e.g. `1.14.2`.
    pub version: String,
    /// Revision of the `StartedService` API (4 in sing-box 1.14).
    pub api_version: i32,
}

/// Go's zero `time.Time` reports as a large negative millisecond count;
/// anything not after the epoch means "not started".
fn started_at_from_millis(millis: i64) -> Option<i64> {
    (millis > 0).then_some(millis)
}

/// One deprecated option sing-box noticed in the running config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeprecatedWarning {
    /// Localized one-line summary.
    pub message: String,
    /// The option is removed in the next release (`scheduled_version`).
    pub impending: bool,
    pub migration_link: String,
    pub description: String,
    pub deprecated_version: String,
    pub scheduled_version: String,
}

impl DeprecatedWarning {
    fn from_proto(warning: pb::DeprecatedWarning) -> Self {
        Self {
            message: warning.message,
            impending: warning.impending,
            migration_link: warning.migration_link,
            description: warning.description,
            deprecated_version: warning.deprecated_version,
            scheduled_version: warning.scheduled_version,
        }
    }
}

/// Lifecycle of the box behind the API (`ServiceStatus.Type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceState {
    Idle,
    Starting,
    Started,
    Stopping,
    /// Failed to start; `ServiceStatus::error_message` says why.
    Fatal,
    /// A value newer than this client.
    Unknown(i32),
}

impl ServiceState {
    fn from_proto(value: i32) -> Self {
        match value {
            0 => ServiceState::Idle,
            1 => ServiceState::Starting,
            2 => ServiceState::Started,
            3 => ServiceState::Stopping,
            4 => ServiceState::Fatal,
            other => ServiceState::Unknown(other),
        }
    }
}

/// One `SubscribeServiceStatus` message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceStatus {
    pub state: ServiceState,
    /// Set only with `ServiceState::Fatal`.
    pub error_message: String,
}

impl ServiceStatus {
    fn from_proto(status: pb::ServiceStatus) -> Self {
        Self {
            state: ServiceState::from_proto(status.status),
            error_message: status.error_message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_states_map_upstream_values() {
        let states: Vec<ServiceState> = (0..=5).map(ServiceState::from_proto).collect();
        assert_eq!(
            states,
            vec![
                ServiceState::Idle,
                ServiceState::Starting,
                ServiceState::Started,
                ServiceState::Stopping,
                ServiceState::Fatal,
                ServiceState::Unknown(5),
            ]
        );
        let fatal = ServiceStatus::from_proto(pb::ServiceStatus {
            status: 4,
            error_message: "bind: address in use".into(),
        });
        assert_eq!(fatal.state, ServiceState::Fatal);
        assert_eq!(fatal.error_message, "bind: address in use");
    }

    #[test]
    fn started_at_treats_go_zero_time_as_unset() {
        assert_eq!(
            started_at_from_millis(1_759_400_000_000),
            Some(1_759_400_000_000)
        );
        assert_eq!(started_at_from_millis(-62_135_596_800_000), None);
        assert_eq!(started_at_from_millis(0), None);
    }

    #[test]
    fn deprecated_warning_maps_every_field() {
        let warning = DeprecatedWarning::from_proto(pb::DeprecatedWarning {
            message: "legacy DNS servers is deprecated".into(),
            impending: true,
            migration_link: "https://sing-box.sagernet.org/migration/".into(),
            description: "legacy DNS servers".into(),
            deprecated_version: "1.12.0".into(),
            scheduled_version: "1.14.0".into(),
        });
        assert_eq!(
            warning,
            DeprecatedWarning {
                message: "legacy DNS servers is deprecated".into(),
                impending: true,
                migration_link: "https://sing-box.sagernet.org/migration/".into(),
                description: "legacy DNS servers".into(),
                deprecated_version: "1.12.0".into(),
                scheduled_version: "1.14.0".into(),
            }
        );
    }
}
