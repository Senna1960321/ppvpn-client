// Screenshot mode for CI (#387): PPVPN_FLUTTER_SAMPLE=overview|nodes|logs|
// messages|settings shows the signed-in pages on fixed data, the design
// handoff's sample nodes and messages (sample-nodes.json,
// sample-messages.json) and a few log lines, with no client behind them: no
// backend, no core, no sign-in, no log or settings file read or written.
// PPVPN_FLUTTER_THEME=dark|light forces the theme. Only
// apps/flutter/scripts/smoke.sh sets these (by hand on iOS, with
// --dart-define).

import 'dart:io';
import 'dart:math' as math;

import 'package:flutter/services.dart';
import 'package:flutter/widgets.dart';

import 'app_state.dart';
import 'messages_state.dart';
import 'settings.dart';
import 'src/rust/api.dart' as rust;
import 'strings.dart';

/// The page the screenshot mode opens on, or null when it is off. Also as
/// a --dart-define, and on iOS from [loadSampleSettings].
String? samplePage = _page(
  Platform.environment['PPVPN_FLUTTER_SAMPLE'] ?? const String.fromEnvironment('PPVPN_FLUTTER_SAMPLE'),
);

/// The forced theme brightness, or null to follow the system.
Brightness? forcedBrightness = _brightness(
  Platform.environment['PPVPN_FLUTTER_THEME'] ?? const String.fromEnvironment('PPVPN_FLUTTER_THEME'),
);

String? _page(String? name) => const {'overview', 'nodes', 'logs', 'messages', 'settings'}.contains(name) ? name : null;

Brightness? _brightness(String? name) => switch (name) {
  'dark' => Brightness.dark,
  'light' => Brightness.light,
  _ => null,
};

/// iOS: an app launched by simctl has the variables in its process but not
/// in Platform.environment (#535); a simulator build asks the app channel
/// (runner/ios PPVPNPlugin), which also reads the launch arguments
/// (`-PPVPN_FLUTTER_SAMPLE nodes`), so one build shows every page. Before
/// anything reads [samplePage].
Future<void> loadSampleSettings() async {
  if (!Platform.isIOS || samplePage != null) return;
  try {
    final settings = await const MethodChannel(
      'com.peakpassvpn.ppvpn/app',
    ).invokeMapMethod<String, String>('sampleSettings');
    samplePage = _page(settings?['page']);
    forcedBrightness ??= _brightness(settings?['theme']);
  } on PlatformException catch (error) {
    debugPrint('sampleSettings: $error');
  } on MissingPluginException {
    // A runner without it: no screenshot mode.
  }
}

/// The logs page in the screenshot mode: its directory (as the footer shows
/// it), its "today" and its lines.
final String sampleLogDir = Platform.isMacOS
    ? '~/Library/Logs/PPVPN'
    : Platform.isWindows
    ? r'%LOCALAPPDATA%\PPVPN\logs'
    : '~/.local/state/ppvpn/logs';
final DateTime sampleLogDate = DateTime.utc(2026, 10, 20);
const sampleLogLines = [
  '2026-10-20T14:30:02.418Z  INFO ppvpn_desktop: client created app_version=0.7.0',
  '2026-10-20T14:30:02.733Z  INFO ppvpn_desktop::session: session restored',
  '2026-10-20T14:30:03.120Z  INFO ppvpn_desktop::session: profile ready nodes=11',
  '2026-10-20T14:30:03.402Z  INFO ppvpn_desktop::standard: core started',
  '2026-10-20T14:30:03.958Z  INFO ppvpn_desktop::standard: local proxies listening 127.0.0.1:7890',
  '2026-10-20T14:30:04.211Z  INFO ppvpn_desktop::connection: connecting node=hk1 line=HKG-A',
  '2026-10-20T14:30:04.597Z  INFO ppvpn_desktop::sysproxy: system proxy applied',
  '2026-10-20T14:30:04.603Z  INFO ppvpn_desktop::connection: connected node=hk1 latency_ms=38',
  '2026-10-20T14:31:12.044Z  INFO ppvpn_desktop::standard: probe finished method=tcp nodes=11',
  '2026-10-20T14:31:12.045Z  WARN ppvpn_desktop::standard: probe timed out node=de1',
  '2026-10-20T14:36:40.881Z  INFO ppvpn_desktop::notifications: unread=3',
  '2026-10-20T14:42:18.306Z  WARN ppvpn_desktop::connection: line failed, failing over from=HKG-A to=HKG-B',
  '2026-10-20T14:42:18.912Z  INFO ppvpn_desktop::connection: connected node=hk1 line=HKG-B latency_ms=46',
  '2026-10-20T14:55:00.007Z  INFO ppvpn_desktop::session: profile unchanged',
];

