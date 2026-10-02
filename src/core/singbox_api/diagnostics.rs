//! Network diagnostics run by sing-box through one of its outbounds:
//! `StartNetworkQualityTest` (Apple's networkQuality / RPM method) and
//! `StartSTUNTest` (NAT behaviour, RFC 5780). Both are server streams that
//! report progress and end on their own with one `is_final` message.

use super::transport::ApiError;
use super::{pb, SingBoxApi};
use std::time::Duration;

/// Per-read bound for the diagnostic streams. Progress arrives every 500ms
/// while measuring, but fetching the test config or a STUN round trip
/// through a slow outbound can be silent for a while first.
const DIAGNOSTIC_READ_TIMEOUT: Duration = Duration::from_secs(30);

impl SingBoxApi {
    /// Run a network quality test and stream its progress until the final
    /// message (`is_final`), which carries the result or `error` (a failed
    /// test still ends the stream with `Ok`). Default runtime is 20s;
    /// returning `false` from `on_progress` cancels the test.
    ///
    /// Sequence: `Idle` (fetching config), `Idle` with `idle_latency_ms`;
    /// then in parallel mode `Download` and `Upload` progress interleaved
    /// (each message carries both directions' latest numbers), in serial
    /// mode `Download` then `Upload`; then `Done` twice — once as progress
    /// and once as the final result (`is_final`, `elapsed_ms` 0).
    ///
    /// Errors before the test starts: `NOT_FOUND` for an unknown outbound.
    pub fn start_network_quality_test(
        &self,
        request: &NetworkQualityRequest,
        mut on_progress: impl FnMut(NetworkQualityProgress) -> bool,
    ) -> Result<(), ApiError> {
        self.stream(
            "StartNetworkQualityTest",
            &request.to_proto(),
            DIAGNOSTIC_READ_TIMEOUT,
            |progress: pb::NetworkQualityTestProgress| {
                on_progress(NetworkQualityProgress::from_proto(progress))
            },
        )
    }

    /// Run a STUN NAT test (UDP through the outbound) and stream progress
    /// until the final message (`is_final`), which carries the result or
    /// `error` (still `Ok` for the call). Returning `false` cancels.
    ///
    /// Sequence: `Binding`; `Binding` with `external_addr` and `latency_ms`;
    /// if the server supports RFC 5780 (`OTHER-ADDRESS`): `NatMapping`,
    /// `NatMapping` with the mapping, `NatFiltering`, then `Done` with both;
    /// otherwise straight to `Done` with `nat_type_supported` false. Then
    /// the final message repeats the result with `is_final` set.
    ///
    /// Errors before the test starts: `NOT_FOUND` for an unknown outbound.
    pub fn start_stun_test(
        &self,
        request: &StunRequest,
        mut on_progress: impl FnMut(StunProgress) -> bool,
    ) -> Result<(), ApiError> {
        let request = pb::StunTestRequest {
            server: request.server.clone(),
            outbound_tag: request.outbound_tag.clone(),
        };
        self.stream(
            "StartSTUNTest",
            &request,
            DIAGNOSTIC_READ_TIMEOUT,
            |progress: pb::StunTestProgress| on_progress(StunProgress::from_proto(progress)),
        )
    }
}

// ---------------------------------------------------------------------------
// Network quality
// ---------------------------------------------------------------------------

/// `StartNetworkQualityTest` parameters; `Default` runs sing-box's defaults.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NetworkQualityRequest {
    /// networkQuality config URL; empty = Apple's
    /// `https://mensura.cdn-apple.com/api/v1/gm/config`.
    pub config_url: String,
    /// Outbound to test through; empty = the default outbound
    /// (`route.final`).
    pub outbound_tag: String,
    /// Measure download, then upload, instead of both at once.
    pub serial: bool,
    /// Measurement budget in seconds; 0 = 20s.
    pub max_runtime_seconds: u32,
    /// Measure over HTTP/3 (QUIC).
    pub http3: bool,
}

