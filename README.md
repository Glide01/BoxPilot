# BoxPilot

> sing-box 的 Windows / Linux 桌面 GUI 管理器，基于 GPUI 构建。

## 功能

- **多配置管理**：远程订阅 / 本地 JSON 文件两种来源，增删改、一键切换；每个订阅可独立设置自动更新间隔
- **一键连接**：Home 页大圆按钮启动 / 停止 sing-box，断开 / 启动中 / 已连接三态可视化
- **双代理模式**：TUN ↔ Mixed inbound 切换，系统代理一键开关（Windows 写注册表；Linux 支持 GNOME / KDE）
- **代理分组**：selector 分组手动选节点，urltest 分组自动选路（只读）；节点协议类型标注、整组延迟测速；分组展开状态由 sing-box 记住
- **运行状态**：侧边栏底部实时上行 / 下行速率；Home 页显示内存、连接数、累计流量、运行时长与 sing-box 版本
- **Clash mode 切换**：配置的路由规则带 `clash_mode` 时，Home 页可在 Rule / Global / Direct 等模式间切换
- **连接列表**：Connections 页实时查看活动 / 已关闭连接（域名、进程、出站链路、规则、速率），可筛选、排序、关闭单个或全部连接
- **实时日志**：来自 sing-box API，按级别过滤（Error / Warn / Info / Debug / Trace，默认跟随配置）、级别着色、可拖选复制与搜索、清空；启动失败与崩溃输出同样可见，缓冲上限 1000 条
- **诊断工具**：Tools 页提供网络质量测试（带宽 / RPM / 延迟）与 NAT 类型（STUN）检测，可指定出站
- **Tailscale**：配置含 Tailscale endpoint 时出现 Tailscale 页——登录 / 登出、设备列表、出口节点、Ping、Taildrop 收件、HTTPS 证书
- **OpenConnect / OpenVPN / USB/IP**：配置含这些 endpoint 时出现 VPN 页——连接状态、隧道信息，以及登录表单 / 一次性验证码等交互式认证
- **深链接导入**：浏览器点击 `sing-box://import-remote-profile` 链接直接导入订阅
- **端口可配**：本地代理端口（默认 7788）与 sing-box API 端口（默认 7789）均可在 Settings 页修改
- **一键复制**代理环境变量（跟随配置端口）：Windows 上为 PowerShell / WSL，Linux 上为 bash/zsh / fish
- 配置与设置持久化（配置列表、代理模式、系统代理开关、端口）

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
- **代理环境变量**：Settings 页可一键复制 bash/zsh 或 fish 的代理环境变量命令。
- 配置和设置保存在 `~/.config/BoxPilot`。

## 使用

1. 启动 BoxPilot（Windows 上同意 UAC 提权）。
2. 在 **Profiles** 页点 **+ Add** 添加配置——粘贴订阅 URL 或选择本地 JSON 文件；也可以直接点击浏览器中的 `sing-box://` 导入链接。
3. 回到 **Home** 页，点大圆按钮连接。
4. 用 **Proxy Mode**（TUN / Mixed）和 **System Proxy** 开关控制代理行为。
5. **Groups** 页选节点、测延迟；**Logs** 页看 sing-box 实时输出；**Settings** 页改端口、清缓存（清缓存会重置节点选择）。

## 从源码构建

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
