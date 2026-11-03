# 移动端接入

本文说明 Rust 引擎给 iOS 和 Android 宿主提供什么、还缺什么。接口契约见 [宿主接入](host-integration.md) 第 11 节，计划和分工见 #448。

Android 的 Firebase 配置、后台 token 注册、VPN 内的备用推送和验收步骤见 [Android 通知接入](android-push.md)。

## 结构

```
界面进程（Flutter + Rust 客户端状态） ── Unix socket，Core API v1 ──  隧道进程
                                                                    VpnService / NEPacketTunnelProvider
                                                                      │ 几个 C 函数（JNI / C ABI）
                                                                    Rust 隧道宿主：Engine + engine-host::dispatch
```

- 引擎只在隧道进程里运行，和系统 VPN 同生共死，不在界面进程里长期运行。
- 界面进程和桌面一样说 Core API v1，只是 transport 换成到隧道进程的 Unix socket。
- 原生层只调隧道宿主的几个 C 函数：启动、停止、网络变化、休眠和唤醒，Android 上还有 socket 保护。完整的引擎 FFI 不做。

## 现状

- **已有：** 引擎的移动端接口（[宿主接入](host-integration.md) 第 11 节）：
  - `tun_settings`：宿主照着它建隧道；
  - `TunConfig::with_fd`：宿主建好的 TUN 交给引擎；
  - `SocketProtect`：Android 的 socket 保护；
  - `Engine::set_network_state`：宿主推送网络；
  - mobile profile 和 `max_connections`。
- **隧道宿主（`crates/tunnel-host`）：**
  - C ABI：`include/ppvpn_tunnel.h` 是契约，函数有 settings、start、stop、network_changed、sleep、wake；
  - 隧道 socket 上的 Core API v1，报文格式见 #448；
  - 隧道进程里的健康检查（见下）；
  - iOS 的 XCFramework：`scripts/build-xcframework.sh <输出目录>`，产物是 `PPVPNTunnel.xcframework`（真机和模拟器的静态库，模块名 `PPVPNTunnel`）和 `PPVPNTunnel.ldflags`（扩展要链接的系统库）。只放进网络扩展，不放进 app。
  - Android 的 JNI：`scripts/build-android.sh <输出目录>` 产出 `<abi>/libppvpn_tunnel.so`（arm64-v8a、x86_64，放进 app 的 jniLibs），Kotlin 类 `com.peakpassvpn.ppvpn.tunnel.TunnelHost`（`apps/flutter/runner/android`）。只有 `:tunnel` 进程的 TunnelService 加载它。函数同 C ABI，错误同样以 JSON 字符串返回；`Listener` 的 `protect(fd)` 转给 `VpnService.protect`，`onExit(reason)` 即 C ABI 的 on_exit。
