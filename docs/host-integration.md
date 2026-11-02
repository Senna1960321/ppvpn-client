# 宿主接入：Rust `ppvpn-core` 的公开 API

状态：**设计稿，待评审**（先由 core 审，再交 Desktop、CLI 评审）。依据：#214 的"宿主对 `ppvpn-core` 的接口要求""engine 与 desktop 的运行时边界"两节，以及 Desktop 给出的大纲（[评论](https://github.com/peakpassvpn/ppvpn-client/issues/45#issuecomment-5958856823)）。本文和 `crates/core` 在同一个 PR 里维护、随 crate 版本一起发布。切换到 Rust 版以后，它取代 `docs/desktop.md` 和 `docs/core-api.md`。

下文的签名是 Rust 草案，用来说明形状和语义。字段名以最终代码为准，但这里写下的语义就是契约。

## 1. 总则

- **形态**：`ppvpn-core` 是 Rust 库，宿主从源码编译并直接链接它，在自己的进程里运行。没有 IPC，也没有会话密钥。宿主之间的进程边界由宿主自己负责，例如 desktop 的 client ↔ service IPC。
- **版本**：遵循 crate 的 semver。1.0 之前，破坏性变更升中间位版本号（0.x → 0.(x+1)），这和 Sail 的约定一致。状态、事件和错误的枚举在同一个版本内**只增加、不修改**，并且都标了 `#[non_exhaustive]`，所以宿主遇到不认识的取值必须能容忍。
- **能映射到 FFI**：本接口要能直接包装成 C ABI 或 JNI（移动端的隧道宿主就是这样包装的，第 11 节），所以公开 API 遵守下面几条：
  - 公开类型都是值类型，能用 serde 序列化成 JSON，字段名用 snake_case；
  - 公开接口不暴露泛型、生命周期参数和 trait 对象，句柄是不透明的 `Engine`。唯一的例外是移动端的 socket 保护（`SocketProtect`，第 11 节）：它是宿主给的回调，fd 没法做成值，所以不参与序列化；
  - 事件按种类订阅，每种一个有界通道（见第 6 节），不用回调闭包；
  - Profile 以原始 JSON 字节传入，宿主不解析它；
  - 异步方法是普通的 `async fn`，FFI 层自己决定是用回调还是阻塞包装。
- **公开面**：只有 `Engine` 和本文列出的值类型（在 crate 根导出）。profile 解析、dns-local 等实现都是 crate 内部的，宿主看不到它们，改动它们也不算破坏性变更。`ppvpn_core::internal` 只给本 crate 的测试和 `ppvpn-core-lab` 用，**不稳定，宿主不得使用**，任何版本都可能改动它而不另行通知。
- **后端地址不在引擎里**：引擎不访问后端。宿主（desktop client、CLI、手机的界面进程）用的后端地址是构建输入 `PPVPN_API_BASE_DEFAULT`，由 CI 和发布从 GitHub 的 `PPVPN_API_BASE` 传入（#517，见 [构建与发布](release.md#后端地址)），代码里不写任何后端域名。登录页只信任这个地址的主机，应用打开的网页也从它推出。
- **语义贴近 Core API v1**：方法和 Core API v1 一一对应（见第 13 节对照表），错误码沿用现有的字符串。这样宿主迁移时，基本只需要把 IPC 调用换成函数调用。

## 2. 实例与拓扑

一个 `Engine` 就是一个实例，对应现在的一个 `ppvpn-core serve` 进程。desktop 运行两个实例（#214 已定）：

| 实例 | 所在进程 | 权限 | 启用的能力 |
| --- | --- | --- | --- |
| 标准实例（`Role::Standard`） | UI 进程，或 CLI 的 daemon | 普通用户 | 共享本地代理（7890）、兼容模式的系统代理监听（7891）、探测、流量与连接统计 |
| TUN 实例（`Role::Tun`） | 特权 service | root / SYSTEM | TUN、路由规则守护、TUN 内的 DNS（劫持、解析、dns-local）、热切换 |

CLI 只运行标准实例，而且不开 7891。

| 平台 | TUN 实例需要的权限 |
| --- | --- |
| Linux | root，或者 `CAP_NET_ADMIN` 加 `CAP_NET_RAW`：用于创建 TUN、改 ip rule/route、bind 到物理网卡 |
| macOS | root（LaunchDaemon）：用于创建 utun、改路由 |
| Windows | SYSTEM（service）：用于创建 Wintun 适配器、配置 WFP（strict_route） |

标准实例在所有平台上都不需要任何特权。

**系统层面的 DNS**：macOS 上由 Sail 负责（Sail 0.17.0 起）。Sail 打开 utun 时写一个临时的 dynamic store 键 `State:/Network/Service/<id>/DNS`（supplemental，匹配全部域名），指向 TUN 通告的 DNS 地址；teardown 时这一项（`LeftKind::Dns`）先于路由撤掉。临时键属于写它的进程，进程退出时由系统删除，被强杀也一样。service 不再用 scutil 覆盖系统 DNS。宿主自己打开 TUN 时（移动端的平台隧道）Sail 不设 DNS，由宿主负责。`ppvpn-core` 自己的 dns-local 读的是默认网卡的 DNS（`State:/Network/Global/DNS` 和该网卡的 scoped 解析器），不含这个键，隧道地址也另有过滤。其他平台上系统层面的 DNS 设置不属于本库，由 service 负责，包括退出和异常后的清理（见 `docs/desktop.md`）。

## 3. 生命周期

```rust
pub struct EngineConfig {
    pub role: Role,                         // Standard | Tun
    pub platform: Platform,                 // Linux | Macos | Windows | Ios | Android（移动端见第 11 节）
    pub state_dir: PathBuf,                 // 私有目录：规则集缓存、本地代理状态
    pub local_proxy: Option<LocalProxyConfig>, // 仅 Standard：listen（默认 127.0.0.1）、preferred_port（默认 7890；0 = 任意空闲端口，供测试）
    pub system_proxy: bool,                 // 仅 Standard：是否允许 set_system_proxy_listener（CLI 设为 false）
    pub tun: Option<TunConfig>,             // 仅 Tun：local_dns_servers 覆盖（等同 --local-dns-servers）；Windows 上 wintun_dll 路径；移动端 fd（宿主打开的 TUN，第 11 节）
    pub log: LogConfig,                     // 级别（info/debug）和日志行的接收端，见第 10 节
    pub max_connections: Option<u32>,       // Sail 的并发会话上限（inbound.max_connections）；None 用 profile 的，0 不限（第 11 节）
    pub socket_protect: Option<SocketProtect>, // 仅移动端的 Tun：出站 socket 的保护回调，不序列化（第 11 节）
}

impl Engine {
    pub async fn new(config: EngineConfig) -> Result<Engine, Error>;
    pub async fn shutdown(&self) -> Result<ShutdownReport, Error>; // 对整个实例生效，幂等；最多 10 秒
}
impl Clone for Engine { /* 引用计数句柄 */ }
impl Drop for Engine { /* 最后一个句柄：交给清理线程，见下文 */ }
```

- **创建**：`new` 创建实例，但不应用 Profile，状态为 `Stopped`。
  - **new 阶段就能确定的失败直接返回 `Err`**，不进入 `Fatal`：
    - 权限不足：`PERMISSION_DENIED`；
    - Windows 上找不到 wintun.dll：`WINTUN_UNAVAILABLE`；
    - `state_dir` 已被另一个实例使用：`STATE_DIR_IN_USE`；
    - 已有 Tun 实例：`TUN_INSTANCE_EXISTS`。

    `Fatal` 只用于运行中发生的、无法恢复的问题。
  - **`state_dir` 独占加锁**：实例打开它时加独占锁，直到 `shutdown` 或最后一个句柄 drop 后才释放。第二个实例打开同一个目录时返回 `STATE_DIR_IN_USE`（retryable=false）。
    锁在清理的最后一步才释放：规则、路由、TUN 的 DNS 等各项都撤销完以后。如果 `shutdown`（10 秒）或 drop（5 秒）到了上限、还有某一步没做完，这把锁就**不释放**，一直保持到进程退出，免得新实例和还没结束的清理同时运行。这时在同一进程里马上再 `new` 一个用同一目录的实例，会得到 `STATE_DIR_IN_USE`；没做完的项会列在 `ShutdownReport.leftovers` 里（drop 时记一行 warn）。
  - **本地代理状态在 `new` 时就生成或读取**：Standard 实例的 prefix、密码和端口，不依赖 apply，所以 `new` 之后就能读凭据和 metadata（第 4.6 节）。监听要到 `start` 才开。
  - **wintun.dll 由宿主随安装包分发**：签名版本和 Sail 使用的 `WINTUN_VERSION` 一致，路径通过 `TunConfig` 传入。引擎不下载它，也不内嵌。
  - 创建时会先**幂等地清扫上次的残留**，只限本库创建、并且能可靠识别的东西：
    - Linux：Sail 在 `state_dir/run` 下的台账记下的改动，由 `sail::embed::sweep` 撤销：ip rule、没有设备的 throw 路由、nft 表和 fw4 的 drop-in；另外按 `tunrules` 的命名空间清扫优先级 9091–9101 的 ip rule 和表 2091；
    - macOS：不需要清扫。强杀后 utun 和经它的路由随进程一起消失（Sail 的常驻 CI 每次都验证）；Sail 接受这个 run_dir，但在 macOS 上不写台账；
    - Windows：不需要清扫。强杀后 Wintun 适配器及其路由、DNS 随进程一起消失：Wintun 在创建它的进程的句柄关闭时删除适配器，路由和 DNS 挂在适配器上。依据是 Sail 在 VM 上的两次实测（Win11，sail 0.16.0 windows-gnu，Wintun 0.14.1，双栈）：
      - tun + auto_route：`Stop-Process -Force` 后 3 秒内，适配器、它的 `0.0.0.0/0` 和 `::/0` 路由、它的 DNS 都没有了；运行中重启 VM 后也没有适配器和 PnP 记录；再次启动用同一 GUID 重建正常。
      - 再加 `strict_route` 和排除段（绕开 `10.0.0.0/8`、`192.168.0.0/16`，加 `::/0`）：运行时有 Wintun 适配器、metric 0 的路由、TUN 的 DNS、6 个 WFP 过滤器和 1 个名为 sail 的子层；强杀后约 0.5 秒内全部消失，+1、+3、+10 秒都没有回来；物理默认路由没有被改动，之后出口和 DNS 正常；第二次启动再强杀，结果相同。WFP 过滤器属于动态会话，随进程一起撤掉。
      - 我们的 MSVC 构建也在 CI 里验证了强杀（ci.yml 的 windows 作业）：Tun 实例加 auto_route 和 `strict_route`，进程被强杀后，适配器、路由、TUN 的 DNS 和 WFP 过滤器都回到启动前，再次启动成功。局限：CI 用的是测试配置（`strict_route` 只路由 198.18.0.0/16），不是翻译出的全量配置；全量配置下的强杀由 G5 实机验收（`docs/rust-parity.md` R1）。实例失败而进程还在的情形见第 9 节。
  - 清扫的结果记一行 info 日志。
