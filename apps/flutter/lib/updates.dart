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
//
// Silent updates (#548, docs/host-integration.md section 16) on macOS and
// Windows, with 「自动更新」 on and a service that installs them: the app
// reads the service's state and checks the channel (rust/src/silent.rs)
// shortly after launch and every 6 hours (every 15 minutes while the
// service does not answer), downloads a newer package, and hands it to the
// service when the app quits, or when it is idle (nothing connected, no
// window: only the tray), never disconnecting anything: an idle install
// the service accepted is let go when the app is no longer idle. Sparkle's
// and WinSparkle's own scheduled checks are off meanwhile; they come back,
// with their prompt, when the switch is off or not saved, the service
// cannot install silently, a release has no silent package or failed three
// times, or the channel has not been read for a day. 「检查更新…」 is always
// theirs.

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
  final String command;
}

/// How a check the user asked for ended (Linux; WinSparkle and Sparkle show their own).
enum CheckResult { upToDate, available, failed }

class Updates {
  Updates._();

  /// The app installs updates itself (macOS, Windows): 「检查更新…」 shows.
  static bool installs = false;

  /// The 「自动更新」 switch; null where there is none (Linux, no updater).
  static final ValueNotifier<bool?> autoUpdate = ValueNotifier(null);

  /// 「已更新到 PPVPN {v}」 on the overview, until closed (update_notices.dart).
  static final ValueNotifier<String?> updatedTo = ValueNotifier(null);

  /// The service's state, as last read.
  static rust.UiSilentUpdate? _service;

  /// The channel's release, as last prepared.
  static rust.UiSilentPrepared? _prepared;

  /// Since when the channel could not be read or the package downloaded.
  static DateTime? _laterSince;

  /// The switch is saved (a first launch's value too).
  static bool _saved = true;

  /// When the loop next reads the service and the channel.
  static DateTime _due = DateTime.now();

  /// The app is quitting: no idle install any more.
  static bool _quitting = false;

  /// An idle install's request, while it is out.
  static Future<bool>? _idleRequest;

  /// The app is idle: nothing connected and no window (main.dart).
  static Future<bool> Function() _idle = () async => false;

  /// Quits the app once the service accepted an idle install (main.dart).
  static Future<void> Function() _quit = () async {};

  /// Linux: the newer build, while there is one.
  static final ValueNotifier<UpdateNotice?> notice = ValueNotifier(null);

  /// Linux: the last check the user asked for.
  static final ValueNotifier<CheckResult?> checked = ValueNotifier(null);

  static String? _feed;
  static String _build = '';
  static String _command = '';

  /// The service is installing an update now: the app shows
  /// InstallingNotice and quits. Asked once, before the window.
  static Future<bool> installingNow() async {
    if (!Platform.isMacOS && !Platform.isWindows) return false;
    await _readService();
    return _service?.installing ?? false;
  }

  /// The service's state; its result for this build shows once.
  static Future<void> _readService() async {
    try {
      _service = await rust.silentUpdateState();
    } catch (error) {
      debugPrint('silent update: $error');
      _service = null;
    }
    if (_service?.updatedTo case final version?) updatedTo.value = version;
  }

