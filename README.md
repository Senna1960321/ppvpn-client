# ppvpn-client

[PPVPN (PeakPass VPN)](https://www.peakpassvpn.com) 的客户端仓库：macOS、Windows、Linux 桌面客户端，命令行 `ppvpn`，以及它们共用的 Rust 网络引擎 `ppvpn-core`。Android、iOS 的 Flutter 界面和隧道宿主也在本仓库，当前范围见[移动端接入](docs/mobile.md)。

下载安装包请到[官网](https://www.peakpassvpn.com)或 [GitHub Releases](https://github.com/peakpassvpn/ppvpn-client/releases)。桌面端、CLI 和引擎同一个版本、同一个 Release，见[构建与发布](docs/release.md)。

## 目录

| 路径 | 内容 |
| --- | --- |
| `crates/core` | 引擎 `ppvpn-core`：内嵌 Sail，负责 Profile 校验与转换、路由、探测、本地代理、流量统计 |
| `crates/account` | `ppvpn-account`：登录、设备、会话和 Profile 下载，桌面端和 CLI 共用 |
| `crates/desktop` | 桌面客户端的 Rust 部分 |
| `crates/service` | 增强模式（TUN）的特权服务 |
| `crates/engine-host` | 在进程内承载引擎的宿主 |
| `crates/cli` | 命令行 `ppvpn` |
| `crates/core-lab` | 引擎的实验与压测工具 |
| `apps/flutter` | 桌面和移动端的 Flutter 应用，以及它的安装包 |
| `apps/macos` | macOS 的推送代理（Swift）和打包脚本 |

引擎是 Rust 库 `ppvpn-core`（`crates/core`），由桌面端和 CLI 在进程内承载，见 [#214](https://github.com/peakpassvpn/ppvpn-client/issues/214)。后端只下发版本化、平台无关的 Proxy Profile；引擎负责严格校验、转换为固定版本的内部运行配置。接口版本：Core API `v1`、Profile Schema `1`。支持 Shadowsocks 2022（含多用户/EIH）、VLESS + REALITY 和 AnyTLS。原来的 Go 内核已从 main 移除，最后一版是 v0.5.21，它的 tag 和 Release 文件保留。

## 文档导航

- [五分钟快速开始](docs/quickstart.md)：构建、在 Rust 宿主或 CLI 里运行引擎、调用测试宿主的 Core API
- [架构与生命周期](docs/architecture.md)：模块边界、状态机、原地 reload 与完整重启
- [Backend Profile](docs/backend-profile.md)：逻辑节点/多入口故障转移、完整字段、路由语义、协议示例和后端生成规则
- [Core API v1](docs/core-api.md)：认证、请求/响应、所有方法、DTO、事件及错误码
- [桌面平台接入](docs/desktop.md)：Windows 与 macOS 都由特权 service 以 TUN 模式运行 core
- [移动端接入](docs/mobile.md)：隧道宿主、Android Play / 官网 APK 分发与应用内更新
- [Rust 版行为对照](docs/rust-parity.md)：Go 测试与 Rust `ppvpn-core`（`crates/core`）用例的逐条对照（#214 硬切换的前提）
- [测试分层](docs/testing.md)：L1 单元、L2 集成、L3 系统，各层的依赖、入口和 CI 时机
- [宿主接入](docs/host-integration.md)：Rust API 契约
- [贡献须知](CONTRIBUTING.md)：敏感信息检查与 pre-push 钩子（每个 clone 执行 `git config core.hooksPath .githooks`）
- [安全模型](docs/security.md)：密钥边界、IPC、日志、持久化和威胁假设
- [构建与发布](docs/release.md)：测试门禁、跨平台构建、校验和与发布检查表
- [完成度审计](docs/completion-audit.md)：需求到代码和测试证据的映射

## 开发命令

```sh
make test-unit          # L1
make test-integration   # L2
make test-system        # L3，Linux，需要 sudo
cargo clippy --all-targets --locked -- -D warnings
cargo fmt --all --check
```

分层和 CI 的安排见 [docs/testing.md](docs/testing.md)。

> 下列文档描述的是 Go 内核（v0.5.21），保留作参考：完成度审计、构建与发布中关于 Go 构建的部分。

## 许可证

本仓库以 [GNU GPL v3 或更高版本](LICENSE) 发布。项目包含和依赖的第三方组件可能适用额外条款；参见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。

安全问题请勿通过公开 Issue 披露，报告方式见 [SECURITY.md](SECURITY.md)。