- **运行时**：`new` 可以在 tokio 运行时上下文里调用，也可以不在。
  - 在**多线程**的 tokio 运行时里调用 `new` 时，实例跑在这个运行时上（Sail 的 `Runtime::Host`），每个实例只另占 Sail 的一个线程；在单线程运行时里或不在 tokio 里调用时，实例用 Sail 自带的运行时。用多线程运行时的宿主，要让它启用 I/O 和定时器（`enable_all()`），并一直活到 `shutdown` 返回；运行时先没了，实例会失败（`Fatal`），系统改动仍由 Sail 自己的线程撤销。Sail 会用这个运行时的 blocking 线程池（系统解析器的查询等），宿主自己的阻塞任务接近上限时要调大 `max_blocking_threads`。
  - **内存（Linux，glibc）**：一个进程里反复起停实例时，glibc 的 malloc 不把释放的内存还给系统，RSS 会一直涨（Sail 的测量：64 MB 涨到 264 MB，用宿主运行时是 133 MB），不是泄漏。宿主进程**应设环境变量 `MALLOC_ARENA_MAX=2`**（同一测量降到 79–92 MB），要在进程启动前设，例如写进 systemd unit 或启动脚本。引擎另外在每次加载配置后调用 `malloc_trim(0)`，把这次加载的峰值还回去。macOS、Windows 和 musl 不受影响。
  - 实例不使用全局单例、静态运行时或全局注册表。同一个进程里可以先后创建多个实例（G7 要求反复启停 100 次不留残留）。
- **实例数量**：
  - **Tun**：同一时刻只能有一个，因为它们会争用同一套规则命名空间。重复创建时返回 `TUN_INSTANCE_EXISTS`（retryable=false）。
  - **Standard**：不限制数量，只要 `state_dir` 和本地代理端口不冲突即可（`cargo test` 和 CLI 的测试会并行创建多个）。
- **句柄**：`Engine` 实现 `Clone`，是同一个实例的引用计数句柄（FFI 包装时句柄同样是引用计数）。
- **正常退出**：用 `shutdown(&self).await`，任何一个句柄都可以调用，对整个实例生效，并且幂等。它先停止接受新连接，再关闭监听和 TUN，最后撤销规则和路由并清理 TUN 内的 DNS。
  - 总耗时上限 **10 秒**（服务管理器的停止流程比这长得多）。正常情况下返回时都已完成；超时就返回，结果 `ShutdownReport { leftovers: Vec<Leftover> }` 里列出没清理完的项，同时记一行 warn，剩下的由下一次 `new` 的清扫兜底。
  - 每一项是 `Leftover { kind, name, detail }`。`kind` 是类别：`runtime`、`task`、`tun`、`route`、`dns`、`wfp`、`rule`（Linux 的 ip rule、nftables）、`steps`（清理本身没做完或没来得及做）。`name` 在类别内稳定，例如步骤名、适配器名、规则表，宿主据此判别是哪一项。`detail` 是出错的说明，用于日志，不用于判别；它不含凭据和 Profile 内容：引擎的步骤只写自己的说明，运行时和 Sail 的原文里出现的实例凭据（节点和本地代理的密码、UUID 等）一律换成 `<redacted>`。Sail 的停止报告给不出类别的项归为 `runtime`，原文放在 `detail`。JSON 形如 `{"kind":"route","name":"routing","detail":"did not finish in time"}`。
  - 之后，所有句柄上的生命周期调用都返回 `ENGINE_SHUT_DOWN`（retryable=false）；查询返回最后的快照，状态为 `Stopped`；订阅收到通道关闭。
- **`drop`**：最后一个句柄被 drop、而之前没有调用过 `shutdown` 时，清理作为兜底仍会进行，保证宿主 panic 后依然干净：
  - `Drop` **不会在调用方的线程上 `block_on`**，在 tokio 运行时线程上那样做会 panic 或卡住线程。它把清理交给实例自己的清理线程，在有限时间内（目前定为 5 秒）同步完成：撤销规则和路由、关闭 TUN 的 fd 和监听 socket；这些都不需要异步。
  - 在异步任务里 drop 是安全的（G7 覆盖"在异步任务里 drop"这个用例）。
  - 进程被强杀时没有机会清理，这种情况由下一次 `new` 的清扫兜底。
- **两项进程内保证**（desktop 要求，G7 验收）：
  1. panic 之后可以丢弃并重建实例：重建时不受任何全局状态影响。
  2. 实例被丢弃后，TUN、ip rule/route、TUN 内的 DNS 和监听端口都已清理干净。
- **panic**：所有公开方法都在边界上用 `catch_unwind` 兜住 panic（库按 `panic=unwind` 构建）。被兜住的 panic 会让这次调用返回 `CORE_PANICKED`，同时实例进入 `Fatal(panic)`，宿主应当丢弃并重建实例。

## 4. 方法

所有方法都是 `&self`。`Engine` 实现了 `Send + Sync` 和 `Clone`（内部是 `Arc`），可以在多个任务里并发调用。

- **生命周期类操作在实例内部串行执行**：`apply`、`start`、`stop`、`select_node`、`pin_ingress`、`set_system_proxy`。
- **查询类操作不受它们阻塞**：`status`、`traffic`、`connections`、凭据读取都不等这把锁。Go 版曾经出过一次锁顺序死锁（`TestApplyDoesNotDeadlockWithStatusAndAWriter`），这里从设计上避免。

### 4.1 apply 与 validate

```rust
pub struct ApplyRequest {
    pub profile: Vec<u8>,                 // 原始 JSON；未知字段忽略；schema 由引擎判定
    pub routing_mode: RoutingMode,        // Rules | Global；默认 Rules
    pub selected_node_id: Option<String>, // 宿主持久化的选中节点；默认 None = 用 default_node_id
    pub pins: Vec<Pin>,                   // 宿主持久化的 ingress pin：{ node_id, endpoint_key }；默认空
    pub allowed_rule_set_hosts: Vec<String>, // 默认空 = 不允许从任何主机下载规则集；宿主必须显式给出
}
// 用 JSON 反序列化 ApplyRequest 时（例如将来的 FFI），只有 profile 是必需的；
// 其余字段缺省时取上面注明的默认值。
pub struct ApplyResult {
    pub applied: bool,                    // false：(revision, routing_mode, selected_node_id, pins) 与当前完全相同，什么也没做
    pub revision: String,
    pub selected_node_id: String,         // 生效的选中节点
    pub selection_reset: bool,            // true：传入的 selected_node_id 不在新 Profile 里，已改用 default_node_id
    pub cleared_pins: Vec<ClearedPin>,    // { node_id, endpoint_key, reason: NodeRemoved | IngressRemoved }
    pub switch: Option<SwitchKind>,       // 运行中：KernelSwitch（不断连）| FullRestart { reasons }
    pub listeners: Vec<ListenerChange>,   // KernelSwitch 时原地增、删、换的监听：{ tag, change: added | removed | replaced }
}
pub async fn apply(&self, request: ApplyRequest) -> Result<ApplyResult, Error>;
pub fn validate(request: &ApplyRequest) -> Result<(), Error>; // 不需要实例，不联网
```

- **原子生效**：Profile、`routing_mode`、`selected_node_id` 和 `pins` 一起生效，或者都不生效。任何一步失败，当前生效的配置都不变，并发出 `ReloadFailed` 事件。
- **宿主持久化的状态**：选中节点、pins 和 `routing_mode` 都由宿主持久化（按设备），每次 apply 一起传入。实例重建后用户的选择不会丢，宿主也不必在 apply 之后补发 `select_node` 或 `pin_ingress`。
- **去重**：去重的键是 `(revision, routing_mode, selected_node_id, pins)`，在引擎里判断。比较的对象是实例**当前生效**的值，包括 apply 之后 `select_node`、`pin_ingress` 做的改动。所以宿主把持久化的最新状态原样传回来时，不会触发重新 apply。
- **`pins` 的校验**：`pins` 里同一个节点出现多次时，按校验错误拒绝：`PINS_INVALID`（field=`pins[i].node_id`，retryable=false）。
- **pin 的处理**：`pins` 是宿主持久化的完整集合。新 Profile 里已经不存在的节点或入口，它的 pin 会被清除，并在 `cleared_pins` 里返回，同时发出 `NodeIngressPinCleared` 事件。返回值和事件内容相同：返回值给发起 apply 的调用方，事件给其他订阅者。宿主对两者的处理应当是幂等的。
- **校验顺序**（D3，#214 已决定）：先校验**原始** Profile，再沿用当前选中的节点。
  - `default_node_id` 不存在时，报 `DEFAULT_NODE_NOT_FOUND`（field=`selection.default_node_id`），与 `validate` 一致。Go 0.5.21 在这种情况下会接受，见 `docs/rust-parity.md`。
  - 后端保证 `default_node_id` 指向下发的节点之一（`nodes[0]`），所以被拒只会发生在异常的 Profile 上。
  - 校验通过后选节点：传入的 `selected_node_id` 仍在新 Profile 里就用它；不在（或没有传）就用 `default_node_id`，传了却不在时 `selection_reset=true`。不再看实例内部"当前选中的节点"，所以重建实例和不重建的结果一样。
