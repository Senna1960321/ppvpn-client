# 安全模型

## 资产与信任边界

高敏感资产包括 Profile 协议凭据、桌面会话密钥和每节点本地代理凭据。受信组件只有后端、核心进程、桌面主进程以及移动端 Network Extension/`VpnService`。WebView、渲染进程、第三方插件、系统剪贴板、日志/分析平台和崩溃报告默认不受信。

本设计防御同机非特权进程读取 IPC、误把秘密写入日志、后端注入 sing-box 私有配置、内部 tag 泄露成产品协议，以及恶意/错误 Profile 使用私网入口做探测。它不防御已经控制当前操作系统用户、已越狱/root 的设备或被替换的应用二进制。

## 秘密生命周期

| 数据 | 来源 | 内存 | 持久化 | 对外可见范围 |
| --- | --- | --- | --- | --- |
| Profile 凭据 | 后端 | 当前 Core Profile | 核心不持久化 | Apply/Validate 输入；不出现在节点/状态 API |
| 桌面会话密钥 | 核心启动时随机生成 | 核心与桌面宿主 | 临时交换文件；退出删除 | Authorization header |
| 本地代理凭据（prefix、共享密码、端口） | 核心随机生成 | 核心与宿主 | `local-proxies.json`（v2） | 仅 GetLocalProxyCredential / GetLocalProxyEndpoints / Bridge 对应方法 |
| sing-box 内部 tag | 核心构建 | 核心内部 | 不持久化 | 永不进入公开 DTO |

Profile 可能由宿主暂存以完成进程间交接，但这属于宿主责任：使用应用私有目录、原子写入、平台数据保护并在读取后删除。

## 桌面 IPC

- Unix socket 创建为 `0600`；只在旧路径确实是 socket 时清理，拒绝覆盖普通文件或符号目标。
- Windows Named Pipe 使用当前 owner-only ACL；状态目录必须位于当前用户私有 LocalAppData。
- Windows 上状态目录、状态文件（含写入用临时文件）和会话密钥文件使用受保护（不继承）的 DACL：普通用户运行的核心只授予
  进程令牌中的当前用户和 SYSTEM；以 LocalSystem 运行的特权服务核心（ProgramData）授予 SYSTEM 和 Administrators。
  读取时逐条检查 ACE（拒绝 ACE 忽略），任何其他账户被授予访问即视为不私有并拒绝，不自动修复。
  ppvpn-core 0.4.0 在用户目录下写入的“仅 SYSTEM + Administrators”ACL 会由该用户（作为 owner，拥有 WRITE_DAC）自动改回，
  改回成功时在核心诊断日志记一行 info（路径、原 DACL 与新 DACL 的 SDDL，不含文件内容）；
  无法修改时报错并在日志中给出路径和处理方法（管理员执行 `icacls "<路径>" /reset /t /c` 或删除该目录），不会删除任何文件。
- macOS 特权服务按代码签名认客户端，不按路径：用 socket 对端的 audit token（`LOCAL_PEERTOKEN`）取到连接进程本身，
  要求它满足 `anchor apple generic and identifier "com.peakpassvpn.ppvpn.desktop" and certificate 1[field.1.2.840.113635.100.6.2.6] and certificate leaf[field.1.2.840.113635.100.6.1.13] and certificate leaf[subject.OU] = "HSVMRQBQJ2"`
  （即 codesign 给我们 Developer ID 签名的指定要求），并启用 hardened runtime。
  它要求证书链锚定在 Apple 根证书、经 Developer ID 中间证书签发，且叶证书主题的 OU 是我们的 Team ID：
  这个 OU 只有 Apple 会签发，自签名证书写同样的 OU 或通用名也过不了；证书续期后要求不变（#471）。
  Team ID 只在 `crates/service/src/macos_team_id` 一处；发布构建导入签名证书后核对它是这个 Team 的
  “Developer ID Application”，不是即失败。
  因此从 DMG 直接运行或被系统移位（translocation）的同一份应用也能连接。debug 构建的服务在签名不符时仍接受
  `/Applications/PPVPN.app` 下 ad-hoc 签名的 debug 应用（并记一条 warn），发布构建没有这条退路。
  改 Team ID 时在同一版里改这个文件：它属于服务的 build id，新版应用首次增强连接时会替换旧服务（需管理员授权）。
  已安装的服务拒绝本应用时（例如 0.7.x 及以前的服务钉的是旧的自签名证书），应用在连接前重装服务一次
  （同样的管理员授权）；新的 `ppvpn-service-install` 只装我们 Team 签名的服务，所以这不放宽任何检查。
  重装被拒绝或失败就以它的错误结束这次连接，重装后仍被拒绝就以原来的拒绝结束，不循环（`enhanced.rs`）。
