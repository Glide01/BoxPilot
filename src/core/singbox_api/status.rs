//! Runtime status and traffic: `SubscribeStatus`.

use super::transport::ApiError;
use super::{pb, SingBoxApi};
use std::time::Duration;

/// `SubscribeStatus` interval. The `uplink`/`downlink` fields are byte deltas
/// per interval, so one second makes them bytes/sec directly.
pub const STATUS_INTERVAL: Duration = Duration::from_secs(1);
/// Per-read bound on `SubscribeStatus`. sing-box emits one message per
/// `STATUS_INTERVAL` even while idle, so this never trips in normal operation;
/// it only lets the reader notice a wedged connection.
const STATUS_READ_TIMEOUT: Duration = Duration::from_secs(8);

impl SingBoxApi {
    /// Stream `SubscribeStatus` at `STATUS_INTERVAL`, feeding each sample to
    /// `on_status`. The first message comes immediately, with zero rates;
    /// then one per second, idle or not, until the callback returns `false`
    /// or sing-box goes away. Works whether or not anything is connected.
    pub fn stream_status(
        &self,
        mut on_status: impl FnMut(RuntimeStatus) -> bool,
    ) -> Result<(), ApiError> {
        let request = pb::SubscribeStatusRequest {
            // Go `time.Duration`: nanoseconds.
            interval: STATUS_INTERVAL.as_nanos() as i64,
        };
        self.stream(
            "SubscribeStatus",
            &request,
            STATUS_READ_TIMEOUT,
            |status: pb::Status| on_status(RuntimeStatus::from_proto(&status)),
        )
    }
}

/// One `SubscribeStatus` sample. Counters are clamped at zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct RuntimeStatus {
    /// Memory sing-box's Go runtime holds, bytes.
    pub memory: u64,
    pub goroutines: u32,
    /// Connections currently tracked by sing-box's router (what the
    /// connections list shows); 0 when `traffic_available` is false.
    pub connections_in: u32,
    /// Outbound connections currently open by sing-box's connection manager.
    pub connections_out: u32,
    /// Whether traffic accounting is on. Always true with the `api` service,
    /// which enables it.
    pub traffic_available: bool,
    /// Bytes uploaded during the last interval = bytes/sec. 0 in the first
    /// sample.
    pub uplink: u64,
    /// Bytes downloaded during the last interval = bytes/sec. 0 in the first
    /// sample.
    pub downlink: u64,
    /// Bytes uploaded since sing-box started (closed connections included).
    pub uplink_total: u64,
    /// Bytes downloaded since sing-box started (closed connections included).
    pub downlink_total: u64,
}

impl RuntimeStatus {
    fn from_proto(status: &pb::Status) -> Self {
        Self {
            memory: status.memory,
            goroutines: status.goroutines.max(0) as u32,
            connections_in: status.connections_in.max(0) as u32,
            connections_out: status.connections_out.max(0) as u32,
            traffic_available: status.traffic_available,
            uplink: status.uplink.max(0) as u64,
            downlink: status.downlink.max(0) as u64,
            uplink_total: status.uplink_total.max(0) as u64,
            downlink_total: status.downlink_total.max(0) as u64,
        }
    }

    /// The up/down rates alone, for the Home readout.
    pub fn traffic(&self) -> TrafficSample {
        TrafficSample {
            up: self.uplink,
            down: self.downlink,
        }
    }
}

/// One traffic sample: bytes transferred in the last one-second window, i.e.
/// an instantaneous rate in bytes/sec.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct TrafficSample {
    /// Upload rate, bytes/sec.
    pub up: u64,
    /// Download rate, bytes/sec.
    pub down: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    /// Field numbers must match upstream `started_service.proto`: interval is
    /// field 1 (varint) — `0x08` tag byte.
    #[test]
    fn subscribe_status_request_uses_upstream_field_number() {
        let request = pb::SubscribeStatusRequest {
            interval: STATUS_INTERVAL.as_nanos() as i64,
        };
        let bytes = request.encode_to_vec();
        assert_eq!(bytes[0], 0x08);
        assert_eq!(
            pb::SubscribeStatusRequest::decode(bytes.as_slice())
                .unwrap()
                .interval,
            1_000_000_000
        );
    }

    #[test]
    fn runtime_status_maps_every_field() {
        let status = pb::Status {
            memory: 50 << 20,
            goroutines: 42,
            connections_in: 3,
            connections_out: 2,
            traffic_available: true,
            uplink: 100,
            downlink: 2000,
            uplink_total: 10_000,
            downlink_total: 200_000,
        };
        assert_eq!(
            RuntimeStatus::from_proto(&status),
            RuntimeStatus {
                memory: 50 << 20,
                goroutines: 42,
                connections_in: 3,
                connections_out: 2,
                traffic_available: true,
                uplink: 100,
                downlink: 2000,
                uplink_total: 10_000,
                downlink_total: 200_000,
            }
        );
    }

    #[test]
    fn traffic_sample_clamps_negative_deltas() {
        let status = pb::Status {
            uplink: -5,
            downlink: 1234,
            goroutines: -1,
            ..Default::default()
        };
        let runtime = RuntimeStatus::from_proto(&status);
        assert_eq!(runtime.goroutines, 0);
        assert_eq!(runtime.traffic(), TrafficSample { up: 0, down: 1234 });
    }
}
