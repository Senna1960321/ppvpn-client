# 构建与发布

## 分支与版本

- 只有 `main` 一个长期分支，与 PPVPN Desktop 一致；功能与修复分支都向 `main` 提 PR，`ci.yml` 通过后合入。
- Rust `ppvpn-core` 不单独发布二进制：Desktop 和 CLI 从本仓库的源码编译它（#214），桌面端的发行流程在
  `release.yml`。
- 原来的 Go 内核已从 main 移除。它的最后一版是 v0.5.21：tag `v0.5.21` 和同名 GitHub Release 的文件保留，
  Desktop 已改用 Rust 引擎，不再 vendor 它；需要 Go 基线的验收（例如 G4、G6）用这些 Release 文件，
  不再从源码构建。
- 版本号接着 Go 版往上走：Rust 的第一个对外版本是 **0.6.0**（Go 的最后一版是 0.5.21）。引擎、账号、CLI、
  桌面客户端和特权服务用同一个版本，写在根 `Cargo.toml` 的 `[workspace.package] version`，各 crate 用
  `version.workspace = true` 继承，`Cargo.lock` 随之更新。`apps/macos/project.yml` 的 `MARKETING_VERSION`
  与它保持一致，改版本时一起改。
- 一个版本一个 tag `vX.Y.Z`、一个 GitHub Release：里面是三个平台的安装包和 CLI 的归档。发行时安装包的版本
  来自手动触发时填的版本（必须在 tag `v<版本>` 上运行）；CLI 的构建核对它等于 `Cargo.toml` 的版本，不等则
  发行失败。
- Go 内核的 Release 用的是同样的 tag 形式（v0.5.14 到 v0.5.21）。`release.yml` 拒绝 0.6.0 以下的 stable 版本，
  更新站点也只从 v0.6.0 及以后的 Release 构建，所以这些旧 Release 原样保留、不会被当成桌面端的发行。
  stable 发行成为仓库的 latest release（勾选 `allow_downgrade` 的回滚除外）。
- 每个版本的发行说明在 `docs/releases/<版本>.md`，例如 [0.6.0](releases/0.6.0.md)。

## CI

测试按依赖分三层，各个 job 和平台测试的约定见 [测试分层](testing.md)。只有两个 workflow：

- `ci.yml`：PR 推送时跑全部测试，7 个 job 并行（`lint`、`unit`、`integration`、`desktop`、`linux`、`macos`、`windows`）。
- `release.yml`：
  - 安装包是 Flutter 版（`apps/flutter`，#401），文件名、更新源和签名与原生版相同；原生版不再发行。
  - 每次合入 main：三个平台的安装包（`linux`、`macos`、`windows`，不读任何密钥：Windows 不签名、macOS ad-hoc 签名、
    没有更新签名；macOS 的安装包和 CLI 只构建 arm64，x86_64 只在手动触发的运行里构建，#459），
    deb 和 rpm 各在固定的 Ubuntu、Fedora 镜像里离线安装一次（依赖包来自缓存），apt/dnf 仓库自测（`linux-smoke`、`linux-repo`）；依赖变了时保存
    `ci.yml` 恢复的构建缓存（`cache-*`）。什么都不发布。
  - 手动触发：`channel=stable` 在 tag `vX.Y.Z` 上，桌面应用、服务和 CLI 的无密钥构建先运行，不进入
    `desktop-release-stable`；中间产物只保留一天，macOS CLI 的未签名 slice 由同一次运行的签名 job 读取。
    `desktop-release-approval` 审批后才进入 `desktop-release-stable` 签名、公证和打包，
    `release-files` 整体核对，`publish` 发 GitHub Release（安装包和 CLI，
    SHA256SUMS 和构建 provenance），`site` 按 Release 重建更新站点，`site-deploy` 把它上传到 R2。
    `channel=dev` 在 main 上、`desktop-release-dev` 里构建，从不发布。`channel=site` 经同一审批，只按已发布的
    Release 重建并上传更新站点。
  - 构建号是 `DESKTOP_BUILD_OFFSET` 加 `release.yml` 的 run number，只增不减。

## 后端地址

客户端连接的后端由构建时的 `PPVPN_API_BASE_DEFAULT` 决定，代码和 workflow 里都不写域名（#517）。它的值是一个 https 源（例如 `https://www.example.com`，不带路径）：

| 名称 | 类型 | 位置 | 用途 |
| --- | --- | --- | --- |
| `PPVPN_API_BASE` | 变量 | 仓库 | 正式后端：合入 main 的构建、`ci.yml` 的应用构建、stable 发布（包括不在环境里的 job，如 Linux CLI） |
| `PPVPN_API_BASE` | secret | 环境 `desktop-release-dev` | dev 构建的后端 |

