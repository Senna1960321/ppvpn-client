import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:ppvpn_flutter/desktop_updates.dart';

Map<String, dynamic> release({String version = '0.7.5', int build = 43, bool mac = false}) => {
  'schema': 2,
  'channel': 'stable',
  'version': version,
  'build': build,
  'platforms': {
    mac ? 'macos-arm64' : 'windows-x64': {
      mac ? 'install_url' : 'url': 'https://updates.example/PPVPN.${mac ? 'pkg' : 'exe'}',
      mac ? 'install_length' : 'length': 3,
      mac ? 'install_ed_signature' : 'ed_signature': base64Encode(List.filled(64, 1)),
    },
  },
};

class Transport implements UpdateTransport {
  Map<String, dynamic> pointer = release();
  Completer<void>? pending;
  Object? feedError;
  int feeds = 0;
  int downloads = 0;
  bool cancelled = false;
  void Function()? onFeed;

  @override
  Future<Map<String, dynamic>> feed(Uri url) async {
    feeds++;
    onFeed?.call();
    if (feedError case final error?) throw error;
    return pointer;
  }

  @override
  Future<void> package(DesktopPackage package, File destination) async {
    downloads++;
    await pending?.future;
    await destination.writeAsBytes([1, 2, 3]);
  }

  @override
  void cancel() {
    cancelled = true;
  }
}

class ResponseHeaders extends Fake implements HttpHeaders {
  ResponseHeaders(this.location);
  final String? location;
  @override
  String? value(String name) => name == HttpHeaders.locationHeader ? location : null;
}

class Response extends Stream<List<int>> implements HttpClientResponse {
  Response(this.body, {this.contentLength = -1, this.statusCode = HttpStatus.ok, String? location})
      : headers = ResponseHeaders(location);
  final Stream<List<int>> body;
  @override
  final int contentLength;
  @override
  final int statusCode;
  @override
  final HttpHeaders headers;
  @override
  bool get isRedirect => statusCode == HttpStatus.found;
  @override
  StreamSubscription<List<int>> listen(void Function(List<int>)? onData,
      {Function? onError, void Function()? onDone, bool? cancelOnError}) =>
      body.listen(onData, onError: onError, onDone: onDone, cancelOnError: cancelOnError);
  @override
  dynamic noSuchMethod(Invocation invocation) => super.noSuchMethod(invocation);
}

class Request extends Fake implements HttpClientRequest {
  Request(this.response);
  final Response response;
  @override
  bool followRedirects = true;
  @override
  Future<HttpClientResponse> close() async {
    expect(followRedirects, isFalse, reason: 'the downloader must validate every redirect itself');
    return response;
  }
}

class DownloadClient extends Fake implements HttpClient {
  DownloadClient(this.responses, {this.onClose});
  final List<Response> responses;
  final void Function()? onClose;
  final List<Uri> urls = [];
  bool closed = false;
  @override
  Future<HttpClientRequest> getUrl(Uri url) async {
    urls.add(url);
    return Request(responses.removeAt(0));
  }
  @override
  void close({bool force = false}) {
    expect(force, isTrue);
    closed = true;
    onClose?.call();
  }
}

