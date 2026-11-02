// In-app updates (#401 step 5), from the native apps' feeds, as they do
// them:
// - macOS: Sparkle in the runner (runner/macos/Runner/MainFlutterWindow.swift),
//   on the channel below; it unregisters the push agent before it replaces
//   the app, as the native app does.
// - Windows: WinSparkle in the bridge (rust/src/update.rs). Once its
//   installer runs, the app hides (activation.dart, `onUpdate`) until the
//   installer asks it to quit.
// - Linux: a notice in the settings page when the repository has a newer
//   build (apps/linux/Update/UpdateNotifier.cs): its latest.json shortly
//   after launch and every 24 hours, and the upgrade command to copy.
// Builds without a feed (development builds) have none of it.

import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'package:flutter/foundation.dart';
import 'package:flutter/services.dart';

import 'src/rust/api.dart' as rust;

const _channel = MethodChannel('ppvpn/update');

/// A newer build in the Linux repository.
class UpdateNotice {
  const UpdateNotice({required this.version, required this.command});

  /// As shown: x.y.z, with the build when only the build differs ("0.6.4 (3)").
  final String version;

  /// The upgrade command for this install's package format.
  final String? command;
}

/// How a check the user asked for ended (Linux; WinSparkle and Sparkle show their own).
enum CheckResult { upToDate, available, failed }

class Updates {
  Updates._();

  /// The app installs updates itself (macOS, Windows): 「检查更新…」 shows.
  static bool installs = false;

  /// The automatic-check switch; null where there is none (Linux, no updater).
  static final ValueNotifier<bool?> autoCheck = ValueNotifier(null);

  /// Linux: the newer build, while there is one.
  static final ValueNotifier<UpdateNotice?> notice = ValueNotifier(null);

  /// Linux: the last check the user asked for.
  static final ValueNotifier<CheckResult?> checked = ValueNotifier(null);
  static final ValueNotifier<bool> busy = ValueNotifier(false);
  static final ValueNotifier<bool> installPermission = ValueNotifier(false);
  static bool _apk = false;

  static String? _feed;
  static String _build = '';
  static String _command = '';

  /// Starts the platform's updater, once the window exists.
  static Future<void> start() async {
    if (Platform.isAndroid) {
      try {
        final state = await _channel.invokeMapMethod<String, Object?>('state');
        _apk = state?['available'] == true;
        installs = _apk;
        if (_apk)
          unawaited(() async {
            await Future<void>.delayed(const Duration(seconds: 15));
            while (true) {
              await check();
              await Future<void>.delayed(const Duration(hours: 24));
            }
          }());
      } on PlatformException catch (error) {
        debugPrint('updates: $error');
      }
      return;
    }
    if (Platform.isMacOS) {
      try {
        final state = await _channel.invokeMapMethod<String, Object?>('state');
        installs = state?['available'] == true;
        if (installs) autoCheck.value = state?['autoCheck'] == true;
      } on PlatformException catch (error) {
        debugPrint('updates: $error');
      }
      return;
    }
    final updater = rust.startUpdater();
    installs = updater.installs;
    if (installs) autoCheck.value = updater.autoCheck;
    _feed = updater.noticeFeed;
    _build = updater.build;
    _command = updater.upgradeCommand;
    if (_feed != null) {
      unawaited(() async {
        await Future<void>.delayed(const Duration(seconds: 15));
        while (true) {
          await _checkFeed();
          await Future<void>.delayed(const Duration(hours: 24));
        }
      }());
    }
  }

  /// Linux: there is a feed to check.
  static bool get notices => _feed != null;

  /// A check the user asked for: Sparkle's or WinSparkle's own windows, or
  /// (Linux) the repository's latest.json.
  static Future<void> check() async {
    if (_apk) {
      if (busy.value) return;
      busy.value = true;
      checked.value = null;
      try {
        final next = await _channel.invokeMapMethod<String, Object?>('check');
        notice.value = next == null
            ? null
            : UpdateNotice(version: '${next['version']} (${next['versionCode']})', command: null);
        checked.value = next == null ? CheckResult.upToDate : CheckResult.available;
      } on PlatformException catch (error) {
        debugPrint('APK update check: $error');
        checked.value = CheckResult.failed;
      } finally {
        busy.value = false;
      }
    } else if (Platform.isMacOS) {
      await _channel.invokeMethod<void>('check');
    } else if (installs) {
      rust.checkForUpdates();
    } else if (notices) {
      checked.value = null;
      checked.value = await _checkFeed();
    }
  }

  static Future<void> installApk() async {
    if (!_apk || busy.value) return;
    busy.value = true;
    checked.value = null;
    installPermission.value = false;
    try {
      installPermission.value = await _channel.invokeMethod<String>('install') == 'permission';
    } on PlatformException catch (error) {
      debugPrint('APK install: $error');
      checked.value = CheckResult.failed;
    } finally {
      busy.value = false;
    }
  }

  static Future<void> setAutoCheck(bool enabled) async {
    try {
      if (Platform.isMacOS) {
        await _channel.invokeMethod<void>('setAutoCheck', enabled);
      } else {
        rust.setAutoCheckUpdates(enabled: enabled);
      }
      autoCheck.value = enabled;
    } catch (error) {
      debugPrint('updates: $error');
    }
  }

  static Future<CheckResult> _checkFeed() async {
    final feed = _feed;
    if (feed == null) return CheckResult.failed;
    final client = HttpClient()..connectionTimeout = const Duration(seconds: 30);
    client.userAgent = 'PPVPN/$_build (linux)';
    try {
      final request = await client.getUrl(Uri.parse(feed));
      final response = await request.close().timeout(const Duration(seconds: 30));
      if (response.statusCode != HttpStatus.ok) throw HttpException('HTTP ${response.statusCode}');
      final latest = jsonDecode(await response.transform(utf8.decoder).join());
      final version = latest is Map ? latest['version'] : null;
      final build = latest is Map ? _parts(latest['build']) : null;
      final installed = _parts(_build);
      if (version is! String || version.trim().isEmpty || build == null || installed == null) {
        throw const FormatException('latest.json has no version or build');
      }
      if (_compare(build, installed) <= 0) {
        debugPrint('update: up to date ($_build)');
        return CheckResult.upToDate;
      }
      // Only the build differs: show it, or "0.6.4 is available" reads as the
      // version already installed.
      final sameVersion = build[0] == installed[0] && build[1] == installed[1] && build[2] == installed[2];
      notice.value = UpdateNotice(
        version: sameVersion ? '${version.trim()} (${build[3]})' : version.trim(),
        command: _command,
      );
      return CheckResult.available;
    } catch (error) {
      debugPrint('update check failed: $error');
      return CheckResult.failed;
    } finally {
      client.close(force: true);
    }
  }

  /// "a.b.c.d" as four numbers.
  static List<int>? _parts(Object? build) {
    if (build is! String) return null;
    final parts = build.trim().split('.').map(int.tryParse).toList();
    if (parts.length != 4 || parts.contains(null)) return null;
    return parts.cast<int>();
  }

  static int _compare(List<int> a, List<int> b) {
    for (var i = 0; i < 4; i++) {
      if (a[i] != b[i]) return a[i].compareTo(b[i]);
    }
    return 0;
  }
}
