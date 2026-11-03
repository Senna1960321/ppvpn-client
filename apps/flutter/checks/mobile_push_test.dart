import 'dart:async';

import 'package:flutter/services.dart';
import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ppvpn_flutter/mobile_push.dart';

void main() {
  final binding = TestWidgetsFlutterBinding.ensureInitialized();
  const codec = StandardMethodCodec();
  late MobilePush push;
  late List<(String, String?)> tokens;
  late List<int?> opened;
  late List<bool> foreground;
  late List<bool> fallback;
  late List<String> calls;

  Future<void> event(String name, Object? arguments) async {
    final done = Completer<void>();
    binding.defaultBinaryMessenger.handlePlatformMessage(
      MobilePush.channel.name,
      codec.encodeMethodCall(MethodCall(name, arguments)),
      (_) => done.complete(),
    );
    await done.future;
  }

  setUp(() {
    tokens = [];
    opened = [];
    foreground = [];
    fallback = [];
    calls = [];
    push = MobilePush(
      onToken: (token, bundle) => tokens.add((token, bundle)),
      onOpen: (id) async => opened.add(id),
      onForeground: foreground.add,
      onFallback: fallback.add,
    );
    binding.defaultBinaryMessenger.setMockMethodCallHandler(MobilePush.channel, (call) async {
      calls.add(call.method);
      return null;
    });
  });
  tearDown(() {
    push.dispose();
    binding.defaultBinaryMessenger.setMockMethodCallHandler(MobilePush.channel, null);
  });

  test('accepts a token before start returns and forwards rotation', () async {
    binding.defaultBinaryMessenger.setMockMethodCallHandler(MobilePush.channel, (call) async {
      if (call.method == 'start') {
        await event('token', {'token': 'first', 'bundle_id': 'com.peakpassvpn.ppvpn'});
      }
      return null;
    });
    await push.start();
    await event('token', {'token': 'rotated', 'bundle_id': 'com.peakpassvpn.ppvpn'});
    expect(tokens, [('first', 'com.peakpassvpn.ppvpn'), ('rotated', 'com.peakpassvpn.ppvpn')]);
  });

  test('notification taps accept only positive message identifiers', () async {
    await push.start();
    for (final id in [42, '43', 0, -1, 'not an id', 1.5, null, '9223372036854775808']) {
      await event('opened', {'message_id': id, 'deep_link': 'file:///private/ignored'});
    }
    expect(opened, [42, 43, null, null, null, null, null, null]);
  });

  test('ignores malformed tokens and unknown callbacks', () async {
    await push.start();
    for (final token in ['', '   ', null, 123]) {
      await event('token', {'token': token});
    }
    await event('token', 'invalid');
    await event('unknown', {'message_id': 1});
    expect(tokens, isEmpty);
    expect(opened, isEmpty);
  });

  test('pauses polling outside foreground and resumes it', () async {
    await push.start();
    push.didChangeAppLifecycleState(AppLifecycleState.inactive);
    push.didChangeAppLifecycleState(AppLifecycleState.paused);
    push.didChangeAppLifecycleState(AppLifecycleState.resumed);
    expect(foreground.sublist(foreground.length - 3), [false, false, true]);
  });

  test('only boolean fallback events change the Android channel', () async {
    await push.start();
    for (final enabled in [true, 'true', 1, null, false]) {
      await event('fallback', {'enabled': enabled});
    }
    expect(fallback, [true, false]);
    expect(tokens, isEmpty);
    expect(opened, isEmpty);
  });

  test('permission is requested separately and only once', () async {
    await push.start();
    await push.start();
    expect(calls, ['start']);
    await push.requestPermission();
    await push.requestPermission();
    expect(calls, ['start', 'requestPermission']);
  });

  test('missing native host does not block startup', () async {
    binding.defaultBinaryMessenger.setMockMethodCallHandler(MobilePush.channel, null);
    await push.start();
    await push.requestPermission();
  });

  test('native registration failure does not block startup', () async {
    binding.defaultBinaryMessenger.setMockMethodCallHandler(MobilePush.channel, (_) async {
      throw PlatformException(code: 'unavailable');
    });
    await push.start();
    await push.requestPermission();
  });

  test('disposing removes the callback handler', () async {
    await push.start();
    push.dispose();
    await event('token', {'token': 'late'});
    expect(tokens, isEmpty);
  });
}
