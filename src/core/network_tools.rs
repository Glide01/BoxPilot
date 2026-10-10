//! Pure presentation logic for the Tools page: folding the streamed progress
//! of a network quality test or a STUN test into what the page shows, plus
//! the formatting and NAT wording. No gpui dependency — the `NetworkTools`
//! entity (`state/network_tools.rs`) owns the threads and calls into here.

use crate::core::singbox_api::{
    Accuracy, ApiError, NatFiltering, NatMapping, NetworkQualityPhase, NetworkQualityProgress,
    OutboundItem, StunPhase, StunProgress,
};
use crate::i18n::s;

/// Max-runtime choices offered for a network quality test, in seconds.
/// 20 is sing-box's own default.
pub const MAX_RUNTIME_CHOICES: [u32; 4] = [10, 20, 30, 60];
pub const DEFAULT_MAX_RUNTIME: u32 = 20;
/// What sing-box tests against when the STUN server field is empty.
pub const DEFAULT_STUN_SERVER: &str = "stun.voipgate.com:3478";

/// Where a test run stands. A run starts `Running` and ends exactly once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    Done,
    /// The test or the call failed; the message is for display.
    Failed(String),
    /// The user cancelled, or sing-box stopped under the test.
    Cancelled,
}

impl RunStatus {
    pub fn is_running(&self) -> bool {
        matches!(self, RunStatus::Running)
    }
}

/// What to show when a test call fails before its final message. A read
/// timeout means sing-box went silent mid-test (the diagnostic streams report
/// at least every few seconds while working), not an idle stream to
/// re-subscribe.
pub fn test_error_message(error: &ApiError) -> String {
    if error.is_timeout() {
        s().tools.progress_timeout.to_string()
    } else {
        error.to_string()
    }
}

/// Shown when a test stream ends without its final message and without an
/// error — sing-box closed it early.
pub fn ended_without_result() -> String {
    s().tools.ended_without_result.to_string()
}

/// Format a capacity in bits/sec the way sing-box's own CLI does
/// (`networkquality.FormatBitrate`): decimal units, one decimal place.
pub fn format_bitrate(bits_per_sec: u64) -> String {
    let bps = bits_per_sec as f64;
    if bits_per_sec >= 1_000_000_000 {
        format!("{:.1} Gbps", bps / 1_000_000_000.0)
    } else if bits_per_sec >= 1_000_000 {
        format!("{:.1} Mbps", bps / 1_000_000.0)
    } else if bits_per_sec >= 1_000 {
        format!("{:.1} Kbps", bps / 1_000.0)
    } else {
        format!("{} bps", bits_per_sec)
    }
}

/// sing-box's own accuracy wording.
pub fn accuracy_label(accuracy: Accuracy) -> &'static str {
    let t = &s().tools;
    match accuracy {
        Accuracy::Low => t.accuracy_low,
        Accuracy::Medium => t.accuracy_medium,
        Accuracy::High => t.accuracy_high,
    }
}

/// Placeholder for a number not measured yet.
pub const NOT_MEASURED: &str = "—";

// ---------------------------------------------------------------------------
// Outbound picker
// ---------------------------------------------------------------------------

/// One entry of the outbound picker. `tag` empty = sing-box's default
/// outbound (`route.final`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboundChoice {
    pub tag: String,
    /// Protocol type for the secondary label; empty for the default entry.
    pub outbound_type: String,
}

impl OutboundChoice {
    pub fn default_outbound() -> Self {
        Self {
            tag: String::new(),
            outbound_type: String::new(),
        }
    }

    pub fn label(&self) -> &str {
        if self.tag.is_empty() {
            s().tools.default_outbound
        } else {
            &self.tag
        }
    }
}

/// The picker list: "Default outbound" first, then every outbound and
/// endpoint in sing-box's order, minus the ones a test can't dial through
/// (`block` rejects everything; `dns` only answers DNS).
pub fn outbound_choices(outbounds: &[OutboundItem]) -> Vec<OutboundChoice> {
    std::iter::once(OutboundChoice::default_outbound())
        .chain(
            outbounds
                .iter()
                .filter(|o| !matches!(o.outbound_type.as_str(), "block" | "dns"))
                .map(|o| OutboundChoice {
                    tag: o.tag.clone(),
                    outbound_type: o.outbound_type.clone(),
                }),
        )
        .collect()
}

