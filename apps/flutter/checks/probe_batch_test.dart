import 'dart:async';

import 'package:flutter_test/flutter_test.dart';
import 'package:ppvpn_flutter/app_state.dart';
import 'package:ppvpn_flutter/sample.dart';
import 'package:ppvpn_flutter/src/rust/api.dart';
import 'package:ppvpn_flutter/src/rust/frb_generated.dart';

class ProbeApi implements RustLibApi {
  ProbeApi(this.nodes);

  final List<UiNode> nodes;
  final batches = <Completer<void>>[];
  Completer<UiEvents>? events;

  @override
  List<UiNode> crateApiNodes() => nodes;

  @override
  Future<List<UiLocalProxy>> crateApiLocalProxies() async => [];

  @override
  Future<UiEvents> crateApiNextEvents() {
    events = Completer<UiEvents>();
    return events!.future;
  }

  @override
  Future<void> crateApiProbeAll({required UiProbeMethod method}) {
    final batch = Completer<void>();
    batches.add(batch);
    return batch.future;
  }

  @override
  void crateApiShutdown() => events?.complete(const UiEvents(probes: []));

  Future<void> result(UiProbe probe) async {
    events!.complete(UiEvents(probes: [probe]));
    await Future<void>.delayed(Duration.zero);
  }

  @override
  dynamic noSuchMethod(Invocation invocation) => super.noSuchMethod(invocation);
}

void main() {
  late ProbeApi api;
  late AppState app;

  setUp(() {
    final sample = sampleState();
    api = ProbeApi(sample.nodes.value.take(2).toList());
    RustLib.initMock(api: api);
    app = AppState(sample.snapshot.value);
  });

  tearDown(() async {
    app.stop();
    await Future<void>.delayed(Duration.zero);
    RustLib.dispose();
  });

  test('a rejected batch settles its pending nodes and reports the error', () async {
    final pending = app.testAll(UiProbeMethod.tcp);
    expect(app.latency.value.values.every((value) => value is LatencyTesting), isTrue);

    api.batches.single.completeError(StateError('standard core unavailable'));
    await pending;

    expect(app.actionError.value, contains('standard core unavailable'));
    expect(app.latency.value.keys, unorderedEquals(api.nodes.map((node) => node.id)));
    expect(app.latency.value.values.every((value) => value is LatencyFailed), isTrue);
  });

  test('failure preserves results already delivered by the event loop', () async {
    final pending = app.testAll(UiProbeMethod.tcp);
    final id = api.nodes.first.id;
    await api.result(UiProbe(nodeId: id, success: true, timedOut: false, latencyMs: 23));
    api.batches.single.completeError(StateError('connection ended'));
    await pending;

    expect((app.latency.value[id] as LatencyMs).ms, 23);
    expect(app.latency.value[api.nodes.last.id], isA<LatencyFailed>());
  });

  test('an older failure leaves a newer batch pending until its results arrive', () async {
    final first = app.testAll(UiProbeMethod.tcp);
    final second = app.testAll(UiProbeMethod.connect);
    api.batches.first.completeError(StateError('old request failed'));
    await first;
    expect(app.latency.value.values.every((value) => value is LatencyTesting), isTrue);

    for (final node in api.nodes) {
      await api.result(UiProbe(nodeId: node.id, success: true, timedOut: false, latencyMs: 42));
    }
    api.batches.last.complete();
    await second;
    expect(app.latency.value.values.every((value) => value is LatencyMs), isTrue);
  });

  test('failure of the newer batch cannot leave the older pending state behind', () async {
    final first = app.testAll(UiProbeMethod.tcp);
    final second = app.testAll(UiProbeMethod.connect);
    api.batches.last.completeError(StateError('new request failed'));
    await second;
    expect(app.latency.value.values.every((value) => value is LatencyFailed), isTrue);
    api.batches.first.completeError(StateError('old request failed'));
    await first;
    expect(app.latency.value.values.every((value) => value is LatencyFailed), isTrue);
  });

  test('a late failure does not restore removed entries or overwrite other results', () async {
    final pending = app.testAll(UiProbeMethod.tcp);
    app.latency.value = {'replacement': const LatencyMs(18)};
    api.batches.single.completeError(StateError('old request failed'));
    await pending;

    expect(app.latency.value.keys, ['replacement']);
    expect((app.latency.value['replacement'] as LatencyMs).ms, 18);
  });

  test('success leaves pending nodes for asynchronous results and preserves their types', () async {
    final pending = app.testAll(UiProbeMethod.tcp);
    api.batches.single.complete();
    await pending;
    expect(app.latency.value.values.every((value) => value is LatencyTesting), isTrue);
    expect(app.actionError.value, isNull);

    await api.result(UiProbe(nodeId: api.nodes.first.id, success: false, timedOut: true));
    await api.result(UiProbe(nodeId: api.nodes.last.id, success: false, timedOut: false));
    expect(app.latency.value[api.nodes.first.id], isA<LatencyTimeout>());
    expect(app.latency.value[api.nodes.last.id], isA<LatencyFailed>());
  });
}