impl NetworkQualityRequest {
    fn to_proto(&self) -> pb::NetworkQualityTestRequest {
        pb::NetworkQualityTestRequest {
            config_url: self.config_url.clone(),
            outbound_tag: self.outbound_tag.clone(),
            serial: self.serial,
            max_runtime_seconds: i32::try_from(self.max_runtime_seconds).unwrap_or(i32::MAX),
            http3: self.http3,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkQualityPhase {
    /// Fetching the config and measuring idle latency.
    Idle,
    Download,
    Upload,
    Done,
    Unknown(i32),
}

impl NetworkQualityPhase {
    fn from_proto(value: i32) -> Self {
        match value {
            0 => NetworkQualityPhase::Idle,
            1 => NetworkQualityPhase::Download,
            2 => NetworkQualityPhase::Upload,
            3 => NetworkQualityPhase::Done,
            other => NetworkQualityPhase::Unknown(other),
        }
    }
}

/// How stable a measurement had become (sing-box's stability tracker).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Accuracy {
    Low,
    Medium,
    High,
}

impl Accuracy {
    fn from_proto(value: i32) -> Self {
        match value {
            2 => Accuracy::High,
            1 => Accuracy::Medium,
            _ => Accuracy::Low,
        }
    }
}

/// One `StartNetworkQualityTest` message.
#[derive(Clone, Debug, PartialEq)]
pub struct NetworkQualityProgress {
    pub phase: NetworkQualityPhase,
    /// Throughput, bits/sec.
    pub download_capacity: u64,
    pub upload_capacity: u64,
    /// Responsiveness under load, round trips per minute.
    pub download_rpm: u32,
    pub upload_rpm: u32,
    pub idle_latency_ms: u32,
    /// Into the current stage: since the config fetch while `Idle`, since
    /// measuring started afterwards; 0 on the final message.
    pub elapsed_ms: u64,
    /// The last message of the stream.
    pub is_final: bool,
    /// Set (with `is_final`) when the test failed.
    pub error: Option<String>,
    pub download_capacity_accuracy: Accuracy,
    pub upload_capacity_accuracy: Accuracy,
    pub download_rpm_accuracy: Accuracy,
    pub upload_rpm_accuracy: Accuracy,
}

impl NetworkQualityProgress {
    fn from_proto(progress: pb::NetworkQualityTestProgress) -> Self {
        Self {
            phase: NetworkQualityPhase::from_proto(progress.phase),
            download_capacity: progress.download_capacity.max(0) as u64,
            upload_capacity: progress.upload_capacity.max(0) as u64,
            download_rpm: progress.download_rpm.max(0) as u32,
            upload_rpm: progress.upload_rpm.max(0) as u32,
            idle_latency_ms: progress.idle_latency_ms.max(0) as u32,
            elapsed_ms: progress.elapsed_ms.max(0) as u64,
            is_final: progress.is_final,
            error: non_empty(progress.error),
            download_capacity_accuracy: Accuracy::from_proto(progress.download_capacity_accuracy),
            upload_capacity_accuracy: Accuracy::from_proto(progress.upload_capacity_accuracy),
            download_rpm_accuracy: Accuracy::from_proto(progress.download_rpm_accuracy),
            upload_rpm_accuracy: Accuracy::from_proto(progress.upload_rpm_accuracy),
        }
    }
}

// ---------------------------------------------------------------------------
// STUN
// ---------------------------------------------------------------------------

/// `StartSTUNTest` parameters; `Default` runs sing-box's defaults.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StunRequest {
    /// `host[:port]`; empty = `stun.voipgate.com:3478`, port defaults to
    /// 3478.
    pub server: String,
    /// Outbound to test through; empty = the default outbound.
    pub outbound_tag: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StunPhase {
    Binding,
    NatMapping,
    NatFiltering,
    Done,
    Unknown(i32),
}

impl StunPhase {
    fn from_proto(value: i32) -> Self {
        match value {
            0 => StunPhase::Binding,
            1 => StunPhase::NatMapping,
            2 => StunPhase::NatFiltering,
            3 => StunPhase::Done,
            other => StunPhase::Unknown(other),
        }
    }
}

/// NAT mapping behaviour (RFC 4787 / 5780 §4.3): how the external address
/// is chosen for successive destinations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NatMapping {
    /// Not determined (yet, or the test could not tell).
    Unknown,
    /// Same external address for every destination ("full cone"-friendly).
    EndpointIndependent,
    AddressDependent,
    /// A new mapping per destination address and port (symmetric NAT).
    AddressAndPortDependent,
}

impl NatMapping {
    /// Upstream numbering skips 1 (reserved): 2, 3, 4.
    fn from_proto(value: i32) -> Self {
        match value {
            2 => NatMapping::EndpointIndependent,
            3 => NatMapping::AddressDependent,
            4 => NatMapping::AddressAndPortDependent,
            _ => NatMapping::Unknown,
        }
    }

