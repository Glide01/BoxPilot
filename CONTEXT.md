# BoxPilot

Windows, Linux and macOS desktop manager for the sing-box proxy: fetches subscription
configs, controls the sing-box process lifecycle, and surfaces its runtime
state.

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
sing-box ≥ 1.14), which BoxPilot injects for every sing-box start on a free
loopback port it picks itself, behind a fresh secret, and uses for
everything it shows of a running sing-box: groups, node switching, delay
tests, runtime status, logs, connections, clash mode, diagnostics and the
Tailscale / OpenConnect / OpenVPN / USB/IP endpoints. Internal: the user
never sees or sets its port. Replaced the Clash API
(`experimental.clash_api`), which BoxPilot no longer uses. Controllers a
profile's config brings itself (its own `api` services, `clash_api`) run
alongside as the config writes them; BoxPilot never talks to them.
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
single running instance, whatever it carried. On macOS most attempts arrive
as Apple events (open URLs, reopen) rather than as new processes.
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

**Local proxy**:
The mixed inbound BoxPilot injects on the local proxy port (Settings ›
Network) in TUN and Proxy mode alike, always reachable on loopback. While
sing-box runs, BoxPilot's own requests go through it: the update check, and
the subscription fetches of profiles with "Update through sing-box" on
(the default), which retry directly once if the proxied attempt fails.
_Avoid_: core proxy, system proxy (that is the OS setting sing-box writes)

**Clash mode**:
The selector that a profile's `clash_mode` route/DNS rules match on (e.g.
Rule / Global / Direct), switched live from Home while sing-box runs. The
modes come from the running config; sing-box remembers the chosen one in its
cache file. Distinct from the **Proxy Mode** toggle (TUN vs. Proxy inbound).
_Avoid_: routing mode, outbound mode

## Simplified Chinese UI terms (zh-CN)

The UI ships in English and Simplified Chinese (`src/i18n/`, Settings ›
General "Language"). Chinese strings use these terms, full-width punctuation
(，。：；（）？「」) and a space between Chinese and Latin words or numbers (compact
durations such as 1小时23分 excepted).

| English | 简体中文 | Notes |
|---|---|---|
| sing-box | sing-box | Never translated. _Avoid_: 内核, 核心 |
| sing-box API | sing-box API | |
| Profile | 配置 | The Profiles page is 「配置」. A profile's downloaded JSON is its 配置文件 |
| config (file) | 配置文件 | e.g. Running config = 运行配置, "Config not found" = 未找到配置文件 |
| Subscription | 订阅 | Subscription URL = 订阅链接 |
| Import link | 导入链接 | _Avoid_: 深链接, URI |
| Group | 分组 | urltest groups' badge: 自动 |
| Node | 节点 | |
| Outbound | 出站 | |
| Test / Test all | 测速 / 全部测速 | Test delay (one node) = 测试延迟 |
| Delay | 延迟 | |
| timeout | 超时 | |
| Clash mode | Clash 模式 | Mode names (Rule / Global / Direct) come from the config, untranslated |
| Proxy Mode: TUN / Proxy | 代理模式：TUN / 代理 | |
| System Proxy | 系统代理 | |
| Allow LAN connections | 允许局域网连接 | |
| Connect / Disconnect | 连接 / 断开 | Power button and tray menu |
| Connected / Disconnected / Starting… | 已连接 / 未连接 / 正在启动… | Connection status |
| Connection (page, list) | 连接 | Open / closed connections = 活动 / 已关闭 |
| Logs | 日志 | Level names stay English (Error / Warn / Info / Debug / Trace), as in the log text |
| Tools | 工具 | |
| Settings | 设置 | Sections: 常规 / 网络 / TUN / 故障排查 / 关于 |
| System (follow the OS) | 跟随系统 | Language and Appearance options |
| Appearance: Light / Dark | 外观：浅色 / 深色 | |
| Close button: Ask / Minimize to tray / Quit | 关闭按钮：询问 / 最小化到托盘 / 退出 | |
| Update (a profile) | 更新 | Check for updates (BoxPilot) = 检查更新 |
| Update through sing-box | 通过 sing-box 更新 | Per-subscription switch: fetch via the local proxy port, direct retry on failure |
