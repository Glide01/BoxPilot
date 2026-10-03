# Talk to sing-box through its own API service, over gRPC-Web

BoxPilot drove sing-box through `experimental.clash_api`: `/proxies` for groups,
`PUT /proxies/{group}` to switch, `/group/{name}/delay` to test, and the
`/traffic` line stream for the Home readout. sing-box 1.14.0 added its own
control plane, the `api` service (`services[]`, type `api`): a gRPC server for
`daemon.StartedService`, the same interface the official graphical clients use.
We now use that instead, and BoxPilot no longer injects a `clash_api` of
its own (a profile's own still runs; see "BoxPilot's API is internal"
below).

The service listens on TCP only (there is no named-pipe option), and one port
serves native gRPC over h2c, gRPC-Web and gRPC-Web over WebSocket. BoxPilot
speaks **gRPC-Web over HTTP/1.1** through the blocking reqwest client it
already had, with hand-derived prost messages (`core/singbox_api/`). Server
streams arrive as length-prefixed frames on an ordinary response body. A
dedicated reader thread holds each long-lived stream, as before.

## Consequences

- **sing-box ≥ 1.14.0 is required.** Older binaries reject the `api` service
  type. The MSI and AppImage bundle a new enough one. For a standalone
  binary run next to a sing-box of the user's own (the portable exe, or a
  Linux build from source), `start_process` refuses with a clear message
  when the reported sing-box version is older.
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
- **Logs** come from `SubscribeLog`, which carries every level whatever
  `log.level` says, but only since the `api` service started and only while
  sing-box runs. The stdout/stderr pipes stay for the rest: early startup,
  config errors, deprecation warnings, panics, and output after exit.
  sing-box writes each line to both, so `core::log_merge` merges them
  without showing a line twice. The Logs page filters by level, defaulting
  to `GetDefaultLogLevel`; its Clear also calls `ClearLogs`.
- **Coverage.** BoxPilot uses every server-streaming and unary RPC that
  does something under the CLI `api` service: also connections, clash mode,
  group expand state, network quality / STUN tests, Tailscale (status, ping,
  exit node, logout, Taildrop inbox, certificates), OpenConnect / OpenVPN
  status and sign-in challenges, and USB/IP server status. Left out:
  `StartTailscaleSSHSession`, `SendTaildropFiles` and `ProvideUSBDevices`
  (client-streaming, out of reach of gRPC-Web), and `SubscribeNotifications`,
  `GetDeprecatedWarnings` and `SubscribeServiceStatus`, which only the
  official GUIs' daemon fills — under `sing-box run` they stay empty or
  report `Started` once.
- The settings field `clash_api_port` became `api_port`; the old name was
  still read. Both are gone now, see below.

## BoxPilot's API is internal; the config's own controllers run

At first BoxPilot owned the whole control plane: the API port was a setting
(`api_port`, default 7789), and every `api` service and the whole
`experimental` section a subscription or local file carried were dropped,
the same ownership rule as `inbounds`. A config's controller often listens
on `0.0.0.0` without a secret, and two `api` services could clash over the
tag or port; that was the reason for stripping them.

The user chose otherwise. BoxPilot's own sing-box API is purely internal,
and the config's own controllers are kept:

- **No API port setting.** For every sing-box start, `pick_api_port` asks
  the OS for a free loopback port (bind `127.0.0.1:0`, read it, drop the
  listener), skipping the local proxy port and every port the config's own
  listeners use (each service's `listen_port`, the `clash_api` and
  `v2ray_api` addresses). Old settings files with `api_port` or
  `clash_api_port` still load; the field is ignored and dropped on the
  next save. Nothing holds the port until sing-box binds it, so another
  program can take it in between. sing-box then fails to start on it, and
  BoxPilot starts once more on a fresh port; a second failure stands, with
  sing-box's `bind` error in Logs.
- **The config's own controllers are kept**, for remote subscriptions and
  local files alike. Its `type: "api"` services pass through untouched.
  `experimental` is merged instead of replaced: `clash_api`, `v2ray_api`
  and the rest stay, and only `cache_file.enabled` is forced on (its other
  `cache_file` fields stay). `inbounds` remain BoxPilot's. BoxPilot's
  service gets the tag `boxpilot-api`, or `boxpilot-api-2`, … if the config
  already uses it; sing-box 1.14 runs two `api` services side by side, and
  each checks only its own `secret`.
- **The trade-off, accepted by the user:** a config's own controller runs
  exactly as written, possibly on a non-loopback address and without a
  secret, open to whoever can reach it; and if its port is taken, sing-box
  fails to start. BoxPilot does not guard against either.
- Profile configs saved before this change were stored with those sections
  stripped. They get them back on their next update or re-import.

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
