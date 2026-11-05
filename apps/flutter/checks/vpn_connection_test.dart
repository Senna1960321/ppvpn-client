import 'dart:async';

import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ppvpn_flutter/app_state.dart';
import 'package:ppvpn_flutter/src/rust/api.dart' as rust;
import 'package:ppvpn_flutter/src/rust/frb_generated.dart';
import 'package:ppvpn_flutter/vpn.dart';

class ConnectionApi implements RustLibApi {
  Future<void> Function() connecting = () async {};
  Future<void> Function() disconnecting = () async {};
  int connectionCalls = 0;

  Future<void> connect() {
    connectionCalls++;
    return connecting();
  }

  @override
  Future<void> crateApiConnect() => connect();
  @override
  Future<void> crateApiRetry() => connect();
  @override
  Future<void> crateApiSetConnectionMode({required rust.UiConnectionMode mode}) async {}
  @override
  Future<void> crateApiDisconnect() => disconnecting();
  @override
  dynamic noSuchMethod(Invocation invocation) => super.noSuchMethod(invocation);
}

rust.UiSnapshot snapshot() => const rust.UiSnapshot(
  auth: rust.UiAuth.signedIn,
  profile: rust.UiProfile.loading,
  profilePrevious: false,
  phase: rust.UiPhase.off,
  standardReady: false,
  unread: 0,
  routingMode: rust.UiRoutingMode.rules,
  serviceInstalled: false,
  connectionMode: rust.UiConnectionMode.enhanced,
  canTakeOver: false,
  suggestCompatible: false,
  apiUnreachable: false,
);