// ---------------------------------------------------------------------------
// Network quality
// ---------------------------------------------------------------------------

/// A network quality run as the page shows it, folded from the progress
/// stream with [`QualityRun::apply`].
#[derive(Clone, Debug, PartialEq)]
pub struct QualityRun {
    pub status: RunStatus,
    /// Serial mode reports one direction at a time; parallel mode
    /// interleaves both, so the phase label differs.
    pub serial: bool,
    /// The measurement budget the run was started with, seconds.
    pub max_runtime_secs: u32,
    /// Latest progress worth showing. `None` until the first message —
    /// sing-box is still fetching the test config.
    pub latest: Option<NetworkQualityProgress>,
}

impl QualityRun {
    pub fn new(serial: bool, max_runtime_secs: u32) -> Self {
        Self {
            status: RunStatus::Running,
            serial,
            max_runtime_secs,
            latest: None,
        }
    }

    /// Fold one stream message. A failed test's final message carries only
    /// the error (every number zeroed), so the last real numbers are kept
    /// for context. Messages after the run ended are ignored.
    pub fn apply(&mut self, progress: NetworkQualityProgress) {
        if !self.status.is_running() {
            return;
        }
        if let Some(error) = progress.error.clone() {
            self.status = RunStatus::Failed(error);
            return;
        }
        if progress.is_final {
            self.status = RunStatus::Done;
        }
        // Idle latency is measured once; keep it if a later message omits it.
        let idle_latency_ms = match (&self.latest, progress.idle_latency_ms) {
            (Some(prev), 0) => prev.idle_latency_ms,
            (_, ms) => ms,
        };
        self.latest = Some(NetworkQualityProgress {
            idle_latency_ms,
            ..progress
        });
    }

    /// The call itself failed (unknown outbound, API unreachable, …), or the
    /// stream ended without a final message.
    pub fn fail(&mut self, message: String) {
        if self.status.is_running() {
            self.status = RunStatus::Failed(message);
        }
    }

    pub fn cancel(&mut self) {
        if self.status.is_running() {
            self.status = RunStatus::Cancelled;
        }
    }

    /// One line describing where the run stands.
    pub fn status_label(&self) -> String {
        let t = &s().tools;
        match &self.status {
            RunStatus::Done => t.done,
            RunStatus::Failed(_) => t.failed,
            RunStatus::Cancelled => t.cancelled,
            RunStatus::Running => {
                let Some(latest) = &self.latest else {
                    return t.fetching_config.to_string();
                };
                match latest.phase {
                    NetworkQualityPhase::Idle => t.measuring_idle,
                    NetworkQualityPhase::Download | NetworkQualityPhase::Upload if !self.serial => {
                        t.measuring_both
                    }
                    NetworkQualityPhase::Download => t.measuring_download,
                    NetworkQualityPhase::Upload => t.measuring_upload,
                    NetworkQualityPhase::Done => t.finishing,
                    NetworkQualityPhase::Unknown(_) => t.measuring,
                }
            }
        }
        .to_string()
    }

    /// Measurement progress 0–100 while measuring (elapsed against the
    /// budget; sing-box may finish early once results are stable). `None`
    /// before measuring starts, i.e. while the bar should be indeterminate.
    pub fn percent(&self) -> Option<f32> {
        let latest = self.latest.as_ref()?;
        match self.status {
            RunStatus::Done => return Some(100.0),
            RunStatus::Running => {}
            _ => return None,
        }
        match latest.phase {
            NetworkQualityPhase::Download | NetworkQualityPhase::Upload => {
                let budget_ms = u64::from(self.max_runtime_secs.max(1)) * 1000;
                Some((latest.elapsed_ms as f32 / budget_ms as f32 * 100.0).clamp(0.0, 100.0))
            }
            NetworkQualityPhase::Done => Some(100.0),
            _ => None,
        }
    }