AppState sampleState() {
  rust.UiNode node(
    String id,
    String country,
    String name,
    String region,
    List<String> lines, {
    String? tier,
    String key = 'standard',
  }) => rust.UiNode(
    id: id,
    name: name,
    exitRegion: region,
    exitCountryCode: country,
    entryLabel: tier,
    entryKey: key,
    udp: true,
    lines: [for (final line in lines) rust.UiLine(key: line, label: line)],
  );
  final nodes = [
    node('hk1', 'HK', 'Hong Kong 01', 'Hong Kong', ['HKG-A', 'HKG-B', 'SZX-R'], tier: 'Premium', key: 'premium'),
    node('hk2', 'HK', 'Hong Kong 02', 'Hong Kong', ['HKG-C', 'HKG-D']),
    node('tw1', 'TW', 'Taipei 01', 'Taiwan', ['TPE-A', 'TPE-B'], tier: 'Optimized', key: 'cn-optimized'),
    node('jp1', 'JP', 'Tokyo 01', 'Japan', ['NRT-A', 'NRT-B', 'HND-A'], tier: 'Premium', key: 'premium'),
    node('sg1', 'SG', 'Singapore 01', 'Singapore', ['SIN-A', 'SIN-B'], tier: 'Optimized', key: 'cn-optimized'),
    node('kr1', 'KR', 'Seoul 01', 'South Korea', ['ICN-A', 'ICN-B']),
    node('jp2', 'JP', 'Osaka 01', 'Japan', ['KIX-A']),
    node('us1', 'US', 'Los Angeles 01', 'United States', ['LAX-A', 'LAX-B', 'SJC-A'], tier: 'Optimized', key: 'cn-optimized'),
    node('us2', 'US', 'New York 01', 'United States', ['JFK-A']),
    node('de1', 'DE', 'Frankfurt 01', 'Germany', ['FRA-A', 'FRA-B']),
    node('gb1', 'GB', 'London 01', 'United Kingdom', ['LHR-A']),
  ];
  const latencies = {'hk1': 38, 'hk2': 46, 'tw1': 55, 'jp1': 62, 'sg1': 74, 'kr1': 83, 'jp2': 91, 'us1': 168, 'us2': 231};
  final app = AppState.fixed(
    rust.UiSnapshot(
      auth: rust.UiAuth.signedIn,
      deviceCode: null,
      accountName: 'Jordan',
      teamName: 'Acme Studio',
      teamId: 'acme',
      profile: rust.UiProfile.ready,
      profilePrevious: false,
      profileRevision: 'sample',
      expiresAt: '2027-03-31T00:00:00Z',
      phase: rust.UiPhase.connected,
      lineKey: 'HKG-A',
      latencyMs: 38,
      standardReady: true,
      localProxyGeneration: 1,
      selectedNodeId: 'hk1',
      error: null,
      unread: _sampleUnread,
      routingMode: rust.UiRoutingMode.rules,
      // The design's Main: system proxy on, the service not installed yet.
      connectionMode: rust.UiConnectionMode.compatible,
      serviceInstalled: false,
      canTakeOver: false,
      suggestCompatible: false,
      apiUnreachable: false,
    ),
  );
  app.nodes.value = nodes;
  app.latency.value = {
    for (final n in nodes)
      n.id: switch (latencies[n.id]) {
        final ms? => LatencyMs(ms),
        null => n.id == 'de1' ? const LatencyTimeout() : const LatencyFailed(),
      },
  };
  app.proxies.value = {
    for (final n in nodes)
      n.id: rust.UiLocalProxy(nodeId: n.id, host: '127.0.0.1', port: 7890, username: 'u8f2k-${n.id}', password: 'sample'),
  };
  app.history.value = [
    for (var i = 0; i < AppState.historyLength; i++)
      rust.UiTraffic(
        up: (1.2e6 * (0.7 + 0.3 * math.sin(i / 3))).round(),
        down: (8.4e6 * (0.4 + 0.6 * i / AppState.historyLength) * (0.85 + 0.15 * math.cos(i / 2))).round(),
      ),
  ];
  app.traffic.value = app.history.value.last;
  Settings.version = '0.6.3';
  app.connectedSince = DateTime.now().subtract(const Duration(minutes: 42, seconds: 17));
  final messages = _sampleMessages();
  app.messages.view.value = MessagesView(items: messages, total: messages.length, loaded: true);
  return app;
}

/// The newest ones are unread.
const _sampleUnread = 4;

