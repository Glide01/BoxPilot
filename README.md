# BoxPilot

> sing-box 的 Windows / Linux 桌面 GUI 管理器，基于 GPUI 构建。

## 功能

- **多配置管理**：远程订阅 / 本地 JSON 文件两种来源，增删改、一键切换（「配置」页点一行，或首页配置卡点配置名打开菜单）；每个订阅可独立设置自动更新间隔
- **通过 sing-box 更新订阅**：sing-box 运行时（TUN 和代理模式都一样），订阅更新经由本地代理端口走 sing-box，失败时自动改为直连重试；sing-box 未运行时直连。每个订阅可在编辑弹窗里关闭「通过 sing-box 更新」（适合拒绝代理 IP 的订阅服务商）。BoxPilot 自身的检查更新同样在 sing-box 运行时经由它
- **订阅用量与到期**：订阅服务器在 `subscription-userinfo` 响应头里报告流量和到期时间时，「配置」页每一行和首页配置卡显示已用 / 总流量进度条与剩余天数；用掉 90% 或只剩 3 天时变黄，用完或过期时变红，并在启动和更新时提示一次
- **一键连接**：首页大圆按钮启动 / 停止 sing-box，未连接 / 正在启动 / 已连接三态可视化（快捷键 Ctrl+S）
- **双代理模式**：TUN ↔ 代理（mixed 入站）切换，系统代理一键开关（Windows 写注册表；Linux 支持 GNOME / KDE）
- **允许局域网连接**：「设置 › 网络」打开后代理监听 `0.0.0.0`，同一网络里的其他设备可以用 `<本机局域网地址>:<代理端口>` 上网（设置页会列出地址；无密码，防火墙可能会询问是否放行 sing-box）
- **系统托盘**：托盘图标已连接时为彩色、未连接时为灰色，悬停显示连接状态；菜单可显示窗口、连接 / 断开、开关系统代理、切换代理模式、Clash 模式和配置、退出。「设置 › 常规 › 关闭按钮」决定关窗时询问 / 最小化到托盘 / 退出——最小化到托盘时 sing-box 保持连接，退出才会停止 sing-box。桌面没有托盘时（如未装 AppIndicator 扩展的 GNOME），关窗即退出
- **代理分组**：selector 分组手动选节点，urltest 分组自动选路（只读）；可按节点名或协议类型搜索、按延迟排序；点节点的延迟标记单独测速，也可整组测速或「全部测速」；上千个节点也流畅滚动；分组展开状态由 sing-box 记住
- **运行状态**：侧边栏底部实时上行 / 下行速率；首页显示内存、连接数、累计流量、运行时长与 sing-box 版本，以及最近 2 分钟的上下行速率曲线
- **Clash 模式切换**：配置的路由规则带 `clash_mode` 时，首页可在 Rule / Global / Direct 等模式间切换
- **连接列表**：「连接」页实时查看活动 / 已关闭连接（域名、进程、出站链路、规则、速率），可筛选、排序、关闭单个或全部连接
- **实时日志**：来自 sing-box API，按级别过滤（Error / Warn / Info / Debug / Trace，默认跟随配置）、级别着色、可拖选复制与搜索、清空；启动失败与崩溃输出同样可见，缓冲上限 1000 条
- **诊断工具**：「工具」页提供网络质量测试（带宽 / RPM / 延迟）与 NAT 类型（STUN）检测，可指定出站
- **运行配置查看**：「设置 › 故障排查 › 运行配置」查看 sing-box 实际运行的配置文件（未连接时预览现在连接会运行的内容）；默认隐藏密码、密钥、UUID 等凭据，可搜索、复制、打开所在文件夹
- **Tailscale**：配置含 Tailscale endpoint 时出现 Tailscale 页——登录 / 登出、设备列表、出口节点、Ping、Taildrop 收件、HTTPS 证书
- **OpenConnect / OpenVPN / USB/IP**：配置含这些 endpoint 时出现 VPN 页——连接状态、隧道信息，以及登录表单 / 一次性验证码等交互式认证
- **导入链接**：浏览器点击 `sing-box://import-remote-profile` 链接，确认后直接导入订阅
- **键盘操作**：Ctrl+1…7 依次打开首页、分组、连接、配置、日志、工具、设置；Tab / Shift+Tab 在按钮、开关、下拉框和输入框之间移动焦点，Enter / 空格按下；Ctrl+S 连接 / 断开，Ctrl+U 更新订阅
- **深色模式**：「设置 › 常规 › 外观」可选跟随系统 / 浅色 / 深色，即时生效；跟随系统时随桌面切换
- **中英文界面**：「设置 › 常规 › 语言」可选跟随系统 / English / 简体中文，即时切换（托盘菜单一并切换）；跟随系统时，系统语言为中文即显示简体中文，否则显示英文
- **检查更新**：每天在 GitHub 上检查一次 BoxPilot 新版本（可在「设置 › 关于」关闭）；有新版本时提示一次，侧边栏「设置」旁出现小圆点，可下载、跳过此版本或立即检查
- **端口可配**：本地代理端口（默认 7788）可在设置页修改；BoxPilot 自用的 sing-box API 每次启动自动挑一个空闲的本地端口，无需设置。配置自带的控制接口（`api` 服务、`clash_api`）照原样运行，互不干扰
- **性能**：页面只在自己的数据变化时重绘；连接后空闲时几乎不占 CPU（后台数据按到达推送，不再定时轮询）；日志页不可见时不重排文本；sing-box API 复用连接；发布版以 `opt-level = 3` 编译
- 配置与设置持久化（配置列表、代理模式、系统代理开关、代理端口、外观、语言、关闭按钮等）

