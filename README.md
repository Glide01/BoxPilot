# BoxPilot

> sing-box 的 Windows 桌面 GUI 管理器，基于 GPUI 构建。

## 功能

- **多配置管理**：远程订阅 / 本地 JSON 文件两种来源，增删改、一键切换；每个订阅可独立设置自动更新间隔
- **一键连接**：Home 页大圆按钮启动 / 停止 sing-box，断开 / 启动中 / 已连接三态可视化
- **双代理模式**：TUN ↔ Mixed inbound 切换，系统代理（注册表）一键开关
- **代理分组**：selector 分组手动选节点，urltest 分组自动选路（只读）；节点协议类型标注、整组延迟测速
- **实时网速**：侧边栏底部显示上行 / 下行速率（连接时）
- **实时日志**：按级别过滤（All / Warn / Error）、可拖选复制、清空，缓冲上限 1000 条
- **深链接导入**：浏览器点击 `sing-box://import-remote-profile` 链接直接导入订阅
- **端口可配**：本地代理端口（默认 7788）与 Clash API 端口（默认 7789）均可在 Settings 页修改
- **一键复制** PowerShell / WSL 代理环境变量（跟随配置端口）
- 配置与设置持久化（配置列表、代理模式、系统代理开关、端口）

## 系统要求

- Windows 10 1809+ 或 Windows 11（仅 x64）
- 支持 Direct3D 11 feature level 11_0 的 GPU
  - 不支持无 GPU passthrough 的 Hyper-V / Parallels 环境
  - 不支持纯 RDP 会话（除非启用 `BasicRender` shim）
- 管理员权限（管理 TUN 适配器、系统代理注册表、DNS 需要；启动时自动请求 UAC 提权）

## 安装

从 [Releases](../../releases) 下载最新版本：

- `*.msi` —— MSI 安装包（推荐；含 `sing-box.exe`，并注册 `sing-box://` / `boxpilot://` 链接协议）
- `box-pilot.exe` —— 单文件绿色版（需自备同目录的 `sing-box.exe`，且无链接协议注册）

## 使用

1. 启动 BoxPilot，同意 UAC 提权。
2. 在 **Profiles** 页点 **+ Add** 添加配置——粘贴订阅 URL 或选择本地 JSON 文件；也可以直接点击浏览器中的 `sing-box://` 导入链接。
3. 回到 **Home** 页，点大圆按钮连接。
4. 用 **Proxy Mode**（TUN / Mixed）和 **System Proxy** 开关控制代理行为。
5. **Groups** 页选节点、测延迟；**Logs** 页看 sing-box 实时输出；**Settings** 页改端口、清缓存（清缓存会重置节点选择）。

## 从源码构建

仅支持 Windows（GPUI 的 Windows 后端基于 DirectX 11 + MSVC，不支持 MinGW 交叉编译）：

```bash
cargo test
cargo build --release --target x86_64-pc-windows-msvc
```

发布产物（exe + MSI）由 GitHub Actions 构建（[`.github/workflows/release.yml`](.github/workflows/release.yml)），捆绑的 sing-box 版本通过仓库变量 `SINGBOX_VERSION` 钉定。

## 许可证

[MIT](LICENSE)
