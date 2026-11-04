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
- **macOS**：打开 DMG（Apple 芯片用 `macos-arm64`，Intel 用 `macos-x64`），把 PPVPN 拖进"应用程序"。需要 macOS 13 或更新的系统。

### macOS：首次打开被拦截

macOS 版用 PPVPN 自己的固定证书签名，没有经过 Apple 公证（还没有 Apple Developer ID）。从浏览器下载后，系统会
先拦下 DMG，再拦下应用，各要放行一次，提示是"无法验证……是否包含恶意软件"：

1. **DMG**：双击 DMG，在提示里点"完成"；马上打开"系统设置 → 隐私与安全性"，拉到最下面的"安全性"一段，点
   "已阻止使用 PPVPN.dmg"旁边的"仍要打开"，输入本机密码确认，DMG 挂载。
2. **安装**：把 PPVPN 拖进"应用程序"，然后推出 DMG。不要在 DMG 里直接运行 PPVPN：那样运行的是系统隔离出来的
   临时副本，放行它不算数。
3. **应用**：从"应用程序"双击 PPVPN，在提示里点"完成"；再到"隐私与安全性"点"已阻止使用 PPVPN"旁边的"仍要打开"，
   输入密码确认。

macOS 13、14 也可以在访达里按住 Control 点（或右键点）DMG 和 PPVPN，选"打开"，再在对话框里点"打开"。

不要用 `xattr -dr com.apple.quarantine` 去掉隔离标记来跳过这两步。去掉之后系统不再询问，也就不会记下你的放行；
而 macOS 仍然不认这张证书，应用会停在启动前，Dock 图标一直跳、没有窗口。已经这样做了的话：在"活动监视器"里
强制退出 PPVPN，把它移到废纸篓，重新下载 DMG，按上面的步骤再来一次。

放行之后不会再问。应用内的自动更新不需要再放行：更新包由应用自己下载，用更新签名（EdDSA）校验。请只从官网或
本仓库的 [Releases](https://github.com/peakpassvpn/ppvpn-client/releases) 下载，可以用 Release 里的 `SHA256SUMS`
核对文件。

## 日志位置（测试排障）

特权服务及其增强模式引擎的日志放在卸载、重装服务都不会删除的目录里；每个文件上限 5 MB，写满后滚动为 `<名称>.1.log`、`<名称>.2.log`，每种日志共保留 3 个文件。

| 平台 | 服务 / 增强模式引擎 | 应用（ppvpn-client、标准模式引擎、推送代理） |
|---|---|---|
| macOS | `/Library/Logs/PPVPN/`：`ppvpn-service.log`、`ppvpn-core.log`，以及 launchd 的 `ppvpn-service.out.log` / `ppvpn-service.err.log`（目录 0755、文件 0644，无需 root 即可读取） | `~/Library/Logs/PPVPN/` |
| Linux | `/var/log/ppvpn/`：`ppvpn-service.log`、`ppvpn-core.log`（需 root 读取） | `~/.local/state/ppvpn/logs/` |
| Windows | `%ProgramData%\PPVPN\logs\`：`ppvpn-service.log`、`ppvpn-core.log` | `%LOCALAPPDATA%\PPVPN\logs\` |

服务日志按 info 级别记录每次增强模式连接各阶段耗时（创建引擎、apply-profile、start、停止），应用日志记录客户端侧各步骤耗时（服务检查、冲突预检、Connect、健康检查的各步），慢连接可据此定位。

增强模式启动失败时，界面显示本地化说明；能识别的驱动、权限、资源占用及启动后健康检查错误各有对应提示。点「复制诊断信息」可取得应用版本、平台、增强模式标识、错误码，以及可识别的原因码和启动阶段，手动附到工单。复制内容不包含原始服务错误详情、路径、域名或凭据，也不会自动上传。

服务启动错误保留原来的前置错误码，并附上 `[start_stage=...]`：`create` 是创建引擎，`version` 是读取版本能力，`profile` 是校验和应用配置，`start` 是启动 TUN 与路由。阶段只缩小排查范围，不能单凭 `start` 判断是驱动、路由还是 DNS 故障；仍需结合原因码与本机日志定位。

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
