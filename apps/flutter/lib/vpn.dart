// The system VPN on a phone (#448, the client's tunnel mode): the
// platform's plugin (Android: runner/android VpnPlugin and TunnelService)
// starts and stops it; the tunnel host inside serves tunnel.sock, which the
// client then connects to. AppState drives it: on connect the permission,
// then the VPN, then the client; on disconnect the client, then the VPN.

import 'dart:async';

import 'package:flutter/foundation.dart';
import 'package:flutter/services.dart';

const _methods = MethodChannel('com.peakpassvpn.ppvpn/vpn');
const _events = EventChannel('com.peakpassvpn.ppvpn/vpn-state');

/// The system VPN's state as the platform reports it.
enum VpnState { off, connecting, on, disconnecting }

/// Client actions sharing one native start. Any successful action establishes
/// it; a failed sibling may only clean up once every pending action has failed.
class _NativeStart {
  final pending = <int>{};
  bool connected = false;
}

class SystemVpn {
  SystemVpn._();

  /// As last reported; off until the platform says otherwise.
  static final ValueNotifier<VpnState> state = ValueNotifier(VpnState.off);

  static StreamSubscription<Object?>? _subscription;
  static int _request = 0;
  static int _attempt = 0;
  static _NativeStart? _started;
  static Future<void> _nativeTail = Future.value();

  /// Order native command acknowledgements. Actual platform state changes
  /// arrive separately on the event channel.
  static Future<T> _native<T>(Future<T> Function() action) {
    final next = _nativeTail.then((_) => action());
    _nativeTail = next.then<void>((_) {}, onError: (Object _, StackTrace _) {});
    return next;
  }

  /// Start the native VPN and connect its client as one attempt. Early
  /// client errors need cleanup even when no connection phase was emitted.
  /// Overlapping client actions share a native start; a failed action cannot
  /// tear down a successful or still-pending sibling.
  static Future<void> connect(Future<void> Function() connectClient) async {
    final attempt = ++_request;
    if (!await prepare() || attempt < _attempt) return;
    // A rejected/busy permission request must not supersede an earlier dialog.
    _attempt = attempt;
    _NativeStart? started;
    try {
      final current = await _native(() async {
        if (attempt != _attempt) return false;
        if (state.value == VpnState.off || state.value == VpnState.disconnecting) {
          _started = started = _NativeStart();
          started!.pending.add(attempt);
          await start();
        } else {
          started = _started;
          started?.pending.add(attempt);
        }
        if (attempt != _attempt) {
          started?.pending.remove(attempt);
          await _cleanup(started);
          return false;
        }
        return true;
      });
      if (!current) return;
      await connectClient();
      started?.connected = true;
      started?.pending.remove(attempt);
    } catch (_) {
      started?.pending.remove(attempt);
      try {
        await _native(() => _cleanup(started));
      } catch (error) {
        debugPrint('vpn rollback: $error');
      }
      rethrow;
    }
  }

  static Future<void> _cleanup(_NativeStart? started) async {
    if (started != null && identical(_started, started) && !started.connected && started.pending.isEmpty) {
      await _stopNative();
    }
  }

  /// Follows the platform's state; once, at start.
  static void listen() {
    _subscription ??= _events.receiveBroadcastStream().listen((value) {
      for (final known in VpnState.values) {
        if (known.name == value) state.value = known;
      }
    }, onError: (Object error) => debugPrint('vpn-state: $error'));
  }

  /// True once the user has allowed the VPN (the system asks the first
  /// time); false when they declined.
  static Future<bool> prepare() async => await _methods.invokeMethod<bool>('prepare') ?? false;

  /// Connecting from here on, before the platform first reports it: a
  /// [stop] right after still reaches the service.
  static Future<void> start() async {
    state.value = VpnState.connecting;
    await _methods.invokeMethod<void>('start');
  }

  /// Invalidate a pending permission/connection attempt before waiting for
  /// its native start. A later connect queues behind this cleanup.
  static Future<void> stop() {
    cancelPending();
    return _native(_stopNative);
  }

  /// Called as soon as disconnect is requested, before the client stops.
  static void cancelPending() => _attempt = ++_request;

  static Future<void> _stopNative() async {
    if (state.value != VpnState.off) {
      state.value = VpnState.disconnecting;
      await _methods.invokeMethod<void>('stop');
    }
    _started = null;
  }
}
