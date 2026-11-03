# Android 通知接入（#539）

Android 使用 FCM。没有 Google Play 服务、没有 Firebase 配置，或取得 FCM token 失败 / 超过 30 秒时，改用已有的自建推送协议。自建代理只在用户已经启动的 VPN 前台服务内运行，不为推送单独启动或保持 VPN。

## 构建配置

在 Firebase 项目里添加包名 `com.peakpassvpn.ppvpn` 的 Android 应用，下载 `google-services.json`。本地 Android 构建使用 Linux 环境及 Android SDK / NDK；设置 `ANDROID_GOOGLE_SERVICES_FILE` 为该文件的绝对路径，设置 `ANDROID_NDK_HOME` 和 [后端地址](release.md#后端地址) `PPVPN_API_BASE_DEFAULT`，再从仓库根目录运行 `FLUTTER_TARGET=android apps/flutter/scripts/ci-build.sh`。脚本会校验包名并把 JSON 复制到生成的 runner；不要把它提交到仓库。

自动发布可配置 `ANDROID_GOOGLE_SERVICES_JSON` secret，内容是完整的 JSON 文本，不是 base64。`release.yml` 将其写到临时文件，构建后删除。没有这个配置也可以构建和运行，但 FCM 不可用，只能在 VPN 运行期间使用自建通道。后端发送 FCM 所需的服务账号仍在后端配置；客户端 JSON 不能替代它。

生成脚本 `apps/flutter/scripts/configure-android-push.py` 同时加入 Firebase Messaging、WorkManager、非导出的接收服务、通知图标和权限。VPN 的服务类型是 `systemExempted`，启动服务后立即显示常驻通知，服务销毁后移除。发布者需要在 Play Console 中填写与 VPN 前台服务相符的声明。

## 注册、展示与点击

- Dart 安装回调后，原生宿主发送缓存的 token 和启动时的通知点击；恢复前台时重新检查 FCM token。通知权限在登录后申请一次，拒绝不会导致每次打开都弹窗。
- `PushMessagingService.onNewToken` 把 token 写入应用私有 SharedPreferences，并排队 WorkManager 网络任务。即使 Activity / Flutter 引擎未运行，后台任务也能通过 JNI 恢复同一个 Rust 客户端、注册最新 token。后台和界面初始化有互斥锁，不创建第二个认证客户端。已退出登录时仅保留 token，下一次登录再注册。
- `onNewToken` 也会通知已有的 Rust 客户端关闭备用通道；客户端未运行时直接移除启用标记，不为此新建客户端。普通 WorkManager 任务在重建进程时恢复已有标记，不改变该开关，避免旧 token 的重试覆盖较新的 FCM 不可用判定。
- 后台注册失败会重试。令牌轮换使用 WorkManager 的 `APPEND_OR_REPLACE` 策略排在已有任务之后，避免任务结束前的新轮换被丢弃；每次执行都读取最新 token，旧请求的响应不能把新 token 标记成已注册。任务不会启动 VPN，也不会恢复后台未读数轮询。
- FCM HTTP v1 的 notification + data 消息在后台由 SDK 显示；前台通知及 data 消息由原生宿主显示。通知权限或消息频道被禁用时，应用仍可使用站内消息。
- 点击只传 `message_id` 给 Dart，再由当前登录账号的消息列表取得正文；不打开 payload 中的 `deep_link`，不信任 payload 提供的正文。重复点击复用消息页面。切换账号或退出登录会清除待处理点击和旧的消息缓存。

## VPN 内的备用通道

启用备用通道时，UI 客户端使用独立的 `desktop-android` 设备注册取得自建通道令牌，不替换 `android` 的 FCM 注册。注册文件与启用标记都在 `filesDir/data`，权限为 0600。

VPN 的 `:tunnel` 进程观察这些文件。只有 VPN 已启动、启用标记存在且注册有效时才运行 `PushAgent`；这个进程不创建认证客户端。显示每条通知前还会检查注册文件是否仍对应同一账号。原生宿主报告取得 FCM token、退出登录或 VPN 停止时，都会结束备用代理。停止可以取消长轮询和 ACK 请求，不等待网络超时。

FCM 的判断依据是服务 / 配置是否可用，以及 token 请求的结果，**不是端到端投递探测**。如果 SDK 成功返回缓存 token，但实际消息通道被网络阻断，当前实现无法据此自动发现和切换。应用进入后台后停止未读数轮询；恢复前台会立即刷新。

## 验证

CI 的 Android 任务在构建后运行配置测试和原生回归测试。原生测试使用实际 Kotlin 宿主、Android / Flutter / Firebase / WorkManager 依赖以及 Robolectric API 26 和 35，JNI 与 FCM 请求边界可替换；它不验证真实 FCM 投递。

```sh
python3 -m unittest discover -s apps/flutter/checks -p 'test_android_push_config.py'
# 设置 Android SDK 36、JDK 17+、Gradle 9.3.1；也可用生成 runner 的 gradlew。
export GRADLE=/absolute/path/to/gradle
export PPVPN_TEST_FLUTTER_JAR=/absolute/path/to/flutter/bin/cache/artifacts/engine/android-arm/flutter.jar
apps/flutter/scripts/test-android-push.sh
cd apps/flutter
flutter test --no-pub checks
```

真实 Firebase / Play 配置和完整 Android 安装验收仍待执行。发布前还需在使用真实 Firebase 配置的设备上验收：

1. 前台、后台和应用未启动时收通知，点击打开对应消息；连续点击不会堆积页面。
2. 拒绝通知权限、关闭消息频道后，登录和 VPN 仍可正常使用。
3. Activity 不运行时轮换 token，确认 WorkManager 更新后端；退出 / 更换账号不展示旧账号内容。
4. 无 Google Play 服务、无 Firebase 配置和 token 请求超时时，VPN 运行期间自建通知可达；关闭 VPN 后代理停止。
5. VPN 快速停止再启动、界面进程被回收、网络切换和 FCM 恢复时，常驻通知与备用代理跟随正确的服务状态。
