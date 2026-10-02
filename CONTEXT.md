# BoxPilot

Windows desktop manager for the sing-box proxy: fetches subscription configs,
controls the sing-box process lifecycle, and surfaces its runtime state.

## Language

**sing-box**:
The bundled proxy engine binary that BoxPilot manages. Always referred to by
its product name, in the UI and in code.
_Avoid_: core, kernel, 内核, engine

**sing-box version**:
The version the sing-box binary reports about itself. Distinct from the
BoxPilot version; "Unknown" when the binary is missing or unreadable.

**sing-box API**:
The control interface of the running sing-box: its `api` service (gRPC,
sing-box ≥ 1.14), which BoxPilot injects on a loopback port and uses for
groups, node switching, delay tests and traffic. Replaced the Clash API
(`experimental.clash_api`), which BoxPilot no longer enables.
_Avoid_: Clash API, external controller, core API

**Connection**:
One network flow (a TCP stream or UDP association) that sing-box is
proxying, as the sing-box API reports it: its destination, the inbound that
accepted it, the route rule that matched, and the outbound chain that
carries it (shown group → node). Listed on the Connections page while open,
and for a while after it closes — sing-box remembers the last 1000 closed
ones. Unrelated to BoxPilot's "Connected" status, which means sing-box is
running.
_Avoid_: request, session

**BoxPilot version**:
The version of the BoxPilot app itself (the Cargo package version).

**Launch attempt**:
One action by the user to start or reach BoxPilot — double-clicking the icon,
launching it a second time, or clicking a link. Every attempt is routed to the
single running instance, whatever it carried.
_Avoid_: ping, second launch, inbound message

**Deep link**:
A URI handed to BoxPilot through one of its registered URL schemes
(`sing-box://`, `boxpilot://`). The transport, not yet a promise that the URI
means anything.

**Import link**:
A deep link whose action is `import-remote-profile` — the only action BoxPilot
understands today. UI text uses this term, never "deep link".
_Avoid_: URI import, import URI, subscription link

**Subscription User-Agent**:
The identity string sent when fetching a subscription. Servers sniff the
literal `sing-box` token in it to decide whether to serve sing-box JSON or
Clash YAML, and read the version after the token to gate config-format
features — so the token must always be present, and the version after it
should be the real sing-box version whenever it is known.

**Clash mode**:
The selector that a profile's `clash_mode` route/DNS rules match on (e.g.
Rule / Global / Direct), switched live from Home while sing-box runs. The
modes come from the running config; sing-box remembers the chosen one in its
cache file. Distinct from the **Proxy Mode** toggle (TUN vs. Proxy inbound).
_Avoid_: routing mode, outbound mode