  /// Starts the platform's updater, once the window exists. [idle] tells
  /// whether the app is idle; [quit] quits it for an idle install.
  static Future<void> start({required Future<bool> Function() idle, required Future<void> Function() quit}) async {
    _idle = idle;
    _quit = quit;
    bool? checks;
    if (Platform.isMacOS) {
      try {
        final state = await _channel.invokeMapMethod<String, Object?>('state');
        installs = state?['available'] == true;
        if (installs) checks = state?['autoCheck'] == true;
      } on PlatformException catch (error) {
        debugPrint('updates: $error');
      }
    } else {
      final updater = rust.startUpdater();
      installs = updater.installs;
      if (installs) checks = updater.autoCheck;
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
    if (!installs) return;
    // The first launch with the switch takes the old automatic-check
    // switch's value; unsaved, the app keeps the prompt this time.
    final saved = rust.autoUpdate();
    final enabled = saved ?? checks ?? true;
    if (saved == null) _saved = rust.setAutoUpdate(enabled: enabled);
    autoUpdate.value = enabled;
    await _promptChecks();
    unawaited(_silentLoop());
  }

  /// The service installs this release silently.
  static bool get _silent =>
      autoUpdate.value == true &&
      _saved &&
      (_service?.available ?? false) &&
      _prepared != rust.UiSilentPrepared.prompt &&
      !(_laterSince != null && DateTime.now().difference(_laterSince!) > const Duration(days: 1));

  /// Sparkle's or WinSparkle's scheduled checks (and their prompt): on
  /// unless the service installs silently.
  static Future<void> _promptChecks() async {
    final on = !_silent;
    try {
      if (Platform.isMacOS) {
        await _channel.invokeMethod<void>('setAutoCheck', on);
      } else {
        rust.setAutoCheckUpdates(enabled: on);
      }
    } catch (error) {
      debugPrint('updates: $error');
    }
  }

  /// Every minute: the service and the channel when due, then an idle
  /// install while a package is ready.
  static Future<void> _silentLoop() async {
    _due = DateTime.now().add(const Duration(seconds: 30));
    while (!_quitting) {
      await Future<void>.delayed(const Duration(minutes: 1));
      if (_quitting) return;
      if (!DateTime.now().isBefore(_due)) {
        await _readService();
        final available = _service?.available ?? false;
        _due = DateTime.now().add(available ? const Duration(hours: 6) : const Duration(minutes: 15));
        if (available && autoUpdate.value == true) await _prepare();
        await _promptChecks();
      }
      if (_silent && _prepared == rust.UiSilentPrepared.ready && await _idle()) {
        // Refused or let go: the package is checked again in an hour.
        if (!await _installIdle()) _due = DateTime.now().add(const Duration(hours: 1));
      }
    }
  }

  static Future<void> _prepare() async {
    try {
      _prepared = await rust.silentUpdatePrepare();
    } catch (error) {
      debugPrint('silent update: $error');
      _prepared = rust.UiSilentPrepared.later;
    }
    if (_prepared == rust.UiSilentPrepared.later) {
      _laterSince ??= DateTime.now();
    } else {
      _laterSince = null;
    }
  }

  /// Hands the package to the service to install once the app exits, and
  /// quits; false when refused, or when the app is no longer idle by the
  /// answer (the service then records that the app did not exit, and
  /// installs nothing).
  static Future<bool> _installIdle() async {
    final request = rust.silentUpdateInstall(relaunch: true).catchError((Object error) {
      debugPrint('silent update: $error');
      return false;
    });
    _idleRequest = request;
    final accepted = await request;
    _idleRequest = null;
    if (!accepted) {
      _prepared = null;
      return false;
    }
    // Quitting meanwhile: quit() goes on, the service installs.
    if (_quitting) return true;
    if (!await _idle()) {
      rust.clearStartHidden();
      _prepared = null;
      return false;
    }
    _quitting = true;
    await _quit();
    return true;
  }

  /// Called first when the app quits: no idle install from now on. True
  /// when an idle install's request was out and the service accepted it:
  /// [installAtQuit] has nothing to send.
  static Future<bool> quitting() async {
    _quitting = true;
    return await _idleRequest ?? false;
  }

  /// A package is ready for [installAtQuit].
  static bool get readyAtQuit => _silent && _prepared == rust.UiSilentPrepared.ready;

  /// At quit, after the window and the tray are gone and the cores
  /// stopped: hands the package to the service, which installs it once the
  /// app has exited and does not open it again. Waits for the answer (at
  /// most a minute); the app exits either way.
  static Future<void> installAtQuit() async {
    try {
      await rust.silentUpdateInstall(relaunch: false);
    } catch (error) {
      debugPrint('silent update: $error');
    }
  }

  static Future<void> setAutoUpdate(bool enabled) async {
    _saved = rust.setAutoUpdate(enabled: enabled);
    autoUpdate.value = enabled;
    if (!enabled) _prepared = null;
    // On: the channel within the minute.
    if (enabled) _due = DateTime.now();
    await _promptChecks();
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