- **过期**（`expires_at`，与 Go 0.5.21 相同）：
  - 只在校验时检查：已过期的 Profile 在 apply 和 `validate` 时报 `PROFILE_EXPIRED`（field=`expires_at`，retryable=false；golden 见 `validation.json` 的 `profile_expired`）。
  - 运行中越过 `expires_at` 时，引擎**不会**主动停止，也不会拒绝转发，已生效的配置继续工作。
  - 同时，引擎在越过 `expires_at` 的那一刻进入 `Degraded{ProfileExpired}`。这由定时器触发，不必等到下一次重建才发现。这个原因只用于上报，不触发 `Fatal`，也不影响转发。apply 一份未过期的新 Profile 后清除。这是 Rust 版新增的行为，Go 0.5.21 只有日志。
  - 但之后任何需要重新构建配置的操作都会失败，报 `PROFILE_EXPIRED` 并发出 `ReloadFailed`，已生效的配置不变。这些操作包括：宿主的 apply、规则集刷新、网卡变化后的重新探测。
  - 什么时候换上新 Profile、过期后还能不能继续用，由宿主决定（第 9 节）。
- **规则集**：apply 前会准备规则集，总共最多等 10 秒。只从 `allowed_rule_set_hosts` 列出的主机下载（宿主传拉取 Profile 的 API 主机）；为空时一个也不下载，只用本地已有的、sha256 相符的缓存，其余规则集报 `RULE_SET_HOST_NOT_PINNED`（和 Go 一致，默认拒绝）。不为空时，Profile 里的规则集 URL 必须都在这些主机上，否则 apply 被拒。下载失败的规则集按降级规则处理，不会让 apply 失败。之后的定时刷新和失败后的恢复都在引擎内部完成，每次状态变化发出 `RuleSetChanged`。宿主不需要（也没有）`reload`。
- **热切换**：运行中的 apply 原地换掉配置，不关监听，已有连接留在原处继续运行。Rust 版没有“旧内核排空”：Sail 在同一个实例里原地 reload，进行中的连接不迁移、也不需要等待，没有旧内核可排空，所以不发 `KernelDrained`，`status.draining_kernels` 恒为 0（与 Go 0.5.21 的差异：Go 换一个新内核，旧内核带着连接排空，最长 10 分钟）。Go 在切换时还会主动关掉三类连接，这一点和排空无关、Rust 照做：所用节点被新 Profile 删除的、经本地代理进来而其用户已不在新列表里的、新规则会拒绝的；`KernelSwitched` 的 `closed_connections` 是这样关掉的连接数，`kept_connections` 是留下的。三类都已实现：被删用户的连接由 Sail 在 reload 时关掉，计数时也算在内；新规则会拒绝的那一类由 Sail 的复查完成：每次 reload 后，Sail 按新路由把进行中的连接重新匹配一遍（不产生副作用），新规则拒绝或丢弃的就关掉，并报告它们的 id 和对应规则。UDP 会话按它的第一个目的地复查。同一个连接属于多类时只计一次。监听的变化大多也原地完成：新增的监听建起来，消失的移除，地址、端口或选项变了的就地替换；被移除或替换的那个监听断开它自己的连接，其他连接不受影响。这些仍算 `KernelSwitch`，`ApplyResult.listeners` 列出哪些监听被增、删、换。route 的网卡选项（`default_interface`、`auto_detect_interface`、`default_mark` 等）的变化也由 reload 接受，不需要重启，进行中的连接保留原来的套接字；新连接改用新值（`default_interface` 由我们的 netns CI 验证；`default_mark` 由 Sail 的 CI 覆盖：Linux 网络命名空间里以 root 运行，sail 41ea4db8；其余选项按 Sail 的说明，尚未验证）。选项指向一张不存在的网卡时，运行时拒绝这次 reload，运行中的配置不变，apply 返回 `CORE_OPERATION_FAILED`（retryable=false），并发出 `ReloadFailed`。只有 TUN 的变化、或者运行时自己表示必须重启时，才走 `FullRestart`（停止再启动，所有连接断开，`reasons` 说明原因，例如 `tun options changed`；这时 `listeners` 为空）。这是 Rust 版和 Go 0.5.21 的行为差异：Go 对任何监听变化都整体重启。重启和 `start` 一样会重新检查监听端口（被占就换，发 `LocalProxyEndpointChanged`），本地代理监听起不来时不带它启动（`Degraded{LocalProxyUnavailable}`）。重启期间状态保持 `Running`，不发 `CoreStopped`、`CoreStarted` 或 `StateChanged`；已有连接断开，监听短暂关闭，宿主从 apply 的结果 `switch = FullRestart { reasons }` 得知发生了重启。新配置启动失败时恢复原来的配置，apply 返回错误；原配置也起不来时实例停止（`CoreStopped`）。细节和 Go 版一致（`docs/core-api.md` 热更新一节，`docs/rust-parity.md` 第 1 组）。

### 4.2 start / stop

```rust
pub async fn start(&self) -> Result<(), Error>;
pub async fn stop(&self) -> Result<(), Error>;
```

- **前提**：`start` 需要已经 apply 过 Profile，否则返回 `PROFILE_NOT_APPLIED`（retryable=false，D1）。Go 0.5.21 在这里返回的是 `CORE_OPERATION_FAILED`。
- **幂等**：重复调用 `start`、`stop` 都是幂等的。
- **`stop` 之后**：实例回到 `Configured`，Profile 保留，可以再次 `start`。

### 4.3 选择与 pin

```rust
pub async fn select_node(&self, node_id: &str) -> Result<(), Error>;
pub async fn pin_ingress(&self, node_id: &str, endpoint_key: Option<&str>) -> Result<(), Error>; // None = 恢复自动
```

- **`select_node`**：只影响新连接。返回成功后由宿主持久化，下次 apply 时作为 `selected_node_id` 传入。
  - 节点不存在时返回 `NODE_NOT_FOUND`（field=`node_id`）；还没有 Profile 时返回 `PROFILE_NOT_APPLIED`（D2）。
- **`pin_ingress`**：立即生效。宿主负责持久化，下次 apply 时放进 `pins` 传入。
  - 入口不存在时返回 `INGRESS_NOT_FOUND`（field=`endpoint_key`）；节点不存在时返回 `NODE_NOT_FOUND`。

### 4.4 查询

```rust
pub fn status(&self) -> Status;                   // 权威快照，见第 5 节
pub fn nodes(&self) -> Vec<NodeInfo>;             // list-nodes
pub fn selected_node(&self) -> Option<NodeInfo>;
pub fn traffic(&self) -> Traffic;                 // 累计上传/下载，方向以客户端为准
pub fn connections(&self) -> Vec<Connection>;
pub fn version() -> VersionInfo;                  // 关联函数，不需要实例
pub fn logs(&self) -> LogReceiver;                // LogSink::Channel 时的日志行，只能取一次，见第 10 节
```

- **运行时数据的时效**：`status` 里的节点健康和当前入口、`traffic`、`connections` 都是引擎最近一次从运行时读到的值。引擎不为它们定时轮询，免得实例空闲时也被唤醒。宿主开始读取后，引擎每秒读一次运行时；宿主停止读取 10 秒后，就不再读。所以空闲后的第一次读取可能是旧值，`Traffic::measured_at` 标明了读取时间，下一次读取就是新的。例外是 `start` 和运行中的切换（热切换和完整重启）：它们结束前各读一次运行时，所以紧接着的第一次读取就是这一刻的值（`measured_at` 不早于它），不会是空的或上一次运行的；之后的变化要等下一次读取。宿主比较一段时间前后的流量时，“之后”那次要取 `measured_at` 晚于这段时间结束的读数。还从没读到过运行时的值时（例如 start 之前，或 start 末尾那次读取失败），计数为 0、`measured_at` 是 Unix 纪元（`1970-01-01T00:00:00Z`），比任何时刻都早，不会被当成新鲜的值。入口切换（`NodeIngressSwitched`）和拨号失败会立即触发一次读取，不受这个节奏影响。`status.tun_routing` 不属于这类数据：它由路由守护实时更新。只有 Linux 的 TUN 实例有守护（补回路由，所以有 `Restoring`、`Unguarded`）。macOS 和 Windows 由 Sail 检测：别的程序改动了 TUN 的路由或地址时，引擎发 `TunRoutingBroken`，`tun_routing` 变为 `broken`，状态进入 `Degraded{TunRoutingBroken}`，实例继续运行；Sail 不补回，所以 `broken` 一直保持到实例下一次 start（或完整重启）重新装上路由为止，这两个平台上也不会出现 `Restoring` 或 `Unguarded`（`docs/rust-parity.md` N2）。

`VersionInfo` 包含以下字段：

- `core_version`：crate 版本；
- `sail_version` 和 `sail_commit`；
- `profile_schema_version`；
- `local_proxy_contract_version`。

宿主的诊断输出里应当带上这些信息。

### 4.5 探测

```rust
pub async fn probe_entrances(&self, request: ProbeEntrancesRequest) -> Result<Vec<EntranceResult>, Error>;
// { node_ids, method: Tcp | Icmp, timeout_ms, concurrency }
pub async fn probe_availability(&self, request: ProbeAvailabilityRequest) -> Result<AvailabilityResult, Error>;
// { node_id, target: URL, timeout_ms }；经该节点的出口（outbound）发出
```

- **语义**：和 Core API v1 相同（`docs/architecture.md` 的"探测语义"一节）。
- **入口探测**：直连测每个入口，不经隧道的规则。每个入口连续采样 3 次，`latency_ms` 取最快的一次（#549 之前只测一次）；耗时因此约为原来的 3 倍，仍在 `timeout_ms` 之内。需要已 apply 的 Profile（否则 `PROFILE_NOT_APPLIED`），不需要已 start；每个节点发一个 `EntranceProbed`。
- **可用性探测**：请求经该节点的出口发出，也就是该节点的本地代理用户所去的地方，但不经本地代理的监听，所以不受本地代理端口的影响（Go 版经本地代理用户发出）。每次探测发一个 `AvailabilityProbed`。
  - **前置检查的顺序同 Go**：实例没有本地代理 → `LOCAL_PROXY_DISABLED`（retryable=false）；没有 apply → `PROFILE_NOT_APPLIED`（retryable=false）；没有 start → `CORE_NOT_RUNNING`（retryable=true）；之后才检查离线和节点（`NODE_NOT_FOUND`）。
  - **TUN 实例**：始终返回 `LOCAL_PROXY_DISABLED`，不论是否在运行（Core 组决定，同 Go）。
  - **结果的 `error_code`**（探测跑了但失败，调用本身成功）：按失败的步骤区分，含义见 `docs/core-api.md` 的"EntranceResult 与 AvailabilityResult"一节：`TARGET_INVALID`、`CANCELED`、`TIMEOUT`、`CONNECT_FAILED`、`TLS_HANDSHAKE_FAILED`、`TLS_CERTIFICATE_INVALID`、`CONNECTION_CLOSED`、`RESPONSE_INVALID`、`REDIRECT_LOOP`、`HTTP_STATUS`，以及只在节点出口本身无法使用时的 `PROXY_REQUEST_FAILED`。#342 之前拨号、TLS、读响应的失败都报 `PROXY_REQUEST_FAILED`；宿主应把不认识的错误码当作一般的探测失败。