## 系统要求

### Windows

- Windows 10 1809+ 或 Windows 11（仅 x64）
- 支持 Direct3D 11 feature level 11_0 的 GPU
  - 不支持无 GPU passthrough 的 Hyper-V / Parallels 环境
  - 不支持纯 RDP 会话（除非启用 `BasicRender` shim）
- 管理员权限（管理 TUN 适配器、系统代理注册表、DNS 需要；启动时自动请求 UAC 提权）

### Linux

- x86_64，glibc 2.35+（Ubuntu 22.04 及同代或更新的发行版），X11 或 Wayland 桌面
- 运行时依赖（不打包进 AppImage，由系统提供）：
  - libxkbcommon（含 libxkbcommon-x11）、libxcb、libwayland-client
  - 支持 Vulkan 或 EGL 的显卡驱动
  - xdg-desktop-portal（文件选择对话框和打开链接要用）
  - 系统托盘需要 StatusNotifierItem 宿主（KDE、装了 AppIndicator 扩展的 GNOME 及多数面板）；没有时关窗即退出
- BoxPilot 始终以普通用户运行，不需要也不应该用 root 启动；TUN 模式所需权限见下文

## 安装

从 [Releases](../../releases) 下载最新版本：

- `*.msi` —— Windows 安装包（含 `sing-box.exe`，并注册 `sing-box://` / `boxpilot://` 链接协议）；也可以用 `winget install Glide01.BoxPilot` 安装
- `BoxPilot-<版本>-x86_64.AppImage` —— Linux 版（含 `sing-box`）

### Linux（AppImage）

```bash
chmod +x BoxPilot-*-x86_64.AppImage
./BoxPilot-*-x86_64.AppImage
```

- **链接协议与菜单项**：以 AppImage 运行时，BoxPilot 每次启动都会在 `~/.local/share` 下注册 `sing-box://` / `boxpilot://` 链接协议，并写入 `.desktop` 菜单项和图标。AppImage 移动位置后重新运行一次即可更新。
- **TUN 模式**：AppImage 是只读挂载，不能直接给里面的 sing-box 授权。第一次用 TUN 模式连接时，BoxPilot 会通过系统密码框（pkexec）请求一次授权：把带网络管理能力（setcap）的 sing-box 副本安装到 `/usr/local/lib/boxpilot/`。之后连接不再询问；升级后自带的 sing-box 版本变了，会再询问一次。Mixed 模式不需要授权。
- **系统代理**：GNOME 和 KDE 下可用（由 sing-box 的 `set_system_proxy` 设置）。
- 配置和设置保存在 `~/.config/BoxPilot`。

## 使用

界面语言默认跟随系统（中文系统显示简体中文），可在「设置 › 常规 › 语言」切换。下文用中文界面的名称，括号里是英文界面的名称。

1. 启动 BoxPilot（Windows 上同意 UAC 提权）。
2. 在「配置」（Profiles）页点「添加」（Add）添加配置——粘贴订阅链接或选择本地 JSON 文件；也可以直接点击浏览器中的 `sing-box://` 导入链接。
3. 回到「首页」（Home），点大圆按钮连接。
4. 用「代理模式」（Proxy Mode：TUN / 代理）和「系统代理」（System Proxy）开关控制代理行为。
5. 「分组」（Groups）页选节点、测速；「连接」（Connections）页看实时连接；「日志」（Logs）页看 sing-box 实时输出；「设置」（Settings）页改代理端口、语言、外观、关闭按钮行为，清缓存（清缓存会重置节点选择）。
6. 关闭窗口：第一次关窗时会询问是否让 BoxPilot 留在托盘中继续运行（可勾选「不再询问」）；之后可在「设置 › 常规 › 关闭按钮」修改。留在托盘时 sing-box 保持连接，点托盘图标重新打开窗口，用托盘菜单的「退出 BoxPilot」退出并停止 sing-box。

## 从源码构建

发布配置（`[profile.release]`）为 `opt-level = 3`、LTO、`codegen-units = 1`：编译慢一些，换来更快的运行速度。

### Windows

GPUI 的 Windows 后端基于 DirectX 11 + MSVC，不支持 MinGW 交叉编译：

```bash
cargo test
cargo build --release --target x86_64-pc-windows-msvc
```

### Linux

以 Ubuntu / Debian 为例，先装构建依赖：

```bash
sudo apt-get install pkg-config libxkbcommon-dev libxkbcommon-x11-dev libxcb1-dev \
  libwayland-dev libvulkan-dev libfontconfig1-dev libfreetype6-dev
cargo test
cargo build --release
```

打包 AppImage（需要一个 Linux 版 `sing-box` 二进制，版本 ≥ 1.14.0；脚本会下载固定版本的 appimagetool）：

```bash
packaging/linux/build-appimage.sh target/release/box_pilot_gui /path/to/sing-box <版本> release
```

### 发布

发布产物（Windows MSI + Linux AppImage）由 GitHub Actions 构建（[`.github/workflows/release.yml`](.github/workflows/release.yml)），捆绑的 sing-box 版本通过仓库变量 `SINGBOX_VERSION` 钉定。

## 许可证

[MIT](LICENSE)
