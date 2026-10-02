# Talk to sing-box through its own API service, over gRPC-Web

BoxPilot drove sing-box through `experimental.clash_api`: `/proxies` for groups,
`PUT /proxies/{group}` to switch, `/group/{name}/delay` to test, and the
`/traffic` line stream for the Home readout. sing-box 1.14.0 added its own
control plane, the `api` service (`services[]`, type `api`): a gRPC server for
`daemon.StartedService`, the same interface the official graphical clients use.
We now use that instead, and the runtime config no longer carries a
`clash_api` at all.

The service listens on TCP only (there is no named-pipe option), and one port
serves native gRPC over h2c, gRPC-Web and gRPC-Web over WebSocket. BoxPilot
speaks **gRPC-Web over HTTP/1.1** through the blocking reqwest client it
already had, with hand-derived prost messages (`core/singbox_api.rs`). Server
streams arrive as length-prefixed frames on an ordinary response body. A
dedicated reader thread holds each long-lived stream, as before.

## Consequences

- **sing-box ≥ 1.14.0 is required.** Older binaries reject the `api` service
  type. The MSI bundles a new enough one. For the portable exe,
  `start_process` refuses with a clear message when the reported sing-box
  version is older.
- **Groups are pushed, not polled.** `SubscribeGroups` sends a snapshot on
  subscribe and again on every URL-test change. So delay badges now also show
  the results of urltest groups' own periodic checks.
- **Delay tests are fire-and-forget.** `URLTest` returns immediately, and
  results arrive on the group stream. sing-box records a failed probe by
  *deleting* the node's history, so there is no "all done" signal when any
  node fails. The Test spinner stops when every member has a fresh result, or
  when the stream has been quiet for 5s, or after 16s
  (`url_test_done`). Tested members with no result show `timeout`; a late
  result replaces it. The probe URL and per-node timeout are now sing-box's
  own: the group's `url`, or its default, and `C.TCPTimeout`. The API has no
  per-request override.
- **Traffic** comes from `SubscribeStatus` at a 1s interval, whose
  `uplink`/`downlink` are per-interval byte deltas, so bytes/sec.
- The settings field `clash_api_port` became `api_port`; the old name is
  still read. Any `api` service a subscription carries is dropped, the same
  ownership rule as `inbounds` and `experimental`.

## Considered options

**tonic + prost with build-time codegen.** This is the idiomatic gRPC
client. Rejected for weight: it brings a tokio runtime for us to own next to
gpui's executor, and `protoc`, or a protox build step, on the Windows/MSVC
build. All of that would buy one unary call shape and three server streams.
Worth reopening if we ever need a client-streaming RPC; gRPC-Web over
HTTP/1.1 can't do those.

**Keep the Clash API.** It is not deprecated upstream. But it is a
compatibility layer for Clash dashboards: a synthetic `GLOBAL` group, a map
with no order, and polling for everything except traffic. The `api` service is
the interface sing-box itself maintains for GUIs.

**The `sing-box-daemon.exe` named pipe** (sing-box for Desktop). It only
accepts peers Authenticode-signed by the same signer, so a third-party GUI
can't use it.
