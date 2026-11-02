// Overview (design Main / DesktopDark): the connection card (status orb,
// title, node and latency, the session timer, the route), the modes and
// node card, then live traffic and the local proxy side by side. A full
// empty state instead while access is limited. System proxy and enhanced
// mode are the client's one connection in either mode: each switch picks
// its mode and connects, and turning it off disconnects. Enhanced mode
// installs the privileged service on first use, after the explanation
// dialog (handoff "7. 增强模式安装授权").

import 'dart:async';

import 'package:flutter/widgets.dart';
import 'package:forui/forui.dart';
import 'package:window_manager/window_manager.dart';

import '../app_state.dart';
import '../src/rust/api.dart' as rust;
import '../strings.dart';
import '../theme.dart';
import '../updates.dart';
import '../widgets.dart';

/// Busy phases: the switch shows its in-between state.
bool isBusy(rust.UiPhase phase) => switch (phase) {
  rust.UiPhase.preparing ||
  rust.UiPhase.waitingPermission ||
  rust.UiPhase.connecting ||
  rust.UiPhase.reconnecting ||
  rust.UiPhase.disconnecting => true,
  _ => false,
};

/// The connection is in enhanced mode and not off: the status follows it.
bool enhancedActive(rust.UiSnapshot snapshot) =>
    snapshot.connectionMode == rust.UiConnectionMode.enhanced && snapshot.phase != rust.UiPhase.off;

/// A mode's switch: on, in between, or off (also while the other mode has
/// the connection).
SwitchState switchState(rust.UiSnapshot snapshot, rust.UiConnectionMode mode) => snapshot.connectionMode != mode
    ? SwitchState.off
    : snapshot.phase == rust.UiPhase.connected
    ? SwitchState.on
    : isBusy(snapshot.phase)
    ? SwitchState.busy
    : SwitchState.off;

/// The title of a contended enhanced mode: held by a session or user, or
/// the network taken by another app (#438).
String contendedTitle(rust.UiContended? contended) => switch (contended) {
  rust.UiContended.networkTaken || rust.UiContended.routesTaken => S.current('h_networkTaken'),
  _ => S.current('h_occupied'),
};

/// The status title (handoff "状态标题规则"): enhanced mode's while it is
/// not off, else the system proxy's.
String statusTitle(rust.UiSnapshot snapshot) {
  final s = S.current;
  final phase = snapshot.phase;
  if (enhancedActive(snapshot)) {
    return switch (phase) {
      rust.UiPhase.connected => s('h_on'),
      rust.UiPhase.error => s('h_failed'),
      rust.UiPhase.contended => contendedTitle(snapshot.contended),
      rust.UiPhase.preparing => s('st_preparing'),
      rust.UiPhase.waitingPermission => s('st_authorizing'),
      rust.UiPhase.connecting => s('st_connecting'),
      rust.UiPhase.reconnecting => s('st_reconnecting'),
      rust.UiPhase.disconnecting => s('st_disconnecting'),
      rust.UiPhase.off => s('h_idle'),
    };
  }
  return switch (phase) {
    rust.UiPhase.connected => s('h_on'),
    rust.UiPhase.disconnecting => s('h_stdStopping'),
    rust.UiPhase.error || rust.UiPhase.contended => s('h_stdFail'),
    _ when isBusy(phase) => s('h_stdStarting'),
    _ => s('h_idle'),
  };
}

/// Enhanced mode's line under the title while it has no node line to show.
String? enhancedDetail(rust.UiSnapshot snapshot) => !enhancedActive(snapshot)
    ? null
    : switch (snapshot.phase) {
        rust.UiPhase.preparing => S.current('d_preparing'),
        rust.UiPhase.waitingPermission => S.current('d_authorizing'),
        rust.UiPhase.disconnecting => S.current('d_disconnecting'),
        _ => null,
      };

Color statusColor(Tokens t, rust.UiSnapshot snapshot) => switch (snapshot.phase) {
  rust.UiPhase.connected => t.ok,
  rust.UiPhase.error => t.bad,
  rust.UiPhase.contended => t.warn,
  final phase when isBusy(phase) => t.accent,
  _ => t.t3,
};

/// Turns enhanced mode on; before the service is installed, only after
/// the install explanation ("Cancel" leaves it off, no failure). The client
/// then installs it behind the system's admin prompt (`WaitingPermission`).
Future<void> turnOnEnhanced(BuildContext context, AppState app) async {
  if (!app.snapshot.value.serviceInstalled && !await confirmInstall(context)) return;
  await app.connectIn(rust.UiConnectionMode.enhanced);
}

class OverviewPage extends StatelessWidget {
  const OverviewPage({super.key, required this.app});

  final AppState app;

  @override
  Widget build(BuildContext context) => ValueListenableBuilder(
    valueListenable: app.snapshot,
    builder: (context, snapshot, _) {
      final limited = LimitedAccess.of(snapshot);
      final loading = snapshot.profile == rust.UiProfile.loading;
      return Swap(
        id: limited != null || loading ? snapshot.profile : null,
        expand: true,
        child: limited ?? (loading ? const LoadingNodes() : _page(snapshot)),
      );
    },
  );