- Windows 特权服务认客户端（`crates/service/src/ipc.rs`）：进程映像必须是服务安装目录里的 `ppvpn.exe`，
  签名过 `WinVerifyTrust`（链到受信任的根，运行时不在线查吊销），且叶证书满足发布者钉：主题 O 是
  `PeakPass Labs LLC`，并带有钉住的身份 EKU（#489）。证书由 Azure Artifact Signing 签发，约每天换一张、
  有效约三天，所以不钉叶证书本身；Artifact Signing 在同一次身份验证签发的每张证书里放同一个
  `1.3.6.1.4.1.311.97.*` EKU，这个 EKU 只有微软会签发，换证书时不变。钉的是完整的 EKU，构建时由
  `PPVPN_WINDOWS_PUBLISHER_EKU`（换一次身份验证时另加 `PPVPN_WINDOWS_NEXT_PUBLISHER_EKU`）给出；
  钉编进服务，安装包升级时替换服务程序即生效。打包时 `check-publisher.ps1` 按同样的规则核对签好的
  `ppvpn.exe`，不符即失败，不会发出拒绝自己客户端的服务。
  没有钉时：发布构建（即使已签名）把 `PPVPN_WINDOWS_ALLOW_UNSIGNED_CLIENT` 设为 1，只认安装目录里的
  `ppvpn.exe`（Program Files 只有管理员能写）加会话握手；第一次签名的构建就是这样，从它的日志读出
  EKU 后设上钉。debug 构建签了名但没有钉时只要求 `WinVerifyTrust` 通过（记一条 warn）。
- macOS 的应用包属于用户（拖进「应用程序」的安装如此，.pkg 安装也由 postinstall 交给用户，见下），用户身份的代码
  能改动包里的文件。所以以 root 运行的东西都先核对签名：
  - 应用请求管理员授权时，root 的 shell 先把 `ppvpn-service-install`/`ppvpn-service-uninstall`（安装时连同
    `ppvpn-service`）复制到只有 root 可写的临时目录，用系统的 `/usr/bin/codesign --verify --strict --test-requirement`
    核对副本满足上面同样形式、标识为 `com.peakpassvpn.ppvpn.service-install`（或 `…service-uninstall`）的要求，
    再运行这个副本（Flutter 应用的 `apps/flutter/rust/src/hooks.rs`，原生应用的 `MacPlatformHooks.swift`）。
    Team ID 取自应用自身的签名。
  - `ppvpn-service-install` 在停掉旧服务之前，以及把 root 所有的暂存副本换上去之前，各按
    同样形式、标识为 `com.peakpassvpn.ppvpn.service` 的要求核对一次服务程序，
    Team ID 来自 `macos_team_id`；不满足就拒绝，旧服务保持原样（`install.rs`）。
  - 三个服务程序在打包时用固定的标识签名（`package-app.sh`），不再是链接器带哈希的默认标识。debug 构建不做这些检查。
  这挡住的是只改了包里文件的情况；能替换整个应用并重新签名的代码本来就能自己弹出管理员授权，不在这道防线之内。
- macOS 首次安装用的 `.pkg` 只装 `/Applications/PPVPN.app`（不可重定位，不会装到别处同标识的副本上），
  唯一的脚本是 `apps/macos/packaging/postinstall`：用 `stat -f %Su /dev/console` 取当前控制台用户，
  `chown -R <用户>:admin /Applications/PPVPN.app`（绝对路径，不跟随符号链接，没有 `-H`/`-L`）。
  控制台用户是 root、loginwindow 或取不到时什么都不做，退出码总是 0。这样应用包的所有者和拖进去安装时一样，
  Sparkle 能原地更新，不用每次要管理员密码；代价就是上一条所说的用户可写，由签名核对兜住。
