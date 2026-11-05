// The UI's view of the client. The state lives in Rust; this only holds the
// latest copies it pushed (ValueNotifier per kind) and forwards actions. On
// a phone it also starts and stops the system VPN around the client's
// connection (vpn.dart).

import 'dart:async';

import 'package:flutter/foundation.dart';

import 'links.dart';
import 'messages_state.dart';
import 'mobile.dart';
import 'src/rust/api.dart' as rust;
import 'vpn.dart';

/// A speed-test result as the nodes page shows it.
sealed class Latency {
  const Latency();
}

class LatencyTesting extends Latency {
  const LatencyTesting();
}

class LatencyMs extends Latency {
  const LatencyMs(this.ms);
  final int ms;
}

class LatencyTimeout extends Latency {
  const LatencyTimeout();
}

class LatencyFailed extends Latency {
  const LatencyFailed();
}

/// The window's pages the tray opens.
enum WindowPage { messages, settings }

class AppState {
  AppState(rust.UiSnapshot initial, {bool? useSystemVpn})
      : fixed = false, _useSystemVpn = useSystemVpn ?? mobile, snapshot = ValueNotifier(initial) {
    _refreshNodes(initial);
    if (_useSystemVpn) SystemVpn.listen();
    unawaited(_pump());
  }

  /// Fixed state with no client behind it (sample.dart, for CI
  /// screenshots): nothing is read from Rust, actions fail quietly.
  AppState.fixed(rust.UiSnapshot initial, {bool? useSystemVpn})
      : fixed = true, _useSystemVpn = useSystemVpn ?? mobile, snapshot = ValueNotifier(initial) {
    _stopped = true;
  }

  /// Made by [AppState.fixed]: sample data, no client.
  final bool fixed;

  /// Native VPN actions; injectable so their ordering can be tested on a host.
  final bool _useSystemVpn;

  final ValueNotifier<rust.UiSnapshot> snapshot;
  final ValueNotifier<List<rust.UiNode>> nodes = ValueNotifier(const []);
  final ValueNotifier<rust.UiTraffic> traffic = ValueNotifier(const rust.UiTraffic(up: 0, down: 0));

  /// The last [historyLength] samples, oldest first (the sparkline).
  final ValueNotifier<List<rust.UiTraffic>> history = ValueNotifier(const []);
  static const historyLength = 30;

  /// When the connection last came on; null while it is not on. The
  /// connection card's 「本次连接」 timer counts from it.
  DateTime? connectedSince;
  final ValueNotifier<Map<String, Latency>> latency = ValueNotifier(const {});
  final ValueNotifier<Map<String, rust.UiLocalProxy>> proxies = ValueNotifier(const {});

  /// The latest failed action, for the one-time error dialog.
  final ValueNotifier<String?> actionError = ValueNotifier(null);

  /// Teams the account can switch between: read when the account menu
  /// opens ([refreshTeams]).
  final ValueNotifier<List<rust.UiTeam>> teams = ValueNotifier(const []);

  /// The message center's list.
  late final MessagesState messages = MessagesState(fixed: fixed, onError: (error) => actionError.value = error);
  /// Bumped when the tray turns enhanced mode on before the service is
  /// installed: the window shows the install explanation first (shell.dart).
  final ValueNotifier<int> enhancedRequests = ValueNotifier(0);

  /// The tray's latest ask for a page in the window (shell.dart); `request`
  /// counts up, so asking for the same page again still notifies.
  final ValueNotifier<({WindowPage page, int request})?> pageRequests = ValueNotifier(null);

  void showPage(WindowPage page) => pageRequests.value = (page: page, request: (pageRequests.value?.request ?? 0) + 1);

  /// A notification click (activation.dart), handled as the native apps
  /// do. The push behind it opens its http(s) link in the browser (and
  /// marks its message read); else the message center opens on its inbox
  /// message, or on the push itself, read-only. An unknown or old push
  /// opens the message center. Signed out the window shows the sign-in.
  /// False when the window need not show (the link opened).
  Future<bool> openPush(int pushId) async {
    if (fixed) return true;
    await _restored();
    rust.UiPush? push;
    try {
      push = rust.shownPush(pushId: pushId);
    } catch (error) {
      debugPrint('shown_push: $error');
    }
    final signedIn = snapshot.value.auth == rust.UiAuth.signedIn;
    final messageId = push?.messageId;
    final link = push?.message.deepLink;
    if (link != null) {
      unawaited(openUrl(link));
      if (messageId != null && signedIn) {
        try {
          await rust.markNotificationRead(id: messageId);
        } catch (error) {
          debugPrint('mark_notification_read: $error');
        }
      }
      return false;
    }
    if (signedIn) {
      if (push != null) messages.requestOpen((messageId: messageId, push: push.message));
      showPage(WindowPage.messages);
    }
    return true;
  }

  /// Until the saved sign-in has been read: a click can start the app.
  Future<void> _restored() async {
    while (snapshot.value.auth == rust.UiAuth.restoring) {
      final changed = Completer<void>();
      void done() {
        if (!changed.isCompleted) changed.complete();
      }

      snapshot.addListener(done);
      await changed.future;
      snapshot.removeListener(done);
    }
  }

  /// Set once [stop] runs: the event loop ends.
  bool _stopped = false;
  String? _revision;
  bool _standardReady = false;

