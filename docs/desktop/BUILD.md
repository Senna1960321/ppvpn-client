# 构建

命令都在仓库根执行，路径相对仓库根。完整步骤在 `apps/flutter/README.md`（三个平台的 Flutter
桌面应用和安装包）和 `apps/macos/README.md`（macOS 打包与保留的旧代理代码）。

工具链：Rust 1.98.1（仓库根的 `rust-toolchain.toml`）；Flutter（`apps/flutter/scripts/install-flutter.sh`）；
macOS 用 Xcode 和 Swift；当前打包不再需要 XcodeGen，只有手动构建旧代理才需要它。

## 共用的前置步骤

引擎是 Rust `ppvpn-core`，客户端和特权服务都在进程内链接它（经 `crates/engine-host`），没有单独的
core 二进制。

```bash
# 客户端 crate（仓库根 workspace 的成员，在进程内链接 Rust ppvpn-core 和 sail）
(cd crates/desktop && cargo test --locked)

# 特权服务（workspace 成员，增强模式的引擎在它的进程里）
cargo build --locked -p ppvpn-service --profile service --bins
```

## 各平台

- 三个平台的桌面应用都是 Flutter 版（`apps/flutter`），`apps/flutter/scripts/ci-build.sh` 构建。
- **macOS**：`apps/flutter/scripts/package-macos.sh` 用 `packaging/macos/SparkleArtifacts` 独立解析
  Sparkle，不再 bootstrap 或构建旧代理/FFI。签名构建所需的 APNs profile 见 [发布配置](../release.md#macos-apns-签名与配置)。
- **Linux、Windows**：deb/rpm 由 `apps/flutter/scripts/package-linux.sh` 打包，Windows 安装包由
  `apps/flutter/scripts/package-windows.ps1` 打包（MSVC 工具链），两者都自己构建 service。Windows
  安装包里另有 `wintun.dll`（增强模式的 TUN 驱动，`package-windows.ps1` 下载并按哈希校验）。

## 嵌入 app 的二进制

```bash
# 构建特权服务（macos / windows），产物放到 build/binaries/
tools/desktop/build-service.sh macos
```

macOS 的打包（`apps/flutter/scripts/package-macos.sh`）需要这些产物。

## 安装包工作流

安装包由本仓库的 `release.yml` 构建；PR 的 macOS 打包检查在 `ci.yml` 的 `flutter` job 里。
触发条件见 [docs/desktop/ci.md](ci.md)，手工验收见 [QA 清单](qa-checklist.md)。