- 桌面静默更新（#548，`crates/service/src/update.rs`，请求和结果见 `docs/host-integration.md` 第 16 节）：
  服务以 root/SYSTEM 安装应用下载的安装包，所以不信任请求里的文件：
  - 只按请求里的路径打开一次：`O_NOFOLLOW`（Windows `FILE_FLAG_OPEN_REPARSE_POINT`），在同一个句柄上查普通文件和
    长度，复制进暂存目录；之后的核验、安装都只用这份副本。
  - 副本要过三道核验：编进服务的 EdDSA 公钥（Sparkle、WinSparkle 用的同一把）；平台签名（macOS：Gatekeeper
    认它已公证，签名者是 `macos_team_id` 那个 Team 的 Developer ID Installer；Windows：和认客户端相同的
    `WinVerifyTrust` 加发布者钉）；版本（构建号大于服务自己的，并且等于包里写的版本）。
  - 暂存目录只有 root/SYSTEM 能写：macOS `/Library/Application Support/PPVPN/updates`（父目录要 root 所有、
    别人不可写，本目录 `700`）；Windows `%ProgramData%\PPVPN\updates`。普通用户能在 `%ProgramData%` 下建目录，
    所以父目录和它本身都要归 SYSTEM 或 Administrators 所有、不是重解析点；本目录新建时就带只含 SYSTEM 和
    Administrators 的受保护 DACL，已有的就重设成这样。不满足时以 `UPDATE_UNSUPPORTED` 拒绝。每次用之前删掉
    旧的 `current`、重新建，新建失败就放弃，不用别的进程建的目录。
  - 核验、等待应用退出和安装期间，`Connect` 以 `UPDATE_IN_PROGRESS` 拒绝。更新检查与核心启动按
    `ACTIVE` → 暂存根目录的 `lock` → `CORE` 的顺序互斥；清理、核心启动和独立安装进程持有同一把
    磁盘锁，锁不随 `current/` 删除。服务先取得锁时在允许连接前作废旧任务，安装进程先取得时服务不得清理或连接；
    打开或取得锁失败时拒绝连接。安装进程还核对命令行传入的每任务随机标识，旧进程不能安装后来复用目录中的新任务。
  - 安装进程是复制到暂存目录里的服务程序，不在服务的进程组（macOS `setsid`）或控制台里，所以安装停掉服务时
    它不受影响。macOS 先把已核验的 pkg 展开到私有的 `current/payload`，从其中的
    `Payload/PPVPN.app/Contents/MacOS` 复制 `ppvpn-service-install` 和 `ppvpn-service` 到 `current/new`。
    helper 必须是普通文件，到 payload 根目录之间的每个目录都不能是符号链接；两个副本分别满足标识为
    `com.peakpassvpn.ppvpn.service-install` 和 `com.peakpassvpn.ppvpn.service` 的 Developer ID 要求，
    Team ID 来自 `macos_team_id`，并启用 hardened runtime。全部通过后才安装 pkg，再以 root 运行私有的
    install helper 副本。postinstall 把应用包交给用户之后，helper 的来源仍是已核验的私有 payload。
  - Windows 安装参数为 `/S /SERVICEUPDATE`：还有任何 `ppvpn.exe` 或进程枚举失败时，安装程序在停服务和
    代理之前退出，不终止其他应用实例；此模式也关闭安装程序自身的应用和代理重新打开逻辑。
  - 重新打开应用用原来那个用户的身份：macOS 用 `launchctl asuser` 加 `sudo -u`，Windows 用该会话的用户令牌，
    不以 root/SYSTEM 运行应用。
- 每次启动轮换至少 256 bit 随机会话密钥；Unix 交换文件为 `0600`。
- Bearer 比较使用恒定时间比较。所有调用都要发送认证，事件流也不例外。
- 生产宿主应使用 `--exit-on-stdin-close` 的父进程存活管道，并在启动超时、异常退出时回收子进程。
- 不要让 HTTP 客户端自动把本机 Bearer header 重定向到 TCP/网络 URL。

## 本地代理

共享本地代理（HTTP/SOCKS5 同一端口）只绑定 `127.0.0.1`。用户名 `<prefix>-<node_id>` 选择节点，
`prefix` 是设备随机 5 位小写字母数字；密码是设备随机 secret（32 字节），所有节点共用，校验使用
常量时间比较，未知用户名同样执行一次比较。只提供用户名/密码认证：SOCKS4、无认证方法和已删除
节点的用户名一律拒绝。状态目录 Unix 权限为 `0700`，文件为 `0600`，写入采用同目录临时文件加原子
rename。启动时持久端口（或 7890）被占用才改用其他端口；宿主不能缓存端点跨越一次启动而不刷新。

回环监听并不等于无认证：本机其他进程也可访问回环端口。因此宿主必须使用返回的凭据，不能降级为无认证代理，也不得把凭据注入环境变量或子进程命令行。

### 可选的无认证系统代理监听器

操作系统代理设置无法携带凭据，所以桌面端的「兼容模式」需要一个无认证的回环监听器
（`/v1/set-system-proxy`）。这是一个有意的、受限的例外：

- 默认关闭，核心每次启动都是关闭的，开关状态不持久化；只持久化端口。
- 只绑定 `127.0.0.1`，不能配置为其他地址；关闭时立即停止监听。
- 特权 TUN 核心永不提供它。
- 开启期间，本机任何进程都可以不经认证通过它访问网络，这是所有系统代理客户端共有的性质。
  因此宿主只能在用户选择兼容模式并处于已连接状态时开启，断开、切换到增强模式、退出登录或退出时关闭。
- 每个节点的共享本地代理不受影响，仍然要求凭据。

## 增强模式（TUN）的 DNS 与防泄漏

TUN 只看到 IP 包：不嗅探就拿不到域名，Profile 的域名规则全部失效；不接管 DNS，系统解析器的查询
会绕过隧道明文发出（被投毒，或者在带 fake-ip 网关的局域网里得到 `198.18.x.x`，节点连不上，要等
约 2 分钟才超时）。因此启用 TUN 时，核心生成的配置固定包含以下内容（本地代理、系统代理、兼容模式
不受影响，它们本来就携带域名）：

- **嗅探**：`tun` 入站的每个连接先执行 sing-box `sniff` 动作（全部嗅探器：TLS SNI、HTTP Host、
  QUIC、DNS 等），域名规则因此能在 TUN 下命中。