/// sample-messages.json, aged as the handoff's prototype ages them
/// (minutes before now).
List<rust.UiMessage> _sampleMessages() {
  const c = rust.UiMessageCategory.values;
  const v = rust.UiMessageSeverity.values;
  rust.UiMessageCategory category(String type) => c.byName(switch (type) {
    'expiring' => 'subscriptionExpiring',
    'expired' => 'subscriptionExpired',
    'bill' => 'billing',
    'broadcast' => 'announcement',
    _ => type,
  });
  const link = 'https://www.example.com/console';
  // (type, severity, link, age, zh title, zh body, en title, en body)
  const sample = [
    ('expiring', 'critical', true, 3, '订阅将在 3 天后到期', '你的 Pro 年付套餐将于 2026年10月2日 到期，到期后将无法连接任何节点。续费后立即生效，剩余时长自动顺延，系统代理与增强模式的设置会保留。', 'Subscription expires in 3 days', 'Your Pro annual plan expires on Oct 2, 2026, after which you can’t connect to any node. Renewal takes effect immediately and remaining time carries over. System Proxy and Enhanced Mode settings are kept.'),
    ('route', 'important', false, 18, '香港 01 线路 HKG-A 不稳定', 'HKG-A 在 14:05–14:20 出现间歇性丢包，客户端已自动切换到 HKG-B。如仍有问题，可在节点页重新测速或切换节点。', 'Hong Kong 01 route HKG-A is unstable', 'HKG-A had intermittent packet loss from 14:05 to 14:20, and the client switched to HKG-B automatically. If problems persist, re-test latency on the Nodes page or switch nodes.'),
    ('bill', 'normal', true, 52, '9 月账单已生成', '本期账单金额 ¥128.00，将于 10月1日 从钱包余额自动扣除。', 'September bill is ready', 'This bill is ¥128.00 and will be charged to your wallet balance on Oct 1.'),
    ('broadcast', 'unspecified', true, 131, '东京节点维护通知', '为提升线路质量，东京 01 将于 10月1日 02:00–04:00（UTC+8）进行维护，期间该节点不可用。建议提前切换到大阪 01 或首尔 01，维护完成后无需任何操作。', 'Tokyo node maintenance', 'To improve route quality, Tokyo 01 will be under maintenance on Oct 1, 02:00–04:00 (UTC+8) and unavailable during that time. Switch to Osaka 01 or Seoul 01 beforehand. No action is needed afterwards.'),
    ('order', 'normal', true, 262, '订单 #20260928-0417 已支付', 'Pro 年付套餐 · ¥1,188.00。发票可在控制台「账单」中申请。', 'Order #20260928-0417 paid', 'Pro annual plan · ¥1,188.00. Request an invoice under Billing in the console.'),
    ('route', 'important', false, 1450, '法兰克福 01 暂时不可用', '上游运营商故障，预计 2 小时内恢复。恢复前测速会显示超时。', 'Frankfurt 01 temporarily unavailable', 'An upstream carrier outage should be fixed within 2 hours. Latency tests will show a timeout until then.'),
    ('expired', 'critical', true, 1690, '团队订阅已过期', 'Lumen Labs 的团队订阅已于 9月1日 到期，团队成员将无法连接节点。请联系团队管理员续费。', 'Team subscription expired', 'The Lumen Labs team subscription expired on Sep 1, so team members can’t connect to nodes. Ask your team admin to renew.'),
    ('broadcast', 'unspecified', true, 3249, '客户端 1.4.0 已发布', '新增消息中心与实时流量显示，修复了若干已知问题。', 'Client 1.4.0 released', 'Adds the message center and live traffic, and fixes several known issues.'),
    ('bill', 'important', true, 5145, '钱包余额不足', '当前余额 ¥12.50，不足以支付下个周期的费用。请在到期前充值，以免服务中断。', 'Low wallet balance', 'Your balance is ¥12.50, which isn’t enough for the next billing cycle. Top up before it’s due to avoid interruption.'),
    ('order', 'unspecified', false, 5878, '退款已完成', '订单 #20260903-1122 的退款 ¥49.00 已原路退回。', 'Refund completed', 'The ¥49.00 refund for order #20260903-1122 was returned to the original payment method.'),
  ];
  final now = DateTime.now();
  final zh = S.current.zh;
  return [
    for (final (i, m) in sample.indexed)
      rust.UiMessage(
        id: i + 1,
        title: zh ? m.$5 : m.$7,
        content: zh ? m.$6 : m.$8,
        category: category(m.$1),
        severity: v.byName(m.$2),
        deepLink: m.$3 ? link : null,
        read: i >= _sampleUnread,
        createdAt: now.subtract(Duration(minutes: m.$4)).toUtc().toIso8601String(),
      ),
  ];
}