  Widget _page(rust.UiSnapshot snapshot) {
    final problem = ProblemCard.of(app, snapshot);
    return SingleChildScrollView(
      padding: Tokens.pagePadding,
      child: Center(
        child: ConstrainedBox(
          constraints: const BoxConstraints(maxWidth: Tokens.pageMaxWidth),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              ValueListenableBuilder(
                valueListenable: Updates.installed,
                builder: (context, version, _) =>
                    version == null ? const SizedBox.shrink() : _InstalledNotice(version: version),
              ),
              Reveal(
                child: snapshot.profile == rust.UiProfile.invalid
                    ? Column(
                        crossAxisAlignment: CrossAxisAlignment.stretch,
                        children: [
                          ConfigWarning(previous: snapshot.profilePrevious),
                          const SizedBox(height: Tokens.cardGap),
                        ],
                      )
                    : null,
              ),
              _Hero(app: app, snapshot: snapshot),
              Reveal(
                id: problem?.title,
                child: problem == null
                    ? null
                    : Column(
                        crossAxisAlignment: CrossAxisAlignment.stretch,
                        children: [const SizedBox(height: Tokens.cardGap), problem],
                      ),
              ),
              const SizedBox(height: Tokens.cardGap),
              _Modes(app: app, snapshot: snapshot),
              const SizedBox(height: Tokens.cardGap),
              IntrinsicHeight(
                child: Row(
                  crossAxisAlignment: CrossAxisAlignment.stretch,
                  children: [
                    Expanded(child: TrafficCard(app: app)),
                    const SizedBox(width: Tokens.cardGap),
                    Expanded(
                      child: _LocalProxy(app: app, nodeId: snapshot.selectedNodeId),
                    ),
                  ],
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }
}

class _InstalledNotice extends StatefulWidget {
  const _InstalledNotice({required this.version});
  final String version;

  @override
  State<_InstalledNotice> createState() => _InstalledNoticeState();
}

class _InstalledNoticeState extends State<_InstalledNotice> with WindowListener {
  @override
  void initState() {
    super.initState();
    windowManager.addListener(this);
    WidgetsBinding.instance.addPostFrameCallback((_) => unawaited(_shown()));
  }

  Future<void> _shown() async {
    if (!mounted) return;
    try {
      if (await windowManager.isVisible() && !await windowManager.isMinimized()
          && await windowManager.isFocused() && mounted) await Updates.noticeShown();
    } catch (_) {
      /* A hidden/unavailable window does not consume the notice. */
    }
  }

  @override
  void onWindowFocus() => unawaited(_shown());

  @override
  void onWindowRestore() => unawaited(_shown());

  @override
  void dispose() {
    windowManager.removeListener(this);
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    return Padding(
      padding: const EdgeInsets.only(bottom: Tokens.cardGap),
      child: Container(
        padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 8),
        decoration: BoxDecoration(
          color: t.ok.withValues(alpha: 0.08),
          borderRadius: BorderRadius.circular(Tokens.cardRadius),
        ),
        child: Row(
          children: [
            Expanded(child: Text(S.current('updatedTo', {'v': widget.version}), style: t.note)),
            Button(
              kind: ButtonKind.icon,
              icon: FLucideIcons.x,
              semanticsLabel: S.current('close'),
              onPress: Updates.dismissInstalled,
            ),
          ],
        ),
      ),
    );
  }
}

/// The connection card.
class _Hero extends StatelessWidget {
  const _Hero({required this.app, required this.snapshot});

  final AppState app;
  final rust.UiSnapshot snapshot;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final s = S.current;
    final phase = snapshot.phase;
    final connected = phase == rust.UiPhase.connected;
    final busy = isBusy(phase);
    final tone = statusColor(t, snapshot);
    final node = app.nodeById(snapshot.selectedNodeId);
    final detail = enhancedDetail(snapshot);
    return Container(
      clipBehavior: Clip.antiAlias,
      decoration: t.cardDecoration(),
      child: Stack(
        children: [
          // The tint from the top left, in the state's colour.
          Positioned.fill(
            child: AnimatedContainer(
              duration: Tokens.motion(context, Tokens.motionBase),
              curve: Tokens.motionCurve,
              decoration: BoxDecoration(
                gradient: RadialGradient(
                  center: Alignment.topLeft,
                  radius: 1.3,
                  colors: [tone.withValues(alpha: 0.11), tone.withValues(alpha: 0)],
                  stops: const [0, 0.55],
                ),
              ),
            ),
          ),
          // The dot grid on the right half, fading in from the left.
          Positioned.fill(child: CustomPaint(painter: _Dots(tone))),
          Padding(
            padding: const EdgeInsets.fromLTRB(22, 20, 22, 18),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                Row(
                  children: [
                    _Orb(snapshot: snapshot),
                    const SizedBox(width: 16),
                    Expanded(
                      child: Column(
                        crossAxisAlignment: CrossAxisAlignment.start,
                        children: [
                          Semantics(
                            header: true,
                            liveRegion: true,
                            child: Text(statusTitle(snapshot), style: t.heroTitle),
                          ),
                          const SizedBox(height: 2),
                          Swap(
                            id: phase == rust.UiPhase.off ? phase : (detail ?? node?.id),
                            child: phase == rust.UiPhase.off
                                ? Text(s('d_idle'), style: t.sub)
                                : detail != null
                                ? Text(detail, style: t.sub)
                                : node == null
                                ? const SizedBox.shrink()
                                : Row(
                                    children: [
                                      Flag(countryCode: node.exitCountryCode),
                                      const SizedBox(width: 8),
                                      Flexible(
                                        child: Text(
                                          nodeTitle(node),
                                          maxLines: 1,
                                          overflow: TextOverflow.ellipsis,
                                          style: t.sub.copyWith(color: t.t1, fontWeight: FontWeight.w600),
                                        ),
                                      ),
                                      const SizedBox(width: 8),
                                      LatencyPill.ms(snapshot.latencyMs),
                                    ],
                                  ),
                          ),
                        ],
                      ),
                    ),
                    Swap(
                      id: connected ? 'timer' : (busy ? 'busy' : null),
                      alignment: AlignmentDirectional.centerEnd,
                      child: connected
                          ? SessionTimer(since: app.connectedSince)
                          : busy
                          ? Column(
                              crossAxisAlignment: CrossAxisAlignment.end,
                              children: [
                                Text(
                                  s(snapshot.connectionMode == rust.UiConnectionMode.enhanced ? 'tunMode' : 'stdMode'),
                                  style: t.sectionTitle.copyWith(color: t.accent),
                                ),
                                Text(statusTitle(snapshot), style: t.note),
                              ],
                            )
                          : const SizedBox.shrink(),
                    ),
                  ],
                ),
                Reveal(
                  child: node == null || phase == rust.UiPhase.off
                      ? null
                      : Padding(
                          padding: const EdgeInsets.only(top: 18, left: Tokens.orbSize + 16),
                          child: RouteLine(
                            lines: lineNames(node),
                            current: node.lines.indexWhere((line) => line.key == snapshot.lineKey),
                            connected: connected,
                          ),
                        ),
                ),
              ],
            ),
          ),
        ],
      ),
    );
  }
}

class _Orb extends StatelessWidget {
  const _Orb({required this.snapshot});