- **DNS 劫持**：`tun` 入站中协议为 `dns`、或目标端口为 53 的流量执行 `hijack-dns`，交给核心的 DNS
  模块。规则不区分地址族，IPv4 与 IPv6 解析器一样被劫持。TUN 把自身对端地址（`10.60.159.90`，桌面端
  另有 `fde2:ec40:9312:c7fd::2`）通告为接口 DNS（Windows 与 Linux systemd-resolved 由 sing-tun 设置），
  发往它的查询都由核心应答；发往其他地址 53 端口的明文查询只要进了隧道也同样被劫持。
- **按路由选择解析器**：
  - `dns-local`（0.5.20 起）：Windows 与 macOS 上是核心自己的 `ppvpn-local` 传输，向**物理默认网卡**
    （即 `auto_detect_interface` 绑定直连 socket 的那块网卡）的 DNS 服务器发 UDP（截断时改用 TCP），
    每个服务器 2 秒，按顺序尝试。服务器列表在默认网卡每次变化时立即作废、下次查询时重读（同一网卡上
    最多每秒一次）；Windows 读该网卡的 `GetAdaptersAddresses`，macOS 读 `scutil` 的
    `State:/Network/Global/DNS`（仅当它属于该网卡），否则取 `scutil --dns` 中该网卡的 scoped 解析器。
    隧道地址段（含 0.5.7 之前的）、回环、`fec0::/10` 一律排除；链路本地 IPv6 只用该网卡上的（zone
    指向该网卡，否则丢弃）。读不到服务器时查询立即失败（客户端得到 SERVFAIL），**从不调用系统解析器、
    从不退回 127.0.0.1**，因此不会经系统 DNS 绕回隧道（0.5.4/0.5.5 的回环）。每次列表变化记一行
    `msg="local dns servers"`（`source`、`interface`、`servers`）。Linux 上不传服务器时仍是 sing-box `local`
    （systemd-resolved 的默认网卡链路 DNS，或 `/etc/resolv.conf`）。宿主用 `serve --local-dns-servers` 传入
    服务器时，它们是静态覆盖：全部（隧道地址段之外的）按顺序使用，不跟随网络变化（启动时记 warn）。
    各种情况都借 `auto_detect_interface` 绑定物理网卡。
  - `dns-remote`：DoT 到 `1.1.1.1:853`，经所选节点（`selected`）拨出，查询不出现在本地网络上。失败时（0.5.11 起）
    依次回退到 `dns-remote-8.8.8.8`、`dns-remote-9.9.9.9`（同样是经所选节点的 DoT）。只用境外公共解析器：
    这里解析的是走代理的域名，境内解析器会记录它们，也可能返回污染结果；全部失败时回 SERVFAIL，不降级到其他解析器。
  - DNS 规则镜像路由规则中的域名部分，顺序不变：路由为直连的域名（包括所有入口节点域名）走
    `dns-local`，路由为代理的走 `dns-remote`，路由为拒绝的直接拒绝；未命中规则时跟随
    `routing.final`：final 为 direct 时走 `dns-local`，否则走 `dns-remote`。端口、协议、CIDR 条件
    在解析时未知，不参与镜像。
  - `route.default_domain_resolver` 为 `dns-local`：节点入口域名和直连目标都经 `dns-local` 解析。
- **把域名交给节点**：sing-box 1.13 已删除 `sniff_override_destination`，嗅探到的域名只用于匹配规则，
  不会改写目标地址。核心在每个代理出站（selected 及固定节点）前加一层 `ppvpn-domain-destination`：
  对来自 `tun` 的连接，只要已知域名（嗅探结果优先，其次是核心 DNS 的 `reverse_mapping`），就把目标
  改写为域名再交给节点，由节点远端解析；直连路径保持 IP 不变。
- **隧道自身地址**：发往隧道自身网段（`10.60.159.88/30`、`fde2:ec40:9312:c7fd::/126`）的流量，除上面
  被劫持的 DNS 外一律立即拒绝：这些地址只存在于隧道里，否则会被下面的底线规则直连发出并挂到超时。
- **fake-ip 快速失败**：`tun` 入站目标位于 `198.18.0.0/15` 且没有已知域名的连接立即拒绝（等待嗅探
  最多约 300ms），不会发给节点空等。已知域名时照常按域名代理。核心自己不使用 fake-ip。
- **客户端底线**：`tun` 入站目标位于私网、CGNAT、回环、链路本地、组播、保留和受限广播网段
  （与 Profile `ip_is_private` 相同的一组，见 backend-profile.md）时直连，排在所有 Profile 规则之前，
  不受路由模式（`routing_mode`）影响，也不由 Profile 控制，保证局域网设备与发现协议（mDNS、SSDP、
  HomeKit/Thread、米家广播）在任何配置下都可用。