- 正式后端的主机本来就是公开的，所以用变量；变量会原样出现在公开仓库的运行日志里。dev 后端的主机不能公开，所以 dev 构建读 secret，日志里会被遮住。不要把 dev 后端放进变量。
- `desktop-release-stable` 里不设 `PPVPN_API_BASE`：stable 只读仓库变量。

- 值为空时构建失败：`apps/flutter/scripts/ci-build.sh`（桌面、Android、iOS）、`tools/cli/build-release.sh`、`tools/desktop/ci/build-macos-native.sh` 都会检查。
- 应用里，登录页只信任这个地址的主机（后端按客户端请求的主机返回 `verification_uri`），增强模式的健康检查对它的主机做完整检查，应用打开的网页（购买、路由规则、手机「我的」里的页面）也从它推出。
- 本地构建不设它时：Flutter 应用需要环境变量 `PPVPN_API_BASE`（或调试构建的设置项），CLI 按 dev 构建处理，也需要 `PPVPN_API_BASE`。
- 换后端域名只改变量，不改代码。
- 原生版（`apps/macos`、`apps/windows`、`apps/linux`）不再发行，它们的构建文件里还留着旧的默认地址，没有改。

## Windows 签名（Azure Artifact Signing）

`windows` job 在发布环境（`desktop-release-dev`、`desktop-release-stable`）里、下表的变量都在时用
Azure Artifact Signing 给程序、安装包和卸载程序签 Authenticode（#489）：`azure/login` 用 OIDC 登录 Entra 应用
（它对每个环境有 federated credential，并在证书配置上有 Certificate Profile Signer 角色），
`tools/desktop/ci/artifact-signing.ps1` 装好 SignTool 的签名插件（版本和 SHA-256 钉在脚本里），
`apps/flutter/scripts/sign-file.ps1` 逐个文件签名、加时间戳并验证。缺任何一个就不签名，和以前一样。
都不是 secret，放在两个环境里：

| 名称 | 内容 |
| --- | --- |
| `AZURE_CLIENT_ID`、`AZURE_TENANT_ID`、`AZURE_SUBSCRIPTION_ID` | Entra 应用、租户和签名账号所在的订阅 |
| `ARTIFACT_SIGNING_ENDPOINT` | 签名账号所在区域的端点，`https://eus.codesigning.azure.net/` |
| `ARTIFACT_SIGNING_ACCOUNT`、`ARTIFACT_SIGNING_PROFILE` | 签名账号和证书配置的名字 |
| `PPVPN_WINDOWS_PUBLISHER_EKU` | 服务的发布者钉：签名证书的身份 EKU（`1.3.6.1.4.1.311.97.*`，[安全](security.md)） |

`PPVPN_WINDOWS_PUBLISHER_EKU` 在第一次签名的 dev 构建之后才设：那次构建的日志（`check-publisher.ps1`）
打印签名证书的主题和 EKU。dev 在设上之前仍可用于取得这个钉；stable 必须设好，缺失时构建失败，不能退回接受未签名客户端。

stable 的服务在审批前编译，所以 **仓库变量**也必须设置 `PPVPN_WINDOWS_PUBLISHER_EKU`，以及启用轮换时的
`PPVPN_WINDOWS_NEXT_PUBLISHER_EKU`。它们是公开的证书身份，不是密钥；Azure 登录配置仍只放在发布环境里。
环境里若保留同名 EKU 变量，必须与仓库值一致。服务产物记录编译时的两个钉和是否接受未签名客户端，
打包阶段逐项核对，再核对程序签名；值不一致直接停止，不重编服务或降低校验要求。

`tools/cli/build-release.sh macos` 默认仍一次构建并打包。流水线用 `PPVPN_CLI_STAGE=build` 只编译 slice，
审批后用 `PPVPN_CLI_STAGE=package` 消费同一次运行的 slice；后一步不编译，仍核对版本、架构、签名和公证。

## 更新站点（R2）

更新站点 `https://pkg.peakpassvpn.com`（仓库变量 `DESKTOP_SITE_BASE`）是 Cloudflare R2 的一个 bucket，
不是 GitHub Pages（Pages 上限 1 GB，只 apt/dnf 仓库 5 个版本就约 600 MB）。内容：

