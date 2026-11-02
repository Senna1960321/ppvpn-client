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

import 'desktop_updates.dart';
import 'src/rust/api.dart' as rust;

const _channel = MethodChannel('ppvpn/update');

/// A newer build in the Linux repository.
class UpdateNotice {
  const UpdateNotice({required this.version, required this.command});

  /// As shown: x.y.z, with the build when only the build differs ("0.6.4 (3)").
  final String version;

  /// The upgrade command for this install's package format.
  final String command;
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

  /// Signed service result, kept until the overview is actually visible.
  static final ValueNotifier<String?> installed = ValueNotifier(null);
  static DesktopUpdates? _desktop;
  static bool installing = false;
  static bool get silent => _desktop != null;
  static bool _noticeShown = false;
  static Timer? _noticeTimer;

  static String? _feed;
  static String _build = '';
  static String _command = '';

  /// Starts the platform's updater, once the window exists.
  static Future<void> start() async {
    if (Platform.isMacOS) {
      try {
        final state = await _channel.invokeMapMethod<String, Object?>('state');
        installs = state?['available'] == true;
        if (installs) autoCheck.value = state?['autoCheck'] == true;
      } on PlatformException catch (error) {
        debugPrint('updates: $error');
      }
      await _startDesktop();
      return;
    }
    final updater = rust.startUpdater();
    installs = updater.installs;
    if (installs) autoCheck.value = updater.autoCheck;
    _feed = updater.noticeFeed;
    _build = updater.build;
    _command = updater.upgradeCommand;
    if (Platform.isWindows) await _startDesktop();
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
    await _desktop?.manualCheck();
    if (Platform.isMacOS) {
      await _channel.invokeMethod<void>('check');
    } else if (installs) {
      rust.checkForUpdates();
    } else if (notices) {
      checked.value = null;
      checked.value = await _checkFeed();
    }
  }

  static Future<void> setAutoCheck(bool enabled) async {
    try {
      if (_desktop case final desktop?) {
        rust.autoUpdatePreference(enabled: enabled, legacy: autoCheck.value ?? true);
        autoCheck.value = enabled;
        await desktop.setEnabled(enabled);
        return;
      }
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

  static Future<void> _promptChecks(bool enabled) async {
    if (Platform.isMacOS) {
      await _channel.invokeMethod<void>('setPromptChecks', enabled);
    } else {
      rust.setUpdatePromptChecks(enabled: enabled);
    }
  }

  static Future<void> _startDesktop() async {
    if (!installs) return;
    try {
      final config = rust.desktopUpdater();
      if (config == null) {
        // Development/old feed-only builds keep the original updater.
        await _promptChecks(autoCheck.value ?? true);
        return;
      }
      final enabled = rust.autoUpdatePreference(enabled: null, legacy: autoCheck.value ?? true);
      autoCheck.value = enabled;
      final desktop = DesktopUpdates(
        config: DesktopUpdateConfig(
          feed: Uri.parse(config.feed),
          platform: config.platform,
          version: config.version,
          build: config.build.toInt(),
          directory: Directory(config.stateDir),
        ),
        enabled: enabled,
        status: () async => jsonDecode(await rust.serviceUpdateStatus()) as Map<String, dynamic>,
        install: (package, path) async {
          await rust.installUpdate(
            path: path,
            version: package.version,
            build: BigInt.from(package.build),
            length: BigInt.from(package.length),
            edSignature: package.signature,
          );
          if (Platform.isMacOS) {
            // The service has accepted; let the bundled push agent exit
            // cleanly before its bundle is replaced. It registers on the
            // next normal launch. Failure still must not prevent quit.
            try {
              await _channel.invokeMethod<void>('prepareForUpdate').timeout(const Duration(seconds: 2));
            } catch (error) {
              debugPrint('update agent cleanup: $error');
            }
          }
        },
        prompts: _promptChecks,
        onInstalled: (version) => installed.value = version,
      );
      _desktop = desktop;
      installing = !await desktop.start();
    } catch (error) {
      debugPrint('desktop updates: $error');
      await _promptChecks(true);
    }
  }

  static Future<void> onQuit({bool manualInstaller = false}) async {
    final desktop = _desktop;
    if (desktop == null) return;
    if (!desktop.hasReadyPackage) {
      await desktop.onQuit(manualInstaller: manualInstaller);
      return;
    }
    if (Platform.isMacOS && !manualInstaller) {
      try {
        manualInstaller = await _channel.invokeMethod<bool>('isInstalling').timeout(const Duration(seconds: 2)) == true;
      } catch (_) {
        /* Missing native updater: service checks its capability. */
      }
    }
    await desktop.onQuit(manualInstaller: manualInstaller);
  }

  static void manualInstallerStarted() => _desktop?.manualInstallerStarted();

  static Future<void> noticeShown() async {
    if (_noticeShown || installed.value == null) return;
    _noticeShown = true;
    try {
      await _desktop?.noticeShown();
    } catch (error) {
      debugPrint('update notice: $error');
    }
    _noticeTimer = Timer(const Duration(seconds: 10), dismissInstalled);
  }

  static void dismissInstalled() {
    unawaited(noticeShown());
    _noticeTimer?.cancel();
    installed.value = null;
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
