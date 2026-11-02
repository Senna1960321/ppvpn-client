// The app downloads; the authenticated service copies, verifies and installs.
// Installation is offered only after the user quits and normal VPN cleanup
// has completed. A hidden window or an idle connection never triggers it.
import 'dart:async';
import 'dart:convert';
import 'dart:io';

typedef UpdateStatus = Future<Map<String, dynamic>> Function();
typedef InstallPackage = Future<void> Function(DesktopPackage package, String path);

/// Tray quit and native Cmd-Q share cleanup and the same bounded handoff.
/// Installer-requested quits bypass silent installation.
class DesktopQuit {
  DesktopQuit({required this.hide, required this.cleanup, required this.handoff, required this.exit});
  final Future<void> Function() hide;
  final void Function() cleanup;
  final Future<void> Function(bool manualInstaller) handoff;
  final Future<void> Function() exit;
  Future<void>? _preparing;
  Future<void>? _exiting;
  bool _manualInstaller = false;
  bool get started => _preparing != null;

  void manualInstallerStarted() => _manualInstaller = true;

  Future<void> prepare({bool manualInstaller = false}) {
    _manualInstaller |= manualInstaller;
    return _preparing ??= _prepare();
  }

  Future<void> _prepare() async {
    try {
      await hide();
    } catch (_) {
      /* Cleanup must run even if the window is already gone. */
    }
    cleanup();
    try {
      await handoff(_manualInstaller);
    } catch (_) {
      /* A failed update never cancels user quit. */
    }
  }

  Future<void> quit({bool manualInstaller = false}) {
    _manualInstaller |= manualInstaller;
    return _exiting ??= () async {
      try { await prepare(); } finally { await exit(); }
    }();
  }
}

class DesktopUpdateConfig {
  const DesktopUpdateConfig({
    required this.feed,
    required this.platform,
    required this.version,
    required this.build,
    required this.directory,
  });

  final Uri feed;
  final String platform;
  final String version;
  final int build;
  final Directory directory;
}

class DesktopPackage {
  const DesktopPackage({
    required this.version,
    required this.build,
    required this.url,
    required this.length,
    required this.signature,
    required this.extension,
  });

  final String version;
  final int build;
  final Uri url;
  final int length;
  final String signature;
  final String extension;
  String get key => '$version.$build';
  String get filename => '$key.$extension';

  /// The publisher's schema-2 stable pointer. A Mac DMG cannot be silently
  /// installed: older pointers without a signed PKG keep the Sparkle prompt.
  static DesktopPackage? fromFeed(Map<String, dynamic> feed, DesktopUpdateConfig config) {
    if (feed['schema'] != 2 || feed['channel'] != 'stable') throw const FormatException('invalid update channel');
    final version = feed['version'];
    final build = feed['build'];
    if (version is! String || build is! int || build < 0 || build > 0x7fffffff) {
      throw const FormatException('invalid update version');
    }
    final release = versionParts(version);
    final current = versionParts(config.version);
    var comparison = 0;
    for (var i = 0; i < release.length && comparison == 0; i++) {
      comparison = release[i].compareTo(current[i]);
    }
    // The service requires a globally increasing build, even if the
    // marketing version increased.
    if (comparison < 0 || build <= config.build) return null;
    final platforms = feed['platforms'];
    final entry = platforms is Map ? platforms[config.platform] : null;
    if (entry is! Map) throw const FormatException('missing update platform');
    final mac = config.platform.startsWith('macos-');
    if (!mac && config.platform != 'windows-x64') throw const FormatException('unsupported update platform');
    final url = entry[mac ? 'install_url' : 'url'];
    final length = entry[mac ? 'install_length' : 'length'];
    final signature = entry[mac ? 'install_ed_signature' : 'ed_signature'];
    if (url is! String ||
        length is! int ||
        length <= 0 ||
        length > UpdateDownload.maxPackageBytes ||
        signature is! String ||
        base64Decode(signature).length != 64) {
      throw const FormatException('missing or invalid signed installer');
    }
    final uri = httpsUrl(url);
    final extension = mac ? 'pkg' : 'exe';
    if (!uri.path.endsWith('.$extension')) throw const FormatException('invalid installer type');
    return DesktopPackage(
      version: version,
      build: build,
      url: uri,
      length: length,
      signature: signature,
      extension: extension,
    );
  }