- `desktop/channels/stable.json`（频道指针）和 `desktop/stable/appcast-<平台>.xml`（macOS、Windows 的 Sparkle 源）；
- 最新版本的 macOS 和 Windows 安装包 `desktop/stable/<版本>/<构建号>/<文件>`：appcast 的 enclosure 和频道指针的
  `url` 指向这里，而不是 GitHub Release（经 Cloudflare 缓存；GitHub 的下载在一些网络下很慢）。文件就是 Release
  里那份（`site` 先按 SHA256SUMS 和 provenance 校验），Sparkle/WinSparkle 再校验 EdDSA 签名；Release 里的
  资产不动，旧的下载地址照旧可用。每个版本约 145 MB（两个 DMG 加 Windows 安装包；macOS 的 .pkg 只用于首次安装，
  不在更新站点上）；
- `linux/`：apt 仓库（`linux/apt`，suite `stable`，保留最近 5 个版本的 deb）、dnf 仓库（`linux/rpm/stable`，
  rpm 本身从 GitHub Release 下载）、安装源设置包、公钥和 `linux/stable/latest.json`（Linux 应用的更新检查）；
- 已停用的 dev 频道：`linux/dev/latest.json` 和 `desktop/dev/appcast-<平台>.xml` 是 stable 的副本，让以前装的
  dev 构建收到 stable 的更新。dev 的 apt suite 和 `linux/rpm/dev` 不再写，bucket 里旧的原样留着。

每次都从 GitHub Releases 重建整个站点（`site`，带签名密钥），再由 `site-deploy`（只带 R2 的密钥）用 S3 API
（runner 自带的 AWS CLI，`tools/desktop/ci/upload-site.sh`）覆盖上传：先传不会变的文件（pool 里的包、by-hash
索引、repodata 数据文件、带版本的设置包），再传索引（`Packages*`）、签名的索引（`Release`/`InRelease`/
`repomd.xml`）、更新检查（`latest.json`、appcast），频道指针最后。不变的文件 `Cache-Control: public,
max-age=31536000, immutable`，其余 `no-cache`；bucket 里已有、内容不同的不变文件会被拒绝，整个上传不开始。
不删除任何对象：客户端已经拿到的索引所指的文件一直在，旧版本的包、安装包和旧的 by-hash 文件会累积，需要时手动清理
（安装包只有当前频道指针所指的版本还在用）。

需要在仓库里建的（名字固定）：

| 名称 | 类型 | 放在 | 内容 |
| --- | --- | --- | --- |
| `R2_ACCOUNT_ID` | 变量 | 环境 `desktop-release-stable` 或仓库 | Cloudflare 账号 ID（32 位十六进制） |
| `PPVPN_PKG_R2_BUCKET` | 变量 | 环境 `desktop-release-stable` 或仓库 | `pkg.peakpassvpn.com` 背后的 bucket 名 |
| `PPVPN_PKG_R2_ACCESS_KEY_ID` | secret | 环境 `desktop-release-stable` | 只对该 bucket 有读写权限的 R2 API token |
| `PPVPN_PKG_R2_SECRET_ACCESS_KEY` | secret | 环境 `desktop-release-stable` | 同上 |

`DESKTOP_SITE_BASE`（仓库变量，`https://pkg.peakpassvpn.com`）保持不变。不再需要 GitHub Pages 和
`github-pages` 环境。

演练（`rehearsal`，在 main 上的 stable 运行）照常构建、签名、出草稿 Release，按本次的文件构建站点，作为
artifact `site` 保留三天，并在日志里打出上传的顺序和缓存设置（`upload-site.sh --plan`）。审批之后，
`site-check` 在 `desktop-release-stable` 里带着 R2 的密钥只读地核对一遍（`upload-site.sh --check`）：
列出桶里 `linux/` 和 `desktop/` 下的对象，像上传那样比对不可变文件（内容不同就失败），并列出真正上传时
会覆盖的现有文件，结果写在运行摘要里。演练不上传任何东西；密钥的写权限在第一次正式发布时才用到。

### 官网下载（私有桶）

官网下载页的五个按钮（`macos-arm64`、`macos-x64`、`windows-x64`、`linux-x64-deb`、`linux-x64-rpm`）和 0.6.0
之前的应用的更新检查都走后端：`/api/v1/desktop/native/releases/<平台>/download` 跳转到私有 R2 桶里安装包的
短时签名地址，`/api/v1/desktop/releases/<平台>/appcast.xml` 是 Sparkle 源。后端没有发布记录，每次请求都读桶里
的渠道指针，所以发布就是上传文件、再移动指针：

- `clients/desktop/native/releases/<版本>/<构建号>/<安装包>` 和同目录的 `release-meta-<平台>.json`，不可变；
- `clients/desktop/native/channels/stable/release-meta-<平台>.json`：指针，即 release-meta 加上 `path`（安装包的 key）。