- **Android 的 TunnelService（`apps/flutter/runner/android`）：** 按 `settings` 用 `VpnService.Builder` 建 TUN，把非 VPN 的最佳网络作为 NetworkState 推给宿主（启动前先推第一份，和 iOS 一样），再在 TUN 的 fd 上启动宿主，socket 是 `filesDir/tunnel.sock`，状态目录是 `filesDir/tunnel-state`。宿主开始服务后报告 on。`apps/flutter/scripts/ci-build.sh` 把两个 ABI 的 `libppvpn_tunnel.so` 放进 APK。
- **CI：** iOS、Android 的构建和 Linux 上的测试；Android 模拟器上的冒烟测试会拉起系统 VPN，经 `tunnel.sock` 调用宿主，再断开。
- **发布（`release.yml`）：** 合入 main 时，`ios` 和 `android` 两个任务都只构建，不签名、不上传，用来提前发现构建问题。stable 渠道（正式发布和演练都算）在批准后才签名上传：iOS 传到 TestFlight，Android 的 App Bundle 用 Play 上传密钥签名后传到 Google Play 的内部测试轨道（`tools/desktop/ci/play_upload.py`）。versionCode 用发布的构建号。
  - Android 用 `desktop-release-stable` 里的这几个 secret：
    - `ANDROID_UPLOAD_KEYSTORE`：上传密钥库，base64 编码；
    - `ANDROID_UPLOAD_KEYSTORE_PASSWORD`、`ANDROID_UPLOAD_KEY_ALIAS`、`ANDROID_UPLOAD_KEY_PASSWORD`；
    - `PLAY_SERVICE_ACCOUNT_JSON`：对这个 App 有发布权限的服务账号密钥；
    - `ANDROID_GOOGLE_SERVICES_JSON`（可选）：Firebase 的完整客户端 JSON 文本，不是 base64。没有配置时构建仍可运行，Android 推送只能在 VPN 运行期间使用自建通道；本地配置方法见 [Android 通知接入](android-push.md#构建配置)。
  - 还没有上传密钥或服务账号时，stable 渠道照样构建，但用 debug 密钥签名，不上传，只给出一条警告。
  - Google Play 规定新 App 的第一个包只能在 Play Console 里手动上传，stable 运行产出的 App Bundle 会作为 artifact 保留，供手动上传使用。
  - 发布状态由仓库变量 `PLAY_RELEASE_STATUS` 决定，默认 `completed`。App 从未发布过时，Play 只接受 `draft`。
- **还没有：**
  - 带 profile 的真实连接（要真机和真实节点，CI 里没有）；Android 的 always-on 启动。

## 隧道进程里的健康检查

界面进程在后台会被挂起，而断线多发生在这时（锁屏、切网络），所以检查和恢复放在隧道宿主里，规则同桌面（`ppvpn_engine_host::health`，#438）：

- **一轮检查：** 引擎在运行（含 degraded），有选中的节点，且它的入口 TCP 探测成功。桌面的直连请求、捕获检查和经本地代理的路径检查在手机上做不了：隧道实例没有本地代理，所有流量都进隧道。
- **恢复：** 在进程内停止再启动引擎，TUN 不动，系统的 VPN 图标不闪。每个恢复周期一次；两轮健康或用户的一次连接（`/v1/start`）开始新的周期。
- **放弃：** 同一周期里第二次需要恢复，或引擎 `Fatal`（无法自愈，而 profile 只在界面进程里）：经 `on_exit` 通知原生层结束隧道，免得设备卡在一个不通的隧道后面。
- **离线：** 原生层报告断网时不检查，这时的失败与隧道无关。
- **报告：** 每一步在 `/v1/watch` 上写一帧 `{"kind": "health", "state": "healthy" | "settling" | "rechecking" | "recovering" | "failed", "detail"?}`。

## 移动端的 TUN

`Role::Tun` 加 `Platform::Ios` 或 `Platform::Android` 时，引擎生成的 TUN 配置和桌面不同：

- **隧道由宿主建立：** 引擎不设置 `auto_route`、`strict_route` 和路由层的地址排除，也不给 TUN 网卡起名。入口地址在路由规则里照样直连。
- **IPv4 和 IPv6 都进隧道（#511）：** 隧道有一个 IPv6 ULA 地址，IPv6 路由是 `::/0`，所以双栈网络上 app 的 IPv6 不会绕过 VPN。MTU 是 9000，和 `tun_settings` 一致。
- **没有 IPv6 出口时（hand-off）：** 和桌面一样。宿主推送的网络没有全局 IPv6 地址时，直连的全局 IPv6 目的地按域名改走 IPv4，直连解析只取 IPv4；代理流量照常发往节点。网络变化后重新判断，变了就热切换（`KernelSwitched`），`host ipv6` 日志行记录判断结果。还没推送网络或断网时按有 IPv6 出口处理。
- **与桌面相同的前导规则：** 嗅探、DNS 劫持、隧道自身网段和 fake-ip 段的拒绝、私网直连。
- **DNS：** 宿主给系统的 DNS 地址是隧道网段里的 `10.60.159.90`。查询进隧道后由 DNS 劫持应答；劫持排在拒绝隧道网段的规则之前。
- **入口探测的 ICMP 不需要特权：** iOS 用 `SOCK_DGRAM` ICMP socket；Android 需要 `net.ipv4.ping_group_range` 包含当前组，否则报 `ICMP_UNSUPPORTED`。

## 宿主的职责

这些由隧道宿主和原生层负责：

- 让引擎实例与系统 VPN 的生命周期同生共死；
- VPN 权限授权、前台服务、Always-on VPN、进程重建和通知；
- 持久化选中节点、ingress pin 和 `routing_mode`，每次 apply 一起传入；
- Profile 里有节点凭据。隧道进程只经 socket 随 `apply` 拿到它，自己不落盘。界面进程把核心最后接受的一份写在自己数据目录的 `saved-profile.json` 里（Android 的 `filesDir`，iOS 的 App Group 容器；Unix 权限 0600），用于启动时新 Profile 拿不到或被拒绝的情况（#532，见 host-integration.md 第 9 节）。
- 后端地址编译进应用：`scripts/ci-build.sh` 要求 `PPVPN_API_BASE_DEFAULT`，CI 和发布从 GitHub 的 `PPVPN_API_BASE` 传入（#517，见 [构建与发布](release.md#后端地址)）。手机上没有环境变量可以改它，「我的」里打开的网页也从这个地址推出。
- 移动端官方推送接口（#539）：iOS 用 APNs，Android 用 FCM。原生层申请通知权限、向系统注册，拿到 APNs device token 或 FCM registration token 后交给 `Client::set_push_token`（界面通过 Flutter 的 `set_push_token(token, bundle_id)` 桥接）。每次启动和令牌轮换时都要调用。
  - 已登录时应用进程里的 Rust 客户端把设备注册到后端（`POST /api/v1/devices/register`，`platform` 为 `ios` / `android`，带 `token` 和可选的 `bundle_id`）。登录、每次启动和令牌变化时都会注册；令牌变了先尽力注销旧的。
  - Android 的 `PushMessagingService` 会持久化新 token 并通过 WorkManager 排队注册。Activity / Flutter 引擎未运行时，后台任务仍可经 JNI 初始化或复用同一个 Rust 客户端，不启动 VPN；通知权限在登录后申请一次。
  - 退出登录时按令牌注销（`POST /api/v1/devices/unregister`，尽力而为）。令牌属于设备，保存在应用数据目录的 `push-token.json`（0600；Android 为 `filesDir/data`），退出登录后保留，下次登录直接再注册。
  - 通知内容是 `title` + `body`，payload 可带 `message_id` 和 `deep_link`。Android 原生层展示通知，点击只把 `message_id` 交给 Dart，由当前登录账号的消息列表取得正文，不打开 payload 的 `deep_link`。切换账号或退出登录会清除待处理点击和旧消息缓存。
  - Android 缺少 Google Play 服务 / Firebase 配置或 token 请求失败、超时后，启用 `desktop-android` 的独立自建注册；代理仅随已经运行的 VPN `:tunnel` 服务运行。成功取得缓存 token 后的实际投递故障不在自动切换的检测范围内，详见 [备用通道](android-push.md#vpn-内的备用通道)。
  - 桌面平台不使用这个调用：Windows、Linux 和目前的 macOS 仍走自建推送代理。
