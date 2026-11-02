// The Flutter desktop prototype (#387): one window (see shell.dart) with the
// overview, nodes and settings pages, plus the menu bar / tray icon, on the client
// (crates/desktop) through flutter_rust_bridge. Closing the window hides
// it; the app stays in the tray until "Quit". `--background` (the login
// entry on Windows and Linux, rust/src/autostart.rs) starts it in the tray
// with no window. One instance runs: another launch, or a click on a
// notification, shows its window (activation.dart); the Windows installer
// has it quit as the tray's Quit does. Updates: updates.dart. On Android
// and iOS (#448) the app is its one screen: no window, tray or updater.

import 'dart:async';
import 'dart:io';

import 'package:flutter/services.dart';
import 'package:flutter/widgets.dart';
import 'package:flutter_rust_bridge/flutter_rust_bridge_for_generated.dart';
import 'package:forui/forui.dart';
import 'package:window_manager/window_manager.dart';

import 'activation.dart';
import 'app_state.dart';
import 'bench.dart';
import 'e2e.dart';
import 'mobile.dart';
import 'phone/phone.dart';
import 'src/rust/api.dart' as rust;
import 'sample.dart';
import 'settings.dart';
import 'shell.dart';
import 'strings.dart';
import 'src/rust/frb_generated.dart';
import 'theme.dart';
import 'tray.dart';
import 'updates.dart';

/// The native side's answers on Android and iOS (runner/android
/// MainActivity): the app's own directory, where the tunnel's socket also
/// lives (#448).
const _app = MethodChannel('com.peakpassvpn.ppvpn/app');

/// The bridge library ci.yml puts next to the app: beside the executable
/// (Windows), in the bundle's lib/ (Linux), in Contents/Frameworks (macOS),
/// in the APK's jniLibs (Android, loaded by name). PPVPN_FLUTTER_LIB overrides it (the end-to-end test's debug build runs
/// with the release library ci-build.sh built).
String _libraryPath() {
  final override = Platform.environment['PPVPN_FLUTTER_LIB'];
  if (override != null && override.isNotEmpty) return override;
  if (Platform.isAndroid) return 'libppvpn_flutter.so';
  final dir = File(Platform.resolvedExecutable).parent.path;
  if (Platform.isWindows) return '$dir\\ppvpn_flutter.dll';
  if (Platform.isMacOS) return '$dir/../Frameworks/libppvpn_flutter.dylib';
  return '$dir/lib/libppvpn_flutter.so';
}

Future<void> main() async {
  mark('main');
  WidgetsFlutterBinding.ensureInitialized();
  await loadSampleSettings();
  final AppState app;
  var background = false;
  if (samplePage != null) {
    app = sampleState();
  } else {
    // iOS: linked into the app itself (scripts/ci-build.sh).
    await RustLib.init(
      externalLibrary: Platform.isIOS
          ? ExternalLibrary.process(iKnowHowToUseIt: true)
          : ExternalLibrary.open(_libraryPath()),
    );
    mark('bridge_loaded');
    // Before anything reads the app's files.
    if (mobile) rust.setAppDir(path: (await _app.invokeMethod<String>('filesDir'))!);
    // Another instance runs (Windows, Linux): it took this launch's
    // arguments and shows its window. Before the window and the client.
    if (!rust.claimInstance()) exit(0);
    // Before the client: it reads the API override when it starts.
    Settings.load();
    background = rust.startedInBackground();
    rust.refreshLaunchAtLogin();
    app = AppState(rust.start());
    mark('client_created');
  }

  // A phone or tablet: no window manager or tray. Website Android builds
  // still initialize their updater before the settings page reads it.
  if (mobile) {
    if (samplePage == null && Platform.isAndroid) await Updates.start();
    runApp(PrototypeApp(app: app));
    return;
  }

  await windowManager.ensureInitialized();
  mark('window_manager_ready');
  await windowManager.waitUntilReadyToShow(
    const WindowOptions(
      size: Tokens.windowSize,
      minimumSize: Tokens.windowMinSize,
      center: true,
      title: 'PPVPN',
      // The sidebar runs to the top; macOS keeps its traffic lights over it.
      titleBarStyle: TitleBarStyle.hidden,
    ),
    () async {
      mark('ready_to_show');
      // The runners leave the window hidden (ci-build.sh); macOS shows its
      // own at launch.
      if (background) {
        await windowManager.hide();
        return;
      }
      await windowManager.show();
      await windowManager.focus();
      mark('window_shown');
    },
  );
  await windowManager.setPreventClose(true);

  Future<void> quit() async {
    app.stop();
    await windowManager.setPreventClose(false);
    await windowManager.destroy();
  }

  Future<void> open() async {
    await windowManager.show();
    await windowManager.focus();
  }

  // Before the tray, whose menu offers a check where the app installs updates.
  if (samplePage == null) await Updates.start();
  final tray = Tray(
    app: app,
    onOpen: () => unawaited(open()),
    onQuit: quit,
    onCheckForUpdates: Updates.installs ? () => unawaited(Updates.check()) : null,
  );
  windowManager.addListener(_HideOnClose());
  mark('tray_created');
  // Off unless PPVPN_FLUTTER_E2E is set (e2e/linux_enhanced.sh).
  if (samplePage == null) startE2e(app, quit);
  runApp(PrototypeApp(app: app, tray: tray));
  if (samplePage == null) {
    listenForActivations(
      onShow: () => unawaited(open()),
      onPush: (pushId) => unawaited(() async {
        if (await app.openPush(pushId)) await open();
      }()),
      // The installer is about to replace the app's files.
      onQuit: () => unawaited(quit()),
      // The update's installer runs: out of sight until it asks the app to quit.
      onUpdate: () => unawaited(() async {
        tray.dispose();
        await windowManager.hide();
      }()),
    );
  }
  unawaited(WidgetsBinding.instance.waitUntilFirstFrameRasterized.then((_) => mark('first_frame')));
}