    /// sing-box's English label.
    pub fn as_str(self) -> &'static str {
        match self {
            NatMapping::Unknown => "Unknown",
            NatMapping::EndpointIndependent => "Endpoint Independent",
            NatMapping::AddressDependent => "Address Dependent",
            NatMapping::AddressAndPortDependent => "Address and Port Dependent",
        }
    }
}

/// NAT filtering behaviour (RFC 4787 / 5780 §4.4): which remote endpoints
/// may send back through an existing mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NatFiltering {
    Unknown,
    /// Anyone may reach the mapping (full cone).
    EndpointIndependent,
    /// Only addresses already contacted (restricted cone).
    AddressDependent,
    /// Only exact address:port pairs already contacted (port-restricted).
    AddressAndPortDependent,
}

impl NatFiltering {
    fn from_proto(value: i32) -> Self {
        match value {
            1 => NatFiltering::EndpointIndependent,
            2 => NatFiltering::AddressDependent,
            3 => NatFiltering::AddressAndPortDependent,
            _ => NatFiltering::Unknown,
        }
    }

    /// sing-box's English label.
    pub fn as_str(self) -> &'static str {
        match self {
            NatFiltering::Unknown => "Unknown",
            NatFiltering::EndpointIndependent => "Endpoint Independent",
            NatFiltering::AddressDependent => "Address Dependent",
            NatFiltering::AddressAndPortDependent => "Address and Port Dependent",
        }
    }
}

/// One `StartSTUNTest` message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StunProgress {
    pub phase: StunPhase,
    /// Our address as the STUN server saw it (`ip:port`); empty until the
    /// binding answered.
    pub external_addr: String,
    /// Binding round trip.
    pub latency_ms: u32,
    pub nat_mapping: NatMapping,
    pub nat_filtering: NatFiltering,
    pub is_final: bool,
    /// Set (with `is_final`) when the test failed.
    pub error: Option<String>,
    /// Whether the server supports NAT type detection (RFC 5780); only
    /// meaningful on the final message.
    pub nat_type_supported: bool,
}

impl StunProgress {
    fn from_proto(progress: pb::StunTestProgress) -> Self {
        Self {
            phase: StunPhase::from_proto(progress.phase),
            external_addr: progress.external_addr,
            latency_ms: progress.latency_ms.max(0) as u32,
            nat_mapping: NatMapping::from_proto(progress.nat_mapping),
            nat_filtering: NatFiltering::from_proto(progress.nat_filtering),
            is_final: progress.is_final,
            error: non_empty(progress.error),
            nat_type_supported: progress.nat_type_supported,
        }
    }
}

