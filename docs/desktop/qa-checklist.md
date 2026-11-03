# 真机 QA 清单

CI 和虚拟机、容器覆盖不到的项目，发版前在真机上逐项过一遍；切换到 Rust 引擎后的第一次完整执行就是
#214 的 G5（三端实机验收），结果记在 #214。

每项记录平台、系统版本、app 版本与 build、引擎版本（应用日志里 `standard core: in-process Rust engine created`
一行的 `core=`）和结果；失败时附上日志（位置见 [docs/desktop/README.md](README.md)）。引擎在进程内运行，没有单独的
core 进程：标准模式的引擎日志是应用日志目录里的 `ppvpn-core.<日期>.log`，增强模式的是服务日志目录里的
`ppvpn-core.log`。

**矩阵**：Windows x64；macOS Apple 芯片和 Intel 各一台；Linux 用 NetworkManager（完整的 Ubuntu Desktop）
和 systemd-networkd 各一台。每个平台测标准模式和增强模式，增强模式下 rules 和 global 两种路由都测。
需要 Go 0.5.21 作对照的项，在同一台机器、同一个节点上用 Go 内核的最后一个版本（dev 0.2.90）测基线。

**远程测试机上的增强模式**：只能通过网络登录的机器（虚拟机、共享测试机），开增强模式前先挂一个自救定时任务，
TUN 或路由出问题、机器连不上时能自己恢复；确认还能登录后再取消。

- Linux：`systemd-run --on-active=5min --unit=ppvpn-rescue systemctl stop ppvpn-service`，取消用
  `systemctl stop ppvpn-rescue.timer`。
- Windows：`schtasks /Create /TN ppvpn-rescue /SC ONCE /ST <5 分钟后的时刻> /RU SYSTEM /TR "sc stop ppvpn_service"`，
  取消用 `schtasks /Delete /TN ppvpn-rescue /F`。
- macOS：`sudo sh -c 'sleep 300; launchctl bootout system/com.peakpassvpn.ppvpn.service' &`，取消就结束这个后台进程。

停服务会撤掉 TUN、路由和规则；客户端随后显示连接失败，重新连接即可。

## 安装与更新

- [ ] 从 Go 内核的版本（0.2.90）升级到 Rust 版：登录、设置、选中的节点、入口 pin、路由模式和本地代理凭据都保留
      （浏览器扩展不需要重新复制凭据）。
- [ ] Rust 版之间升级（rc → rc）：同上。
- [ ] 重启电脑后本地代理凭据不变。

## macOS

- [ ] 首次安装（.pkg）：浏览器下载正式发布的 `PPVPN-<版本>-macos-<架构>.pkg`，确认 Developer ID 签名和公证有效，
      无需「仍要打开」放行，由安装器正常安装；`/Applications/PPVPN.app` 属于当前用户（`ls -ld`），没有 `com.apple.quarantine`，
      打开时不再提示，进程路径是 `/Applications/PPVPN.app/...`（没有被移位到 AppTranslocation）。
- [ ] 首次安装后增强模式：第一次增强连接要一次管理员密码（安装特权服务），之后连上。
- [ ] Sparkle ZIP 更新：分别从当前版和 0.6.0 之前的旧版更新到 ZIP 构建（新版与 legacy feed 各一次），更新后能正常启动，无 Gatekeeper 提示。
- [ ] 应用内更新（Sparkle）：更新安装后直接打开，不需要再次放行。
- [ ] Sparkle 更新时推送助手先退出：安装前应用注销 PPVPN Agent（日志 `push agent: unregistered for the update`），
      系统日志里没有 `Launch Constraint Violation`，也没有新的"PPVPN Agent"崩溃报告；新版启动后几秒内
      `launchctl print gui/<uid>/com.peakpassvpn.ppvpn.desktop.agent` 显示 running，推送日志里重新出现 `push agent: Running`。
      从 0.6.1 及更早版本更新时旧版还没有这一步，仍会出现一次上述终止，约 1 分钟后恢复。
- [ ] Sparkle 更新后替换特权服务：从已装过服务的上一版（例如 0.6.0 build 133）用 Sparkle 更新到新版，
      新版的 `crates/service`、`crates/engine-host`、`crates/core` 或 `Cargo.lock` 有改动。更新后服务不会被
      替换，打开应用和标准模式连接也不会。第一次增强模式连接前：弹出管理员密码框；
      `/Library/PrivilegedHelperTools/com.peakpassvpn.ppvpn.service.bundle/Contents/MacOS/ppvpn-service` 的
      SHA-256 等于新版应用内置的那份，服务进程的 pid 换了；应用日志里先有
      `privileged service differs from this build; reinstalling`，再有 `privileged service reinstalled`。
      在密码框里取消时，连接照常用旧服务继续；应用重启之前不再询问（每次运行只试一次）。