    /// The numbers to show, `NOT_MEASURED` where there is nothing yet.
    pub fn metrics(&self) -> QualityMetrics {
        let Some(p) = &self.latest else {
            return QualityMetrics::empty();
        };
        let finished = p.is_final;
        let number = |value: u64, format: fn(u64) -> String| {
            if value == 0 && !finished {
                NOT_MEASURED.to_string()
            } else {
                format(value)
            }
        };
        // Accuracy comes only with the final result.
        let accuracy = |a: Accuracy| finished.then(|| accuracy_label(a));
        QualityMetrics {
            download: number(p.download_capacity, format_bitrate),
            upload: number(p.upload_capacity, format_bitrate),
            download_rpm: number(p.download_rpm.into(), format_rpm),
            upload_rpm: number(p.upload_rpm.into(), format_rpm),
            idle_latency: number(p.idle_latency_ms.into(), format_ms),
            download_accuracy: accuracy(p.download_capacity_accuracy),
            upload_accuracy: accuracy(p.upload_capacity_accuracy),
            download_rpm_accuracy: accuracy(p.download_rpm_accuracy),
            upload_rpm_accuracy: accuracy(p.upload_rpm_accuracy),
        }
    }
}

/// Display strings for a network quality run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QualityMetrics {
    pub download: String,
    pub upload: String,
    pub download_rpm: String,
    pub upload_rpm: String,
    pub idle_latency: String,
    pub download_accuracy: Option<&'static str>,
    pub upload_accuracy: Option<&'static str>,
    pub download_rpm_accuracy: Option<&'static str>,
    pub upload_rpm_accuracy: Option<&'static str>,
}

impl QualityMetrics {
    fn empty() -> Self {
        Self {
            download: NOT_MEASURED.to_string(),
            upload: NOT_MEASURED.to_string(),
            download_rpm: NOT_MEASURED.to_string(),
            upload_rpm: NOT_MEASURED.to_string(),
            idle_latency: NOT_MEASURED.to_string(),
            download_accuracy: None,
            upload_accuracy: None,
            download_rpm_accuracy: None,
            upload_rpm_accuracy: None,
        }
    }
}

fn format_rpm(rpm: u64) -> String {
    format!("{} RPM", rpm)
}

fn format_ms(ms: u64) -> String {
    format!("{} ms", ms)
}

// ---------------------------------------------------------------------------
// STUN
// ---------------------------------------------------------------------------

/// A STUN run as the page shows it, folded with [`StunRun::apply`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StunRun {
    pub status: RunStatus,
    pub phase: Option<StunPhase>,
    /// Empty until the binding request is answered.
    pub external_addr: String,
    pub latency_ms: u32,
    pub mapping: NatMapping,
    pub filtering: NatFiltering,
    /// From the final message: whether the server could tell the NAT type
    /// at all (RFC 5780 `OTHER-ADDRESS`). `None` until then.
    pub nat_type_supported: Option<bool>,
}

impl StunRun {
    pub fn new() -> Self {
        Self {
            status: RunStatus::Running,
            phase: None,
            external_addr: String::new(),
            latency_ms: 0,
            mapping: NatMapping::Unknown,
            filtering: NatFiltering::Unknown,
            nat_type_supported: None,
        }
    }

    /// Fold one stream message. A failure's final message carries only the
    /// error, so what was learned before it (e.g. the external address) is
    /// kept. Messages after the run ended are ignored.
    pub fn apply(&mut self, progress: StunProgress) {
        if !self.status.is_running() {
            return;
        }
        if let Some(error) = progress.error {
            self.status = RunStatus::Failed(error);
            return;
        }
        self.phase = Some(progress.phase);
        if !progress.external_addr.is_empty() {
            self.external_addr = progress.external_addr;
            self.latency_ms = progress.latency_ms;
        }
        if progress.nat_mapping != NatMapping::Unknown {
            self.mapping = progress.nat_mapping;
        }
        if progress.nat_filtering != NatFiltering::Unknown {
            self.filtering = progress.nat_filtering;
        }
        if progress.is_final {
            self.nat_type_supported = Some(progress.nat_type_supported);
            self.status = RunStatus::Done;
        }
    }