- **离线时**：没有默认网卡时立即返回 `NO_DEFAULT_INTERFACE`（retryable=true），不等超时，也不拨号。默认网卡的状态来自 Sail 的网络事件；在那之前，引擎当作"不确定"，照常探测。

### 4.6 本地代理凭据（仅 Standard）

```rust
pub fn local_proxy_metadata(&self) -> Result<Vec<LocalProxyMetadata>, Error>;        // 不含密码
pub fn local_proxy_credential(&self, node_id: &str) -> Result<LocalProxyCredential, Error>;
pub fn local_proxy_routed_credential(&self) -> Result<LocalProxyCredential, Error>;  // 用户名是裸 prefix
pub async fn set_system_proxy_listener(&self, enabled: bool) -> Result<SystemProxyStatus, Error>;
```

- **凭据接口分开**：按节点的凭据和 routed 凭据分别读取。metadata 不含密码，只有原生凭据面板才读凭据。
- **`state_dir` 里有什么**：只有规则集缓存和本地代理状态（prefix、密码、端口）。**不保存 Profile 原文**，Profile 只在内存里。宿主同样可以只把 Profile 放在内存里，不落盘，因为里面有节点凭据。
- **持久化**：prefix、密码和端口在 `new` 时生成或读取，存在 `state_dir` 里，重启和升级后保持不变，不在每次启动时重新生成。
- **端口**：
  - 优先级依次为：持久化的端口、`EngineConfig` 里的首选端口、7890、任意空闲端口。`preferred_port=0` 表示直接用任意空闲端口，供测试用。
  - 实际端口有变化时，持久化新端口，发出 `LocalProxyEndpointChanged` 事件，`status` 里也能看到。
  - 监听失败时，例如端口被占用而且换不了，进入 `Degraded{LocalProxyUnavailable}`，并按退避重试。
- **系统代理监听**：`set_system_proxy_listener` 只开关 7891 的无认证监听。操作系统的代理设置（指向这个端口）由宿主负责。
  - 幂等；开关状态不持久化，`new` 之后是关的。端口优先级：持久化的端口、7891、任意空闲端口，不会和本地代理端口相同。
  - 未 start 时只改状态，`start` 时生效（届时端口被占用就换一个）。运行中开关时，引擎生成带或不带这个监听的配置，交给运行时做一次只涉及入站的 reload：这个监听被加上或移除，出站、节点组（包括故障转移的当前入口和健康状态）、DNS、路由和规则集都原样保留，不算内核切换，不发 `KernelSwitched`。效果和 Go 版的原地增删一样。都发出 `SystemProxyChanged`（`message` 为 `enabled` 或 `disabled`）。
  - 运行中开关的保证：
    - 打开时，任何已有连接都不受影响；
    - 按节点的本地代理和 routed 用户的监听始终可用，地址和端口不变；
    - 关闭时停止系统代理的监听，并断开它上面已经建立的连接；其他监听和它们上面的连接不受影响。`stop()` 时所有连接都会断开。
  - 拿不到端口或运行时拒绝时返回 `SYSTEM_PROXY_START_FAILED`（retryable=true），开关保持原样。
- **TUN 实例**：本组方法返回 `LOCAL_PROXY_DISABLED`；`set_system_proxy_listener` 返回 `SYSTEM_PROXY_UNAVAILABLE`。
- **与 Go 版有意不同的三处**（Core 组和 Desktop 已定）：
  1. **状态文件损坏时重建**：不是 JSON、版本未知、prefix 非法、prefix 和密码不成对时，Go 版拒绝启动；Rust 版重建（新的 prefix 和密码），否则 `new` 会一直失败，直到有人手动删掉文件。读文件本身的 I/O 错误仍然返回错误。
  2. **权限不安全时重建凭据**：状态文件其他用户可读时（Unix），Go 版拒绝；Rust 版保留端口，重新生成 prefix 和密码。
  3. **apply 之前就能读 routed 凭据**：routed 用户不依赖 Profile，`new` 之后就能读（第 3 节）；Go 版在没有节点时没有 routed 用户。按节点的凭据仍然只对已 apply 的 Profile 里的节点有效。

  前两处重建的文件都只有本用户可读（Unix 上为 0600）。重建时 `new` 照常成功，`status().local_proxy.credentials_reset` 给出原因：`corrupt` 或 `insecure_permissions`。持有旧凭据的客户端（例如浏览器扩展）需要重新读取凭据。

  - **重置只在 `new` 打开状态文件时发生**，之后不会再有，所以没有对应的事件：宿主在 `new` 之后读一次 `status().local_proxy.credentials_reset` 即可。
  - 这个字段在本实例存活期间一直存在，core 不提供清除；是否重复提示由宿主决定。

### 4.7 休眠与唤醒（可选）

```rust
pub async fn sleep(&self);  // 系统即将休眠
pub async fn wake(&self);   // 系统已唤醒
```

宿主收到系统的休眠、唤醒通知时转告引擎，引擎转给 Sail（同 libbox 的 `Pause`/`Wake`）。不调用也可以：Sail 自己检测休眠（按时钟跳变），行为见第 9 节“网卡变化后”。

- **`sleep`**：Sail 的定时检查和规则集更新暂停到 `wake`。实例没在运行时什么都不做。
  - 一次运行里第一次 `sleep` 之后，Sail 不再自己检测休眠，改听宿主的。为防宿主漏掉 `wake`（中途退出、崩溃，或系统最终没有休眠），`sleep` 之后清醒时间累计 60 秒还没收到 `wake`，引擎自己唤醒 Sail。tokio 的时钟在系统休眠时不走（macOS、Linux），所以真唤醒后约 60 秒兜底生效。
- **`wake`**：休眠（从 `sleep` 起按墙上时钟算）不到 2 分钟时什么都不断开；2 分钟以上时断开闲置的承载连接，各组重新测速。正在用的连接都保留。休眠期间网络变了，照常按网卡变化处理（仍发 `NetworkChanged`）。本次运行没有 `sleep` 在先时什么都不做。
- 两者都不返回错误。Core API v1 路径是 `/v1/sleep`、`/v1/wake`（请求体为空对象，回 `{}`）。
- 实例重建（`shutdown` 后 `new`）或重新 `start` 之后，新的运行没收到过 `sleep`，`wake` 对它什么都不做，Sail 仍自己检测休眠。

### 4.8 网络状态（仅移动端）

```rust
pub fn set_network_state(&self, state: NetworkState) -> Result<(), Error>;
```

移动端的宿主推送当前网络，语义见第 11 节。桌面平台上返回 `CORE_OPERATION_FAILED`。实例已 `shutdown` 或 `Fatal` 时，和其他生命周期调用一样返回错误。它不经过 Core API v1：由隧道宿主在同一进程里直接调用。

## 5. 状态

```rust
pub struct Status {
    pub state: EngineState,
    pub revision: Option<String>,
    pub routing_mode: Option<RoutingMode>,
    pub selected_node_id: Option<String>,
    pub selected_ingress: Option<IngressStatus>, // endpoint_key、label、role、previous_endpoint_key、switched_at
    pub node_count: u32,
    pub nodes: Vec<NodeStatus>,             // 每个节点：node_id、pinned_endpoint_key、各入口（endpoint_key、role、label、
                                            // healthy、last_check_at、consecutive_failures、active），同 get-status
                                            // 名称、entry_label、region 等节点资料见 nodes() / selected_node()（同 list-nodes）
    pub local_proxy: Option<LocalProxyStatus>, // listen、port、listening（不含凭据）；credentials_reset：本实例 new 时
                                            // 重建过凭据的原因（corrupt | insecure_permissions），没有重建时省略；
                                            // 重置只在 new 时发生，没有对应事件，实例存活期间一直保留（第 4.6 节）
    pub rule_sets: Vec<RuleSetStatus>,     // id、state（ready | stale | unavailable）、updated_at、error、
                                            // failures（非 ready 时连续下载失败次数，0 时省略）、
                                            // next_retry_at（非 ready 时下次重试时间；主机未固定或没有存储时省略），同 get-status
    pub system_proxy: SystemProxyStatus,
    pub draining_kernels: u32,             // Rust 版恒为 0：没有旧内核排空（第 4.1 节）
    pub tun_routing: Option<TunRouting>,    // TUN 实例：Ok | Restoring | Unguarded | Broken（Restoring、Unguarded 只在 Linux；
                                            // Broken：macOS、Windows 上别的程序改动了路由，见 4.4）
    pub replaced_routes: Vec<String>,       // 仅 macOS：TUN 接管的其他 VPN 的路由，每条一行，如
                                            // "route 128.0.0.0/1 via 192.0.2.1 on utun4"；Sail 停止时放回。
                                            // start、完整重启、每次热切换时取的快照，两次之间可能已过时；
                                            // 停止后清空；为空时 JSON 省略。与 tun_routing（broken）互相独立，
                                            // 可以同时出现：接管别人的路由是正常行为，不是故障
    pub dropped_log_lines: u64,             // 日志接收端阻塞而丢弃的行数，见第 10 节
}
#[non_exhaustive]
pub enum EngineState {
    Stopped,                       // 未 apply
    Configured,                    // 已 apply，未 start
    Running,
    Degraded { reasons: Vec<DegradedReason> },
    Fatal { reason: FatalReason },
}
```

`status()` 任何时候都能取到，开销很小，不受生命周期操作阻塞。宿主重连（或者重新订阅）时，先读快照，再接收事件。

Core API v1 的 `get-status`、`list-nodes`、`get-selected-node` 里 CLI 直接展示的字段，这里全部保留，名字也不变。

`EngineState` 序列化成带标签的 JSON，取值用 snake_case：

```json
{"state":"running"}
{"state":"degraded","reasons":[{"kind":"ingress_unavailable","node_id":"jp"},{"kind":"no_default_interface"}]}
{"state":"fatal","reason":{"kind":"tun_routing_broken","missing":["9093/v4 iif tun0 goto 9101"]}}
```