void main() {
  final binding = TestWidgetsFlutterBinding.ensureInitialized();
  const channel = MethodChannel('com.peakpassvpn.ppvpn/vpn');
  final api = ConnectionApi();
  RustLib.initMock(api: api);
  late AppState app;
  late List<String> calls;
  late Completer<void> nativeStarted;
  late Completer<void> nativeStopped;
  Completer<bool>? permission;
  Completer<void>? starting;
  Completer<void>? stopping;
  var failStart = false;
  var failStop = false;
  var failPrepare = false;

  setUp(() {
    calls = [];
    nativeStarted = Completer<void>();
    nativeStopped = Completer<void>();
    permission = null;
    starting = null;
    stopping = null;
    failStart = false;
    failStop = false;
    failPrepare = false;
    api.connectionCalls = 0;
    api.disconnecting = () async {};
    SystemVpn.state.value = VpnState.off;
    app = AppState.fixed(snapshot(), useSystemVpn: true);
    api.connecting = () async => throw StateError('PROFILE_REQUIRED');
    binding.defaultBinaryMessenger.setMockMethodCallHandler(channel, (call) async {
      calls.add(call.method);
      if (call.method == 'prepare') {
        if (failPrepare) throw PlatformException(code: 'busy');
        return permission?.future ?? true;
      }
      if (call.method == 'start') {
        if (!nativeStarted.isCompleted) nativeStarted.complete();
        await starting?.future;
        if (failStart) throw PlatformException(code: 'START_FAILED');
        SystemVpn.state.value = VpnState.on;
      }
      if (call.method == 'stop') {
        if (!nativeStopped.isCompleted) nativeStopped.complete();
        await stopping?.future;
        if (failStop) throw PlatformException(code: 'STOP_FAILED');
        SystemVpn.state.value = VpnState.off;
      }
      return null;
    });
  });
  tearDown(() async {
    if (permission case final gate? when !gate.isCompleted) gate.complete(false);
    if (starting case final gate? when !gate.isCompleted) gate.complete();
    if (stopping case final gate? when !gate.isCompleted) gate.complete();
    failStop = false;
    await SystemVpn.stop();
    binding.defaultBinaryMessenger.setMockMethodCallHandler(channel, null);
  });

  for (final action in ['connect', 'retry', 'connectIn']) {
    test('$action rolls back a native start when the client phase does not change', () async {
      switch (action) {
        case 'connect':
          await app.connect();
        case 'retry':
          await app.retry();
        case 'connectIn':
          await app.connectIn(rust.UiConnectionMode.enhanced);
      }
      expect(calls, ['prepare', 'start', 'stop']);
      expect(SystemVpn.state.value, VpnState.off);
      expect(app.snapshot.value.phase, rust.UiPhase.off);
      expect(app.actionError.value, contains('PROFILE_REQUIRED'));
    });
  }

  test('permission rejection does not start or connect', () async {
    permission = Completer<bool>()..complete(false);
    await app.connect();
    expect(calls, ['prepare']);
    expect(api.connectionCalls, 0);
  });

  test('successful connection keeps its native VPN', () async {
    api.connecting = () async {};
    await app.connect();
    expect(calls, ['prepare', 'start']);
    expect(SystemVpn.state.value, VpnState.on);
    expect(app.actionError.value, isNull);
  });

  test('a busy second permission request does not discard the first approval', () async {
    api.connecting = () async {};
    permission = Completer<bool>();
    final first = app.connect();
    await Future<void>.delayed(Duration.zero);
    failPrepare = true;
    await app.connect();
    failPrepare = false;
    permission!.complete(true);
    await first;
    expect(api.connectionCalls, 1);
    expect(calls, ['prepare', 'prepare', 'start']);
    expect(SystemVpn.state.value, VpnState.on);
  });

  test('failure does not tear down an already established native VPN', () async {
    SystemVpn.state.value = VpnState.on;
    await app.connect();
    expect(calls, ['prepare']);
    expect(SystemVpn.state.value, VpnState.on);
    expect(app.actionError.value, contains('PROFILE_REQUIRED'));
  });

  test('late failure cannot stop a newer successful connection', () async {
    final first = Completer<void>();
    final entered = Completer<void>();
    api.connecting = () {
      entered.complete();
      return first.future;
    };
    final old = app.connect();
    await entered.future;
    api.connecting = () async {};
    await app.connect();
    first.completeError(StateError('old failure'));
    await old;
    expect(calls.where((call) => call == 'stop'), isEmpty);
    expect(api.connectionCalls, 2);
    expect(SystemVpn.state.value, VpnState.on);
  });

  test('a newer failure cannot stop an older successful connection', () async {
    final first = Completer<void>();
    final second = Completer<void>();
    final firstEntered = Completer<void>();
    final secondEntered = Completer<void>();
    api.connecting = () {
      firstEntered.complete();
      return first.future;
    };
    final old = app.connect();
    await firstEntered.future;
    api.connecting = () {
      secondEntered.complete();
      return second.future;
    };
    final newer = app.connect();
    await secondEntered.future;
    first.complete();
    await old;
    second.completeError(StateError('new failure'));
    await newer;
    expect(calls.where((call) => call == 'stop'), isEmpty);
    expect(SystemVpn.state.value, VpnState.on);
  });

  test('a newer failure waits for the older connection still in progress', () async {
    final first = Completer<void>();
    final entered = Completer<void>();
    api.connecting = () {
      entered.complete();
      return first.future;
    };
    final old = app.connect();
    await entered.future;
    api.connecting = () async => throw StateError('new failure');
    await app.connect();
    expect(calls.where((call) => call == 'stop'), isEmpty);
    first.complete();
    await old;
    expect(calls.where((call) => call == 'stop'), isEmpty);
    expect(SystemVpn.state.value, VpnState.on);
  });

  test('two failed connections clean up after the last pending connection', () async {
    final first = Completer<void>();
    final entered = Completer<void>();
    api.connecting = () {
      entered.complete();
      return first.future;
    };
    final old = app.connect();
    await entered.future;
    api.connecting = () async => throw StateError('new failure');
    await app.connect();
    expect(calls.where((call) => call == 'stop'), isEmpty);
    first.completeError(StateError('old failure'));
    await old;
    expect(calls.where((call) => call == 'stop'), hasLength(1));
    expect(SystemVpn.state.value, VpnState.off);
  });

  test('declining a newer attempt does not orphan the older failed start', () async {
    final first = Completer<void>();
    final entered = Completer<void>();
    api.connecting = () {
      entered.complete();
      return first.future;
    };
    final old = app.connect();
    await entered.future;
    permission = Completer<bool>()..complete(false);
    await app.connect();
    first.completeError(StateError('old failure'));
    await old;
    expect(calls.where((call) => call == 'stop'), hasLength(1));
    expect(SystemVpn.state.value, VpnState.off);
  });

  test('a new native start waits for earlier failure cleanup', () async {
    stopping = Completer<void>();
    final first = app.connect();
    await nativeStopped.future;
    api.connecting = () async {};
    final next = app.connect();
    await Future<void>.delayed(Duration.zero);
    expect(calls.where((call) => call == 'start'), hasLength(1));
    stopping!.complete();
    await Future.wait([first, next]);
    expect(calls, ['prepare', 'start', 'stop', 'prepare', 'start']);
    expect(SystemVpn.state.value, VpnState.on);
  });

  test('disconnect invalidates a pending permission response', () async {
    permission = Completer<bool>();
    final pending = app.connect();
    await Future<void>.delayed(Duration.zero);
    await app.disconnect();
    permission!.complete(true);
    await pending;
    expect(calls, ['prepare']);
    expect(api.connectionCalls, 0);
  });

  test('disconnect invalidates permission before waiting for the Rust client', () async {
    final disconnecting = Completer<void>();
    final disconnectEntered = Completer<void>();
    api.disconnecting = () {
      disconnectEntered.complete();
      return disconnecting.future;
    };
    api.connecting = () async {};
    permission = Completer<bool>();
    final pending = app.connect();
    await Future<void>.delayed(Duration.zero);
    final disconnected = app.disconnect();
    await disconnectEntered.future;
    permission!.complete(true);
    await pending;
    final whileDisconnecting = calls.toList();
    disconnecting.complete();
    await disconnected;
    expect(whileDisconnecting, ['prepare']);
    expect(api.connectionCalls, 0);
  });

  test('disconnect drains a pending native start before stopping it', () async {
    starting = Completer<void>();
    final pending = app.connect();
    await nativeStarted.future;
    final disconnected = app.disconnect();
    await Future<void>.delayed(Duration.zero);
    expect(calls, ['prepare', 'start']);
    starting!.complete();
    await Future.wait([pending, disconnected]);
    expect(calls, ['prepare', 'start', 'stop']);
    expect(api.connectionCalls, 0);
    expect(SystemVpn.state.value, VpnState.off);
  });

  test('native start failure is cleaned up and reported', () async {
    failStart = true;
    await app.connect();
    expect(calls, ['prepare', 'start', 'stop']);
    expect(api.connectionCalls, 0);
    expect(app.actionError.value, contains('START_FAILED'));
  });

  test('cleanup failure keeps the original client error', () async {
    failStop = true;
    await app.connect();
    expect(calls, ['prepare', 'start', 'stop']);
    expect(app.actionError.value, contains('PROFILE_REQUIRED'));
  });

  test('desktop actions do not call the native VPN channel', () async {
    app = AppState.fixed(snapshot(), useSystemVpn: false);
    await app.connect();
    expect(calls, isEmpty);
    expect(api.connectionCalls, 1);
  });
}