    pub fn fail(&mut self, message: String) {
        if self.status.is_running() {
            self.status = RunStatus::Failed(message);
        }
    }

    pub fn cancel(&mut self) {
        if self.status.is_running() {
            self.status = RunStatus::Cancelled;
        }
    }

    pub fn status_label(&self) -> &'static str {
        let t = &s().tools;
        match &self.status {
            RunStatus::Done => t.done,
            RunStatus::Failed(_) => t.failed,
            RunStatus::Cancelled => t.cancelled,
            RunStatus::Running => match self.phase {
                None | Some(StunPhase::Binding) if self.external_addr.is_empty() => t.stun_binding,
                None | Some(StunPhase::Binding) => t.stun_binding_answered,
                Some(StunPhase::NatMapping) => t.stun_mapping,
                Some(StunPhase::NatFiltering) => t.stun_filtering,
                Some(StunPhase::Done) => t.finishing,
                Some(StunPhase::Unknown(_)) => t.stun_testing,
            },
        }
    }

    /// External address, or `NOT_MEASURED`.
    pub fn external_addr_label(&self) -> String {
        if self.external_addr.is_empty() {
            NOT_MEASURED.to_string()
        } else {
            self.external_addr.clone()
        }
    }

    pub fn latency_label(&self) -> String {
        if self.external_addr.is_empty() {
            NOT_MEASURED.to_string()
        } else {
            format_ms(self.latency_ms.into())
        }
    }

    /// Mapping/filtering label: sing-box's RFC 4787 wording, `NOT_MEASURED`
    /// while not determined (yet).
    pub fn mapping_label(&self) -> &'static str {
        let t = &s().tools;
        match self.mapping {
            NatMapping::Unknown => NOT_MEASURED,
            NatMapping::EndpointIndependent => t.nat_endpoint_independent,
            NatMapping::AddressDependent => t.nat_address_dependent,
            NatMapping::AddressAndPortDependent => t.nat_address_port_dependent,
        }
    }

    pub fn filtering_label(&self) -> &'static str {
        let t = &s().tools;
        match self.filtering {
            NatFiltering::Unknown => NOT_MEASURED,
            NatFiltering::EndpointIndependent => t.nat_endpoint_independent,
            NatFiltering::AddressDependent => t.nat_address_dependent,
            NatFiltering::AddressAndPortDependent => t.nat_address_port_dependent,
        }
    }

    /// The human summary, once the run is done and the server supported
    /// NAT type detection.
    pub fn summary(&self) -> Option<NatSummary> {
        if self.status != RunStatus::Done || self.nat_type_supported != Some(true) {
            return None;
        }
        nat_summary(self.mapping, self.filtering)
    }
}

impl Default for StunRun {
    fn default() -> Self {
        Self::new()
    }
}

/// Plain-language reading of a NAT's mapping + filtering behaviour.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NatSummary {
    /// The classic RFC 3489 cone/symmetric name, only where the combination
    /// matches one exactly.
    pub classic: Option<&'static str>,
    pub explanation: &'static str,
}