宿主按结构化字段判断，不从别的地方推断：`state` 是 `degraded` 时看 `reasons` 里的每个 `kind`（例如有没有 `tun_routing_broken`），`state` 是 `fatal` 时看 `reason.kind`。只要有任何一个 `Degraded` 原因，`state` 就是 `"degraded"`，不是 `"running"`；Core API v1 的 `get-status`（engine-host）也照此给出，不会出现 `"running"` 加 `reasons` 的组合。

### 状态机

```
Stopped ──apply──▶ Configured ──start──▶ Running ⇄ Degraded
   ▲                  │  ▲                 │         │
   └──── (drop) ──────┘  └────── stop ─────┴─────────┘
                    任意状态 ──不可恢复──▶ Fatal（只能 drop 重建）
```

- **`Degraded`**：除 `TunRoutingBroken` 外，所有原因都会由引擎自愈，宿主**不需要重建**，只需要把原因映射成提示。`TunRoutingBroken`（macOS、Windows）不会自愈，持续到实例下一次 start；宿主可以重建实例，建议 10 分钟内最多自动重建 3 次，之后提示用户（见表中这一行）。原因可以同时有多个，都是可枚举的：

  | `DegradedReason` | 含义 |
  | --- | --- |
  | `NoDefaultInterface` | 离线；探测立即失败；网络恢复后自动回到 `Running` |
  | `IngressUnavailable { node_id }` | 节点的所有入口都不可用；故障转移会继续重试 |
  | `PinnedLineDown { node_id, endpoint_key }` | 被 pin 的入口不可用；pin 时不会转移到别的入口 |
  | `TunRoutingRestoring` | TUN 的路由（Linux 的规则，macOS/Windows 的路由）被删，正在补回 |
  | `TunRoutingUnguarded` | 路由守护没能启动，路由保持安装时的样子（相当于 Go 的 `unguarded`） |
  | `RuleSetUnavailable { rule_set_id }` | 规则集不可用，相关规则按降级处理 |
  | `LocalDnsUnavailable` | 默认网卡上读不到 DNS 服务器，直连域名只能得到 SERVFAIL；网卡或 DNS 变化后自动重试（Go 版只有日志） |
  | `LocalProxyUnavailable` | 本地代理端口监听失败，正在按退避重试 |
  | `ProfileExpired { expires_at }` | 已生效的 Profile 越过了 `expires_at`。转发照常；之后的重建都会因 `PROFILE_EXPIRED` 失败；apply 一份未过期的 Profile 后清除。宿主据此提示用户，或者去刷新 Profile（第 9 节） |
  | `DefaultRouteOverridden` | 其他 VPN 抢走了默认路由，流量不再进入本 TUN；对方撤走后自动恢复 |
  | `TunRoutingBroken { missing }` | macOS、Windows：别的程序（通常是另一个 VPN）改动了 TUN 的路由或地址，Sail 只报告、不补回，部分流量可能绕过 TUN。`missing` 是 Sail 对每处改动的描述，以 TUN 名开头。这是 `Degraded` 里唯一不会自愈的原因：持续到实例下一次 start。宿主可以重建实例（stop 再 start 即可重新装上路由），但另一个 VPN 可能马上又改，所以**建议限制自动重连**：桌面端 10 分钟内最多自动重连 3 次，之后提示用户（例如“另一个 VPN 正在改动路由”），由用户决定。Linux 不出现这个原因（守护会补回，补不回进入 `Fatal{TunRoutingBroken}`）。与 `Fatal` 里的同名原因同名不同义：宿主按所在的状态（`Degraded` 还是 `Fatal`）处理，不按名字 |

- **`Fatal`**：引擎无法自愈。宿主**丢弃并重建**实例；这是宿主重建实例的唯一理由，另外两个是 panic 和会话丢失。

  | `FatalReason` | 含义 |
  | --- | --- |
  | `TunRoutingBroken { missing }` | 路由被删后补不回来，流量可能绕过 TUN（对应 Go 的 `TunRoutingBroken`）。只在 Linux 上出现；macOS、Windows 上别的程序改动路由时是 `Degraded{TunRoutingBroken}`，不是 `Fatal`。宿主按所在的状态处理，不按名字 |
  | `TunDeviceLost` | TUN 设备消失，例如适配器被外部删除，并且重建失败 |
  | `Panic` | 公开方法里兜住了一次 panic |
  | `KernelUnrecoverable` | 内核启动失败，也恢复不到上一个内核 |

- **宿主的端到端检查**：例如经 TUN 或本地代理访问后端的 `/health`。这类检查只用来提示用户，不改变引擎状态，也不触发重建。

## 6. 事件

```rust
pub fn subscribe(&self, kinds: &[EventKind]) -> EventReceiver;   // EventKind::ALL：全部
impl EventReceiver { pub async fn recv(&mut self) -> Option<EventItem>; }
pub enum EventItem { Event { event: Event }, Lagged { kind: EventKind, dropped: u64 } }
```

- **按种类订阅**：每种事件一个有界缓冲，宿主处理慢时收到 `Lagged { kind, dropped }`，表示这种事件丢了 `dropped` 个。高频事件不会挤掉状态变化，例如连接事件不会挤掉 `StateChanged`。`EventReceiver` 被 drop 就是取消订阅。
- **格式**：`Event` 是带 `type` 标签的枚举，序列化成 JSON 后和 Core API v1 的 `watch-events` 一致：类型名保持 CamelCase，字段沿用现有名字，`at` 是 RFC 3339 格式的时间。
- **只增加**：新版本只会增加事件种类和字段。

| 种类 | 来自 Core API v1 | 说明 |
| --- | --- | --- |
| `StateChanged` | 新增 | `{ state, previous }`，第 5 节状态机的每一次变化 |
| `ProfileApplied`、`ReloadFailed` | 是 | apply 的结果；`ReloadFailed` 带 `code` |
| `CoreStarted`、`CoreStopped` | 是 | |
| `NodeSelected`、`NodeEndpointChanged` | 是 | |
| `NodeIngressSwitched`、`NodeIngressPinned`、`NodeIngressPinCleared` | 是 | 入口故障转移和 pin；pin 被 apply 清除时也发出 `NodeIngressPinCleared`，带 `reason` |
| `EntranceProbed`、`AvailabilityProbed` | 是 | |
| `RuleSetChanged` | 是 | |
| `SystemProxyChanged` | 是 | |
| `LocalProxyEndpointChanged` | 新增 | `{ listen, port }`，本地代理的实际端口变化 |
| `KernelSwitched`、`KernelDrained` | 是 | 热切换；Rust 版不发 `KernelDrained`（没有旧内核排空，第 4.1 节） |
| `NetworkChanged` | 是 | 默认网卡变化，来自 Sail 的网络事件：`InterfaceChanged`（换了默认网卡）、`Moved`（同一张网卡换了网络，例如唤醒后；只换接入点、地址不变的漫游不算；从 Sail 0.19 起，只是多出一个地址，例如 Wi-Fi 连上后拿到新的 IPv6 /64，也不算）、`Restored`（断网后恢复）。`Offline` 不发这个事件，而是进入 `Degraded{NoDefaultInterface}` |
| `TunRoutingBroken`、`TunRoutingRestored` | 是 | Go 版只在 Linux 上有。Rust 版 `TunRoutingBroken` 在 Linux、macOS 和 Windows 上都有：Linux 上是补不回（随后 `Fatal`），macOS、Windows 上是别的程序改动了路由（随后 `Degraded`，`error` 是“<类别> changed by another program”，`missing` 以 TUN 名开头）；`TunRoutingRestored` 只在 Linux 上有（只有那里补回路由）。同时会反映在 `StateChanged` 里。宿主不按事件名决定怎么处理，而按随后的状态（`Fatal` 还是 `Degraded`）。 |

## 7. 错误

```rust
#[non_exhaustive]
pub struct Error {
    pub code: &'static str,      // 稳定的字符串错误码，见 ppvpn_core::codes
    pub field: Option<String>,   // 出错的字段路径，例如 nodes[0].ingresses[1].endpoint.ip、routing_mode
    pub retryable: bool,
    pub message: String,         // 给开发者看的英文说明，不属于契约，不含上游原文和凭据
}
```

- **Profile 校验类错误码的权威列表**：`ppvpn_core::codes::PROFILE_VALIDATION`，或者 `Error::is_profile_validation()`。`validate` 和 `apply` 在改动任何东西之前，用这些码拒绝请求：Profile 本身、`routing_mode`、`allowed_rule_set_hosts`、`pins`。它们都是 retryable=false，已应用的状态不变。宿主直接使用这份列表，不要自己维护；列表只增不减。golden 运行器会校验 `validation.json` 里每一步的结果码都在列表中。
- **错误码沿用 Core API v1**：Profile 校验类（`PROFILE_*`、`SCHEMA_UNSUPPORTED`、`RULE_SET_*`、`ROUTING_*` 等，列表见上一条，每个码对应的 field 见 `testdata/golden/contract/validation.json`）、`ROUTING_MODE_INVALID`（field=`routing_mode`）、`NODE_NOT_FOUND`、`INGRESS_NOT_FOUND`、`PROFILE_NOT_APPLIED`、`CORE_NOT_RUNNING`、`LOCAL_PROXY_DISABLED`、`SYSTEM_PROXY_UNAVAILABLE`、`SYSTEM_PROXY_START_FAILED`、`NO_DEFAULT_INTERFACE`、`PROBE_METHOD_UNSUPPORTED`。
- **Profile 本身的问题**（D5）：空 Profile 返回 `PROFILE_REQUIRED`；不是合法 JSON，或者字段类型不对，返回 `PROFILE_MALFORMED`。两者都是 retryable=false。Go 版在这两种情况下都折叠成 `CORE_OPERATION_FAILED`。
- **新增的码**：
  - `CORE_PANICKED`：retryable=false，实例已进入 `Fatal`；
  - `ENGINE_FATAL`：retryable=false，实例已处于 `Fatal` 时，任何生命周期调用都返回它；
  - `ENGINE_SHUT_DOWN`：retryable=false，实例已经 `shutdown` 之后的生命周期调用；
  - `TUN_INSTANCE_EXISTS`：retryable=false，同一进程里已有一个 Tun 实例；
  - `STATE_DIR_IN_USE`：retryable=false，`state_dir` 已被另一个实例使用；
  - `PERMISSION_DENIED`：retryable=false，Tun 实例的权限不足（见第 2 节）；
  - `WINTUN_UNAVAILABLE`：retryable=false，找不到或加载不了宿主传入的 wintun.dll；
  - `TUN_NAME_TAKEN`：`start` 时 TUN 网卡名被其他程序占用（第 12 节）。message 里有被占用的名字；名字由 Sail 选时，还有它试过的次数。名字由我们配置时（Linux `ppvpn0`、Windows `PPVPN`）retryable=false，多半是另一个 ppvpn-core 正在运行；名字由 Sail 选时（macOS）retryable=true，再次 `start` 可能成功。和 `TUN_INSTANCE_EXISTS` 不同：后者是同一进程里重复创建 Tun 实例，在 `new` 时返回；
  - `PINS_INVALID`：retryable=false，`pins` 里同一个节点出现多次，field=`pins[i].node_id`；
  - `PROFILE_MALFORMED`：retryable=false，Profile 不是合法 JSON 或者字段类型不对（D5）message 只说错误类别（`Syntax`、`Data`、`Eof`、`Io`），语法错误再加行列号，不带 Profile 里的任何值。