  final rust.UiSnapshot snapshot;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final phase = snapshot.phase;
    final connected = phase == rust.UiPhase.connected;
    final busy = isBusy(phase);
    final tone = statusColor(t, snapshot);
    final light = t.brightness == Brightness.light;
    return SizedBox(
      width: Tokens.orbSize,
      height: Tokens.orbSize,
      child: Stack(
        alignment: Alignment.center,
        children: [
          AnimatedContainer(
            duration: Tokens.motion(context, Tokens.motionBase),
            curve: Tokens.motionCurve,
            decoration: BoxDecoration(
              shape: BoxShape.circle,
              gradient: connected && light ? Tokens.connectedOrb : null,
              color: connected && light ? null : tone.withValues(alpha: 0.10),
              border: connected && light ? null : Border.all(color: tone.withValues(alpha: 0.25)),
              boxShadow: [
                BoxShadow(color: tone.withValues(alpha: 0.12), spreadRadius: 6),
                BoxShadow(color: tone.withValues(alpha: 0.06), spreadRadius: 13),
                if (connected && light) BoxShadow(color: tone.withValues(alpha: 0.30), offset: const Offset(0, 6), blurRadius: 16),
              ],
            ),
          ),
          if (busy) Spinner(color: tone, size: Tokens.orbSize),
          Swap(
            id: (problem: phase == rust.UiPhase.error || phase == rust.UiPhase.contended, off: phase == rust.UiPhase.off, tone: tone),
            alignment: Alignment.center,
            child: phase == rust.UiPhase.error || phase == rust.UiPhase.contended
                ? Icon(FLucideIcons.circleAlert, size: 26, color: tone)
                : phase == rust.UiPhase.off
                ? Icon(FLucideIcons.link2Off, size: 24, color: tone)
                : ShieldCheckIcon(size: 26, stroke: 2.2, color: connected && light ? const Color(0xFFFFFFFF) : tone),
          ),
        ],
      ),
    );
  }
}

class _Dots extends CustomPainter {
  _Dots(this.color);
  final Color color;

  @override
  void paint(Canvas canvas, Size size) {
    // The right 48 %, fading in from its left edge over 60 % of it.
    final left = size.width * 0.52;
    final span = size.width - left;
    for (var x = left + 6; x < size.width; x += 12) {
      final fade = ((x - left) / (span * 0.6)).clamp(0.0, 1.0);
      final paint = Paint()..color = color.withValues(alpha: 0.2 * 0.7 * fade);
      for (var y = 6.0; y < size.height; y += 12) {
        canvas.drawCircle(Offset(x, y), 1, paint);
      }
    }
  }

  @override
  bool shouldRepaint(_Dots old) => old.color != color;
}

/// 「本次连接」: hh:mm:ss since the connection came on, once a second.
class SessionTimer extends StatefulWidget {
  const SessionTimer({required this.since});

  final DateTime? since;

  @override
  State<SessionTimer> createState() => _SessionTimerState();
}

class _SessionTimerState extends State<SessionTimer> {
  // Started in initState: a `late final` initializer would only run on first
  // access, which was dispose(), so the timer never ticked.
  late final Timer _tick;

  @override
  void initState() {
    super.initState();
    _tick = Timer.periodic(const Duration(seconds: 1), (_) => setState(() {}));
  }