void main() {
  late Directory directory;
  late Transport transport;
  late Map<String, dynamic> service;
  late List<bool> prompts;
  late List<String> notices;
  late int installs;
  late int probes;
  Object? installError;
  Object? probeError;
  Completer<void>? installing;

  DesktopUpdateConfig config({String version = '0.7.4', int build = 42, String platform = 'windows-x64'}) =>
      DesktopUpdateConfig(
        feed: Uri.parse('https://updates.example/desktop/channels/stable.json'),
        platform: platform,
        version: version,
        build: build,
        directory: directory,
      );

  DesktopUpdates controller({
    bool enabled = true,
    DesktopUpdateConfig? current,
    Duration timeout = const Duration(seconds: 1),
    Future<void> Function(File, String)? saveState,
  }) => DesktopUpdates(
    config: current ?? config(),
    enabled: enabled,
    transport: transport,
    saveState: saveState,
    status: () async {
      probes++;
      if (probeError case final error?) throw error;
      return service;
    },
    install: (package, path) async {
      installs++;
      expect(File(path).isAbsolute, isTrue);
      expect(await File(path).readAsBytes(), [1, 2, 3]);
      if (installError case final error?) throw error;
      await installing?.future;
    },
    prompts: (enabled) async => prompts.add(enabled),
    onInstalled: notices.add,
    installTimeout: timeout,
  );

  Future<Map<String, dynamic>> saved() async =>
      jsonDecode(await File('${directory.path}/state.json').readAsString()) as Map<String, dynamic>;

  setUp(() async {
    directory = await Directory.systemTemp.createTemp('ppvpn-update-test-');
    transport = Transport();
    service = {'build': 42, 'update': null, 'last_update': null};
    prompts = [];
    notices = [];
    installs = 0;
    probes = 0;
    installError = null;
    probeError = null;
    installing = null;
  });
  tearDown(() async => directory.delete(recursive: true));

  test('download is background only; duplicate quits wait for one acknowledgement', () async {
    final updates = controller();
    expect(await updates.start(schedule: false), isTrue);
    await updates.check();
    expect(updates.hasReadyPackage, isTrue);
    expect(installs, 0);
    installing = Completer<void>();
    final first = updates.onQuit();
    expect(identical(first, updates.onQuit()), isTrue);
    await Future<void>.delayed(Duration.zero);
    expect(installs, 1);
    expect(transport.cancelled, isTrue);
    expect(notices, isEmpty);
    installing!.complete();
    await first;
    expect(notices, isEmpty, reason: 'accepted is not an installed result');
  });

  test('without a ready package quit makes no check or service call', () async {
    final updates = controller();
    await updates.start(schedule: false);
    await updates.onQuit().timeout(const Duration(milliseconds: 100));
    expect(probes, 1);
    expect(transport.feeds, 0);
    expect(installs, 0);
  });

  test('disabled preference uses prompts and never downloads or installs', () async {
    final updates = controller(enabled: false);
    await updates.start(schedule: false);
    await updates.check();
    await updates.onQuit();
    expect(prompts.last, isTrue);
    expect(transport.feeds, 0);
    expect(installs, 0);
  });

  for (final manual in [false, true]) {
    test(
      manual ? 'manual check takes ownership of a ready install' : 'turning off after download prevents install',
      () async {
        final updates = controller();
        await updates.start(schedule: false);
        await updates.check();
        if (manual) {
          await updates.manualCheck();
        } else {
          await updates.setEnabled(false);
        }
        await updates.onQuit();
        expect(installs, 0);
        expect(prompts.last, isTrue);
      },
    );
  }

  test('off then on during a download schedules a fresh check after the stale one', () async {
    transport.pending = Completer<void>();
    final updates = controller();
    await updates.start(schedule: false);
    final first = updates.check();
    while (transport.downloads == 0) { await Future<void>.delayed(Duration.zero); }
    await updates.setEnabled(false);
    final restarted = Completer<void>();
    transport.onFeed = () { if (transport.feeds == 2) restarted.complete(); };
    await updates.setEnabled(true);
    transport.pending!.complete();
    await first;
    await restarted.future.timeout(const Duration(seconds: 1));
    await updates.check();
    expect(updates.hasReadyPackage, isTrue);
    await updates.onQuit();
    expect(installs, 1);
  });

  test('a manual installer quit bypasses an otherwise ready service update', () async {
    final updates = controller();
    await updates.start(schedule: false);
    await updates.check();
    await updates.onQuit(manualInstaller: true);
    expect(installs, 0);
  });

  test('old and unavailable services keep native prompting', () async {
    service.remove('build');
    final old = controller();
    await old.start(schedule: false);
    await old.check();
    await old.onQuit();
    expect(prompts.last, isTrue);
    expect(transport.feeds, 0);
    probeError = const SocketException('service unavailable');
    final unavailable = controller();
    await unavailable.start(schedule: false);
    await unavailable.check();
    expect(prompts.last, isTrue);
    expect(installs, 0);
  });

  test('an unversioned build-zero service is unsupported', () async {
    service['build'] = 0;
    final updates = controller();
    await updates.start(schedule: false);
    await updates.check();
    await updates.onQuit();
    expect(prompts.last, isTrue);
    expect(transport.feeds, 0);
    expect(installs, 0);
  });

  test('busy is retried at the next quit without consuming failure budget', () async {
    installError = StateError('UPDATE_BUSY');
    var updates = controller();
    await updates.start(schedule: false);
    await updates.check();
    await updates.onQuit();
    expect((await saved())['failures'], isNull);
    installError = null;
    updates = controller();
    await updates.start(schedule: false);
    await updates.check();
    await updates.onQuit();
    expect(installs, 2);
    expect(transport.downloads, 1, reason: 'a validated-length cache can be handed to the service again');
  });

  test('three failures persist across launches; a newer release gets a fresh attempt', () async {
    installError = StateError('UPDATE_SIGNATURE_INVALID: invalid signature');
    for (var attempt = 0; attempt < 3; attempt++) {
      final updates = controller();
      await updates.start(schedule: false);
      await updates.check();
      await updates.onQuit();
    }
    expect(installs, 3);
    var updates = controller();
    await updates.start(schedule: false);
    await updates.check();
    expect(updates.hasReadyPackage, isFalse);
    expect(prompts.last, isTrue);
    await updates.onQuit();
    expect(installs, 3);
    expect((await saved())['failures']['0.7.5.43'], 3);
    transport.pointer = release(version: '0.7.6', build: 44);
    installError = null;
    updates = controller();
    await updates.start(schedule: false);
    await updates.check();
    await updates.onQuit();
    expect(installs, 4);
  });

  test('unsupported and not-newer replies immediately fall back for that release', () async {
    for (final code in ['UPDATE_UNSUPPORTED', 'UPDATE_NOT_NEWER']) {
      installError = StateError(code);
      final first = controller();
      await first.start(schedule: false);
      await first.check();
      await first.onQuit();
      final next = controller();
      await next.start(schedule: false);
      await next.check();
      expect(next.hasReadyPackage, isFalse);
      expect(prompts.last, isTrue);
      await File('${directory.path}/state.json').delete();
    }
  });

  test('an installer result is counted only once even when polled and relaunched', () async {
    service['last_update'] = {
      'version': '0.7.5',
      'build': 43,
      'outcome': 'failed',
      'error': 'UPDATE_INSTALL_FAILED: installer failed',
      'finished_at_ms': 100,
    };
    final first = controller();
    await first.start(schedule: false);
    await first.check();
    await first.check();
    final second = controller();
    await second.start(schedule: false);
    expect((await saved())['failures']['0.7.5.43'], 1);
    expect(notices, isEmpty);
  });

  test('matching installed notice survives hidden launches until shown, then stays consumed', () async {
    service['last_update'] = {
      'version': '0.7.5',
      'build': 43,
      'outcome': 'installed',
      'error': null,
      'finished_at_ms': 100,
    };
    final current = config(version: '0.7.5', build: 43);
    final first = controller(current: current);
    await first.start(schedule: false);
    expect(notices, ['0.7.5']);
    final hidden = controller(current: current);
    await hidden.start(schedule: false);
    expect(notices, ['0.7.5', '0.7.5']);
    await hidden.noticeShown();
    final shown = controller(current: current);
    await shown.start(schedule: false);
    await shown.check();
    expect(notices, ['0.7.5', '0.7.5']);
    expect((await saved())['seen'], 100);
    expect((await saved())['notice'], isNull);
  });

  test('success must match both current version and build; failed or rejected status cannot show it', () async {
    for (final result in [
      {'version': '0.7.4', 'build': 99, 'outcome': 'installed'},
      {'version': '0.7.5', 'build': 42, 'outcome': 'installed'},
      {'version': '0.7.4', 'build': 42, 'outcome': 'failed'},
    ]) {
      service['last_update'] = {...result, 'error': 'UPDATE_INSTALL_FAILED', 'finished_at_ms': probes + 1};
      await controller().start(schedule: false);
    }
    probeError = StateError('invalid response signature');
    service['last_update'] = {'version': '0.7.4', 'build': 42, 'outcome': 'installed', 'finished_at_ms': 999};
    await controller().start(schedule: false);
    expect(notices, isEmpty);
  });

  test('startup while service is installing gates the host instead of scheduling an update', () async {
    service['update'] = {'version': '0.7.5', 'build': 43, 'phase': 'installing'};
    expect(await controller().start(schedule: false), isFalse);
    expect(transport.feeds, 0);
    expect(installs, 0);
  });

  test('invalid feed or download failure clears a ready package and restores prompting', () async {
    final updates = controller();
    await updates.start(schedule: false);
    await updates.check();
    expect(updates.hasReadyPackage, isTrue);
    transport.feedError = const FormatException('bad feed');
    await updates.check();
    await updates.onQuit();
    expect(installs, 0);
    expect(prompts.last, isTrue);
  });

  test('quit never waits for an in-flight download and its completion cannot resurrect readiness', () async {
    transport.pending = Completer<void>();
    final updates = controller();
    await updates.start(schedule: false);
    final checking = updates.check();
    while (transport.downloads == 0) {
      await Future<void>.delayed(Duration.zero);
    }
    await updates.onQuit().timeout(const Duration(milliseconds: 100));
    expect(transport.cancelled, isTrue);
    transport.pending!.complete();
    await checking;
    expect(updates.hasReadyPackage, isFalse);
    expect(installs, 0);
  });

  test('no acknowledgement times out, records fallback and never reports success', () async {
    installing = Completer<void>();
    final updates = controller(timeout: const Duration(milliseconds: 5));
    await updates.start(schedule: false);
    await updates.check();
    await updates.onQuit().timeout(const Duration(milliseconds: 200));
    expect((await saved())['fallback'], '0.7.5.43');
    expect(notices, isEmpty);
    installing!.complete();
  });

  test('tray and native quit share hide, normal cleanup and one bounded handoff', () async {
    final events = <String>[];
    final accepted = Completer<void>();
    final quit = DesktopQuit(
      hide: () async => events.add('hide'),
      cleanup: () => events.add('cleanup'),
      handoff: (manual) async {
        events.add('handoff:$manual');
        await accepted.future;
      },
      exit: () async => events.add('exit'),
    );
    final native = quit.prepare();
    final tray = quit.quit();
    expect(identical(tray, quit.quit()), isTrue);
    await Future<void>.delayed(Duration.zero);
    expect(events, ['hide', 'cleanup', 'handoff:false']);
    accepted.complete();
    await Future.wait([native, tray]);
    expect(events, ['hide', 'cleanup', 'handoff:false', 'exit']);
  });

  test('failed hide/update cannot prevent normal cleanup or exit', () async {
    final events = <String>[];
    final quit = DesktopQuit(
      hide: () async => throw StateError('window gone'),
      cleanup: () => events.add('cleanup'),
      handoff: (manual) async {
        expect(manual, isTrue);
        throw StateError('service gone');
      },
      exit: () async => events.add('exit'),
    );
    await quit.quit(manualInstaller: true);
    expect(events, ['cleanup', 'exit']);
  });

  test('installer activation during a natural quit monotonically owns the handoff', () async {
    final hiding = Completer<void>();
    final handoffs = <bool>[];
    final quit = DesktopQuit(hide: () => hiding.future, cleanup: () {},
      handoff: (manual) async => handoffs.add(manual), exit: () async {});
    final natural = quit.quit();
    final installer = quit.quit(manualInstaller: true);
    expect(identical(natural, installer), isTrue);
    hiding.complete();
    await natural;
    expect(handoffs, [true]);
  });

  for (final manual in [false, true]) {
    test('${manual ? 'manual check' : 'auto-off'} during cache-state write cannot be undone by stale check', () async {
      final writing = Completer<void>();
      final releaseWrite = Completer<void>();
      final updates = controller(saveState: (file, contents) async {
        if (jsonDecode(contents)['cached'] != null) {
          writing.complete();
          await releaseWrite.future;
        }
        await file.writeAsString(contents);
      });
      await updates.start(schedule: false);
      final check = updates.check();
      await writing.future;
      if (manual) { await updates.manualCheck(); } else { await updates.setEnabled(false); }
      releaseWrite.complete();
      await check;
      expect(updates.hasReadyPackage, isFalse);
      expect(prompts.last, isTrue);
      await updates.onQuit();
      expect(installs, 0);
    });
  }

  test('stable pointer chooses signed PKG on macOS and EXE on Windows, comparing version then build', () {
    expect(DesktopPackage.fromFeed(release(), config())!.extension, 'exe');
    expect(DesktopPackage.fromFeed(release(mac: true), config(platform: 'macos-arm64'))!.extension, 'pkg');
    expect(DesktopPackage.fromFeed(release(version: '0.7.3', build: 999), config()), isNull);
    expect(DesktopPackage.fromFeed(release(version: '0.7.4', build: 42), config()), isNull);
    expect(DesktopPackage.fromFeed(release(version: '0.8.0', build: 42), config()), isNull);
    expect(DesktopPackage.fromFeed(release(version: '0.7.4', build: 43), config())!.build, 43);
  });

  test('feed rejects wrong channel/schema, missing PKG, unsafe URLs, signature shape and size', () {
    for (final change in <void Function(Map<String, dynamic>)>[
      (p) => p['channel'] = 'dev',
      (p) => p['schema'] = 1,
      (p) => p['version'] = '../evil',
      (p) => p['build'] = -1,
      (p) => p['platforms']['windows-x64']['url'] = 'http://updates.example/setup.exe',
      (p) => p['platforms']['windows-x64']['url'] = 'https://user:pass@updates.example/setup.exe',
      (p) => p['platforms']['windows-x64']['url'] = 'https://updates.example/payload.zip',
      (p) => p['platforms']['windows-x64']['length'] = UpdateDownload.maxPackageBytes + 1,
      (p) => p['platforms']['windows-x64']['length'] = 0,
      (p) => p['platforms']['windows-x64']['ed_signature'] = 'bad',
    ]) {
      final pointer = release();
      change(pointer);
      expect(() => DesktopPackage.fromFeed(pointer, config()), throwsFormatException);
    }
    final oldMac = release(mac: true);
    oldMac['platforms']['macos-arm64'].remove('install_url');
    expect(() => DesktopPackage.fromFeed(oldMac, config(platform: 'macos-arm64')), throwsFormatException);
  });

  test('bounded streaming rejects oversize chunks and stalled responses', () async {
    await expectLater(
      UpdateDownload.bounded(
        Stream.fromIterable([
          [1, 2],
          [3, 4],
        ]),
        3,
        const Duration(seconds: 1),
      ).toList(),
      throwsFormatException,
    );
    final source = StreamController<List<int>>();
    await expectLater(
      UpdateDownload.bounded(source.stream, 3, const Duration(milliseconds: 5)).toList(),
      throwsA(isA<TimeoutException>()),
    );
    await source.close();
  });

  test('actual package path publishes only the complete file and closes the connection', () async {
    final package = DesktopPackage.fromFeed(release(), config())!;
    final client = DownloadClient([Response(Stream.fromIterable([[1], [2, 3]]), contentLength: 3)]);
    final target = File('${directory.path}/setup.exe');
    await UpdateDownload(createClient: () => client).package(package, target);
    expect(await target.readAsBytes(), [1, 2, 3]);
    expect(await File('${target.path}.part').exists(), isFalse);
    expect(client.closed, isTrue);
  });

  for (final bytes in [[1, 2], [1, 2, 3, 4]]) {
    test('actual package path rejects ${bytes.length < 3 ? 'truncated' : 'oversize'} chunked bytes', () async {
      final package = DesktopPackage.fromFeed(release(), config())!;
      final client = DownloadClient([Response(Stream.value(bytes))]);
      final target = File('${directory.path}/setup.exe');
      await expectLater(UpdateDownload(createClient: () => client).package(package, target), throwsFormatException);
      expect(await target.exists(), isFalse);
      expect(await File('${target.path}.part').exists(), isFalse);
      expect(client.closed, isTrue);
    });
  }

  test('redirect downgrade and oversized feed are rejected with resources closed', () async {
    final redirect = DownloadClient([Response(const Stream.empty(), statusCode: HttpStatus.found,
      location: 'http://updates.example/redirect.json')]);
    await expectLater(UpdateDownload(createClient: () => redirect).feed(config().feed), throwsFormatException);
    expect(redirect.urls, hasLength(1));
    expect(redirect.closed, isTrue);
    final oversized = DownloadClient([Response(const Stream.empty(), contentLength: UpdateDownload.maxFeedBytes + 1)]);
    await expectLater(UpdateDownload(createClient: () => oversized).feed(config().feed), throwsFormatException);
    expect(oversized.closed, isTrue);
  });

  test('whole-package deadline closes a stalled connection without publishing partial bytes', () async {
    final body = StreamController<List<int>>();
    final client = DownloadClient([Response(body.stream)], onClose: () => unawaited(body.close()));
    final target = File('${directory.path}/setup.exe');
    final download = UpdateDownload(createClient: () => client, readTimeout: const Duration(seconds: 1),
      packageTimeout: const Duration(milliseconds: 10));
    final work = download.package(DesktopPackage.fromFeed(release(), config())!, target);
    body.add([1]);
    await expectLater(work, throwsA(isA<TimeoutException>()));
    expect(client.closed, isTrue);
    expect(await target.exists(), isFalse);
    // close() cancels the active request; give its stream's cleanup one turn.
    for (var i = 0; i < 20 && await File('${target.path}.part').exists(); i++) {
      await Future<void>.delayed(const Duration(milliseconds: 1));
    }
    expect(await File('${target.path}.part').exists(), isFalse);
  });
}