  Future<void> _pump() async {
    while (!_stopped) {
      final rust.UiEvents events;
      try {
        events = await rust.nextEvents();
      } catch (error) {
        debugPrint('next_events: $error');
        await Future<void>.delayed(const Duration(seconds: 1));
        continue;
      }
      final next = events.snapshot;
      if (next != null) {
        _followVpn(snapshot.value.phase, next.phase);
        _followSignIn(snapshot.value, next);
        if (next.phase == rust.UiPhase.connected) {
          connectedSince ??= DateTime.now();
        } else if (next.phase != rust.UiPhase.reconnecting) {
          connectedSince = null;
        }
        snapshot.value = next;
        _refreshNodes(next);
      }
      final sample = events.traffic;
      if (sample != null) {
        traffic.value = sample;
        final list = [...history.value, sample];
        history.value = list.length > historyLength ? list.sublist(list.length - historyLength) : list;
      }
      if (events.probes.isNotEmpty) {
        final map = Map.of(latency.value);
        for (final probe in events.probes) {
          final ms = probe.latencyMs;
          map[probe.nodeId] = probe.success && ms != null
              ? LatencyMs(ms)
              : probe.timedOut
              ? const LatencyTimeout()
              : const LatencyFailed();
        }
        latency.value = map;
      }
    }
  }

  /// Phone: the client hands the sign-in's page to the app (rust/src/hooks.rs
  /// open_url), which opens each new one; the page goes once signed in.
  void _followSignIn(rust.UiSnapshot before, rust.UiSnapshot after) {
    if (!mobile) return;
    final url = after.deviceCode?.verificationUrl;
    if (url != null && url != before.deviceCode?.verificationUrl) unawaited(openUrl(url));
    if (after.auth == rust.UiAuth.signedIn && before.auth == rust.UiAuth.awaitingBrowser) {
      unawaited(closeBrowser());
    }
  }

  /// Phone: the client's connection ended (off, or an error such as the
  /// host not coming up or stopping): the system VPN goes too, so that no
  /// tunnel is left up with no engine behind it. Retry starts it again.
  void _followVpn(rust.UiPhase before, rust.UiPhase after) {
    if (!_useSystemVpn || before == after) return;
    if (after == rust.UiPhase.off || after == rust.UiPhase.error) unawaited(_stopVpn());
  }

  /// Phone: permission, native start and client connection share one attempt,
  /// so early failures roll back even without a phase change.
  Future<void> _withVpn(Future<void> Function() connectClient) =>
      _useSystemVpn ? SystemVpn.connect(connectClient) : connectClient();

  Future<void> _stopVpn() async {
    try {
      await SystemVpn.stop();
    } catch (error) {
      debugPrint('vpn stop: $error');
    }
  }

  void _refreshNodes(rust.UiSnapshot next) {
    if (next.profileRevision != _revision) {
      _revision = next.profileRevision;
      try {
        nodes.value = rust.nodes();
      } catch (error) {
        debugPrint('nodes: $error');
      }
    }
    if (next.standardReady && !_standardReady) unawaited(_loadProxies());
    _standardReady = next.standardReady;
  }

  Future<void> _loadProxies() async {
    try {
      final list = await rust.localProxies();
      proxies.value = {for (final proxy in list) proxy.nodeId: proxy};
    } catch (error) {
      debugPrint('local_proxies: $error');
    }
  }

  rust.UiNode? nodeById(String? id) {
    for (final node in nodes.value) {
      if (node.id == id) return node;
    }
    return null;
  }

  Future<void> _run(Future<void> Function() action) async {
    try {
      await action();
    } catch (error) {
      actionError.value = '$error';
    }
  }

  Future<void> signIn() => _run(rust.signIn);
  void cancelSignIn() => rust.cancelSignIn();
  Future<void> signOut() => _run(rust.signOut);
  Future<void> connect() => _run(() => _withVpn(rust.connect));

  /// Phone: the client first (it stops the tunnel host), then the system VPN.
  Future<void> disconnect() => _run(() async {
    if (_useSystemVpn) SystemVpn.cancelPending();
    try {
      await rust.disconnect();
    } finally {
      if (_useSystemVpn) await _stopVpn();
    }
  });

  Future<void> retry() => _run(() => _withVpn(rust.retry));

  /// Turns the connection on in [mode] (the overview's switches, the tray,
  /// "use the system proxy"). From the other mode while it is not off, the
  /// client reconnects in [mode] itself; otherwise connect follows.
  Future<void> connectIn(rust.UiConnectionMode mode) => _run(() async {
    final before = snapshot.value;
    await rust.setConnectionMode(mode: mode);
    if (before.connectionMode == mode || before.phase == rust.UiPhase.off) await _withVpn(rust.connect);
  });

  /// "Use on this device" while enhanced mode is contended.
  Future<void> takeOver() => _run(rust.enhancedTakeOver);

  /// Installs the privileged service without connecting (the admin prompt).
  Future<void> installService() => _run(rust.serviceInstall);
  Future<void> refreshNodes() => _run(rust.refreshNodes);
  Future<void> selectNode(String id) => _run(() => rust.selectNode(nodeId: id));
  Future<void> refreshTeams() => _run(() async => teams.value = await rust.teams());
  Future<void> switchTeam(String id) => _run(() => rust.switchTeam(teamId: id));
  Future<void> setRoutingMode(rust.UiRoutingMode mode) => _run(() => rust.setRoutingMode(mode: mode));

  Future<void> testAll(rust.UiProbeMethod method) async {
    latency.value = {for (final node in nodes.value) node.id: const LatencyTesting()};
    await _run(() => rust.probeAll(method: method));
  }

  /// Puts the system proxy back and stops the core; before the app quits.
  void stop() {
    if (_stopped) return;
    _stopped = true;
    rust.shutdown();
  }
}
