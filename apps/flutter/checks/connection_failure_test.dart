import 'package:flutter/widgets.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ppvpn_flutter/app_state.dart';
import 'package:ppvpn_flutter/connection_failure.dart';
import 'package:ppvpn_flutter/main.dart';
import 'package:ppvpn_flutter/pages/overview.dart';
import 'package:ppvpn_flutter/settings.dart';
import 'package:ppvpn_flutter/src/rust/api.dart' as rust;
import 'package:ppvpn_flutter/strings.dart';

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();
  test('known causes have guidance and unknown failures have a useful fallback', () {
    for (final (code, detail, key) in [
      ('ConnectFailed', 'WINTUN_UNAVAILABLE: missing', 'fr_driver'),
      ('ConnectFailed', 'PERMISSION_DENIED: denied', 'fr_permission'),
      ('ConnectFailed', 'TUN_NAME_TAKEN: occupied', 'fr_tunBusy'),
      ('ConnectHealthCheckFailed', 'private detail', 'fr_health'),
      ('ConnectHealthCheckFailed', 'HEALTH_DIRECT_FAILED: direct API unavailable', 'fr_health'),
      ('ServiceClientRejected', 'signature rejected', 'fr_clientRejected'),
      ('ServiceIncompatible', 'PROFILE_SCHEMA_UNSUPPORTED: old', 'fr_service'),
      ('ConnectFailed', 'unexpected private detail', 'fr_connect'),
      ('FutureError', '', 'fr_connect'),
    ]) {
      expect(enhancedFailureMessage(code, detail), key);
    }
    expect(enhancedFailureMessage(null, null), 'fr_connect');
  });

  test('copied report contains codes and stage but no raw details', () {
    const secret = 'C:\\private\\config.json https://private.example.net?token=secret';
    final text = enhancedFailureReport(
      version: '0.7.4',
      platform: 'windows',
      code: 'ConnectFailed',
      detail: 'PERMISSION_DENIED: $secret [start_stage=start]',
    );
    expect(text, 'PPVPN 0.7.4 (windows)\nmode=enhanced\nerror=ConnectFailed\nreason=PERMISSION_DENIED\nstage=start');
    expect(text, isNot(contains('secret')));
    final hostile = enhancedFailureReport(
      version: 'private\nvalue',
      platform: 'bad platform',
      code: 'bad\ncode',
      detail: '$secret [start_stage=secret]',
    );
    expect(hostile, 'PPVPN unknown (unknown)\nmode=enhanced\nerror=unknown');
    expect(serviceFailureCode('error: PERMISSION_DENIED'), isNull);
  });

  for (final language in [rust.UiLanguage.chinese, rust.UiLanguage.english]) {
    testWidgets('enhanced failure copy fits a narrow screen in $language', (tester) async {
      tester.view.physicalSize = const Size(320, 720);
      tester.view.devicePixelRatio = 1;
      addTearDown(tester.view.resetPhysicalSize);
      addTearDown(tester.view.resetDevicePixelRatio);
      Settings.language.value = language;
      Settings.version = '0.7.4';
      String? copied;
      tester.binding.defaultBinaryMessenger.setMockMethodCallHandler(SystemChannels.platform, (call) async {
        if (call.method == 'Clipboard.setData') copied = (call.arguments as Map)['text'] as String;
        return null;
      });
      addTearDown(() => tester.binding.defaultBinaryMessenger.setMockMethodCallHandler(SystemChannels.platform, null));
      final snapshot = rust.UiSnapshot(
        auth: rust.UiAuth.signedIn,
        profile: rust.UiProfile.ready,
        profilePrevious: false,
        phase: rust.UiPhase.error,
        standardReady: false,
        unread: 0,
        routingMode: rust.UiRoutingMode.rules,
        serviceInstalled: true,
        connectionMode: rust.UiConnectionMode.enhanced,
        canTakeOver: false,
        suggestCompatible: true,
        apiUnreachable: false,
        error: const rust.UiError(code: 'ConnectFailed', detail: 'WINTUN_UNAVAILABLE: private [start_stage=create]'),
      );
      final app = AppState.fixed(snapshot);
      await tester.pumpWidget(PrototypeApp(notice: Center(child: ProblemCard.of(app, snapshot)!)));
      await tester.pumpAndSettle();
      expect(find.text(S.current('fr_driver')), findsOneWidget);
      await tester.tap(find.text(S.current('copyDiagnostics')));
      await tester.pump();
      expect(copied, contains('reason=WINTUN_UNAVAILABLE\nstage=create'));
      expect(copied, isNot(contains('private')));
      expect(find.text(S.current('retry')), findsOneWidget);
      expect(find.text(S.current('useCompat')), findsOneWidget);
      expect(tester.takeException(), isNull);
      await tester.pump(const Duration(seconds: 2));
    });
  }
}