  @override
  void dispose() {
    _tick.cancel();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final since = widget.since;
    final elapsed = since == null ? Duration.zero : DateTime.now().difference(since);
    String two(int n) => n.toString().padLeft(2, '0');
    final text = '${two(elapsed.inHours)}:${two(elapsed.inMinutes % 60)}:${two(elapsed.inSeconds % 60)}';
    return Column(
      crossAxisAlignment: CrossAxisAlignment.end,
      children: [
        Text(text, style: t.monoText(22, weight: FontWeight.w500, height: 28)),
        Text(S.current('thisSession'), style: t.note),
      ],
    );
  }
}

/// 本机 → the node's lines → 互联网. The line in use and those before it are
/// lit while connected; the rest wait (dashed).
class RouteLine extends StatelessWidget {
  const RouteLine({required this.lines, required this.current, required this.connected, this.compact = false});

  /// [lineNames], in failover order.
  final List<String> lines;

  /// The line in use, an index into [lines]; -1 when none is.
  final int current;
  final bool connected;

  /// The phone's: 24 high hops of 11 · 600, no icons, centred, short links.
  final bool compact;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final tone = connected ? t.ok : t.accent;
    final reached = current + 1;
    Widget hop(String text, {IconData? icon, bool lit = false, bool wait = false, bool mono = false}) => Container(
      height: compact ? 24 : 26,
      padding: EdgeInsets.symmetric(horizontal: compact ? 9 : 10),
      decoration: BoxDecoration(
        color: wait ? null : t.hop,
        borderRadius: BorderRadius.circular(13),
        border: Border.all(color: lit ? tone.withValues(alpha: 0.25) : (wait ? t.line : t.hair)),
      ),
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          if (icon != null && !compact) ...[Icon(icon, size: 13, color: wait ? t.t3 : t.t2), const SizedBox(width: 6)],
          Text(
            text,
            style: compact
                ? (mono ? t.monoText(11, weight: FontWeight.w600, color: lit ? tone : (wait ? t.t3 : t.t2)) : t.label)
                      .copyWith(color: lit ? tone : (wait ? t.t3 : t.t2))
                : mono
                ? t.monoText(12, weight: FontWeight.w500, color: lit ? tone : (wait ? t.t3 : t.t2))
                : t.note.copyWith(fontWeight: FontWeight.w500, color: wait ? t.t3 : t.t2),
          ),
        ],
      ),
    );
    Widget link(bool wait) => Flexible(
      child: ConstrainedBox(
        constraints: BoxConstraints(maxWidth: compact ? 22 : 72),
        child: Container(
          height: 2,
          margin: EdgeInsets.symmetric(horizontal: compact ? 3 : 4),
          child: CustomPaint(painter: _Dashes(wait ? t.t3.withValues(alpha: 0.4) : tone.withValues(alpha: 0.55), wait)),
        ),
      ),
    );
    final shown = lines.take(3).toList();
    return Semantics(
      label: [S.current('thisDevice'), ...shown, S.current('internet')].join(' → '),
      child: ExcludeSemantics(
        child: Row(
          mainAxisAlignment: compact ? MainAxisAlignment.center : MainAxisAlignment.start,
          children: [
            hop(S.current('thisDevice'), icon: FLucideIcons.monitor),
            for (var i = 0; i < shown.length; i++) ...[
              link(i >= reached),
              hop(shown[i], lit: i < reached, wait: i >= reached, mono: true),
            ],
            link(!connected),
            hop(S.current('internet'), icon: FLucideIcons.globe, wait: !connected),
          ],
        ),
      ),
    );
  }
}

class _Dashes extends CustomPainter {
  _Dashes(this.color, this.short);
  final Color color;
  final bool short;

  @override
  void paint(Canvas canvas, Size size) {
    final paint = Paint()..color = color;
    final (dash, gap) = short ? (3.0, 5.0) : (6.0, 4.0);
    for (var x = 0.0; x < size.width; x += dash + gap) {
      canvas.drawRRect(
        RRect.fromRectAndRadius(Rect.fromLTWH(x, 0, dash, size.height), const Radius.circular(1)),
        paint,
      );
    }
  }

  @override
  bool shouldRepaint(_Dashes old) => old.color != color || old.short != short;
}

/// System proxy, enhanced mode and the current node, 58-high rows.
class _Modes extends StatelessWidget {
  const _Modes({required this.app, required this.snapshot});

  final AppState app;
  final rust.UiSnapshot snapshot;

  @override
  Widget build(BuildContext context) {
    final s = S.current;
    // While one mode is on its way, the other mode's switch waits.
    bool enabled(rust.UiConnectionMode mode) => !isBusy(snapshot.phase) || snapshot.connectionMode == mode;
    return CardBox(
      child: Column(
        children: [
          SettingRow(
            icon: FLucideIcons.monitor,
            title: s('stdMode'),
            description: s('stdDesc'),
            trailing: TriSwitch(
              key: const ValueKey('switch-system-proxy'),
              label: s('stdMode'),
              state: switchState(snapshot, rust.UiConnectionMode.compatible),
              enabled: enabled(rust.UiConnectionMode.compatible),
              onChanged: (on) => on ? app.connectIn(rust.UiConnectionMode.compatible) : app.disconnect(),
            ),
          ),
          SettingRow(
            icon: FLucideIcons.zap,
            title: s('tunMode'),
            description: s('tunDesc'),
            divider: true,
            trailing: TriSwitch(
              label: s('tunMode'),
              state: switchState(snapshot, rust.UiConnectionMode.enhanced),
              enabled: enabled(rust.UiConnectionMode.enhanced),
              onChanged: (on) => on ? turnOnEnhanced(context, app) : app.disconnect(),
            ),
          ),
          if (!snapshot.serviceInstalled && !enhancedActive(snapshot)) _InstallHint(app: app),
          SettingRow(
            icon: FLucideIcons.mapPin,
            title: s('currentNode'),
            divider: true,
            trailing: NodePicker(app: app, selectedId: snapshot.selectedNodeId),
          ),
        ],
      ),
    );
  }
}

