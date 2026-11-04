import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ppvpn_flutter/app_state.dart';
import 'package:ppvpn_flutter/main.dart';
import 'package:ppvpn_flutter/pages/sign_in.dart';
import 'package:ppvpn_flutter/phone/sign_in.dart';
import 'package:ppvpn_flutter/src/rust/api.dart' as rust;
import 'package:ppvpn_flutter/src/rust/frb_generated.dart';
import 'package:ppvpn_flutter/strings.dart';

class DiagnosticsApi implements RustLibApi {
  int calls = 0;
  @override
  Future<String> crateApiDiagnoseReachability() async {
    calls++;
    return 'API DNS recovery enabled for this session';
  }

  @override
  dynamic noSuchMethod(Invocation invocation) => super.noSuchMethod(invocation);
}

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();
  final api = DiagnosticsApi();
  RustLib.initMock(api: api);

  for (final phone in [false, true]) {
    testWidgets('signed-out ${phone ? 'phone' : 'desktop'} can explicitly diagnose and retry', (tester) async {
      tester.view.physicalSize = phone ? const Size(320, 844) : const Size(1000, 760);
      tester.view.devicePixelRatio = 1;
      addTearDown(tester.view.resetPhysicalSize);
      addTearDown(tester.view.resetDevicePixelRatio);
      api.calls = 0;
      final app = AppState.fixed(
        rust.UiSnapshot(
          auth: rust.UiAuth.signedOut,
          profile: rust.UiProfile.ready,
          profilePrevious: false,
          phase: rust.UiPhase.off,
          standardReady: false,
          unread: 0,
          routingMode: rust.UiRoutingMode.rules,
          serviceInstalled: false,
          connectionMode: rust.UiConnectionMode.enhanced,
          canTakeOver: false,
          suggestCompatible: false,
          apiUnreachable: false,
          error: const rust.UiError(code: 'NetworkUnreachable', detail: 'offline'),
        ),
      );
      await tester.pumpWidget(
        PrototypeApp(
          notice: phone ? PhoneSignIn(app: app) : SignInPage(app: app),
        ),
      );
      await tester.pumpAndSettle();
      final entry = find.text(S.current('diagnostics'));
      await tester.ensureVisible(entry);
      await tester.pumpAndSettle();
      await tester.tap(entry);
      await tester.pumpAndSettle();
      expect(api.calls, 0);
      expect(find.text(S.current('diagnosticsIntro')), findsOneWidget);
      await tester.tap(find.text(S.current('diagnosticsRun')));
      await tester.pumpAndSettle();
      expect(api.calls, 1);
      expect(find.text('API DNS recovery enabled for this session'), findsOneWidget);
      await tester.ensureVisible(find.text(S.current('close')));
      await tester.pumpAndSettle();
      await tester.tap(find.text(S.current('close')));
      await tester.pumpAndSettle();
      expect(find.text(S.current('tryAgain')), findsOneWidget);
      expect(tester.takeException(), isNull);
    });
  }
}