macOS 从 0.6.2 起另有首次安装用的 `PPVPN-<版本>-macos-<架构>.pkg`（与 DMG 同在 Release、SHA256SUMS 和 provenance 里）。
它的 release-meta 里 `file`、`length`、`sha256`、`ed_signature` 仍是 DMG 的（Sparkle 更新用），另加
`install_file`、`install_length`、`install_sha256`；上传时指针再加 `install_path`（`.pkg` 的 key，与 `path` 同一个
`<版本>/<构建号>` 目录）。这四项要么都有且都对，要么都没有；只允许出现在 macOS 上。后端遇到不全或不对的一组会让整个
指针 503（下载和旧版更新源一起停），所以 `upload-downloads.sh` 在 `--plan` 阶段就按同样的规则拒绝，演练和 PR 的测试
都能发现。有这四项时 `/download` 给 `.pkg`，后端的旧版更新源仍指向 DMG。

`downloads-deploy`（`tools/desktop/ci/upload-downloads.sh`）在 GitHub Release 发出之后，用同一批文件（artifact
`release-files`）上传：先按 release-meta 核对每个安装包（长度、SHA-256，macOS 和 Windows 还要 `ed_signature`），
桶里已有而内容不同的发布文件直接拒绝、内容相同的保留（失败后可以重跑），全部传完才移动五个指针。指针一动，
官网下载的就是这个版本，旧版应用也会收到更新提示。演练时由 `downloads-check` 只读地做同样的核对
（`--check`），并列出每个指针现在指向的版本，不写任何东西。

| 名称 | 类型 | 放在 | 内容 |
| --- | --- | --- | --- |
| `R2_CLIENT_BUCKET` | 变量 | 仓库 | 后端读的私有 bucket 名 |
| `R2_ACCESS_KEY_ID` | secret | 环境 `desktop-release-stable` | 只对该 bucket 有对象读写权限的 R2 API token |
| `R2_SECRET_ACCESS_KEY` | secret | 环境 `desktop-release-stable` | 同上 |

## v0.5.21 的发布文件

v0.5.21 的文件由当时的 `release.yml` 在 tag 上构建，带 GitHub artifact attestation。仓库当时叫 `ppvpn-core`（后改名 `ppvpn-client`），attestation 记的是旧名，所以校验时仍写旧名。下载后校验：

```sh
sha256sum -c --ignore-missing SHA256SUMS   # macOS: shasum -a 256 -c --ignore-missing SHA256SUMS
gh attestation verify ppvpn-core-linux-amd64 \
  --repo peakpassvpn/ppvpn-core \
  --signer-workflow peakpassvpn/ppvpn-core/.github/workflows/release.yml \
  --source-ref refs/tags/v0.5.21
```

`--ignore-missing` 允许只下载部分文件。Windows 没有 `sha256sum`，用 `Get-FileHash -Algorithm SHA256 <文件>` 与 `SHA256SUMS` 中的值比对。
只带 `--repo` 时，attestation 只证明文件由本仓库的某个工作流构建；加上 `--signer-workflow` 和
`--source-ref` 才把它限定为 `release.yml` 在该 tag 上的构建。

Go 内核的构建、移动端产物和 Go 时代的发布流程见 tag `v0.5.21` 上的本文件。

## 固定工具链

- Rust：`rust-toolchain.toml`（与 Sail 的 CI 和发行一致）；改它时同时改 `ci.yml` 里的版本。
- Sail：`Cargo.toml` 按提交固定（`[workspace.dependencies]`），`[patch.crates-io]` 与该提交的 Sail 保持一致，
  CI 的 lint job 会核对。
- BoringSSL：用 btls 为所固定提交发布的预编译库（校验 `SHA256SUMS` 和构建 attestation），不在 CI 里编译。
- 发行构建的 crate 来自 Actions 缓存（`.github/actions/cargo-home`，键为 Cargo.lock 的哈希，只在 main 上保存），之后 Cargo 离线，构建用的正是缓存里的 crate；缓存未命中时先 `cargo fetch --locked`。
- 测试工具里的 Go（`test/go.mod`）：只用于测试时启动的辅助程序（假节点、`ldnslab` 等），不进入任何发行物。

## 发布检查表（Desktop 和 CLI 的发行）

- 发行提交的 PR 上 `ci.yml` 全部通过，没有只改文档而跳过的步骤。
- 源码归档包含 `LICENSE`、`THIRD_PARTY_NOTICES.md` 和对应版本的依赖清单。
- 发行构建不含测试专用 feature（`tools/release-features-check.sh`）。
- Profile golden 变化已人工审阅，文档示例与公开 API（`docs/host-integration.md`）同步。
- 上层产品完成平台签名、公证/发布签名、权限声明和升级演练。
- 发布环境不包含 Profile、会话密钥、本地代理状态或开发日志。