fn non_empty(text: String) -> Option<String> {
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_quality_request_maps_to_proto() {
        let request = NetworkQualityRequest {
            config_url: "https://example.com/config".into(),
            outbound_tag: "节点选择".into(),
            serial: true,
            max_runtime_seconds: 10,
            http3: true,
        };
        assert_eq!(
            request.to_proto(),
            pb::NetworkQualityTestRequest {
                config_url: "https://example.com/config".into(),
                outbound_tag: "节点选择".into(),
                serial: true,
                max_runtime_seconds: 10,
                http3: true,
            }
        );
        let huge = NetworkQualityRequest {
            max_runtime_seconds: u32::MAX,
            ..Default::default()
        };
        assert_eq!(huge.to_proto().max_runtime_seconds, i32::MAX);
    }

    #[test]
    fn network_quality_progress_maps_every_field() {
        let progress = NetworkQualityProgress::from_proto(pb::NetworkQualityTestProgress {
            phase: 3,
            download_capacity: 100_000_000,
            upload_capacity: 20_000_000,
            download_rpm: 900,
            upload_rpm: 600,
            idle_latency_ms: 25,
            elapsed_ms: 0,
            is_final: true,
            error: String::new(),
            download_capacity_accuracy: 2,
            upload_capacity_accuracy: 1,
            download_rpm_accuracy: 0,
            upload_rpm_accuracy: 2,
        });
        assert_eq!(
            progress,
            NetworkQualityProgress {
                phase: NetworkQualityPhase::Done,
                download_capacity: 100_000_000,
                upload_capacity: 20_000_000,
                download_rpm: 900,
                upload_rpm: 600,
                idle_latency_ms: 25,
                elapsed_ms: 0,
                is_final: true,
                error: None,
                download_capacity_accuracy: Accuracy::High,
                upload_capacity_accuracy: Accuracy::Medium,
                download_rpm_accuracy: Accuracy::Low,
                upload_rpm_accuracy: Accuracy::High,
            }
        );
        let phases: Vec<_> = (0..=4).map(NetworkQualityPhase::from_proto).collect();
        assert_eq!(
            phases,
            vec![
                NetworkQualityPhase::Idle,
                NetworkQualityPhase::Download,
                NetworkQualityPhase::Upload,
                NetworkQualityPhase::Done,
                NetworkQualityPhase::Unknown(4),
            ]
        );
    }

    #[test]
    fn network_quality_failure_carries_error() {
        let progress = NetworkQualityProgress::from_proto(pb::NetworkQualityTestProgress {
            is_final: true,
            error: "fetch config: EOF".into(),
            ..Default::default()
        });
        assert!(progress.is_final);
        assert_eq!(progress.error.as_deref(), Some("fetch config: EOF"));
        assert_eq!(progress.phase, NetworkQualityPhase::Idle);
    }

    #[test]
    fn stun_progress_maps_every_field() {
        let progress = StunProgress::from_proto(pb::StunTestProgress {
            phase: 3,
            external_addr: "203.0.113.7:40000".into(),
            latency_ms: 31,
            nat_mapping: 2,
            nat_filtering: 3,
            is_final: true,
            error: String::new(),
            nat_type_supported: true,
        });
        assert_eq!(
            progress,
            StunProgress {
                phase: StunPhase::Done,
                external_addr: "203.0.113.7:40000".into(),
                latency_ms: 31,
                nat_mapping: NatMapping::EndpointIndependent,
                nat_filtering: NatFiltering::AddressAndPortDependent,
                is_final: true,
                error: None,
                nat_type_supported: true,
            }
        );
        let phases: Vec<_> = (0..=4).map(StunPhase::from_proto).collect();
        assert_eq!(
            phases,
            vec![
                StunPhase::Binding,
                StunPhase::NatMapping,
                StunPhase::NatFiltering,
                StunPhase::Done,
                StunPhase::Unknown(4),
            ]
        );
    }

    /// Mapping values skip 1 upstream (reserved); filtering values don't.
    #[test]
    fn nat_enums_follow_upstream_numbering() {
        let mappings: Vec<_> = (0..=5).map(NatMapping::from_proto).collect();
        assert_eq!(
            mappings,
            vec![
                NatMapping::Unknown,
                NatMapping::Unknown,
                NatMapping::EndpointIndependent,
                NatMapping::AddressDependent,
                NatMapping::AddressAndPortDependent,
                NatMapping::Unknown,
            ]
        );
        let filterings: Vec<_> = (0..=4).map(NatFiltering::from_proto).collect();
        assert_eq!(
            filterings,
            vec![
                NatFiltering::Unknown,
                NatFiltering::EndpointIndependent,
                NatFiltering::AddressDependent,
                NatFiltering::AddressAndPortDependent,
                NatFiltering::Unknown,
            ]
        );
        assert_eq!(
            NatMapping::AddressAndPortDependent.as_str(),
            "Address and Port Dependent"
        );
        assert_eq!(
            NatFiltering::EndpointIndependent.as_str(),
            "Endpoint Independent"
        );
    }
}