class _HideOnClose with WindowListener {
  @override
  void onWindowClose() => unawaited(windowManager.hide());
}

class PrototypeApp extends StatefulWidget {
  const PrototypeApp({super.key, required this.app, this.tray});

  final AppState app;

  /// Held for the app's life; none on Android and iOS.
  final Tray? tray;

  @override
  State<PrototypeApp> createState() => _PrototypeAppState();
}

/// The appearance reaches every widget through [AppearanceScope]
/// (Tokens.of). The language has no such dependency (S.current takes no
/// context): a change rebuilds every element once, keeping their state
/// (the page, the scroll position).
class _PrototypeAppState extends State<PrototypeApp> {
  @override
  void initState() {
    super.initState();
    Settings.language.addListener(_relabel);
  }

  @override
  void dispose() {
    Settings.language.removeListener(_relabel);
    super.dispose();
  }

  void _relabel() {
    setState(() {});
    void rebuild(Element element) {
      element.markNeedsBuild();
      element.visitChildren(rebuild);
    }

    (context as Element).visitChildren(rebuild);
  }

  @override
  Widget build(BuildContext context) => AppearanceScope(
    notifier: Settings.appearance,
    child: WidgetsApp(
      title: 'PPVPN',
      color: Tokens.light.accent,
      debugShowCheckedModeBanner: false,
      // Follows the language setting; the system's while it follows that.
      locale: Settings.language.value == rust.UiLanguage.system ? null : S.current.locale,
      localizationsDelegates: FLocalizations.localizationsDelegates,
      supportedLocales: FLocalizations.supportedLocales,
      pageRouteBuilder: <T>(RouteSettings settings, WidgetBuilder builder) =>
          PageRouteBuilder<T>(settings: settings, pageBuilder: (context, _, _) => builder(context)),
      home: Builder(
        builder: (context) {
          final t = Tokens.of(context);
          return FTheme(
            data: t.forui,
            child: DefaultTextStyle(
              style: t.body,
              // Phones and tablets get the phone layout, on a tablet centred
              // at Tokens.phoneMaxWidth until the iPad boards are built
              // (#535); elsewhere below 600 wide too (#448), else the
              // desktop's.
              child: ColoredBox(
                color: t.bg,
                child: LayoutBuilder(
                  builder: (context, box) => mobile
                      ? Center(
                          child: ConstrainedBox(
                            constraints: const BoxConstraints(maxWidth: Tokens.phoneMaxWidth),
                            child: PhoneShell(app: widget.app),
                          ),
                        )
                      : box.maxWidth < Tokens.phoneBreakpoint
                      ? PhoneShell(app: widget.app)
                      : Shell(app: widget.app),
                ),
              ),
            ),
          );
        },
      ),
    ),
  );
}