/// The current-node dropdown (34 high): flag, name, latency, chevron.
class NodePicker extends StatelessWidget {
  const NodePicker({super.key, required this.app, required this.selectedId});

  final AppState app;
  final String? selectedId;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    return ValueListenableBuilder(
      valueListenable: app.nodes,
      builder: (context, nodes, _) {
        final selected = app.nodeById(selectedId);
        return FPopoverMenu(
          maxHeight: 320,
          menuAnchor: Alignment.topRight,
          childAnchor: Alignment.bottomRight,
          menu: [
            FItemGroup(
              children: [
                for (final node in nodes)
                  FItem(
                    selected: node.id == selectedId,
                    prefix: Flag(countryCode: node.exitCountryCode),
                    title: Text(nodeTitle(node), maxLines: 1, overflow: TextOverflow.ellipsis),
                    suffix: ValueListenableBuilder(
                      valueListenable: app.latency,
                      builder: (context, latency, _) => LatencyPill(latency: latency[node.id]),
                    ),
                    onPress: () => app.selectNode(node.id),
                  ),
              ],
            ),
          ],
          builder: (context, controller, _) => Pressable(
            key: const ValueKey('node-picker'),
            onPress: controller.toggle,
            label: '${S.current('currentNode')}: ${selected == null ? '—' : nodeTitle(selected)}',
            builder: (context, hovered) => Container(
              height: 34,
              constraints: const BoxConstraints(maxWidth: 320),
              padding: const EdgeInsets.fromLTRB(8, 0, 10, 0),
              decoration: BoxDecoration(
                color: t.brightness == Brightness.light ? t.card : t.sunk,
                borderRadius: BorderRadius.circular(Tokens.controlRadius),
                border: Border.all(color: t.line),
              ),
              child: Row(
                mainAxisSize: MainAxisSize.min,
                children: [
                  if (selected != null) ...[Flag(countryCode: selected.exitCountryCode), const SizedBox(width: 10)],
                  Flexible(
                    child: Text(
                      selected == null ? '—' : nodeTitle(selected),
                      maxLines: 1,
                      overflow: TextOverflow.ellipsis,
                      style: t.control.copyWith(color: t.t1),
                    ),
                  ),
                  if (selected != null) ...[
                    const SizedBox(width: 10),
                    ValueListenableBuilder(
                      valueListenable: app.snapshot,
                      builder: (context, snapshot, _) => LatencyPill.ms(snapshot.latencyMs),
                    ),
                  ],
                  const SizedBox(width: 10),
                  Icon(FLucideIcons.chevronDown, size: 12, color: t.t3),
                ],
              ),
            ),
          ),
        );
      },
    );
  }
}

/// Under the enhanced-mode row while the privileged service is missing
/// (handoff "7. 增强模式安装授权", step 1): a soft-accent line with the
/// install button, which explains first and installs without connecting.
class _InstallHint extends StatelessWidget {
  const _InstallHint({required this.app});

  final AppState app;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final s = S.current;
    return Container(
      margin: const EdgeInsets.fromLTRB(20, 0, 20, 12),
      padding: const EdgeInsets.fromLTRB(12, 4, 4, 4),
      decoration: BoxDecoration(color: t.accentSoft, borderRadius: BorderRadius.circular(Tokens.controlRadius)),
      child: Row(
        children: [
          Icon(FLucideIcons.info, size: 14, color: t.accent),
          const SizedBox(width: 8),
          Expanded(
            child: Text(s('needInstall'), style: t.note.copyWith(color: t.accent)),
          ),
          Button(
            kind: ButtonKind.text,
            label: s('installBtn'),
            height: 26,
            onPress: () async {
              if (await confirmInstall(context)) await app.installService();
            },
          ),
        ],
      ),
    );
  }
}

/// A one-line problem card (handoff "异常提示") with its buttons on the
/// right: enhanced mode failed or is in use elsewhere, or the system proxy
/// failed.
class ProblemCard extends StatelessWidget {
  const ProblemCard({required this.warn, required this.title, required this.description, required this.actions});

  /// Warning tone (in use elsewhere) instead of the failure tone.
  final bool warn;
  final String title;
  final String description;
  final List<Widget> actions;