  static List<int> versionParts(String version) {
    if (!RegExp(r'^\d{1,5}\.\d{1,5}\.\d{1,5}$').hasMatch(version)) {
      throw const FormatException('invalid stable version');
    }
    return version.split('.').map(int.parse).toList();
  }

  static Uri httpsUrl(String value) {
    final uri = Uri.parse(value);
    if (uri.scheme != 'https' || uri.host.isEmpty || uri.userInfo.isNotEmpty || uri.hasFragment) {
      throw const FormatException('update URL must use HTTPS');
    }
    return uri;
  }
}

abstract interface class UpdateTransport {
  Future<Map<String, dynamic>> feed(Uri url);
  Future<void> package(DesktopPackage package, File destination);
  void cancel();
}

/// Bounded reads, HTTPS-only redirects and atomic cache publication. Signature
/// and publisher verification happen again on the service's private copy.
class UpdateDownload implements UpdateTransport {
  UpdateDownload({
    HttpClient Function()? createClient,
    this.readTimeout = const Duration(seconds: 30),
    this.packageTimeout = const Duration(minutes: 10),
  }) : _createClient = createClient ?? HttpClient.new;

  static const maxPackageBytes = 1 << 30; // Same ceiling as the service.
  static const maxFeedBytes = 64 * 1024;
  final HttpClient Function() _createClient;
  final Duration readTimeout;
  final Duration packageTimeout;
  HttpClient? _active;
  bool _cancelled = false;

  Future<HttpClientResponse> _get(HttpClient client, Uri url) async {
    for (var redirects = 0; redirects <= 5; redirects++) {
      DesktopPackage.httpsUrl(url.toString());
      final request = await client.getUrl(url).timeout(readTimeout);
      request.followRedirects = false;
      final response = await request.close().timeout(readTimeout);
      if (!response.isRedirect) {
        if (response.statusCode != HttpStatus.ok) throw HttpException('update HTTP ${response.statusCode}');
        return response;
      }
      final location = response.headers.value(HttpHeaders.locationHeader);
      if (location == null) throw const HttpException('update redirect without Location');
      url = url.resolve(location);
      // Never follow an HTTPS-to-HTTP redirect, including between CDNs.
      DesktopPackage.httpsUrl(url.toString());
      await response.drain<void>().timeout(readTimeout);
    }
    throw const HttpException('too many update redirects');
  }

  Future<T> _request<T>(Duration timeout, Future<T> Function(HttpClient client) run) async {
    if (_cancelled) throw const HttpException('update download cancelled');
    final client = _createClient();
    _active = client;
    try {
      return await run(client).timeout(timeout);
    } finally {
      client.close(force: true);
      if (identical(_active, client)) _active = null;
    }
  }

  /// Shared by feed and installer reads, including chunked responses whose
  /// server supplies no Content-Length. No partial download becomes ready.
  static Stream<List<int>> bounded(Stream<List<int>> source, int limit, Duration timeout) async* {
    var bytes = 0;
    await for (final chunk in source.timeout(timeout)) {
      bytes += chunk.length;
      if (bytes > limit) throw const FormatException('update download exceeds its length');
      yield chunk;
    }
  }

  @override
  Future<Map<String, dynamic>> feed(Uri url) => _request(readTimeout, (client) async {
    final response = await _get(client, url);
    if (response.contentLength > maxFeedBytes) throw const FormatException('update feed is too large');
    final bytes = <int>[];
    await for (final chunk in bounded(response, maxFeedBytes, readTimeout)) {
      bytes.addAll(chunk);
    }
    final value = jsonDecode(utf8.decode(bytes));
    if (value is! Map<String, dynamic>) throw const FormatException('invalid update feed');
    return value;
  });

  @override
  Future<void> package(DesktopPackage package, File destination) => _request(packageTimeout, (client) async {
    final response = await _get(client, package.url);
    if (response.contentLength >= 0 && response.contentLength != package.length) {
      throw const FormatException('installer length differs from feed');
    }
    final temporary = File('${destination.path}.part');
    final sink = temporary.openWrite();
    try {
      await sink.addStream(bounded(response, package.length, readTimeout));
      await sink.flush();
      await sink.close();
      if (await temporary.length() != package.length) throw const FormatException('incomplete installer');
      await temporary.rename(destination.path);
    } catch (_) {
        try { await sink.close(); } catch (_) { /* Preserve the original read/write failure. */ }
        if (await temporary.exists()) await temporary.delete();
      rethrow;
    }
  });