- [ ] 换 Wi‑Fi 后直连域名仍能解析（引擎的 dns-local 自己读取物理网卡的 DNS）。
- [ ] Wi‑Fi 与有线之间切换后连接恢复，直连与代理都可用。
- [ ] 同时开着另一个 VPN 时，引擎日志里 `msg="local dns servers"` 的 interface 是物理网卡，
      不是对方的虚拟网卡。
- [ ] 增强模式已连接时，`scutil --dns` 里只有一份指向 TUN 的 resolver；断开后没有残留。
- [ ] 增强模式的路由完整性（N2，Sail 的路由管理在 macOS 上的第一次实机验证）：已连接时用 `sudo route delete`
      删掉 TUN 的路由，引擎补回（引擎日志里 `TunRoutingBroken` 之后有 `TunRoutingRestored`），期间没有流量绕过
      TUN；补不回来时进入 `Fatal` 并停止，应用提示连接失败，没有残留路由。
- [ ] 兼容模式（系统代理）：与其他会设置系统代理或系统 DNS 的软件共存时，开启和关闭都不残留、
      不覆盖对方的设置。
- [ ] 兼容 → 增强 → 兼容 → 增强来回切换（开始时 Surge 之类的软件开着系统代理，中途退出它）后，系统代理
      要么是对方的设置，要么关闭，不会停在已经没有监听的 7891 上（#536）。

## Windows

- [ ] 下载并运行安装包：SmartScreen 与 UAC 的提示和发布者显示符合预期（未做受信任签名时发布者为「未知」，
      SmartScreen 选「仍要运行」后能安装）。
- [ ] 应用内更新（WinSparkle）：更新过程中的 UAC 提示正常，更新后版本正确、连接可用。
- [ ] 增强模式的路由完整性（N2）：已连接时用 `route delete` 删掉 TUN 的路由，引擎补回，期间没有流量绕过 TUN；
      补不回来时进入 `Fatal`，strict_route 的过滤器随运行时一起撤掉，断开后能正常上网。
- [ ] 增强模式下切换默认网卡（Wi‑Fi ↔ 有线）后直连域名能解析（B7：dns-local 的查询不进 TUN 回环）。
- [ ] 精确式触控板平移时，日志页的「跟随」状态正确。

## Linux

在完整的桌面系统上测；没有 udev 的容器里测不了网卡事件。

- [ ] 增强模式已连接时把物理网卡 down 再 up：策略路由规则（`ip rule`）仍在或被立即补回，
      期间没有流量绕过隧道泄漏；恢复时间不超过 Go 版（约 1 秒）。
- [ ] systemd-networkd 那台：重启 networkd 删掉 `ip rule` 后被补回。

## 所有平台

- [ ] 整机 failover：当前节点的入口整体不可用时切换到其他入口，连接恢复；原入口恢复后约 15 秒才切回。
- [ ] 入口 pin：pin 到一条入口后断开再连接、重启应用、重启服务，pin 都还在；pin 的入口不可用时有提示；
      刷新 Profile 去掉该入口后提示 pin 已清除，回到自动。
- [ ] 热切换：rules 和 global 互切、刷新 Profile 时，已有的长连接（例如下载）不断开。
- [ ] 睡眠和唤醒后连接自动恢复，直连和代理都可用。
  - 日志里有 `the system goes to sleep` 和 `the system woke`（标准模式和增强模式各一对）；睡眠不到 2 分钟时，正在下载的连接不断。
- [ ] 本地代理：浏览器扩展用复制的凭据能连上；把凭据文件改坏（或在 macOS、Linux 上改成其他用户可读）后重新打开
      应用，出现「本地代理账号已重置」提示，扩展重新复制凭据后可用，关掉提示后不再出现。
- [ ] 服务被强杀（macOS `sudo kill -9`、Windows 任务管理器结束、Linux `systemctl kill -s KILL`）后：
      没有残留的路由、`ip rule`、Wintun 网卡、utun 路由或 DNS 设置，能正常上网；服务重启后可再次连接。
- [ ] 内存：空闲未连接、增强模式连接后空闲、下载中三种场景，记录 app 进程和服务进程的 RSS，
      与 Go 0.5.21 基线对比（阈值见 #214：不超过 Go 的 120%；Windows 和 Linux 已有数据时只补 macOS）。
