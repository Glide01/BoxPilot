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