  /// The card for the snapshot's connection, or null while there is none.
  static ProblemCard? of(AppState app, rust.UiSnapshot snapshot) {
    final s = S.current;
    final enhanced = snapshot.connectionMode == rust.UiConnectionMode.enhanced;
    final retry = Button(kind: ButtonKind.secondary, label: s('retry'), onPress: app.retry);
    final compatible = [
      if (enhanced && snapshot.suggestCompatible)
        Button(
          kind: ButtonKind.text,
          label: s('useCompat'),
          onPress: () => app.connectIn(rust.UiConnectionMode.compatible),
        ),
    ];
    final expired = snapshot.error?.code == 'ProfileExpired';
    return switch (snapshot.phase) {
      // The core refused the profile: neither the tunnel nor the system
      // proxy is at fault (#532).
      rust.UiPhase.error when expired || snapshot.error?.code == 'ProfileInvalid' => ProblemCard(
        warn: false,
        title: s(expired ? 'cfgExpiredT' : 'cfgT'),
        description: s(expired ? 'fr_profileExpired' : 'fr_profile'),
        actions: [
          Button(kind: ButtonKind.secondary, label: s('refreshNodes'), onPress: app.refreshNodes),
          retry,
        ],
      ),
      rust.UiPhase.error when enhanced => ProblemCard(
        warn: false,
        title: '${s('tunMode')} · ${s('st_failed')}',
        description: enhancedReason(snapshot.error),
        actions: [...compatible, retry],
      ),
      rust.UiPhase.contended => ProblemCard(
        warn: true,
        title: contendedTitle(snapshot.contended),
        description: switch (snapshot.contended) {
          rust.UiContended.anotherUser => s('d_occupiedUser'),
          rust.UiContended.networkTaken => s('fr_contended'),
          rust.UiContended.routesTaken => s('d_routesTaken'),
          rust.UiContended.anotherSession || null => s('d_occupied'),
        },
        actions: [
          ...compatible,
          if (snapshot.canTakeOver)
            Button(kind: ButtonKind.secondary, label: s('takeOver'), onPress: app.takeOver)
          // Nothing to retry against another OS user's connection.
          else if (snapshot.contended != rust.UiContended.anotherUser)
            retry,
        ],
      ),
      // The tunnel works; only the PPVPN API is unreachable for now (#438).
      rust.UiPhase.connected when enhanced && snapshot.apiUnreachable => ProblemCard(
        warn: true,
        title: s('h_on'),
        description: s('d_apiUnreachable'),
        actions: const [],
      ),
      rust.UiPhase.error => ProblemCard(
        warn: false,
        title: s('h_stdFail'),
        description: switch (snapshot.error?.code) {
          null || 'SystemProxyFailed' => s('fr_port'),
          final code => code,
        },
        actions: [retry],
      ),
      _ => null,
    };
  }

  /// Why enhanced mode failed, for people; the code when there is no line
  /// for it.
  static String enhancedReason(rust.UiError? error) {
    final s = S.current;
    return switch (error?.code) {
      'ServiceInstallCancelled' => s('fr_auth'),
      'ServiceInstallFailed' => s('fr_install'),
      'ServiceUnavailable' || 'ServiceIncompatible' => s('fr_service'),
      'NetworkPathContended' => s('fr_contended'),
      'Timeout' => s('fr_timeout'),
      final code? => code,
      null => s('st_failed'),
    };
  }

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final tone = warn ? t.warn : t.bad;
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 10),
      decoration: BoxDecoration(
        color: warn ? t.warnSoft : t.badSoft,
        borderRadius: BorderRadius.circular(Tokens.cardRadius),
        border: Border.all(color: t.hair),
      ),
      child: Row(
        children: [
          Icon(FLucideIcons.circleAlert, size: 16, color: tone),
          const SizedBox(width: 10),
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(title, style: t.sectionTitle),
                Text(description, style: t.note),
              ],
            ),
          ),
          for (final (i, action) in actions.indexed) ...[if (i > 0) const SizedBox(width: 8), action],
        ],
      ),
    );
  }
}

class TrafficCard extends StatelessWidget {
  const TrafficCard({required this.app, this.phone = false});

  final AppState app;

  /// The phone's (手机 · 概览): no heading, radius 16, the sparkline 48
  /// high across the card's whole width.
  final bool phone;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final s = S.current;
    Widget rate(String label, Color key, int value) => Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              Container(
                width: 8,
                height: 3,
                decoration: BoxDecoration(color: key, borderRadius: BorderRadius.circular(2)),
              ),
              const SizedBox(width: 6),
              Text(label, style: t.note.copyWith(color: t.t2)),
            ],
          ),
          // The number rolls from the last value to this one (#535).
          TweenAnimationBuilder<double>(
            tween: Tween(end: value.toDouble()),
            duration: Tokens.motion(context, _rateRoll),
            curve: Curves.easeOut,
            builder: (context, shown, _) {
              final (number, unit) = formatRate(shown.round());
              return Text.rich(
                TextSpan(
                  children: [
                    TextSpan(text: number, style: t.rate),
                    TextSpan(
                      text: ' $unit',
                      style: t.monoText(12, weight: FontWeight.w500, color: t.t3),
                    ),
                  ],
                ),
              );
            },
          ),
        ],
      );

    final rates = ValueListenableBuilder(
      valueListenable: app.traffic,
      builder: (context, sample, _) => Row(
        children: [
          Expanded(child: rate(s('upload'), t.accent2, sample.up)),
          const SizedBox(width: 12),
          Expanded(child: rate(s('download'), t.ok, sample.down)),
        ],
      ),
    );
    final sparkline = _SlidingSparkline(app: app, up: t.accent2, down: t.ok);
    if (phone) {
      return Container(
        clipBehavior: Clip.antiAlias,
        decoration: t.phoneCardDecoration(),
        child: Semantics(
          label: s('traffic'),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              Padding(padding: const EdgeInsets.fromLTRB(16, 14, 16, 0), child: rates),
              const SizedBox(height: 6),
              SizedBox(height: 48, child: sparkline),
            ],
          ),
        ),
      );
    }
    return CardBox(
      padding: const EdgeInsets.symmetric(horizontal: 20, vertical: 16),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Text(s('traffic'), style: t.sectionTitle),
          const SizedBox(height: 10),
          ValueListenableBuilder(
            valueListenable: app.traffic,
            builder: (context, sample, _) => Row(
              children: [
                Expanded(child: rate(s('upload'), t.accent2, sample.up)),
                const SizedBox(width: 12),
                Expanded(child: rate(s('download'), t.ok, sample.down)),
              ],
            ),
          ),
          const SizedBox(height: 8),
          SizedBox(height: 44, child: sparkline),
        ],
      ),
    );
  }
}

