//! Live connections: `SubscribeConnections`, `CloseConnection`,
//! `CloseAllConnections`, and `ConnectionTable`, the pure reducer that turns
//! the event stream into a connection list.

use super::transport::{ApiError, IDLE_STREAM_READ_TIMEOUT};
use super::{pb, SingBoxApi};
use std::collections::HashMap;
use std::time::Duration;

/// `SubscribeConnections` interval: how often sing-box diffs per-connection
/// byte counters into UPDATE events. One second makes the deltas bytes/sec.
pub const CONNECTIONS_INTERVAL: Duration = Duration::from_secs(1);
/// sing-box keeps this many closed connections (oldest evicted first);
/// `ConnectionTable` keeps no more either.
pub const CLOSED_CONNECTIONS_LIMIT: usize = 1000;

impl SingBoxApi {
    /// Stream `SubscribeConnections`; feed every batch to a
    /// `ConnectionTable`.
    ///
    /// - On subscribe: one batch with `reset` set, holding a NEW event per
    ///   open connection and per remembered closed one (up to
    ///   `CLOSED_CONNECTIONS_LIMIT`, `closed_at` set). Possibly empty.
    /// - Then, as they happen: NEW for each opened connection and CLOSED
    ///   (with the final totals) for each closed one, batched.
    /// - Every `CONNECTIONS_INTERVAL`: UPDATE for each open connection whose
    ///   counters moved, carrying the byte deltas since the last tick, and a
    ///   final zero UPDATE when one goes quiet. Nothing is sent for a tick
    ///   without changes, so an idle sing-box leaves the stream silent:
    ///   `TimedOut` after `IDLE_STREAM_READ_TIMEOUT`; re-subscribe (the new
    ///   stream opens with a fresh `reset` batch).
    ///
    /// A connection can be reported CLOSED twice (the tick and the close
    /// event race upstream); `ConnectionTable` absorbs that.
    pub fn stream_connections(
        &self,
        mut on_events: impl FnMut(ConnectionEvents) -> bool,
    ) -> Result<(), ApiError> {
        let request = pb::SubscribeConnectionsRequest {
            // Go `time.Duration`: nanoseconds.
            interval: CONNECTIONS_INTERVAL.as_nanos() as i64,
        };
        self.stream(
            "SubscribeConnections",
            &request,
            IDLE_STREAM_READ_TIMEOUT,
            |events: pb::ConnectionEvents| on_events(ConnectionEvents::from_proto(events)),
        )
    }

    /// `CloseConnection` — close one open connection by id. An unknown or
    /// already-closed id is not an error. The CLOSED event follows on the
    /// stream.
    pub fn close_connection(&self, id: &str) -> Result<(), ApiError> {
        let request = pb::CloseConnectionRequest { id: id.to_string() };
        self.unary("CloseConnection", &request)
    }

    /// `CloseAllConnections` — close every open connection (closed ones stay
    /// listed). CLOSED events follow on the stream.
    pub fn close_all_connections(&self) -> Result<(), ApiError> {
        self.unary("CloseAllConnections", &())
    }
}

/// The process behind a connection, when sing-box looked it up (TUN with
/// `find_process`, or a route rule that matches on process).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessInfo {
    pub process_id: u32,
    pub user_id: i32,
    pub user_name: String,
    pub process_path: String,
    /// Android only.
    pub package_names: Vec<String>,
}

/// One tracked connection. Times are unix milliseconds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Connection {
    /// UUID, stable for the connection's life.
    pub id: String,
    /// Inbound tag and type (`mixed`, `tun`…).
    pub inbound: String,
    pub inbound_type: String,
    /// 4 or 6; 0 when unknown.
    pub ip_version: u8,
    /// `tcp` or `udp`.
    pub network: String,
    /// `host:port` strings.
    pub source: String,
    pub destination: String,
    /// Domain from sniffing or the request; empty when only an IP is known.
    pub domain: String,
    /// Sniffed protocol (`tls`, `http`, `quic`, `dns`…); empty if none.
    pub protocol: String,
    pub user: String,
    /// Outbound that handed this connection back to the router (detour);
    /// usually empty.
    pub from_outbound: String,
    pub created_at: i64,
    /// `Some` once closed.
    pub closed_at: Option<i64>,
    /// Upload rate, bytes/sec: the latest UPDATE delta (sing-box itself
    /// leaves the proto field unset). 0 when idle or closed.
    pub uplink: u64,
    /// Download rate, bytes/sec, likewise.
    pub downlink: u64,
    pub uplink_total: u64,
    pub downlink_total: u64,
    /// The matched route rule, as sing-box prints it; empty for `final`.
    pub rule: String,
    /// The final (non-group) outbound and its type.
    pub outbound: String,
    pub outbound_type: String,
    /// Outbound path, final outbound *first*: e.g. `["香港-01", "auto",
    /// "节点选择"]` for a rule that picked `节点选择`.
    pub chain: Vec<String>,
    pub process: Option<ProcessInfo>,
}