- **`CORE_OPERATION_FAILED`**：只用于真正的内部错误，原因写进日志。Go 版有几种本该是结构化错误的情况会折叠成这个码，Rust 版改成具体的码（D1、D2）。
- **IPC 专用的码不再出现**：`UNAUTHENTICATED`、`CORE_API_UNSUPPORTED`、`REQUEST_INVALID`、`API_NOT_FOUND`、`STREAM_UNSUPPORTED`，库里没有对应的情形。
- **CLI 依赖**：CLI 的退出码和 `--json` 输出依赖 `code`、`field`、`retryable` 这三个字段。

## 8. 路由与数据面的约定（摘要）

完整的行为基准是 `testdata/golden/routing`、`test/lab/engine/cases` 和 `docs/rust-parity.md`。和宿主直接相关的几点：

- **客户端底线规则**：私网、CGNAT 和保留地址段直连，排在所有 Profile 规则之前；隧道自身网段和不带域名的 fake-ip 段直接拒绝。
- **按节点用户**：本地代理的按节点用户固定走该节点，不受规则影响；routed 用户和系统代理按 Profile 规则走。
- **`capabilities.udp=false`**（D4，#214 已决定）：UDP 被路由到 `udp=false` 的节点或入口时立即拒绝，不改走别的节点，也不直连，并记一行 debug 日志；多入口节点做故障转移时，`udp=false` 的入口不承接 UDP。Go 0.5.21 不检查这个字段。

## 9. 由引擎负责的恢复

以下这些归引擎，宿主不再介入：

- 入口故障转移和 pin 的生效；
- 网卡变化后：重新选默认网卡、重新探测 IPv6 出口、重新读本地 DNS，以及离线期间的处理。对此引擎保证两点：
  - 网络变化期间（包括断网、切换网卡）**不会进入 `Fatal`**，只会出现 `Degraded`，网络稳定后自动回到 `Running`；
  - 网络变化时由 Sail 自己的网卡监视器断开受影响的连接，引擎不另行通知 Sail（不调用 `network_changed`）。从 Sail 0.19 起只断开受影响的连接：绑定在已消失的网卡或地址上的连接；走默认路由的连接，在默认网卡、它的序号、网关或类型变了，或者它的地址消失时断开。地址未知的连接（UDP）遇到任何地址消失都按受影响处理。从 Sail 0.20 起，唤醒本身不算网络变化（仍发 `NetworkChanged`，`Moved`）：休眠不到 2 分钟时什么都不断开；更久时只断开闲置的承载连接（上面没有打开的连接的复用连接，DNS 服务器的也在内），正在用的连接保留；0.20 之前，休眠超过 5 秒，唤醒就断开全部连接。0.19 之前的版本在默认网卡状态有任何变化时都断开全部连接；
  - **Linux**：流量一旦绕过 TUN，**立即处理**：先尝试补回路由，补不回来就进入 `Fatal{TunRoutingBroken}`。
  - **macOS、Windows**：Sail 只检测、不补回。别的程序改动了 TUN 的路由或地址时进入 `Degraded{TunRoutingBroken}`，实例继续运行，由宿主按第 5 节 `Degraded` 的说明处理（可以重建，建议 10 分钟内最多 3 次）。下面关于 `Fatal{TunRoutingBroken}` 的要求只适用于 Linux。
  - （Linux）`Fatal{TunRoutingBroken}` 时运行时和 TUN 照常运行，引擎不会自己停止（同 Go 0.5.20）：这时流量绕过 TUN 直连，用户的流量**正在泄露**到隧道之外，但没有断网。宿主**必须立即**丢弃并重建实例（先 `shutdown`，再 `new`、`apply`、`start`），由新实例重新装上路由，期间不要套用“网络正在稳定”的宽限。重建失败时，宿主告诉用户保护已经中断，由用户决定是否继续。Go 版宿主就是这样做的：Desktop 增强模式的健康检查读到 `tun_routing` 为 broken，就按泄露处理，不等宽限，立刻重连。
  - 运行时失败或 panic 进入的 `Fatal`（`KernelUnrecoverable`、`Panic`）不一样：运行时已经失效，留着只剩系统里的残留（Windows 上 strict_route 的过滤器会挡住所有不走 TUN 的流量）。进入这类 `Fatal` 时，引擎立即停止运行时（有界，至多 10 秒），不等宿主：Sail 先撤路由、规则、过滤器和 DNS，再关设备。撤不掉的记下来，宿主之后 `shutdown`（或 drop）时列在 `ShutdownReport.leftovers` 里，每项带类别和手工清除的说明。三个平台上 Sail 都保证这一清理，并有 CI 测试（Linux、macOS 自 sail dddc2d1c，Windows 自 sail 99b8daef：先撤 WFP 过滤器，再撤 DNS、路由，最后关 Wintun 会话）。我们自己的构建也验证：Linux 在 netns CI，Windows 在 CI 的 `windows` 作业（MSVC 构建、管理员、strict_route）；运行时失败后、宿主还没 shutdown，系统已经回到启动前，再次启动成功。正常情况下引擎已经撤干净，leftovers 为空。leftovers 里出现 `wfp`，或者 Windows 上出现无法归类的 `runtime` 残留，说明清理没有成功、过滤器可能还挡着流量：宿主应当退出进程，由服务管理器重启，进程退出时系统会回收这些过滤器。这是清理失败时唯一的恢复手段。局限：清理步骤自身 panic 的用例只在 Linux 上有；进程被强杀的情形在我们的 MSVC 构建上还没有测；Sail 的 Windows 测试跑在 Windows Server 的 CI 机器上，不是桌面版实机。宿主看到这类 `Fatal`，仍应 `shutdown` 并重建实例。
- 路由规则守护，以及 Wintun、utun 的自愈；
- 热切换，以及切换时关掉新 Profile 拿走的连接；
- 路由和规则层面的完整性：规则或路由都在，流量没有绕过 TUN。Linux 沿用 Go 0.5.20 的规则守护；**macOS 和 Windows 是 Rust 版新增的能力**，至少要能检测到并上报，能自愈的就自愈，由 G5 实机验收。macOS 和 Windows 由 Sail 检测、不自愈：别的程序改动了 TUN 的路由或地址时进入 `Degraded{TunRoutingBroken}`，实例继续运行，是否重建、重建几次由宿主决定（建议见第 5 节 `Degraded` 表）。引擎通过 `TunRouting*` 事件以及 `Degraded`/`Fatal` 状态表达。

留在宿主的：

- 端到端可用性的提示：例如经 TUN 能不能访问后端，只用于提示用户；
- 竞品检测；
- 会话和租约；
- 多用户和接管；
- service 的安装；
- UI 状态的映射；
- 系统层面的 DNS 设置（macOS 除外：TUN 期间由 Sail 设置，见第 2 节）；
- 拉取 Profile 失败时，保留上一份可用的 Profile，不调用 apply。这一条属于 `ppvpn-account` 和宿主，不属于引擎。后端的口径见 proxy-profile 格式文档（ingress-endpoints.md）：
  - **404**：没有有效订阅、订阅已过期、没有可用节点，或者任一实例的入口没有全部渲染出来（"one or more instances have no available ingress"）。`GET /api/v1/me/proxy-profile` 的 404 用问题码 `code` 区分（backend #469）：`SUBSCRIPTION_REQUIRED` 是没有生效的订阅，`PROXY_NODES_NOT_READY` 是订阅已生效、但还没有发布任何入口。旧版后端两种情况都发 `NOT_FOUND` 或不带 `code`；
  - **500**：违反客户端契约，或者规则集读不出来。
- **续用的硬上限**：保留上一份 Profile 最多到它自己的 `expires_at`。过期后不能再用：引擎进入 `Degraded{ProfileExpired}`（第 4.1 节、第 5 节）。宿主收到后去刷新 Profile 并提示用户；拿不到新 Profile 时，是否停止由宿主决定，因为引擎自己不会因为过期而停止转发。
- **对 `ppvpn-account` 的要求**：拉取错误分成三类，宿主按类别处理：
  - **暂时性错误**（网络错误、5xx、超时）：保留上一份 Profile（以 `expires_at` 为限），退避重试；
  - **无可用服务**（404）：不再续用上一份 Profile，提示用户。桌面端和手机端会丢掉内存里的 Profile 和保存下来的那份（见下文“Profile 落盘”），两个核心都停止。`ppvpn-account` 的 `SubscriptionProblem` 把 `SUBSCRIPTION_REQUIRED` 归为 `NoSubscription`，把 `PROXY_NODES_NOT_READY` 归为 `NodesNotReady`；裸 404 和不认识的码不归类，由宿主按旧口径处理。桌面端：`NodesNotReady` 对应 `ProfileStatus::NodesNotReady`（`ErrorCode::ProxyNodesNotReady`），客户端在 30 秒、60 秒后各重新拉取一次，之后每 2 分钟一次，直到状态改变（节点到位即 `Ready`，不需要用户操作）或退出登录；其余 404（`SUBSCRIPTION_REQUIRED`、裸 404、不认识的码）仍是 `ProfileStatus::NoSubscription`；
  - **需要重新登录**（刷新令牌后仍然 401，或者 403）：不再续用旧 Profile，要求用户重新登录。