- **路由层排除**：桌面 TUN 的 `route_exclude_address` 除入口 IP 外还包含 `224.0.0.0/4`、
  `255.255.255.255/32`、`169.254.0.0/16`、`fe80::/10`、`ff00::/8`，以及私网 `10.0.0.0/8`、`172.16.0.0/12`、
  `192.168.0.0/16`、`100.64.0.0/10`、`fc00::/7`（挖掉 TUN 自己的 `10.60.159.88/30` 和
  `fde2:ec40:9312:c7fd::/126`），这些流量在系统路由层就不进入隧道。私网这一项是 #317 加的：Linux 上只要
  有排除，2091 表里就没有默认路由，排在主表之前的 `lookup 2091 suppress_prefixlength 0` 会接下所有目的
  地址，局域网主机连进来（例如 SSH）的回包就进了隧道。排除后私网目的地址回落到主表，从原网卡出去；路由
  规则里私网本来就直连（上一条），行为不变。仍未覆盖：用公网地址段的局域网、全局 IPv6 的局域网，以及从
  公网连进本机的连接，它们的回包仍会进隧道，要靠连接标记（connmark）解决。
  sing-tun 在三个平台上都以“从隧道路由范围中减去”实现排除，排除的地址回落到系统主路由表：macOS、
  Linux 上结果确定。Windows 的 WFP（`strict_route`）只放行核心进程与隧道网卡、拦截其他网卡的 53
  端口，不拦截组播；但 Windows 会给每块网卡（含 Wintun）自动加 `224.0.0.0/4` 与
  `255.255.255.255/32` 链路路由，而 sing-tun 把 Wintun 的 metric 设为 0，未指定出口网卡的组播/广播
  仍可能选中 Wintun。这一点需要真机验证（米家、SSDP、mDNS）。

### IPv6

桌面 TUN（`auto_route` + `strict_route`）同时持有 `10.60.159.89/30` 与 ULA `fde2:ec40:9312:c7fd::1/126`
（取自自有随机 ULA `fde2:ec40:9312::/48`）。0.5.7 起不再使用 sing-tun 的默认地址 `172.19.0.1/30`、
`fdfe:dcba:9876::1/126`：其他基于 sing-box 的客户端（mihomo/Clash Verge 等）也用这组默认值，两个 TUN
地址相同时后启动的一方会因 “object already exists” 启动失败。宿主（service）写死同一组地址，二者必须
同一版本一起更换。
只有 IPv4 地址时 sing-tun 只装 IPv4 路由：macOS 上 IPv6 流量（包括发往运营商 IPv6 DNS 的查询）
直接绕过隧道，泄露真实 IPv6 地址；Linux/Windows 的 `strict_route` 则把 IPv6 整个封掉。带上 IPv6
地址后：

- **路由**：sing-tun 按同一套 `auto_route` 捕获 IPv6。macOS 装 `100::/8`、`200::/7` … `8000::/1`
  这组拆分路由（覆盖 `::/0` 中除 `::/8` 外的全部，含 `2000::/3` 全球单播）；Linux 在同一张
  `2091` 表、同一段 `9091`–`9101` 优先级下加 IPv6 规则；Windows 给 Wintun 网卡配 IPv6 地址与
  IPv6 DNS，WFP 不再加 "block ipv6"，而是与 IPv4 一样放行隧道网卡、拦截其他网卡的 53 端口。
- **分流**：IPv6 连接与 IPv4 走同一套嗅探、DNS 劫持和 Profile 路由。代理目标由节点拨出
  （SS/VLESS/AnyTLS 都能承载 IPv6 目标），已知域名时交给节点的是域名，由节点自行选择地址族；
  只有 IPv6 字面地址且没有已知域名时，节点需要自身有 IPv6 出口，否则连接失败而不是泄露。直连规则
  仍然直连（经物理网卡）。
- **入口排除**：`route_exclude_address` 包含所有入口 IP，IPv4 为 `/32`，IPv6 为 `/128`。
- **主机关闭 IPv6**：sing-tun 加不上 IPv6 地址时会让整个 TUN 启动失败（Linux netlink `EACCES`、
  Windows 设置 IPv6 地址失败），所以核心每次 apply 前探测主机：Linux 读
  `/proc/sys/net/ipv6/conf/{all,default}/disable_ipv6`（`/proc/sys/net/ipv6` 不存在即内核
  `ipv6.disable=1`），Windows 看 `Tcpip6\Parameters\DisabledComponents` 的 `0x10` 位以及是否有
  AF_INET6 网卡，macOS 视为可用。IPv6 不可用时 TUN 只保留 IPv4 地址，`route_exclude_address`
  也只留 IPv4 前缀；这样不会泄漏，因为主机本身没有绕开隧道的 IPv6 通路。探测读不到时按可用处理，
  不做“失败后回退 IPv4”，真实错误照常暴露。
