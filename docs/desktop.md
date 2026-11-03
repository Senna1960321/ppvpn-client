# Desktop 平台接入

桌面产品只依赖版本化的 ppvpn-core 公共契约，不感知或选择 core 的内部实现。

Windows 与 macOS 的做法相同：特权 service 以 `--tun` 拉起 core，由 core 打开 TUN。两个平台都
不使用 Network Extension 或 System Extension。

DNS 分两层，归属不同：

- **系统层面的 DNS 设置**：让系统 DNS 指向 TUN 通告的地址。macOS 上由 core（Sail 0.17.0 起）在
  打开 utun 时写一个 supplemental 解析器、停止时撤掉；Windows 上 TUN 网卡的 DNS 由 core 设置。
  两个平台上 service 都不另行改写（见下）。
- **TUN 内部的 DNS 归 core**：劫持隧道内的 DNS 查询、按 Profile 解析，以及本地 DNS（`dns-local`）
  读取物理网卡的解析器。

## Windows

特权 `ppvpn-service` 作为唯一 runtime owner 启动 Windows x64 core 制品，先读取
`version` 并要求 Core API v1、Profile Schema 1。service 负责权限、以 `--tun` 拉起 core、
进程校验和控制通道。系统层面的 DNS 在 Windows 上由 TUN 网卡的 DNS 设置完成（sing-tun 设置），
service 不另行改写；TUN 内部的 DNS 由 core 负责（见上）。Profile 的
`DIRECT`、`REJECT`、selected/fixed-node `PROXY` 语义只由 core 判定。

增强模式（TUN）下 DNS 由 core 接管：TUN 同时持有 IPv4 与 IPv6 地址，IPv6 也进入隧道；TUN 通告的 DNS
（`10.60.159.90`、`fde2:ec40:9312:c7fd::2`，0.5.7 之前为 `172.19.0.2`、`fdfe:dcba:9876::2`）以及隧道内任何 53 端口查询（不分 IPv4/IPv6）都被
劫持到 core，按 Profile 路由分流到系统解析器或经节点的 DoT。service 不要另行改写系统 DNS，也不要
把 TUN 接口的 DNS 指向其他地址。详见 [安全模型](security.md#增强模式tun的-dns-与防泄漏)。

共享本地代理由同一 runtime 创建：所有节点共用一个 loopback 端口（优先 7890），同一端口
支持 HTTP 和 SOCKS5，用户名 `<prefix>-<node_id>` 选择并固定走该节点。service 可读取完整 endpoint，
但发往 WebView 的 DTO 只能使用不含 secret 的 metadata；credential 只进入原生凭据
面板调用栈。

## macOS

macOS 与 Windows 相同，由特权 service 以 `--tun` 拉起 core。service 以 LaunchDaemon 形式运行，
启动用两个 release 文件 `ppvpn-core-darwin-arm64` 与 `ppvpn-core-darwin-amd64` 经 `lipo` 合成的
universal 可执行文件；core 打开 utun 设备作为 TUN。系统 DNS 由 core（Sail）在打开 utun 时指向 TUN 通告的
DNS 地址（`State:/Network/Service/<id>/DNS`，supplemental，匹配全部域名），停止时撤掉，进程被强杀时
由系统删除；隧道内的 DNS 劫持与解析也由 core 负责。service 不写系统 DNS，只在启动和卸载时删除旧版本
service 用 `scutil` 写下、可能残留的 `State:/Network/Service/com.peakpassvpn.ppvpn.tun/DNS`。
Desktop 不使用 Network Extension 或 System Extension，也不需要 NE entitlement。发布应用使用 Developer ID
签名；APNs 使用自己的 production entitlement 和 provisioning profile，见 [发布配置](release.md#macos-apns-签名与配置)。

保留的 `PPVPNClientFFI` XCFramework 是客户端 crate 的旧 UniFFI 绑定产物，不是 core 的产物；macOS Flutter
安装包已不再携带它或旧推送代理。应用直接接收 APNs，构建和迁移说明见 [macOS 打包](../apps/flutter/README.md#macos-packaging)。

### Flow adapter（`PPVPNCore.xcframework`，目前没有宿主使用）

下面的 flow adapter 目前没有任何宿主使用，构建和 release 产物照常保留，是否下线另行决定。

`build/PPVPNCore.xcframework` 提供 macOS 13+ universal slice，设计为运行在
`NETransparentProxyProvider` System Extension 进程内。公开 Objective-C API 包括：

- `MobileBridge.start/applyProfile/stop/status`
- `classifyFlow`：只读已编译规则快照，不做 host、磁盘或网络 I/O
- `openFlow(flowJSON, decisionJSON, timeoutMS)`：验证首次决策的 snapshot/HMAC 后，为其中
  固定的 node 打开 PROXY TCP/UDP outbound；不得按当前 selected 重新分类
- `MobileFlowConnection.read/write/close`
- `localProxyMetadata` 与 `localProxyCredential`

Provider 把 Apple flow 的 hostname、目标 IP、端口和 TCP/UDP 编码成严格 JSON DTO。
`DIRECT` 返回系统处理，`REJECT` 由 Provider 关闭，`PROXY` 在后台把首次 decision 原样
交给 `openFlow` 并负责双向复制、背压与关闭。`handleNewFlow` 回调内只能调用
`classifyFlow`，不能同步拨号。selected 在两次调用之间切换时，`openFlow` 仍执行首次
decision 的 node；Profile snapshot 已替换时则拒绝旧 decision。

Objective-C module 名为 `PPVPNCore`；生成头文件中的关键签名是：

```objc
- (NSString *)classifyFlow:(NSString *)flowJSON error:(NSError **)error;
- (MobileFlowConnection *)openFlow:(NSString *)flowJSON
                      decisionJSON:(NSString *)decisionJSON
                         timeoutMS:(long)timeoutMS
                             error:(NSError **)error;
- (NSData *)read:(long)maxBytes timeoutMS:(long)timeoutMS error:(NSError **)error;
- (BOOL)write:(NSData *)data timeoutMS:(long)timeoutMS error:(NSError **)error;
```

UDP 的一次 `read`/`write` 对应一个完整 datagram。若调用方 read buffer 太小，core 消费该
datagram 并显式返回错误，不会把截断数据当成功结果；单次写入上限为 65507 bytes。
FlowConnection 的 `timeoutMS <= 0` 表示不安装 I/O deadline，适合长连接空闲等待；
正值最大 120 秒。`openFlow` 的拨号 `timeoutMS <= 0` 仍使用 15 秒默认值。

## 固定优先级

1. 平台安全、控制通道和防递归
2. 共享 local-proxy 按用户名固定节点（未知用户名在认证阶段即被拒绝）
3. Profile ordered rules
4. `routing.final`

selected-node 切换只影响新 flow；Profile revision 更新走候选构建、受控替换和失败回滚。