- **`ppvpn-account` 的请求走系统代理（#476）**：和原生版一致，每次请求时读取：先看环境变量（`HTTPS_PROXY` 等），Windows 再读 WinINet，macOS 再读 SystemConfiguration，Linux 只看环境变量。API 地址是本机时总是直连。系统代理连不上时（代理软件已退出、设置没恢复），这次请求改为直连，并记一条 warn。宿主不需要做任何配置。
- **Profile 落盘**：引擎的 `state_dir` 里不保存 Profile 原文（第 4.6 节）。Profile 里有节点凭据，宿主如果要跨重启保留，按其他密钥的方式保护。桌面端和手机端（`ppvpn-client`，#532）只保留核心最后接受的一份，写在数据目录的 `saved-profile.json`（Unix 上 0600）里，并记下它来自哪个 API 地址；启动时新 Profile 拿不到或被核心拒绝，就先用这份。以下情况删除这份文件：退出登录、重新登录、切换团队，以及进入不可用状态（没有订阅、订阅过期、团队停用、节点未就绪），这时内存里的 Profile 也一起丢掉。来自其他 API 地址的文件会被删除。保存的这份过期后不再可用：核心会以 `PROFILE_EXPIRED` 拒绝它。

## 10. 日志

- **输出方式**：日志行通过 `LogConfig` 交给宿主，可以是写入宿主提供的文件，也可以是一个按行接收的通道。格式与 Go 版一致（logfmt：`level=… msg=… key=value`），lab 和性能检查会解析这些行。
  - 一行是 `<RFC 3339 UTC 时间，纳秒> level=<error|warn|info|debug> msg=<消息> key=value … source=<core|sail>`。`source=core` 是 `ppvpn-core` 自己的行，其余字段是事件的字段；`source=sail` 是 Sail 的行，Sail 的原文整个放在 `msg` 里。
  - **级别与隐私**：`LogConfig.level` 为 `Info`（默认）时，日志里不出现连接的目的地址或域名，与 Go 版一致。`ppvpn-core` 自己写目的地的行（`connection`、`outbound failed`、DNS 交换）都在 debug 级。Sail 在 info 级会给每条连接写一行 `handled … dst=<目的地>`，所以引擎在 `Info` 下只让 Sail 记 warn 及以上，`Debug` 下让 Sail 记 debug。
  - 一个实例的两种行进同一个有界队列（1024 行），再按 `LogSink` 输出：
    - `None`：不保留任何行，也不计丢弃；
    - `File { path }`：`Engine::new` 时打开文件（不存在就创建，Unix 上权限 0600），只追加；打不开时 `new` 返回 `CORE_OPERATION_FAILED`（field=`log.sink.path`）。写盘在引擎自己的线程里做，不阻塞打日志的一方；写失败的行计入丢弃。只追加，core 不做轮转，也没有大小上限；实例存活期间引擎一直持有这个文件，宿主只能在 `Engine::new` 之前截断或改名；
    - `Channel`：用 `Engine::logs()` 取接收端。只能取一次，第二次调用（以及 sink 不是 `Channel` 时调用）得到一个已经结束的接收端（`recv()` 立即返回 `None`）。宿主不读或者已经丢掉接收端时，行照样丢弃并计数。实例的最后一个句柄释放后通道结束。
- **tracing 订阅者**：`tracing` 的全局订阅者整个进程只有一个。`Engine::new` 尝试装一次 `tracing_subscriber::registry().with(ppvpn_core::tracing_layer())`；进程里已经有全局订阅者时什么也不装。
  - 宿主如果有自己的 tracing 订阅者，**必须**在上面加上 `ppvpn_core::tracing_layer()`（它已包含 Sail 的 layer，宿主不直接依赖 `sail`，也不写 `sail::` 路径），否则 Sail 和 `ppvpn-core` 的日志行进不了各实例的 sink；宿主的全局过滤器也要放行 `sail`、`ppvpn_core` 这两个 target，到实例所用的级别（debug）为止。
  - 这时 Sail 不再装它自己的订阅者，不再往 stdout/stderr 打带颜色的日志。宿主自己的 layer 也会看到这两个 target 的事件，怎么处理是宿主的事。
- **多实例**：同一进程里有几个实例时，`ppvpn-core` 的一行属于哪个实例，看事件上的 `instance` 字段，没有就看离它最近的、带 `instance` 字段的外层 span（引擎为每个实例建一个）。两者都没有的行是进程级的，发给每个实例。Sail 的行由 Sail 按实例区分。`instance` 字段不写进日志行。
- **轮转**：由宿主负责，引擎只按行输出，不管文件大小。
- **不阻塞数据面**：日志接收端阻塞时，引擎丢弃日志行，不让数据面等待。丢弃的行数计入 `status().dropped_log_lines`（引擎侧和 Sail 侧的合计，只增不减），恢复后再补一行 warn 汇总这段时间丢了多少：`level=warn msg="log lines dropped" dropped=<上次汇总以来丢的行数> source=core`，排在恢复后第一行的前面。
- **级别**：默认 info；debug 级别会记录每个连接和每次 DNS 查询（含域名），只在排障时开启。`ppvpn-core` 的行由引擎按 `LogConfig.level` 过滤，Sail 的行由 Sail 按同一级别过滤。
- **脱敏**：任何级别都不记录凭据（本地代理的密码、节点的密钥和密码）。本地代理状态文件损坏时，日志只写解析错误的类别和位置，不写文件内容。

## 11. 移动端宿主

iOS 和 Android 上，引擎运行在系统要求的隧道进程里：Android 是 `VpnService`，iOS 是 `NEPacketTunnelProvider`。持有引擎的是一个小的 Rust 隧道宿主，界面进程经 Unix socket 说 Core API v1（#448）。本节是隧道宿主和引擎之间的约定。

- **实例**：一个 `Role::Tun` 实例，`platform` 为 `Ios` 或 `Android`，没有本地代理。
- **隧道由宿主建立**：
  - 先调 `ppvpn_core::tun_settings(platform)`，得到 `TunSettings { mtu, ipv4_address, ipv4_routes, ipv6_address, ipv6_routes, dns_servers }`，再照着它建隧道：
    - Android：用 `VpnService.Builder` 的 `setMtu`、`addAddress`（IPv4 和 IPv6 各一次）、`addRoute`、`addDnsServer`，然后 `establish()`；
    - iOS：在 `NEPacketTunnelNetworkSettings` 里设 IPv4 和 IPv6 地址（`NEIPv4Settings`、`NEIPv6Settings`）及各自的 `includedRoutes`、DNS 和 MTU，然后取 utun 的 fd。

    桌面上这个函数返回 `None`。引擎生成的 TUN 配置用的是同一组值，所以两边不会对不上。
  - 隧道有 IPv4 和 IPv6（#511），两边的路由都是全部流量，所以 app 的 IPv6 不会绕过隧道。私网目的地按路由规则直连，经保护过的 socket（Android）或扩展自己的 socket（iOS）出去，不靠路由排除。DNS 地址在隧道网段内（IPv4），引擎劫持所有查询，宿主不另设解析器。
  - 宿主推送的网络没有全局 IPv6 地址时，引擎对直连的全局 IPv6 目的地做和桌面一样的 hand-off（按域名改走 IPv4）；网络变化后重新判断（见下面的网络状态）。
  - 建好以后，把 fd 放进 `TunConfig::with_fd(fd)`。
    - 每次启动 Sail（`start`、完整重启、热切换失败后的回退），引擎都交给 Sail 一个 `dup` 的副本，Sail 停止时关闭副本。
    - 原 fd 引擎**不关**，由宿主在 `shutdown` 返回后关闭。所以引擎重启时不需要重建隧道，系统的 VPN 图标也不会闪。
  - 不给 fd 时，Sail 自己打开设备。只有有权限的 Linux 能这样运行（lab 的 `ne-mem`）；在手机上这样做，`start` 会失败。
  - `tun.fd` 只有移动端的 Tun 实例接受。其他组合以及负数，`new` 都返回 `CORE_OPERATION_FAILED`（field=`tun.fd`）。
- **socket 保护（Android）**：用 `EngineConfig::with_socket_protect(SocketProtect::new(|fd| ...))` 设置，宿主在回调里调 `VpnService.protect(fd)`。
  - **覆盖范围**：引擎的每个出站 socket 连接前都先经过它：Sail 的出站（节点、直连、DNS 上游、规则集下载），以及入口探测的 TCP 和 ICMP。不保护的话，探测会进隧道，TUN 会自己应答 TCP 握手。
  - **失败与线程**：回调返回错误时，这次拨号或探测失败。回调在 Sail 或探测的线程上执行，里面不能再调用实例。
  - **适用平台**：iOS 不需要，因为扩展自己的流量本来就不进隧道。只有移动端的 Tun 实例接受，其他情况 `new` 返回 `CORE_OPERATION_FAILED`（field=`socket_protect`）。
  - **不序列化**：它是 `#[serde(skip)]` 字段，由 FFI 层直接设置。
- **网络状态**：宿主监听 `ConnectivityManager` 或 `NWPathMonitor`，在 `start` 之前和每次变化时调 `Engine::set_network_state(NetworkState)`；断网时传 `NetworkState::offline()`。
  - **格式**：`NetworkState` 的字段是 `interface`、`type`（wifi、cellular、ethernet、other）、`addresses`、`gateway`、`expensive`、`constrained`，序列化后就是 Sail 的网络状态 JSON。`interface` 为空表示断网。
  - **传递**：引擎保存最后一次的状态，每次启动 Sail 时先交给它；运行中调用时立即交给 Sail。
  - **之后的行为**：从此 Sail 按宿主的说法判断网络，不再自己探测。变化照常经 `NetworkChanged` 事件和离线状态（`Degraded{NoDefaultInterface}`）报告，和桌面一样。
  - **IPv6 出口**：`addresses` 里有全局单播 IPv6 地址（2000::/3）就算有 IPv6 出口。所以宿主要把默认网络的 IPv6 地址也放进去。
  - **桌面**：调用返回 `CORE_OPERATION_FAILED`，因为桌面上 Sail 自己的监视器是唯一来源（第 12 节）。
- **内存**：
  - 移动端的实例用 Sail 的 mobile profile：单线程，小缓冲和小窗口，`inbound.max_connections` 为 512。`sail::embed` 默认是 desktop profile，所以由引擎按 `platform` 设置。
  - `EngineConfig::with_max_connections(n)` 替换 profile 的上限，0 表示不限。**iOS 网络扩展设 128**：系统在 50 MiB 时结束扩展进程，#388 在这组参数下测得各场景峰值 28.8 MiB。Android 用 profile 的 512。
  - 没有内存告警接口，因为 Sail 没有收缩缓存的手段。内存上限靠 profile 加并发上限保证。