/// A rate's roll from one sample to the next (the motion board, #535).
const _rateRoll = Duration(milliseconds: 300);

/// The sparkline, moving (#535): each new sample slides the lines left by
/// one step over the time since the previous one, linearly, and the scale
/// eases to the new peak; under reduced motion it jumps.
class _SlidingSparkline extends StatefulWidget {
  const _SlidingSparkline({required this.app, required this.up, required this.down});

  final AppState app;
  final Color up;
  final Color down;

  @override
  State<_SlidingSparkline> createState() => _SlidingSparklineState();
}

class _SlidingSparklineState extends State<_SlidingSparkline> with SingleTickerProviderStateMixin {
  late final AnimationController _slide = AnimationController(vsync: this, value: 1);
  DateTime? _last;
  double _fromPeak = 1;
  double _toPeak = 1;

  @override
  void initState() {
    super.initState();
    _toPeak = _fromPeak = _peak(widget.app.history.value);
    widget.app.history.addListener(_sampled);
  }

  @override
  void dispose() {
    widget.app.history.removeListener(_sampled);
    _slide.dispose();
    super.dispose();
  }

  static double _peak(List<rust.UiTraffic> samples) {
    var peak = 1;
    for (final s in samples) {
      if (s.down > peak) peak = s.down;
      if (s.up > peak) peak = s.up;
    }
    return peak.toDouble();
  }

  void _sampled() {
    final now = DateTime.now();
    final since = _last == null ? Duration.zero : now.difference(_last!);
    _last = now;
    _fromPeak = _fromPeak + (_toPeak - _fromPeak) * _slide.value;
    _toPeak = _peak(widget.app.history.value);
    final duration = Tokens.motion(context, since > const Duration(seconds: 3) ? const Duration(seconds: 3) : since);
    if (duration == Duration.zero) {
      _slide.value = 1;
    } else {
      _slide.duration = duration;
      unawaited(_slide.forward(from: 0));
    }
  }

  @override
  Widget build(BuildContext context) => AnimatedBuilder(
    animation: _slide,
    builder: (context, _) {
      final t = _slide.value;
      final peak = _fromPeak + (_toPeak - _fromPeak) * Curves.easeOutCubic.transform(t);
      return CustomPaint(
        painter: _Sparkline(widget.app.history.value, up: widget.up, down: widget.down, shift: 1 - t, peak: peak),
      );
    },
  );
}

/// Download (filled) and upload lines over the last samples, on one scale
/// ([peak]), [shift] steps to the right of their place (the slide in).
class _Sparkline extends CustomPainter {
  _Sparkline(this.samples, {required this.up, required this.down, this.shift = 0, required this.peak});

  final List<rust.UiTraffic> samples;
  final Color up;
  final Color down;
  final double shift;
  final double peak;

  @override
  void paint(Canvas canvas, Size size) {
    if (samples.length < 2) return;
    final step = size.width / (AppState.historyLength - 1);
    final start = size.width - step * (samples.length - 1) + step * shift;
    canvas.clipRect(Offset.zero & size);
    Path line(int Function(rust.UiTraffic) pick) {
      final path = Path();
      for (var i = 0; i < samples.length; i++) {
        final point = Offset(start + step * i, size.height - 2 - (size.height - 6) * pick(samples[i]) / peak);
        i == 0 ? path.moveTo(point.dx, point.dy) : path.lineTo(point.dx, point.dy);
      }
      return path;
    }

    Paint stroke(Color color) => Paint()
      ..color = color
      ..style = PaintingStyle.stroke
      ..strokeWidth = 1.6
      ..strokeJoin = StrokeJoin.round;
    final downLine = line((s) => s.down);
    final area = Path.from(downLine)
      ..lineTo(size.width, size.height)
      ..lineTo(start, size.height)
      ..close();
    canvas.drawPath(
      area,
      Paint()
        ..shader = LinearGradient(
          begin: Alignment.topCenter,
          end: Alignment.bottomCenter,
          colors: [down.withValues(alpha: 0.22), down.withValues(alpha: 0)],
        ).createShader(Offset.zero & size),
    );
    canvas.drawPath(downLine, stroke(down));
    canvas.drawPath(line((s) => s.up), stroke(up));
  }

  @override
  bool shouldRepaint(_Sparkline old) =>
      !identical(old.samples, samples) ||
      old.up != up ||
      old.down != down ||
      old.shift != shift ||
      old.peak != peak;
}

class _LocalProxy extends StatelessWidget {
  const _LocalProxy({required this.app, required this.nodeId});