/// Summarise RFC 4787 behaviour. The classic names ("Full cone (NAT1)" …)
/// are given only for the combinations they actually describe: the three
/// cone types are endpoint-independent mapping with each filtering
/// behaviour, and symmetric is address-and-port-dependent mapping and
/// filtering. Other combinations get an explanation without a label.
pub fn nat_summary(mapping: NatMapping, filtering: NatFiltering) -> Option<NatSummary> {
    use NatFiltering as F;
    use NatMapping as M;
    let t = &s().tools;
    let (classic, explanation) = match (mapping, filtering) {
        (M::Unknown, _) => return None,
        (M::EndpointIndependent, F::EndpointIndependent) => {
            (Some(t.nat_full_cone), t.nat_full_cone_hint)
        }
        (M::EndpointIndependent, F::AddressDependent) => {
            (Some(t.nat_restricted_cone), t.nat_restricted_cone_hint)
        }
        (M::EndpointIndependent, F::AddressAndPortDependent) => (
            Some(t.nat_port_restricted_cone),
            t.nat_port_restricted_cone_hint,
        ),
        (M::EndpointIndependent, F::Unknown) => (None, t.nat_independent_unknown_hint),
        (M::AddressAndPortDependent, F::AddressAndPortDependent) => {
            (Some(t.nat_symmetric), t.nat_symmetric_hint)
        }
        (M::AddressDependent | M::AddressAndPortDependent, _) => (None, t.nat_dependent_hint),
    };
    Some(NatSummary {
        classic,
        explanation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nq(phase: NetworkQualityPhase) -> NetworkQualityProgress {
        NetworkQualityProgress {
            phase,
            download_capacity: 0,
            upload_capacity: 0,
            download_rpm: 0,
            upload_rpm: 0,
            idle_latency_ms: 0,
            elapsed_ms: 0,
            is_final: false,
            error: None,
            download_capacity_accuracy: Accuracy::Low,
            upload_capacity_accuracy: Accuracy::Low,
            download_rpm_accuracy: Accuracy::Low,
            upload_rpm_accuracy: Accuracy::Low,
        }
    }

    fn stun(phase: StunPhase) -> StunProgress {
        StunProgress {
            phase,
            external_addr: String::new(),
            latency_ms: 0,
            nat_mapping: NatMapping::Unknown,
            nat_filtering: NatFiltering::Unknown,
            is_final: false,
            error: None,
            nat_type_supported: false,
        }
    }

    #[test]
    fn bitrate_uses_decimal_units_like_sing_box() {
        assert_eq!(format_bitrate(0), "0 bps");
        assert_eq!(format_bitrate(999), "999 bps");
        assert_eq!(format_bitrate(1_000), "1.0 Kbps");
        assert_eq!(format_bitrate(1_500_000), "1.5 Mbps");
        assert_eq!(format_bitrate(94_340_000), "94.3 Mbps");
        assert_eq!(format_bitrate(999_999_999), "1000.0 Mbps");
        assert_eq!(format_bitrate(2_500_000_000), "2.5 Gbps");
    }

    #[test]
    fn test_errors_read_as_test_failures() {
        assert_eq!(
            test_error_message(&ApiError::TimedOut),
            "Timed out waiting for progress from sing-box"
        );
        assert_eq!(
            test_error_message(&ApiError::Status {
                code: 5,
                message: "outbound not found: x".into()
            }),
            "sing-box API: outbound not found: x"
        );
    }

    #[test]
    fn outbound_choices_lead_with_default_and_skip_undialable() {
        let item = |tag: &str, ty: &str| OutboundItem {
            tag: tag.into(),
            outbound_type: ty.into(),
            url_test: None,
        };
        let choices = outbound_choices(&[
            item("节点选择", "selector"),
            item("block", "block"),
            item("direct", "direct"),
            item("dns-out", "dns"),
            item("wg", "wireguard"),
        ]);
        let tags: Vec<_> = choices.iter().map(|c| c.tag.as_str()).collect();
        assert_eq!(tags, vec!["", "节点选择", "direct", "wg"]);
        assert_eq!(choices[0].label(), "Default outbound");
        assert_eq!(choices[1].label(), "节点选择");
        assert_eq!(choices[3].outbound_type, "wireguard");
    }

    #[test]
    fn quality_run_walks_parallel_phases() {
        let mut run = QualityRun::new(false, 20);
        assert_eq!(run.status_label(), "Fetching test config…");
        assert_eq!(run.percent(), None);
        assert_eq!(run.metrics().download, NOT_MEASURED);

        run.apply(nq(NetworkQualityPhase::Idle));
        assert_eq!(run.status_label(), "Measuring idle latency…");
        run.apply(NetworkQualityProgress {
            idle_latency_ms: 42,
            ..nq(NetworkQualityPhase::Idle)
        });
        assert_eq!(run.metrics().idle_latency, "42 ms");

        run.apply(NetworkQualityProgress {
            download_capacity: 50_000_000,
            download_rpm: 800,
            elapsed_ms: 5_000,
            ..nq(NetworkQualityPhase::Download)
        });
        assert_eq!(run.status_label(), "Measuring download and upload…");
        assert_eq!(run.percent(), Some(25.0));
        let metrics = run.metrics();
        assert_eq!(metrics.download, "50.0 Mbps");
        assert_eq!(metrics.download_rpm, "800 RPM");
        assert_eq!(
            metrics.upload, NOT_MEASURED,
            "zero while measuring = not yet"
        );
        assert_eq!(metrics.idle_latency, "42 ms", "idle latency is sticky");
        assert_eq!(metrics.download_accuracy, None, "accuracy only when final");

        run.apply(NetworkQualityProgress {
            download_capacity: 50_000_000,
            upload_capacity: 10_000_000,
            elapsed_ms: 40_000,
            ..nq(NetworkQualityPhase::Upload)
        });
        assert_eq!(run.status_label(), "Measuring download and upload…");
        assert_eq!(run.percent(), Some(100.0), "clamped");

        run.apply(NetworkQualityProgress {
            download_capacity: 52_000_000,
            upload_capacity: 11_000_000,
            download_rpm: 900,
            upload_rpm: 0,
            idle_latency_ms: 42,
            is_final: true,
            download_capacity_accuracy: Accuracy::High,
            upload_rpm_accuracy: Accuracy::Medium,
            ..nq(NetworkQualityPhase::Done)
        });
        assert_eq!(run.status, RunStatus::Done);
        assert_eq!(run.status_label(), "Done");
        assert_eq!(run.percent(), Some(100.0));
        let metrics = run.metrics();
        assert_eq!(metrics.download, "52.0 Mbps");
        assert_eq!(metrics.upload_rpm, "0 RPM", "a final zero is a result");
        assert_eq!(metrics.download_accuracy, Some("High"));
        assert_eq!(metrics.upload_rpm_accuracy, Some("Medium"));
        assert_eq!(metrics.upload_accuracy, Some("Low"));
    }

    #[test]
    fn quality_run_labels_serial_phases() {
        let mut run = QualityRun::new(true, 10);
        run.apply(nq(NetworkQualityPhase::Download));
        assert_eq!(run.status_label(), "Measuring download…");
        run.apply(nq(NetworkQualityPhase::Upload));
        assert_eq!(run.status_label(), "Measuring upload…");
    }

    #[test]
    fn quality_failure_keeps_last_numbers() {
        let mut run = QualityRun::new(false, 20);
        run.apply(NetworkQualityProgress {
            idle_latency_ms: 30,
            ..nq(NetworkQualityPhase::Idle)
        });
        run.apply(NetworkQualityProgress {
            is_final: true,
            error: Some("measure download: EOF".into()),
            ..nq(NetworkQualityPhase::Idle)
        });
        assert_eq!(
            run.status,
            RunStatus::Failed("measure download: EOF".into())
        );
        assert_eq!(run.status_label(), "Failed");
        assert_eq!(run.metrics().idle_latency, "30 ms");
        assert_eq!(run.percent(), None);

        // Nothing changes an ended run.
        run.apply(nq(NetworkQualityPhase::Done));
        run.fail("late".into());
        run.cancel();
        assert_eq!(
            run.status,
            RunStatus::Failed("measure download: EOF".into())
        );
    }

    #[test]
    fn quality_call_failure_and_cancel() {
        let mut run = QualityRun::new(false, 20);
        run.fail("sing-box API: outbound not found: x".into());
        assert_eq!(
            run.status,
            RunStatus::Failed("sing-box API: outbound not found: x".into())
        );
        let mut run = QualityRun::new(false, 20);
        run.cancel();
        assert_eq!(run.status, RunStatus::Cancelled);
        assert_eq!(run.status_label(), "Cancelled");
    }

    #[test]
    fn stun_run_with_nat_detection() {
        let mut run = StunRun::new();
        assert_eq!(run.status_label(), "Sending binding request…");
        run.apply(stun(StunPhase::Binding));
        assert_eq!(run.external_addr_label(), NOT_MEASURED);
        run.apply(StunProgress {
            external_addr: "203.0.113.7:40000".into(),
            latency_ms: 31,
            ..stun(StunPhase::Binding)
        });
        assert_eq!(run.status_label(), "Binding answered…");
        assert_eq!(run.external_addr_label(), "203.0.113.7:40000");
        assert_eq!(run.latency_label(), "31 ms");
        run.apply(StunProgress {
            external_addr: "203.0.113.7:40000".into(),
            latency_ms: 31,
            ..stun(StunPhase::NatMapping)
        });
        assert_eq!(run.status_label(), "Detecting NAT mapping behavior…");
        assert_eq!(run.mapping_label(), NOT_MEASURED);
        run.apply(StunProgress {
            external_addr: "203.0.113.7:40000".into(),
            latency_ms: 31,
            nat_mapping: NatMapping::EndpointIndependent,
            ..stun(StunPhase::NatFiltering)
        });
        assert_eq!(run.mapping_label(), "Endpoint Independent");
        assert_eq!(run.summary(), None, "no summary while running");
        run.apply(StunProgress {
            external_addr: "203.0.113.7:40000".into(),
            latency_ms: 31,
            nat_mapping: NatMapping::EndpointIndependent,
            nat_filtering: NatFiltering::AddressAndPortDependent,
            is_final: true,
            nat_type_supported: true,
            ..stun(StunPhase::Done)
        });
        assert_eq!(run.status, RunStatus::Done);
        assert_eq!(run.filtering_label(), "Address and Port Dependent");
        assert_eq!(
            run.summary().and_then(|s| s.classic),
            Some("Port-restricted cone (NAT3)")
        );
    }

    #[test]
    fn stun_run_without_nat_detection() {
        let mut run = StunRun::new();
        run.apply(StunProgress {
            external_addr: "127.0.0.1:5000".into(),
            latency_ms: 0,
            ..stun(StunPhase::Done)
        });
        run.apply(StunProgress {
            external_addr: "127.0.0.1:5000".into(),
            is_final: true,
            nat_type_supported: false,
            ..stun(StunPhase::Done)
        });
        assert_eq!(run.status, RunStatus::Done);
        assert_eq!(run.nat_type_supported, Some(false));
        assert_eq!(run.latency_label(), "0 ms");
        assert_eq!(run.mapping_label(), NOT_MEASURED);
        assert_eq!(run.summary(), None);
    }

    #[test]
    fn stun_failure_keeps_external_address() {
        let mut run = StunRun::new();
        run.apply(StunProgress {
            external_addr: "198.51.100.1:1234".into(),
            latency_ms: 12,
            ..stun(StunPhase::Binding)
        });
        run.apply(StunProgress {
            is_final: true,
            error: Some("binding request: i/o timeout".into()),
            ..stun(StunPhase::Binding)
        });
        assert_eq!(
            run.status,
            RunStatus::Failed("binding request: i/o timeout".into())
        );
        assert_eq!(run.external_addr_label(), "198.51.100.1:1234");
        assert_eq!(run.summary(), None);
    }

    #[test]
    fn nat_summary_names_only_exact_classic_types() {
        use NatFiltering as F;
        use NatMapping as M;
        let classic = |m, f| nat_summary(m, f).and_then(|s| s.classic);
        assert_eq!(
            classic(M::EndpointIndependent, F::EndpointIndependent),
            Some("Full cone (NAT1)")
        );
        assert_eq!(
            classic(M::EndpointIndependent, F::AddressDependent),
            Some("Restricted cone (NAT2)")
        );
        assert_eq!(
            classic(M::EndpointIndependent, F::AddressAndPortDependent),
            Some("Port-restricted cone (NAT3)")
        );
        assert_eq!(
            classic(M::AddressAndPortDependent, F::AddressAndPortDependent),
            Some("Symmetric (NAT4)")
        );
        // No exact classic name: explained, but not labelled.
        for (m, f) in [
            (M::AddressDependent, F::AddressDependent),
            (M::AddressDependent, F::EndpointIndependent),
            (M::AddressAndPortDependent, F::EndpointIndependent),
            (M::EndpointIndependent, F::Unknown),
        ] {
            let summary = nat_summary(m, f).expect("explained");
            assert_eq!(summary.classic, None, "{:?} + {:?}", m, f);
            assert!(!summary.explanation.is_empty());
        }
        assert_eq!(nat_summary(M::Unknown, F::EndpointIndependent), None);
        assert_eq!(nat_summary(M::Unknown, F::Unknown), None);
    }
}