  @override
  void cancel() {
    _cancelled = true;
    _active?.close(force: true);
  }
}

class DesktopUpdates {
  DesktopUpdates({
    required this.config,
    required this.status,
    required this.install,
    required this.prompts,
    required this.onInstalled,
    required this.enabled,
    UpdateTransport? transport,
    Future<void> Function(File file, String contents)? saveState,
    this.probeTimeout = const Duration(seconds: 5),
    this.installTimeout = const Duration(seconds: 75),
  }) : transport = transport ?? UpdateDownload(), _saveState = saveState ?? _atomicState;

  final DesktopUpdateConfig config;
  final UpdateStatus status;
  final InstallPackage install;
  final Future<void> Function(bool enabled) prompts;
  final void Function(String version) onInstalled;
  final UpdateTransport transport;
  final Future<void> Function(File file, String contents) _saveState;
  final Duration probeTimeout;
  // The service response has 60 s, plus its one-shot authentication handshake.
  final Duration installTimeout;
  bool enabled;
  bool _supported = false;
  bool _closed = false;
  bool _manual = false;
  int _mode = 0;
  Timer? _timer;
  Future<void>? _checking;
  Future<void>? _quitting;
  Future<void> _writes = Future.value();
  Map<String, dynamic> _saved = {};
  DesktopPackage? _ready;
  bool get hasReadyPackage => !_closed && !_manual && enabled && _supported && _ready != null;
  File get _stateFile => File('${config.directory.path}/state.json');

  /// False means a service installation is already in progress. The host
  /// must not offer connection actions; it shows the installation message
  /// and exits, letting the service finish.
  Future<bool> start({bool schedule = true}) async {
    await config.directory.create(recursive: true);
    try {
      final value = jsonDecode(await _stateFile.readAsString());
      if (value is Map<String, dynamic>) _saved = value;
    } on FileSystemException {
      /* First launch. */
    } on FormatException {
      /* A damaged app-owned state cannot authorize installation. */
    }
    final progress = await _probe();
    if (progress) return false;
    final pending = _saved['notice'];
    if (pending == '${config.version}.${config.build}') onInstalled(config.version);
    await _prompts(!_supported || !enabled || _saved['fallback'] != null);
    if (schedule) {
      _timer = Timer.periodic(const Duration(hours: 24), (_) => unawaited(check()));
      unawaited(Future<void>.delayed(const Duration(seconds: 15), check));
    }
    return true;
  }

  Future<bool> _probe() async {
    try {
      final result = await status().timeout(probeTimeout);
      _supported = result['build'] is int && (result['build'] as int) > 0;
      await _result(result['last_update']);
      return result['update'] is Map;
    } catch (_) {
      _supported = false;
      return false;
    }
  }

  Future<void> setEnabled(bool value) async {
    _mode++;
    enabled = value;
    if (!value) _ready = null;
    await _prompts(!value || !_supported || _saved['fallback'] != null);
    if (value) {
      // An off/on toggle invalidates the old check. Queue a new one after
      // it releases the single-check slot rather than waiting until tomorrow.
      final previous = _checking;
      unawaited(() async {
        await previous;
        await check();
      }());
    }
  }

  /// A user-requested Sparkle/WinSparkle check owns this session's install
  /// flow. Do not race its installer with a service handoff on the same quit.
  Future<void> manualCheck() async {
    manualInstallerStarted();
    await _prompts(true);
  }

  void manualInstallerStarted() {
    _mode++;
    _manual = true;
    _ready = null;
  }

  Future<void> check() {
    if (_closed || !enabled || _manual) return Future.value();
    return _checking ??= _check().whenComplete(() => _checking = null);
  }