impl Connection {
    fn from_proto(connection: pb::Connection) -> Self {
        Self {
            id: connection.id,
            inbound: connection.inbound,
            inbound_type: connection.inbound_type,
            ip_version: u8::try_from(connection.ip_version).unwrap_or(0),
            network: connection.network,
            source: connection.source,
            destination: connection.destination,
            domain: connection.domain,
            protocol: connection.protocol,
            user: connection.user,
            from_outbound: connection.from_outbound,
            created_at: connection.created_at,
            closed_at: (connection.closed_at > 0).then_some(connection.closed_at),
            uplink: connection.uplink.max(0) as u64,
            downlink: connection.downlink.max(0) as u64,
            uplink_total: connection.uplink_total.max(0) as u64,
            downlink_total: connection.downlink_total.max(0) as u64,
            rule: connection.rule,
            outbound: connection.outbound,
            outbound_type: connection.outbound_type,
            chain: connection.chain_list,
            process: connection.process_info.map(|info| ProcessInfo {
                process_id: info.process_id,
                user_id: info.user_id,
                user_name: info.user_name,
                process_path: info.process_path,
                package_names: info.package_names,
            }),
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed_at.is_some()
    }
}

/// What happened to one connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionEventKind {
    /// Opened — or, in a `reset` batch, already known (possibly closed:
    /// check `closed_at`).
    New(Connection),
    /// Bytes moved since the previous UPDATE (both 0: went quiet).
    Update {
        uplink_delta: u64,
        downlink_delta: u64,
    },
    /// Closed. `connection` carries the final state when sing-box still had
    /// it.
    Closed {
        closed_at: i64,
        connection: Option<Connection>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionEvent {
    pub id: String,
    pub kind: ConnectionEventKind,
}

impl ConnectionEvent {
    /// `None` for an event type newer than this client, or a NEW without
    /// its connection.
    fn from_proto(event: pb::ConnectionEvent) -> Option<Self> {
        let kind = match event.r#type {
            0 => ConnectionEventKind::New(Connection::from_proto(event.connection?)),
            1 => ConnectionEventKind::Update {
                uplink_delta: event.uplink_delta.max(0) as u64,
                downlink_delta: event.downlink_delta.max(0) as u64,
            },
            2 => ConnectionEventKind::Closed {
                closed_at: event.closed_at,
                connection: event.connection.map(Connection::from_proto),
            },
            _ => return None,
        };
        Some(Self { id: event.id, kind })
    }
}

/// One `SubscribeConnections` message.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectionEvents {
    /// Replace everything known with this batch.
    pub reset: bool,
    pub events: Vec<ConnectionEvent>,
}

impl ConnectionEvents {
    fn from_proto(events: pb::ConnectionEvents) -> Self {
        Self {
            reset: events.reset,
            events: events
                .events
                .into_iter()
                .filter_map(ConnectionEvent::from_proto)
                .collect(),
        }
    }
}

/// Connection id → latest state, maintained from the event stream. Closed
/// connections stay (marked by `closed_at`) up to `CLOSED_CONNECTIONS_LIMIT`,
/// oldest-closed evicted first, matching what sing-box itself remembers.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConnectionTable {
    connections: HashMap<String, Connection>,
}

