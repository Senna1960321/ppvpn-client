// The signed-in flow end to end (#401 step 1, #387 acceptance 1), on the real
// app, client and core against e2e/mock_backend.py and test/fakenode
// (apps/flutter/e2e/run.sh starts both and sets the environment):
//
// sign in with the device code → the overview → the nodes page, make the
// second node current with a double click → connect in standard mode (the
// system proxy) → a request through that node's local proxy reaches the
// fakenode's HTTP server → disconnect → sign out.
//
// The UI is English on the runners: the finders use keys where a label
// shows twice, and the English strings otherwise.

import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';

import 'package:ppvpn_flutter/main.dart' as app;
import 'package:ppvpn_flutter/src/rust/api.dart' as rust;

/// Pumps real time until [finder] matches (the client and the core work on
/// their own threads), or fails after [timeout].
Future<void> pumpUntil(WidgetTester tester, Finder finder, {Duration timeout = const Duration(seconds: 60)}) async {
  final deadline = DateTime.now().add(timeout);
  while (DateTime.now().isBefore(deadline)) {
    await tester.runAsync(() => Future<void>.delayed(const Duration(milliseconds: 200)));
    await tester.pump();
    if (finder.evaluate().isNotEmpty) return;
  }
  fail('timed out waiting for $finder');
}

/// GET [url] through the HTTP proxy of [proxy] (with its credentials).
Future<String> getThrough(rust.UiLocalProxy proxy, String url) async {
  final client = HttpClient()
    ..findProxy = ((_) => 'PROXY ${proxy.host}:${proxy.port}')
    ..addProxyCredentials(proxy.host, proxy.port, '', HttpClientBasicCredentials(proxy.username, proxy.password));
  try {
    final request = await client.getUrl(Uri.parse(url));
    final response = await request.close();
    return await response.transform(utf8.decoder).join();
  } finally {
    client.close(force: true);
  }
}

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  testWidgets('sign in, pick a node, connect, disconnect, sign out', (tester) async {
    final target = Platform.environment['PPVPN_E2E_TARGET']!;
    await app.main();

    // Signed out at first (nothing saved).
    await pumpUntil(tester, find.text('Sign In with Browser'));
    await tester.tap(find.text('Sign In with Browser'));
    // The device code shows, then the mock backend approves it.
    await pumpUntil(tester, find.byKey(const ValueKey('nav-overview')));

    // The nodes page: both nodes; make the second one current.
    await tester.tap(find.byKey(const ValueKey('nav-nodes')));
    await pumpUntil(tester, find.text('CI Two'));
    // A double tap is two taps 40 to 300 ms apart in real time: wait that
    // without a frame. pump waits for one, and macOS gives a window in the
    // background (the runners') its frames late, past 300 ms (#425).
    final row = find.text('CI Two');
    await tester.tap(row);
    await tester.runAsync(() => Future<void>.delayed(const Duration(milliseconds: 100)));
    await tester.tap(row);
    // The overview's node picker shows the current node.
    await tester.tap(find.byKey(const ValueKey('nav-overview')));
    await pumpUntil(tester, find.descendant(of: find.byKey(const ValueKey('node-picker')), matching: find.text('CI Two')));

    // Connect in standard mode.
    await tester.tap(find.byKey(const ValueKey('switch-system-proxy')));
    await pumpUntil(tester, find.text('Connected'));

    // Through the second node's local proxy to the fakenode's HTTP server.
    final proxies = await tester.runAsync(() => rust.localProxies(generation: rust.start().localProxyGeneration!));
    final proxy = proxies!.firstWhere((p) => p.nodeId == 'ci-two');
    final body = await tester.runAsync(() => getThrough(proxy, target));
    expect(body, contains('ppvpn-e2e-ok'));

    // Disconnect.
    await tester.tap(find.byKey(const ValueKey('switch-system-proxy')));
    await pumpUntil(tester, find.text('Not Connected'));

    // Sign out from the account menu.
    await tester.tap(find.byKey(const ValueKey('account')));
    await pumpUntil(tester, find.text('Sign Out'));
    await tester.tap(find.text('Sign Out'));
    await pumpUntil(tester, find.text('Sign In with Browser'));
  });
}