  final AppState app;
  final String? nodeId;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final s = S.current;
    return CardBox(
      padding: const EdgeInsets.symmetric(horizontal: 20, vertical: 16),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Text(s('localProxy'), style: t.sectionTitle),
          Padding(padding: const EdgeInsets.only(top: 2), child: Text(s('proxyNote'), style: t.note)),
          ValueListenableBuilder(
            valueListenable: app.proxies,
            builder: (context, proxies, _) {
              final proxy = proxies[nodeId];
              if (proxy == null) {
                return Padding(
                  padding: const EdgeInsets.only(top: 9),
                  child: Text('—', style: t.note),
                );
              }
              return ProxyRows(proxy: proxy);
            },
          ),
        ],
      ),
    );
  }
}

/// HTTP and SOCKS5 with copy buttons, then the user name and the masked
/// password: 28-high sunk rows. The copied URL carries the credentials.
class ProxyRows extends StatelessWidget {
  const ProxyRows({super.key, required this.proxy});

  final rust.UiLocalProxy proxy;

  static String url(rust.UiLocalProxy proxy, String scheme) {
    final user = Uri.encodeComponent(proxy.username);
    final password = Uri.encodeComponent(proxy.password);
    return '$scheme://$user:$password@${proxy.host}:${proxy.port}';
  }

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    Widget row(String key, String value, Widget trailing) => Container(
      height: 28,
      margin: const EdgeInsets.only(top: 9),
      padding: const EdgeInsets.fromLTRB(10, 0, 4, 0),
      decoration: BoxDecoration(color: t.sunk, borderRadius: BorderRadius.circular(Tokens.segmentRadius)),
      child: Row(
        children: [
          // 54 in the design (Chinese); no wrapping for longer labels.
          ConstrainedBox(
            constraints: const BoxConstraints(minWidth: 54),
            child: Text(key, maxLines: 1, softWrap: false, style: t.note),
          ),
          const SizedBox(width: 8),
          Expanded(
            child: Text(
              value,
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              style: t.monoText(12),
            ),
          ),
          trailing,
        ],
      ),
    );
    final endpoint = '${proxy.host}:${proxy.port}';
    return Column(
      children: [
        row('HTTP', 'http://$endpoint', CopyButton(text: url(proxy, 'http'))),
        row('SOCKS5', 'socks5://$endpoint', CopyButton(text: url(proxy, 'socks5h'))),
        row(
          S.current('authUser'),
          proxy.username,
          Padding(
            padding: const EdgeInsets.only(right: 8),
            child: Text('${S.current('authPass')} ••••••', style: t.monoText(12, color: t.t3)),
          ),
        ),
      ],
    );
  }
}

/// Access limited: no subscription, subscription expired, team disabled
/// (a full empty state replaces the page; the tray shows its title).
class LimitedAccess extends StatelessWidget {
  const LimitedAccess._({required this.icon, required this.title, required this.description});

  final IconData icon;
  final String title;
  final String description;

  static LimitedAccess? of(rust.UiSnapshot snapshot) {
    final s = S.current;
    return switch (snapshot.profile) {
      rust.UiProfile.noSubscription => LimitedAccess._(
        icon: FLucideIcons.shield,
        title: s('noSubT'),
        description: s('noSubD'),
      ),
      rust.UiProfile.subscriptionExpired => LimitedAccess._(
        icon: FLucideIcons.clock,
        title: s('expiredT'),
        description: s('expiredD', {'d': snapshot.expiresAt ?? '—'}),
      ),
      rust.UiProfile.teamDisabled => LimitedAccess._(
        icon: FLucideIcons.circleAlert,
        title: s('teamOffT'),
        description: s('teamOffD', {'team': snapshot.teamName ?? '—'}),
      ),
      _ => null,
    };
  }

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    return Center(
      child: Padding(
        padding: Tokens.pagePadding,
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Icon(icon, size: 44, color: t.t3),
            const SizedBox(height: 12),
            Semantics(header: true, child: Text(title, style: t.heroTitle, textAlign: TextAlign.center)),
            const SizedBox(height: 6),
            Text(description, style: t.sub, textAlign: TextAlign.center),
          ],
        ),
      ),
    );
  }
}

class LoadingNodes extends StatelessWidget {
  const LoadingNodes({super.key});

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    return Center(
      child: Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          Spinner(color: t.accent, size: 24),
          const SizedBox(height: 12),
          Text(S.current('loadingT'), style: t.sectionTitle),
          const SizedBox(height: 4),
          Text(S.current('loadingD'), style: t.sub),
        ],
      ),
    );
  }
}

/// The profile is invalid: says whether an earlier one is still in use
/// (`previous`) or there is none (#532).
class ConfigWarning extends StatelessWidget {
  const ConfigWarning({super.key, required this.previous});

  final bool previous;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 10),
      decoration: BoxDecoration(
        color: t.warnSoft,
        borderRadius: BorderRadius.circular(Tokens.cardRadius),
        border: Border.all(color: t.hair),
      ),
      child: Row(
        children: [
          Icon(FLucideIcons.circleAlert, size: 16, color: t.warn),
          const SizedBox(width: 10),
          Expanded(
            child: Text.rich(
              TextSpan(
                children: [
                  TextSpan(text: '${S.current('cfgT')} · ', style: t.sectionTitle),
                  TextSpan(text: S.current(previous ? 'cfgD' : 'invalidD'), style: t.sub),
                ],
              ),
            ),
          ),
        ],
      ),
    );
  }
}