- **休眠**：iOS 的 `sleep()` / `wake()` 照转 `Engine::sleep` / `Engine::wake`（第 4.7 节）。
- **Profile**：经 `apply` 传入，只放在内存里（`docs/mobile.md`）。
- **健康检查和恢复**：由隧道宿主负责，因为界面进程在后台会被挂起。规则同 #438，在手机上怎么做见 `docs/mobile.md`。
- **隧道宿主**：`crates/tunnel-host`。原生层调用的 C 函数以 `crates/tunnel-host/include/ppvpn_tunnel.h` 为契约；Android 经 JNI 调用同样的函数（Kotlin 类 `com.peakpassvpn.ppvpn.tunnel.TunnelHost`，socket 保护经它的 `Listener.protect`）。引擎的 `max_connections` 在 iOS 上由它设为 128。

## 12. 内部结构（不是契约，供评审参考）

- **运行时**：启停、原地 reload、事件、日志和经指定出站拨号，都在一个内部 trait 后面实现。Sail 的嵌入式 API（`sail::embed`，E1）就绪后接到这个 trait 上，在那之前不依赖 Sail 的内部模块。
- **配置检查**：Profile 到 Sail 配置（sing-box JSON）的翻译是纯函数，只通过 `translate::check` 一处调用 Sail 的配置检查。E1 之前用 Sail 内部的检查函数，之后换成 `sail::embed::check`。
- **产品策略**：故障转移的防抖参数、DNS 回退预算、本地代理用户名规则等，在 `ppvpn-core` 里实现，叠加在 Sail 提供的机制之上。
- **dns-local（Tun 实例）**：用 `ppvpn-core` 自己的实现，移植自 Go 的 `internal/localdns`，不用 Sail 的 `local`。原因是 Sail 的 `local` 有几处直接影响用户的缺口：忽略 macOS 的手动 DNS、丢弃链路本地 DNS、每个服务器没有独立超时、不处理 TC、不过滤回环、没有默认网卡时不立即失败。这些作为 Sail 的通用改进继续推进，不阻塞切换。
  - Engine 在 `new` 时（仅 Tun 实例）在 127.0.0.1 的随机端口上起 UDP 和 TCP 监听。监听自己读默认网卡的 DNS 服务器（Windows 读适配器，macOS 读 scutil，Linux 读 resolv.conf 或 systemd-resolved 里该网卡的 DNS）；宿主给了 `local_dns_servers` 时改用这些静态服务器。查询按顺序发出，每个服务器有超时，TC 时改用 TCP，没有服务器时立即回 SERVFAIL。
  - `local_dns_servers` 每项是 IP、`IP:port` 或 `[IPv6]:port`，默认端口 53；IPv6 可以带 zone（网卡名或编号），如 `fe80::1%en0`、`[fe80::1%en0]:53`。其他写法（域名、端口 0、未指定地址、IPv4 带 zone、`[IPv6%zone]` 不带端口）让 `new` 失败：`CORE_OPERATION_FAILED`，field=`tun.local_dns_servers`。隧道网段内的地址（宿主残留的旧系统 DNS）被去掉，其余按原顺序使用；一个都不剩时与没给一样，读默认网卡。带 zone 的链路本地服务器只在 zone 是当前默认网卡时使用（按该网卡的编号发出），别的网卡的被跳过。移动端没有自己的 dns-local，服务器直接交给 Sail：Sail 的 DNS 服务器地址不接受 zone，带 zone 的项被跳过，不剩时用 Sail 的 `local`。
  - Sail 的 dns-local 服务器渲染成指向这个监听的 **tcp** 服务器。Sail 的 udp 客户端遇到截断应答不会改走 TCP，用 tcp 能拿到完整应答。
  - 服务器列表变化不需要 reload Sail；网卡变化的触发来自 Sail 的网络事件（只用 Sail 一个网卡监视器），监听收到后让缓存失效。
  - 桌面上 Engine 不向 Sail 推送网络状态，Sail 自己的监视器是唯一来源。移动端由宿主推送（`Engine::set_network_state`，第 11 节）。Sail 在"宿主推送"加 `auto_detect_interface` 时两边都会宣告网络变化（#214），移动端的 TUN 不设 `auto_detect_interface`，所以不会这样。
  - 上游查询优先经 Runtime 的 `dial_udp`/`dial_tcp` 走 direct 出站，由 Sail 的默认拨号器绑定物理网卡，不进 TUN。
- **TUN 网卡名**：Linux 固定为 `ppvpn0`，Windows 固定为 `PPVPN`（Wintun 适配器名，按名字复用；适配器 GUID 由名字确定生成，不会每次变化）。名字要显式交给 Sail，原因是 Sail 只有在名字显式给出时，才会把这块网卡当作自己的：选默认网卡和过滤 DNS 服务器时都要排除它；名字也便于日志和抓包。
  - macOS 不写名字，由 Sail 选：取比现有最大的 `utunN` 大一的编号，并通过 embed 报告实际拿到的名字（`tun_names()`），Runtime 读出来用于日志和状态。选好的名字在打开前被别的程序抢走时，Sail 换下一个空闲的名字重试，最多试 3 个；仍然失败时 `start` 返回 `TUN_NAME_TAKEN`（retryable=true）。排除隧道网段由 Sail 的这一行为保证，再加上 `ppvpn-core` 自己的 dns-local 的隧道地址过滤。
  - Linux 和 Windows 的名字是配置的，被占用时不换名，`start` 直接返回 `TUN_NAME_TAKEN`（retryable=false）。
  - 宿主不依赖这个名字：Desktop 靠地址、规则优先级和表号识别自己的 TUN。

## 13. 与 Core API v1 的对照

| Core API v1 | Rust `ppvpn-core` |
| --- | --- |
| `get-version` | `Engine::version()` |
| `validate-profile` | `Engine::validate(&ApplyRequest)` |
| `apply-profile`（`profile`、`routing_mode`、`allowed_rule_set_hosts`） | `apply(ApplyRequest)`，另外带 `selected_node_id` 和 `pins`，返回 `cleared_pins` 和 `selection_reset` |
| `start` / `stop` / `reload` | `start()` / `stop()`；`reload` 去掉（规则集刷新在引擎内部完成） |
| `get-status` | `status()` |
| `list-nodes` / `get-selected-node` | `nodes()` / `selected_node()` |
| `select-node` / `pin-ingress` | `select_node()` / `pin_ingress()` |
| `probe-entrances` / `probe-availability` | `probe_entrances()` / `probe_availability()` |
| `get-local-proxy-metadata` | `local_proxy_metadata()` |
| `get-local-proxy-credential`（`node_id` / `kind=routed`） | `local_proxy_credential(node_id)` / `local_proxy_routed_credential()` |
| `get-local-proxy-endpoints`（兼容接口） | 不提供（用前两项代替） |
| `set-system-proxy` / `get-system-proxy-endpoints` | `set_system_proxy_listener()` / `status().system_proxy` |
| `get-traffic` / `get-connections` | `traffic()` / `connections()` |
| `watch-events` | `subscribe(kinds)` |
| （无） | `set_network_state()`：移动端的隧道宿主直接调用（第 11 节） |
| 会话密钥、`X-Core-API-Version` | 不需要（进程内调用） |

## 14. 已定的问题（Core 第一轮审阅）

1. **cleared_pins 和 `NodeIngressPinCleared` 事件都保留**：内容相同，宿主幂等处理（第 4.1 节）。
2. **去掉 `reload`**：规则集的刷新和恢复由引擎内部完成，并发出 `RuleSetChanged`（第 4.1 节）。
3. **"流量是否进了 TUN"**：路由和规则层面的完整性归引擎，端到端的可达性归宿主，只用于提示用户（第 9 节）。
4. **实例数量**：只限制 Tun 实例（`TUN_INSTANCE_EXISTS`），Standard 实例不限（第 3 节）。

## 15. 评审结论（Desktop、CLI 第一轮）

已写进正文的：

- **CLI A1**：去重比较的是当前生效的值；pins 重复节点报 `PINS_INVALID`。
- **CLI A2**：`state_dir` 独占锁，冲突时报 `STATE_DIR_IN_USE`。
- **CLI A3**：本地代理端口的优先级、`LocalProxyEndpointChanged` 事件、`LocalProxyUnavailable` 状态，以及 `preferred_port=0`。
- **CLI A4**：凭据在 `new` 时就生成或读取。
- **CLI A5、Desktop E**：`shutdown` 上限 10 秒，超时返回遗留项；Drop 上限 5 秒。
- **CLI**：状态字段全部保留，`EngineState` 的 JSON 示例见第 5 节。
- **Desktop A**：`set_system_proxy_listener` 只管 7891 的监听。
- **Desktop B**：TUN 路由完整性扩展到 macOS 和 Windows（新增能力，G5 实机验收）。
- **Desktop C**：新增 `DefaultRouteOverridden`、`LocalProxyUnavailable` 两个降级原因。
- **Desktop D**：wintun.dll 由宿主分发，通过 `TunConfig` 传入。
- **Desktop F**：日志接收端阻塞时丢弃日志，不阻塞数据面。
- **Desktop G**：new 阶段能确定的失败直接返回错误，不进入 `Fatal`。
- **Desktop 第 14 节第 2 项**：不照搬"稳定期"，改为第 9 节的两条保证。

- **过期**（Core 定）：转发与 Go 一致；新增只用于上报的 `Degraded{ProfileExpired}`，由定时器触发（第 4.1 节、第 5 节、第 9 节）。
- **CLI 补充**（不阻塞合并）：
  - Profile 续用以它自己的 `expires_at` 为硬上限，并写清了 Go 版在运行中越过 `expires_at` 时的行为（第 4.1 节）；
  - `ppvpn-account` 的错误分三类：暂时性错误、无可用服务、需要重新登录；
  - Profile 只放内存，`state_dir` 不保存 Profile 原文（第 4.6 节、第 9 节）。
- **Desktop H**：Backend 确认 `default_node_id` 一定存在，并且固定为 `nodes[0]`；有实例的入口一个都渲染不出来时返回错误，`nodes` 为空时返回 404，不会下发残缺或空的 Profile。已写进第 4.1 节和第 9 节（第 9 节附了后端 404 和 500 的口径）。
