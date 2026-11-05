import 'package:flutter/widgets.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';

import '../lib/pages/settings.dart';
import '../lib/app_state.dart';
import '../lib/main.dart' show PrototypeApp;
import '../lib/settings.dart';
import '../lib/src/rust/api.dart' as rust;
import '../lib/strings.dart';
import '../lib/theme.dart';
import '../lib/updates.dart';

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();
  setUpAll(() async {
    // Use the shipped font metrics when checking the phone-width layout.
    final sans = FontLoader(Tokens.sans)..addFont(rootBundle.load('packages/forui/assets/fonts/inter/Inter.ttf'));
    final mono = FontLoader(Tokens.mono)..addFont(rootBundle.load('assets/fonts/JetBrainsMono-Regular.ttf'));
    await Future.wait([sans.load(), mono.load()]);
  });
  tearDown(() {
    Updates.installs = false;
    Updates.notice.value = null;
    Updates.checked.value = null;
    Updates.busy.value = false;
    Updates.installPermission.value = false;
    Updates.autoUpdate.value = null;
    Settings.version = '';
    Settings.language.value = rust.UiLanguage.system;
  });

  testWidgets('APK settings render available, downloading, permission, failure, and current states', (tester) async {
    Settings.language.value = rust.UiLanguage.english;
    Settings.version = '0.4.0 (100)';
    Updates.installs = true;
    final s = S.current;
    await tester.pumpWidget(
      Directionality(
        textDirection: TextDirection.ltr,
        child: MediaQuery(
          data: const MediaQueryData(size: Size(320, 844)),
          child: DefaultTextStyle(
            style: Tokens.light.body,
            child: const Align(
              alignment: Alignment.topCenter,
              child: SizedBox(
                width: 320,
                child: Padding(padding: EdgeInsets.all(16), child: VersionSettings()),
              ),
            ),
          ),
        ),
      ),
    );
    expect(find.text(s('checkUpdates')), findsOneWidget);
    expect(find.text(s('apkInstall')), findsNothing);

    Updates.notice.value = const UpdateNotice(version: '0.4.1 (101)', command: null);
    Updates.checked.value = CheckResult.available;
    await tester.pump();
    expect(find.text(s('updAvailT', {'v': '0.4.1 (101)'})), findsOneWidget);
    expect(find.text(s('apkInstall')), findsOneWidget);
    expect(find.text(s('copyCommand')), findsNothing);

    Updates.busy.value = true;
    await tester.pump();
    expect(find.text(s('apkDownloading')), findsOneWidget);
    expect(find.text(s('apkInstall')), findsNothing);
    Updates.busy.value = false;
    Updates.installPermission.value = true;
    await tester.pump();
    expect(find.text(s('apkPermission')), findsOneWidget);
    expect(find.text(s('apkInstall')), findsOneWidget);

    Updates.installPermission.value = false;
    Updates.checked.value = CheckResult.failed;
    await tester.pump();
    expect(find.text(s('apkInstallFailed')), findsOneWidget);
    expect(find.text(s('apkPermission')), findsNothing);
    Updates.notice.value = null;
    Updates.checked.value = CheckResult.upToDate;
    await tester.pump();
    expect(find.text(s('upToDate')), findsOneWidget);
    expect(find.text(s('apkInstall')), findsNothing);
    expect(find.text(s('apkInstallFailed')), findsNothing);
    Updates.checked.value = CheckResult.failed;
    await tester.pump();
    expect(find.text(s('updCheckFailT')), findsOneWidget);
    expect(tester.takeException(), isNull);
  });

  testWidgets('signed-out APK users can reach update settings; unavailable builds hide the entry', (tester) async {
    tester.view.physicalSize = const Size(320, 844);
    tester.view.devicePixelRatio = 1;
    addTearDown(tester.view.resetPhysicalSize);
    addTearDown(tester.view.resetDevicePixelRatio);
    Settings.language.value = rust.UiLanguage.english;
    Settings.version = '0.4.0 (100)';
    final app = AppState.fixed(
      const rust.UiSnapshot(
        auth: rust.UiAuth.signedOut,
        profile: rust.UiProfile.loading,
        profilePrevious: false,
        phase: rust.UiPhase.off,
        standardReady: false,
        unread: 0,
        routingMode: rust.UiRoutingMode.rules,
        serviceInstalled: false,
        connectionMode: rust.UiConnectionMode.compatible,
        canTakeOver: false,
        suggestCompatible: false,
        apiUnreachable: false,
      ),
    );
    final label = S.current('checkUpdates');
    Updates.installs = false;
    await tester.pumpWidget(PrototypeApp(app: app));
    expect(find.text(label), findsNothing);
    Updates.installs = true;
    await tester.pumpWidget(PrototypeApp(app: app));
    final entry = find.text(label);
    expect(entry, findsOneWidget);
    await tester.ensureVisible(entry);
    await tester.tap(entry);
    await tester.pumpAndSettle();
    expect(find.byType(VersionSettings), findsOneWidget);
    expect(app.snapshot.value.auth, rust.UiAuth.signedOut);
    expect(tester.takeException(), isNull);
  });
}