- **不设置 `prefer_ipv4`**：sing-box 的 `strategy` 只影响核心自身的域名查找（`Lookup`），对被劫持
  的原始查询（`Exchange`）只有 `ipv4_only` 会过滤 AAAA，`prefer_ipv4` 不起作用。代理流量已经按域名
  交给节点，AAAA 应答不会因为节点缺 IPv6 而失败；过滤 AAAA 反而会让仅 IPv6 的站点不可达，因此核心
  原样返回 AAAA。物理网络无 IPv6 时直连怎么办见下一条。
- **主机启用 IPv6 但没有 IPv6 出口**（物理网卡上没有“全局单播地址 + IPv6 默认路由”）：TUN 不变，
  仍持有 IPv6 地址和路由，防止绕过隧道的泄漏；但应用会优先用 AAAA，连到 TUN 后命中直连规则，
  直连出站按 IPv6 拨号立即失败。TUN 栈已在本地完成握手，应用看到的是“连上又断”，不会回落到
  IPv4（Windows VM 102，0.5.16）。所以此时核心把 `direct` 换成一层包装：发往全局单播 IPv6
  地址（`2000::/3`）、且域名已知（嗅探或 DNS 反查）的连接，TCP 和 UDP 都改成按域名，由物理直连
  出站 `direct-host` 经 `dns-local` 只解析 IPv4 后拨出。IPv4、私有、ULA、链路本地目标，以及不知道
  域名的 IPv6 字面地址照旧（后者和不开 VPN 时一样失败）。按 IP 规则直连的目标（比如含 IPv6 段的
  规则集）同样覆盖，浏览器 DoH 或应用缓存的 AAAA 也不例外，因为改写发生在出站而不是 DNS。只改
  出站，TUN inbound 和有 IPv6 出口的主机完全相同，所以主机 IPv6 状态变化不会重启 TUN。
  探测：Linux 读 `/proc/net/if_inet6` 与 `/proc/net/ipv6_route`（排除 reject 路由），Windows 用
  `GetAdaptersAddresses`（适配器 Up、有全局地址、有 IPv6 网关），macOS 读路由表中的 `::/0` 及其
  网卡地址；TUN 自己只有 ULA，不会被算作出口。每次 apply 时探测，start 时结果变化就先重建；
  info 日志写 `msg="host ipv6" host_ipv6_enabled=… host_ipv6_route=… policy=tun_ipv6|tun_ipv6_direct_ipv4|tun_ipv4_only`。
  探测失败按有出口处理（即旧行为），并记一条 warn 写明原因。运行中也会重新探测：
  默认网卡变化（`msg="default interface" event=changed`）后，等最后一次变化过去 2 秒再探测，
  连续多次变化（比如 Wi‑Fi 先断后连）只探测一次；结果和当前构建不同，就用当前 Profile 重建，
  走一次内核热切换，不断开已有连接，并记 info `msg="host ipv6 changed"`（前后两次的
  `host_ipv6_route`、`policy`，以及 `switch=kernel`）。重建和 apply 共用同一把锁，期间如果有
  apply，以 apply 的探测结果为准。主机运行中把 IPv6 整个关掉会改变 TUN，这种情况留到下一次 apply
  或 start 处理。
- **macOS 边界**：Darwin 上 `strict_route` 不起作用。发往全球单播 IPv6
  解析器（如运营商 `240e:…`）的查询会进入 TUN 被劫持；但在链路上的解析器（`fe80::…%en0`、路由器
  通告的本地 ULA、局域网 IPv4 网关）命中更具体的直连路由，不进入 TUN。因此 Sail（0.17.0 起）在打开
  utun 时把系统 DNS 指向 TUN 通告的地址（supplemental 解析器，匹配全部域名），停止时撤掉；宿主不再
  另行改写，用 `scutil --dns` 确认首个解析器是它，且只有一份。

移动端的 TUN 配置（不启用 `auto_route`，由宿主建隧道）保持仅 IPv4 地址，生成同样的嗅探、DNS 与
拒绝规则；宿主应把隧道 DNS 设为隧道内地址（如 `10.60.159.90`），让查询进入 TUN 被劫持。

已知边界：应用自带 DoH/DoT 的查询不会被劫持，但其连接仍会被嗅探并按域名路由。

## Linux TUN 策略路由的守护（0.5.20 起）

Linux 上 TUN 靠策略路由生效：sing-tun 在优先级 `9091`–`9101` 安装规则，指向表 `2091` 里经 TUN 的路由
（`strict_route` 的 unreachable 规则也在这个区间）。sing-tun 只在 TUN 启动时安装一次，之后不再检查。
systemd-networkd 默认 `ManageForeignRoutingPolicyRules=yes`，网卡 down 时会删除不是它自己加的规则。
规则一旦被删，流量就回到主路由表、绕过 TUN，`strict_route` 也随之失效，这是泄漏。热切换不会重装规则。

核心因此自己守护这些规则：