  Future<void> _check() async {
    final mode = _mode;
    bool eligible() => !_closed && enabled && !_manual && mode == _mode;
    try {
      if (await _probe()) return;
      if (!eligible()) return;
      if (!_supported) {
        _ready = null;
        await _prompts(true);
        return;
      }
      final package = DesktopPackage.fromFeed(await transport.feed(config.feed), config);
      if (!eligible()) return;
      if (package == null) {
        _ready = null;
        await _prompts(false);
        return;
      }
      if (_saved['fallback'] == package.key || _failures(package.key) >= 3) {
        _ready = null;
        await _prompts(true);
        return;
      }
      // A newer release gets its own retries; an old fallback cannot disable
      // silent updates forever.
      _saved.remove('fallback');
      await _save();
      if (!eligible()) return;
      final target = File('${config.directory.path}/${package.filename}');
      if (_saved['cached'] != '${package.key}:${package.signature}' ||
          !await target.exists() ||
          await target.length() != package.length) {
        await transport.package(package, target);
      }
      if (!eligible()) return;
      _saved['cached'] = '${package.key}:${package.signature}';
      await _save();
      if (!eligible()) return;
      _ready = package;
      await _prompts(false);
    } catch (_) {
      // Missing PKG metadata, network/length failure or an invalid feed:
      // keep the old signed updater available; never install a partial file.
      _ready = null;
      if (!_closed) await _prompts(true);
    }
  }

  /// Called after normal app shutdown. Duplicate quits share the handoff.
  /// No ready package means no network or service wait on the quit path.
  Future<void> onQuit({bool manualInstaller = false}) => _quitting ??= _quit(manualInstaller);

  Future<void> _quit(bool manualInstaller) async {
    _mode++;
    _closed = true;
    _timer?.cancel();
    transport.cancel();
    final package = _ready;
    if (manualInstaller || _manual || !enabled || !_supported || package == null) return;
    try {
      await install(
        package,
        File('${config.directory.path}/${package.filename}').absolute.path,
      ).timeout(installTimeout);
    } catch (error) {
      final code = RegExp(r'\bUPDATE_[A-Z_]+\b').firstMatch('$error')?.group(0);
      _failure(package.key, code);
      if (!const {'UPDATE_BUSY', 'UPDATE_IN_PROGRESS', 'UPDATE_CLIENT_STILL_RUNNING'}.contains(code) &&
          !_countedFailure(code)) {
        _saved['fallback'] = package.key;
      }
      await _save();
    }
    // Accepted is not installed. Only a signed GetVersion result in the
    // matching installed build can produce the success notice.
  }

  int _failures(String key) {
    final value = (_saved['failures'] is Map ? _saved['failures'][key] : null);
    return value is int && value > 0 ? value : 0;
  }

  static bool _countedFailure(String? code) => const {
    'UPDATE_FILE_INVALID',
    'UPDATE_SIGNATURE_INVALID',
    'UPDATE_PUBLISHER_MISMATCH',
    'UPDATE_VERSION_MISMATCH',
    'UPDATE_INSTALL_FAILED',
  }.contains(code);

  void _failure(String key, String? code) {
    if (!_countedFailure(code)) return;
    final failures = _saved['failures'] is Map ? Map<String, dynamic>.from(_saved['failures']) : <String, dynamic>{};
    failures[key] = _failures(key) + 1;
    _saved['failures'] = failures;
    if (failures[key] >= 3) _saved['fallback'] = key;
  }

  Future<void> _result(dynamic result) async {
    if (result is! Map || result['finished_at_ms'] is! int || result['version'] is! String || result['build'] is! int)
      return;
    final at = result['finished_at_ms'] as int;
    final seen = _saved['seen'];
    if (at <= 0 || (seen is int && at <= seen)) return;
    final key = '${result['version']}.${result['build']}';
    if (result['outcome'] == 'installed') {
      if (key == '${config.version}.${config.build}') _saved['notice'] = key;
    } else if (result['outcome'] == 'failed') {
      _failure(key, RegExp(r'^UPDATE_[A-Z_]+').firstMatch('${result['error']}')?.group(0));
    } else {
      return;
    }
    _saved['seen'] = at;
    await _save();
  }

  /// Called when the overview actually shows the notice. A background launch
  /// that never opens its window keeps it for the next visible overview.
  Future<void> noticeShown() async {
    _saved.remove('notice');
    await _save();
  }

  Future<void> _prompts(bool enabled) async {
    if (_closed) return;
    await prompts(enabled);
  }

  Future<void> _save() {
    final json = jsonEncode(_saved);
    final next = _writes.then((_) => _saveState(_stateFile, json));
    _writes = next.catchError((Object _) {});
    return next;
  }

  static Future<void> _atomicState(File file, String contents) async {
    final temporary = File('${file.path}.new');
    await temporary.writeAsString(contents, flush: true);
    await temporary.rename(file.path);
  }
}