impl ConnectionTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one batch, in order:
    /// - `reset` drops everything first;
    /// - NEW inserts (or replaces) the connection, rates zeroed;
    /// - UPDATE on an open connection adds the deltas to the totals and makes
    ///   them the current rates; unknown or closed ids are ignored;
    /// - CLOSED takes the final state when given (totals included), else
    ///   just stamps `closed_at`; rates drop to 0. A repeat CLOSED keeps the
    ///   first `closed_at`. Unknown ids without a connection are ignored.
    pub fn apply(&mut self, batch: ConnectionEvents) {
        if batch.reset {
            self.connections.clear();
        }
        for event in batch.events {
            match event.kind {
                ConnectionEventKind::New(mut connection) => {
                    connection.uplink = 0;
                    connection.downlink = 0;
                    self.connections.insert(event.id, connection);
                }
                ConnectionEventKind::Update {
                    uplink_delta,
                    downlink_delta,
                } => {
                    if let Some(connection) = self.connections.get_mut(&event.id) {
                        if !connection.is_closed() {
                            connection.uplink = uplink_delta;
                            connection.downlink = downlink_delta;
                            connection.uplink_total += uplink_delta;
                            connection.downlink_total += downlink_delta;
                        }
                    }
                }
                ConnectionEventKind::Closed {
                    closed_at,
                    connection,
                } => {
                    let earlier = self
                        .connections
                        .get(&event.id)
                        .and_then(|known| known.closed_at);
                    let closed_at = earlier.unwrap_or(closed_at);
                    let entry = match connection {
                        Some(connection) => {
                            self.connections.insert(event.id.clone(), connection);
                            self.connections.get_mut(&event.id)
                        }
                        None => self.connections.get_mut(&event.id),
                    };
                    if let Some(entry) = entry {
                        entry.closed_at = Some(closed_at);
                        entry.uplink = 0;
                        entry.downlink = 0;
                    }
                }
            }
        }
        self.evict_closed();
    }

    /// Drop the oldest-closed connections beyond `CLOSED_CONNECTIONS_LIMIT`.
    fn evict_closed(&mut self) {
        let mut closed: Vec<(i64, String)> = self
            .connections
            .values()
            .filter_map(|c| c.closed_at.map(|at| (at, c.id.clone())))
            .collect();
        if closed.len() <= CLOSED_CONNECTIONS_LIMIT {
            return;
        }
        closed.sort();
        for (_, id) in &closed[..closed.len() - CLOSED_CONNECTIONS_LIMIT] {
            self.connections.remove(id);
        }
    }

    pub fn get(&self, id: &str) -> Option<&Connection> {
        self.connections.get(id)
    }

    /// Every known connection, open and closed, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = &Connection> {
        self.connections.values()
    }

    pub fn len(&self) -> usize {
        self.connections.len()
    }

    pub fn is_empty(&self) -> bool {
        self.connections.is_empty()
    }

    pub fn open_count(&self) -> usize {
        self.connections.values().filter(|c| !c.is_closed()).count()
    }

    pub fn clear(&mut self) {
        self.connections.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proto_connection(id: &str) -> pb::Connection {
        pb::Connection {
            id: id.into(),
            inbound: "mixed-in".into(),
            inbound_type: "mixed".into(),
            ip_version: 4,
            network: "tcp".into(),
            source: "127.0.0.1:50000".into(),
            destination: "1.1.1.1:443".into(),
            domain: "example.com".into(),
            protocol: "tls".into(),
            user: String::new(),
            from_outbound: String::new(),
            created_at: 1_000,
            closed_at: 0,
            uplink: 0,
            downlink: 0,
            uplink_total: 10,
            downlink_total: 20,
            rule: "domain_suffix=[example.com] => route(节点选择)".into(),
            outbound: "香港-01".into(),
            outbound_type: "vless".into(),
            chain_list: vec!["香港-01".into(), "节点选择".into()],
            process_info: None,
        }
    }

    fn new_event(id: &str) -> ConnectionEvent {
        ConnectionEvent {
            id: id.into(),
            kind: ConnectionEventKind::New(Connection::from_proto(proto_connection(id))),
        }
    }

    fn update(id: &str, up: u64, down: u64) -> ConnectionEvent {
        ConnectionEvent {
            id: id.into(),
            kind: ConnectionEventKind::Update {
                uplink_delta: up,
                downlink_delta: down,
            },
        }
    }

    fn closed(id: &str, at: i64, connection: Option<Connection>) -> ConnectionEvent {
        ConnectionEvent {
            id: id.into(),
            kind: ConnectionEventKind::Closed {
                closed_at: at,
                connection,
            },
        }
    }

    fn batch(reset: bool, events: Vec<ConnectionEvent>) -> ConnectionEvents {
        ConnectionEvents { reset, events }
    }

    #[test]
    fn connection_maps_every_field() {
        let mut proto = proto_connection("a");
        proto.closed_at = 5_000;
        proto.process_info = Some(pb::ProcessInfo {
            process_id: 1234,
            user_id: 1000,
            user_name: "me".into(),
            process_path: r"C:\Program Files\app.exe".into(),
            package_names: vec!["pkg".into()],
        });
        let connection = Connection::from_proto(proto);
        assert_eq!(
            connection,
            Connection {
                id: "a".into(),
                inbound: "mixed-in".into(),
                inbound_type: "mixed".into(),
                ip_version: 4,
                network: "tcp".into(),
                source: "127.0.0.1:50000".into(),
                destination: "1.1.1.1:443".into(),
                domain: "example.com".into(),
                protocol: "tls".into(),
                user: String::new(),
                from_outbound: String::new(),
                created_at: 1_000,
                closed_at: Some(5_000),
                uplink: 0,
                downlink: 0,
                uplink_total: 10,
                downlink_total: 20,
                rule: "domain_suffix=[example.com] => route(节点选择)".into(),
                outbound: "香港-01".into(),
                outbound_type: "vless".into(),
                chain: vec!["香港-01".into(), "节点选择".into()],
                process: Some(ProcessInfo {
                    process_id: 1234,
                    user_id: 1000,
                    user_name: "me".into(),
                    process_path: r"C:\Program Files\app.exe".into(),
                    package_names: vec!["pkg".into()],
                }),
            }
        );
        assert!(connection.is_closed());
        assert!(!Connection::from_proto(proto_connection("b")).is_closed());
    }

    #[test]
    fn events_map_upstream_types() {
        let events = ConnectionEvents::from_proto(pb::ConnectionEvents {
            reset: true,
            events: vec![
                pb::ConnectionEvent {
                    r#type: 0,
                    id: "a".into(),
                    connection: Some(proto_connection("a")),
                    ..Default::default()
                },
                pb::ConnectionEvent {
                    r#type: 1,
                    id: "a".into(),
                    uplink_delta: 5,
                    downlink_delta: -1,
                    ..Default::default()
                },
                pb::ConnectionEvent {
                    r#type: 2,
                    id: "a".into(),
                    closed_at: 9_000,
                    ..Default::default()
                },
                // NEW without a connection, and an unknown type: dropped.
                pb::ConnectionEvent {
                    r#type: 0,
                    id: "b".into(),
                    ..Default::default()
                },
                pb::ConnectionEvent {
                    r#type: 7,
                    id: "c".into(),
                    ..Default::default()
                },
            ],
        });
        assert!(events.reset);
        assert_eq!(events.events.len(), 3);
        assert!(matches!(events.events[0].kind, ConnectionEventKind::New(_)));
        assert_eq!(
            events.events[1].kind,
            ConnectionEventKind::Update {
                uplink_delta: 5,
                downlink_delta: 0
            }
        );
        assert_eq!(
            events.events[2].kind,
            ConnectionEventKind::Closed {
                closed_at: 9_000,
                connection: None
            }
        );
    }

    #[test]
    fn reset_replaces_the_table() {
        let mut table = ConnectionTable::new();
        table.apply(batch(true, vec![new_event("a"), new_event("b")]));
        assert_eq!(table.len(), 2);
        table.apply(batch(true, vec![new_event("c")]));
        assert_eq!(table.len(), 1);
        assert!(table.get("c").is_some());
        table.apply(batch(true, vec![]));
        assert!(table.is_empty(), "an empty reset empties the table");
    }

    #[test]
    fn updates_accumulate_totals_and_set_rates() {
        let mut table = ConnectionTable::new();
        table.apply(batch(true, vec![new_event("a")]));
        table.apply(batch(false, vec![update("a", 100, 1_000)]));
        let a = table.get("a").unwrap();
        assert_eq!((a.uplink, a.downlink), (100, 1_000));
        assert_eq!((a.uplink_total, a.downlink_total), (110, 1_020));

        table.apply(batch(false, vec![update("a", 0, 0)]));
        let a = table.get("a").unwrap();
        assert_eq!((a.uplink, a.downlink), (0, 0), "quiet resets the rate");
        assert_eq!((a.uplink_total, a.downlink_total), (110, 1_020));

        table.apply(batch(false, vec![update("ghost", 1, 1)]));
        assert!(table.get("ghost").is_none(), "unknown ids are ignored");
    }

    #[test]
    fn closed_takes_final_state_or_stamps_and_stays_listed() {
        let mut table = ConnectionTable::new();
        table.apply(batch(true, vec![new_event("a"), new_event("b")]));
        table.apply(batch(false, vec![update("a", 5, 5), update("b", 7, 7)]));

        let mut last = Connection::from_proto(proto_connection("a"));
        last.uplink_total = 999;
        table.apply(batch(false, vec![closed("a", 4_000, Some(last))]));
        let a = table.get("a").unwrap();
        assert_eq!(a.closed_at, Some(4_000));
        assert_eq!(a.uplink_total, 999, "final totals from sing-box");
        assert_eq!((a.uplink, a.downlink), (0, 0));

        table.apply(batch(false, vec![closed("b", 4_500, None)]));
        let b = table.get("b").unwrap();
        assert_eq!(b.closed_at, Some(4_500));
        assert_eq!(b.uplink_total, 17, "keeps what it accumulated");
        assert_eq!(table.len(), 2, "closed connections stay listed");
        assert_eq!(table.open_count(), 0);

        table.apply(batch(false, vec![update("b", 3, 3)]));
        assert_eq!(table.get("b").unwrap().uplink, 0, "no updates after close");

        table.apply(batch(false, vec![closed("ghost", 1, None)]));
        assert!(table.get("ghost").is_none());
    }

    #[test]
    fn repeated_closed_is_idempotent() {
        let mut table = ConnectionTable::new();
        table.apply(batch(true, vec![new_event("a")]));
        table.apply(batch(false, vec![closed("a", 4_000, None)]));
        let mut late = Connection::from_proto(proto_connection("a"));
        late.closed_at = Some(4_100);
        table.apply(batch(false, vec![closed("a", 4_100, Some(late))]));
        assert_eq!(table.get("a").unwrap().closed_at, Some(4_000));
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn reset_snapshot_may_carry_closed_connections() {
        let mut proto = proto_connection("old");
        proto.closed_at = 2_000;
        let mut table = ConnectionTable::new();
        table.apply(batch(
            true,
            vec![
                new_event("live"),
                ConnectionEvent {
                    id: "old".into(),
                    kind: ConnectionEventKind::New(Connection::from_proto(proto)),
                },
            ],
        ));
        assert_eq!(table.open_count(), 1);
        assert_eq!(table.get("old").unwrap().closed_at, Some(2_000));
        table.apply(batch(false, vec![update("old", 9, 9)]));
        assert_eq!(
            table.get("old").unwrap().uplink_total,
            10,
            "closed: untouched"
        );
    }

    #[test]
    fn closed_connections_are_capped_oldest_first() {
        let mut table = ConnectionTable::new();
        let mut events = Vec::new();
        for i in 0..CLOSED_CONNECTIONS_LIMIT + 2 {
            let mut connection = Connection::from_proto(proto_connection(&i.to_string()));
            connection.closed_at = Some(10_000 + i as i64);
            events.push(ConnectionEvent {
                id: connection.id.clone(),
                kind: ConnectionEventKind::New(connection),
            });
        }
        events.push(new_event("open"));
        table.apply(batch(true, events));
        assert_eq!(table.len(), CLOSED_CONNECTIONS_LIMIT + 1);
        assert!(table.get("0").is_none() && table.get("1").is_none());
        assert!(table.get("2").is_some());
        assert!(
            table.get("open").is_some(),
            "open connections never evicted"
        );
    }
}