- TUN 启动后，核心立即对该区间内的规则和表 `2091` 里 TUN 网卡的路由拍快照。快照是 sing-tun 实际装上的
  结果，不是核心按选项拼出来的，所以优先级、表号和匹配条件天然一致。只有能确认属于 sing-tun 的规则才进
  快照：查表 `2091`、goto 到区间内、带 TUN 网卡的 iif/oif，或者是 sing-tun 特有的几类规则（goto 目标
  `nop`、严格路由的 `unreachable`、`not dport 53 lookup main suppress_prefixlength 0`）。区间里其他程序加的
  规则只记一行 info 日志，不纳入快照。
- 核心常驻监听 netlink 的规则和路由删除通知。区间内的规则或表 `2091` 的路由一被删除，核心就在约 50 ms
  内检查，并按快照只补回缺的那几条：先补路由，再补规则（goto 目标先补），已有的不动，不会先删后加。
  补回后记一行 warn：`msg="tun routing rules restored" missing=… by=…`。`by` 是删除方，尽力判断，
  例如 `systemd-network (port 1234)`；判断不了写 `unknown`。另外三处各做一次兜底检查：默认网卡变化、
  每次内核切换之后，以及每 30 秒一次（防止通知丢失）。
- 每次 down 都会被 networkd 删一遍、再被核心补回，这种来回是预期内的。10 秒内补回超过 10 次时
  记一条 error。
- 补不回来（权限不足、补完复查仍缺）时，`get-status` 的 `tun_routing` 变为 `broken`，并发出
  `TunRoutingBroken`，附缺失列表 `missing` 和原因 `error`。核心不会自行拦截流量，也不会自行停止；宿主
  收到事件后应重启核心。之后核心继续检查，恢复后 `tun_routing` 回到 `ok`，并发出 `TunRoutingRestored`。
- 守护本身起不来时（例如读不到规则表），`tun_routing` 为 `unguarded`，核心记一条 error 日志说明原因，
  不发 `TunRoutingBroken`：规则保持 sing-tun 安装时的样子，与 0.5.19 相同，重启核心多半也起不来。
- 核心停止或整体重启时，先停守护，再关闭 TUN，所以 sing-tun 自己的清理不会被补回。
- 只在 Linux 上做。Windows 和 macOS 的 TUN 不靠策略规则路由，`tun_routing` 不出现。

额外防护：使用 systemd-networkd 的系统（Ubuntu Server、netplan 等）建议在
`/etc/systemd/networkd.conf` 的 `[Network]` 段设置 `ManageForeignRoutingPolicyRules=no`，从源头避免规则
被删。这只是额外防护，不能替代核心的守护：其他程序同样可能删除规则。NetworkManager 推测不会删除，
但尚未实测。

## 与其他 sing-tun 应用共存（mihomo/Clash、sing-box 等）

桌面 TUN 与其他基于 sing-tun 的应用（mihomo/Clash Meta、Clash Verge、原版 sing-box 等）同时运行时：

- **Linux**：sing-tun 默认使用 iproute2 路由表 `2022`、规则优先级 `9000`–`9010`。它在安装规则前和关闭时
  都会按优先级区间 `[rule_index, rule_index+10]` 删除规则，不区分归属。若与其他应用共用默认值，我们的一次
  失败连接就会删掉 mihomo 的规则，使其表 2022 里的默认路由失去入口，用户断网直到 mihomo 重启。因此核心固定
  使用自己的 `iproute2_table_index: 2091` 与 `iproute2_rule_index: 9091`（占用 `9091`–`9101`），与 sing-tun
  默认值、Tailscale（表 52，`5210`–`5270`）、wg-quick（表/fwmark `51820`，`32764`–`32765`）和内核
  `0/32766/32767` 均不重叠。路由按我们自己的表和 TUN 网卡删除，也不会误删他人的路由。两者同时开启时，
  优先级更小的对方规则先匹配，流量会先进对方的 TUN；这是功能上的抢占，不再是破坏。
- **macOS**：`auto_route` 在全局路由表里添加相同的分段前缀（`1.0.0.0/8` … `128.0.0.0/1`）。sing-tun 遇到
  `EEXIST` 会先删除已存在的同名前缀再添加自己的，关闭时再按前缀删除。因此先开的应用的路由会被后开的
  接管，后者停止后前者的路由已不存在：网络回落到物理网卡（不断网），但对方 TUN 被静默绕过，需重启对方的
  TUN。这是 sing-tun 在 macOS 上的固有限制，无法用配置隔离；客户端应提示用户不要同时开启两个 TUN。
- **Windows**：路由挂在各自 Wintun 网卡的 LUID 上、只清理本网卡路由；网卡名自动取未占用的 `tunN`；严格路由的
  WFP 过滤器位于动态会话和随机 sublayer 中，会话关闭即自动移除。两个应用之间没有共享状态可被误删，
  但同样存在“谁的路由度量更优谁接管流量”的功能抢占。

## 规则集下载

规则集（`routing.rule_sets`）由核心自己下载和缓存，不使用 sing-box 的 remote 规则集：

