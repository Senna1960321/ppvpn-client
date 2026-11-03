# PPVPN Desktop

PPVPN 的桌面客户端：三个平台都是 Flutter 版（`apps/flutter`），macOS 的推送代理是 Swift 写的，
共用 Rust 客户端核心 `crates/desktop`（crate `ppvpn-desktop`，UniFFI 绑定）和特权服务 `crates/service`。
网络引擎是 Rust `ppvpn-core`（仓库根的 `crates/core`），都在进程内运行：标准模式在
应用进程里，增强模式（TUN）在特权服务进程里。

路径和命令都以仓库根为准。

- 构建：见 [BUILD.md](BUILD.md) 和各平台的 `apps/<platform>/README.md`。
- 真机 QA：见 [qa-checklist.md](qa-checklist.md)。
- CI：见 [ci.md](ci.md)（`ci.yml` 里桌面端的各个 job 与触发条件）。安装包由 `release.yml` 构建，发行是
  tag `vX.Y.Z` 的 GitHub Release（与 CLI 同一个），更新站点在 R2：见 [构建与发布](../release.md)。
- 引擎本身（Core API、Profile、Rust 版行为对照）的文档在 [docs/](../)。
- 第三方组件：见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。

## 安装

- **Windows**：运行 `PPVPN-<版本>-windows-x64-setup.exe`。
- **Linux**：从 PPVPN 的 apt、dnf 仓库安装，之后的更新也从那里来，命令见 [apps/flutter/packaging/linux/README.md](../../apps/flutter/packaging/linux/README.md)。
- **macOS**：打开 `PPVPN-<版本>-macos-<架构>.pkg`（Apple 芯片用 `macos-arm64`，Intel 用 `macos-x64`），安装器将应用放入「应用程序」。需要 macOS 13 或更新的系统。

macOS 正式发布的应用和 PKG 使用 Apple Developer ID 签名并经过公证。ZIP 是 Sparkle 的更新包，
包含已装订公证票据的应用；首次安装使用 PKG。旧版本的 DMG 仍保留在原来的下载地址。
应用内更新会校验 EdDSA 签名。请从官网或本仓库的
[Releases](https://github.com/peakpassvpn/ppvpn-client/releases) 下载，并可用 Release 的 `SHA256SUMS` 核对文件。

## 日志位置（测试排障）

特权服务及其增强模式引擎的日志放在卸载、重装服务都不会删除的目录里；每个文件上限 5 MB，写满后滚动为 `<名称>.1.log`、`<名称>.2.log`，每种日志共保留 3 个文件。

| 平台 | 服务 / 增强模式引擎 | 应用（ppvpn-client、标准模式引擎、推送代理） |
|---|---|---|
| macOS | `/Library/Logs/PPVPN/`：`ppvpn-service.log`、`ppvpn-core.log`，以及 launchd 的 `ppvpn-service.out.log` / `ppvpn-service.err.log`（目录 0755、文件 0644，无需 root 即可读取） | `~/Library/Logs/PPVPN/` |
| Linux | `/var/log/ppvpn/`：`ppvpn-service.log`、`ppvpn-core.log`（需 root 读取） | `~/.local/state/ppvpn/logs/` |
| Windows | `%ProgramData%\PPVPN\logs\`：`ppvpn-service.log`、`ppvpn-core.log` | `%LOCALAPPDATA%\PPVPN\logs\` |

服务日志按 info 级别记录每次增强模式连接各阶段耗时（创建引擎、apply-profile、start、停止），应用日志记录客户端侧各步骤耗时（服务检查、冲突预检、Connect、健康检查的各步），慢连接可据此定位。

## 结构

```
apps/
├── flutter/        # Flutter 应用（桌面和移动端）、推送代理（Windows、Linux）、安装包（packaging/）
├── macos/          # 推送代理（macOS）和它的 AppLogic（可在 Linux 上测试）、打包脚本
└── assets/icons/   # 品牌与托盘图标
crates/
├── account/        # 账号代码（与 CLI 共用）：后端 API、设备登录、推送设备注册、Profile 视图
├── desktop/        # 共享客户端核心（会话、连接状态机、健康检查、UniFFI）
├── engine-host/    # 进程内引擎上的 Core API v1（客户端与特权服务共用）
└── service/        # 特权服务：进程内运行增强模式引擎、client↔service IPC、清理旧版残留的系统 DNS（macOS）
tools/desktop/      # build-service.sh、签名、图标与 CI 辅助脚本
docs/desktop/       # 本文、构建说明、CI、真机 QA 清单
```

与仓库其余部分的关系：

- `crates/desktop` 和 `crates/engine-host` 是仓库根 Cargo workspace 的成员，按路径依赖
  `crates/account` 和 `crates/core`；它们不在 workspace 的 default-members 里，
  要在各自目录下或用 `-p` 构建。
- `crates/service` 也是仓库根 Cargo workspace 的成员，用根的 `Cargo.lock`。
- 工具链版本由仓库根的 `rust-toolchain.toml` 固定（1.98.1）。
- 测试在仓库根的 `.github/workflows/ci.yml`（`desktop`、`linux`、`macos`、`windows` job），安装包和发行在 `release.yml`。

## 许可证

与仓库其余部分相同：GPL-3.0-or-later，见仓库根的 [LICENSE](../../LICENSE)。
