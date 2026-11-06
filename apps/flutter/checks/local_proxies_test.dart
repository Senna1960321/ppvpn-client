import 'dart:async';

import 'package:flutter_test/flutter_test.dart';
import 'package:ppvpn_flutter/app_state.dart';
import 'package:ppvpn_flutter/src/rust/api.dart';
import 'package:ppvpn_flutter/src/rust/frb_generated.dart';

UiSnapshot state({String revision = 'r1', int? generation = 1, UiAuth auth = UiAuth.signedIn}) => UiSnapshot(
  auth: auth,
  profile: UiProfile.ready,
  profilePrevious: false,
  profileRevision: revision,
  phase: UiPhase.off,
  standardReady: generation != null,
  localProxyGeneration: generation,
  unread: 0,
  routingMode: UiRoutingMode.rules,
  serviceInstalled: false,
  connectionMode: UiConnectionMode.compatible,
  canTakeOver: false,
  suggestCompatible: false,
  apiUnreachable: false,
);

UiLocalProxy proxy(String id, {int port = 1080}) =>
    UiLocalProxy(nodeId: id, host: '127.0.0.1', port: port, username: 'test-user', password: 'test-password');

UiNode node(String id) => UiNode(id: id, name: id, entryKey: '', udp: true, lines: const []);

class FakeApi implements RustLibApi {
  var pendingEvent = Completer<UiEvents>();
  final requests = <({int generation, Completer<List<UiLocalProxy>> result})>[];
  List<UiNode> nodes = [node('a')];

  @override
  Future<UiEvents> crateApiNextEvents() {
    final pending = pendingEvent;
    return pending.future;
  }

  Future<void> emit(UiSnapshot snapshot) async {
    final pending = pendingEvent;
    pendingEvent = Completer<UiEvents>();
    pending.complete(UiEvents(snapshot: snapshot, probes: const []));
    await flush();
  }

  @override
  List<UiNode> crateApiNodes() => nodes;

  @override
  Future<List<UiLocalProxy>> crateApiLocalProxies({required int generation}) {
    final result = Completer<List<UiLocalProxy>>();
    requests.add((generation: generation, result: result));
    return result.future;
  }

  @override
  void crateApiShutdown() {
    if (!pendingEvent.isCompleted) pendingEvent.complete(const UiEvents(probes: []));
  }

  @override
  dynamic noSuchMethod(Invocation invocation) => super.noSuchMethod(invocation);
}

Future<void> flush() => Future<void>.delayed(Duration.zero);

void main() {
  late FakeApi api;
  late AppState app;

  setUp(() {
    api = FakeApi();
    RustLib.initMock(api: api);
    app = AppState(state());
  });

  tearDown(() async {
    app.stop();
    await flush();
    RustLib.dispose();
  });

  test('new node proxies refresh only after the live profile is applied', () async {
    api.requests.single.result.complete([proxy('a')]);
    await flush();
    api.nodes = [node('a'), node('b')];
    // R2 arrived, but the standard core still serves R1. A failed apply
    // also keeps this token, so neither case may reload/label R1 as R2.
    await api.emit(state(revision: 'r2'));
    expect(app.nodes.value.map((node) => node.id), ['a', 'b']);
    expect(api.requests, hasLength(1));
    expect(app.proxies.value.keys, ['a']);
    await api.emit(state(revision: 'r2', generation: 2));
    expect(api.requests, hasLength(2));
    expect(app.proxies.value, isEmpty);
    api.requests.last.result.complete([proxy('a'), proxy('b')]);
    await flush();
    expect(app.proxies.value['b'], proxy('b'));
    await api.emit(state(revision: 'r2', generation: 2));
    expect(api.requests, hasLength(2));
  });

  test('out-of-order responses cannot overwrite a newer revision', () async {
    await api.emit(state(revision: 'r2', generation: 2));
    await api.emit(state(revision: 'r3', generation: 3));
    api.requests[2].result.complete([proxy('c')]);
    await flush();
    api.requests[1].result.complete([proxy('b')]);
    api.requests[0].result.complete([proxy('a')]);
    await flush();
    expect(app.proxies.value.keys, ['c']);
  });

  test('same-revision restart refreshes even when intermediate events coalesce', () async {
    await api.emit(state(generation: 3));
    api.requests.last.result.complete([proxy('a', port: 2080)]);
    await flush();
    api.requests.first.result.complete([proxy('a')]);
    await flush();
    expect(app.proxies.value['a']?.port, 2080);
  });

  test('stopping the core clears proxies and rejects an in-flight response', () async {
    await api.emit(state(generation: null));
    api.requests.first.result.complete([proxy('a')]);
    await flush();
    expect(app.proxies.value, isEmpty);
    await api.emit(state(generation: 3));
    api.requests.last.result.complete([proxy('a', port: 2080)]);
    await flush();
    expect(app.proxies.value['a']?.port, 2080);
    await api.emit(state(generation: null));
    expect(app.proxies.value, isEmpty);
  });

  test('sign-out invalidates pending proxies before another account is ready', () async {
    await api.emit(state(auth: UiAuth.signedOut, generation: null));
    api.requests.first.result.complete([proxy('a')]);
    await flush();
    expect(app.proxies.value, isEmpty);
    await api.emit(state(generation: 3));
    api.requests.last.result.complete([proxy('b')]);
    await flush();
    expect(app.proxies.value.keys, ['b']);
  });

  test('a failed read does not retain the previous generation', () async {
    api.requests.first.result.complete([proxy('a')]);
    await flush();
    await api.emit(state(revision: 'r2', generation: 2));
    api.requests.last.result.completeError(StateError('read failed'));
    await flush();
    expect(app.proxies.value, isEmpty);
    await api.emit(state(revision: 'r3', generation: 3));
    api.requests.last.result.complete([proxy('c')]);
    await flush();
    expect(app.proxies.value.keys, ['c']);
  });

  test('app shutdown rejects outstanding proxy results', () async {
    app.stop();
    api.requests.first.result.complete([proxy('a')]);
    await flush();
    expect(app.proxies.value, isEmpty);
  });
}