- **主机固定**：URL 必须是 https，且主机必须在宿主通过 `apply-profile` 传入的
  `allowed_rule_set_hosts`（获取 Profile 的 API 主机）中，否则整份 Profile 以
  `RULE_SET_HOST_NOT_ALLOWED` 拒绝。宿主未传该字段时核心不发起任何规则集请求。核心不跟随重定向，
  不使用环境变量代理，不发送任何凭据或 Cookie。这样后端（或篡改的 Profile）无法让核心向任意主机发请求。
- **内容固定**：下载内容的 SHA-256 必须等于 Profile 中的 `sha256`，否则丢弃（`RULE_SET_SHA256_MISMATCH`），
  不写盘；还必须能被解析为 sing-box 二进制规则集，且不超过 32 MiB。Profile 本身经已认证的 API 获取，
  因此 `sha256` 是信任锚：即使下载通道或缓存被篡改，也不会加载未经 Profile 认可的内容。
  缓存文件在每次使用前重新计算 SHA-256。
- **始终直连**：下载从不经过节点或 TUN。TUN 核心运行时通过当前实例的 `direct` 出站拨号，该出站借
  `auto_detect_interface` 绑定物理网卡；核心未运行时本核心没有隧道，直接用普通 socket。
- **原子写与保留旧副本**：文件写在 `<state_dir>/rule-sets/<id>.srs`（目录 0700、文件 0600），先写临时文件、
  fsync 后 rename。失败的下载不会覆盖上一个已校验副本；不再被 Profile 引用的文件在下次应用时删除。
- **失败降级而非失败关闭**：规则集缺失只会让引用它的规则被跳过（见 backend-profile.md），核心仍然启动；
  跳过的规则集在 `get-status` 与 `RuleSetChanged` 事件中可见。注意这意味着规则集不可用期间，本应直连的流量
  会按 `routing.final` 走节点，本应拒绝的流量不会被拒绝。

已知边界：sing-box 1.13 读取二进制规则集后不关闭文件句柄（留给 GC）。Windows 上替换正被引用的文件时核心
会触发 GC 并重试 rename；仍失败则报告 `RULE_SET_STORAGE_FAILED` 并在下次刷新重试。

## Profile 防护

- JSON 严格解码，未知字段、歧义 credential union 和尾随值全部失败关闭。
- 只接受固定协议集合；Profile 拒绝未知 transport。
- 入口探测 IP 必须是公开单播，并明确拒绝私网、回环、链路本地、CGNAT、文档、基准测试和保留网段。
- 实际协议连接使用域名；需要 TLS 时 SNI 必须与该域名相等。
- 后端不能控制平台 TUN、本地监听、日志或任何 sing-box/Clash 字段。

这些约束减少 SSRF 和配置注入面，但可用性探测的 `target` 目前由受信宿主提供。产品层应把目标限制为PPVPN运营的固定 HTTPS 健康检查 URL，不要直接接受网页或不受信 IPC 调用方输入。

## 日志、错误与诊断

sing-box 上游日志被关闭，因为其文本没有第一方脱敏保证。公开事件只含稳定节点 ID、revision 和安全摘要。未知运行时错误统一折叠为 `CORE_OPERATION_FAILED` / `core operation failed`，不回传上游错误文本；原因只写入本机核心日志（stderr 或 `--log-file`，文件 0600），并经过代理 URL 凭据脱敏。日志不含 Profile 凭据、会话密钥或本地代理密码。`--log-level debug` 另外逐连接记录目标地址与域名（即浏览记录），
只供排查时临时开启；默认 `info` 不含任何连接目标。

CLI `render` 会对已知敏感 JSON 键和代理 URL 认证信息脱敏，但脱敏输出仍可能暴露拓扑、域名、IP 和节点数量。仅在受控开发环境使用，不要自动上传。

## 运维检查表

- 应用签出、卸载或“清除数据”时删除核心状态目录。
- 崩溃采集器排除 Profile、session secret 文件、`local-proxies.json` 和 API 原始 body/header。
- 密钥文件、socket、Named Pipe 名称不放入全局可读目录。
- API 客户端对认证失败重新握手，不复用上一次进程的 secret。
- macOS 签名证书（`MACOS_SIGNING_P12_BASE64`）必须是 `crates/service/src/macos_team_id` 这个 Team 的
  Developer ID Application，安装包证书（`MACOS_INSTALLER_P12_BASE64`）是同一 Team 的 Developer ID Installer，
  否则发布构建失败；同一 Team 的证书续期只换这两个 secret。
- Windows 由 Azure Artifact Signing 签名（[构建与发布](release.md)）；换一次身份验证（组织信息变了或验证到期）
  身份 EKU 就变：先把新 EKU 设为 `PPVPN_WINDOWS_NEXT_PUBLISHER_EKU` 发一版，再改用新验证签名。
- 发布前执行 race 测试、跨平台构建和校验和验证，详见 [构建与发布](release.md)。
