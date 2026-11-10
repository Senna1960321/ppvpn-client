import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:forui/forui.dart';
import 'package:ppvpn_flutter/app_state.dart';
import 'package:ppvpn_flutter/phone/nodes.dart';
import 'package:ppvpn_flutter/sample.dart';
import 'package:ppvpn_flutter/settings.dart';
import 'package:ppvpn_flutter/src/rust/api.dart';
import 'package:ppvpn_flutter/strings.dart';
import 'package:ppvpn_flutter/theme.dart';

UiNode node(String id, String name, String? region) => UiNode(
  id: id,
  name: name,
  exitRegion: region,
  exitCountryCode: 'JP',
  entryLabel: null,
  entryKey: 'standard',
  udp: true,
  lines: const [],
);

Widget screen(AppState app) => WidgetsApp(
  color: Tokens.light.bg,
  locale: const Locale('en'),
  localizationsDelegates: FLocalizations.localizationsDelegates,
  supportedLocales: FLocalizations.supportedLocales,
  pageRouteBuilder: <T>(settings, builder) => PageRouteBuilder<T>(
    settings: settings,
    pageBuilder: (context, _, _) => builder(context),
  ),
  home: FTheme(
    data: Tokens.light.forui,
    child: DefaultTextStyle(
      style: Tokens.light.body,
      child: PhoneNodes(app: app),
    ),
  ),
);

void main() {
  late AppState app;
  late UiLanguage language;
  final hongKong = node('hk', 'Hong Kong node', 'Hong Kong');
  final tokyo = node('jp', 'Tokyo node', 'Japan');
  final singapore = node('sg', 'Singapore node', 'Singapore');

  setUp(() {
    language = Settings.language.value;
    Settings.language.value = UiLanguage.english;
    app = sampleState();
    app.nodes.value = [hongKong, tokyo];
  });
  tearDown(() => Settings.language.value = language);

  Future<void> selectHongKong(WidgetTester tester) async {
    tester.view.physicalSize = const Size(400, 900);
    tester.view.devicePixelRatio = 1;
    addTearDown(tester.view.resetPhysicalSize);
    addTearDown(tester.view.resetDevicePixelRatio);
    await tester.pumpWidget(screen(app));
    await tester.tap(find.text('Hong Kong').first);
    await tester.pumpAndSettle();
    expect(find.text('Hong Kong node'), findsOneWidget);
    expect(find.text('Tokyo node'), findsNothing);
  }

  testWidgets('removed region clears before single-region controls disappear', (
    tester,
  ) async {
    await selectHongKong(tester);
    app.nodes.value = [tokyo];
    await tester.pumpAndSettle();
    expect(find.text(S.current('allRegions')), findsNothing);
    expect(find.text('Tokyo node'), findsOneWidget);
    expect(find.text(S.current('noMatches')), findsNothing);
  });

  testWidgets('removed region shows all remaining regions', (tester) async {
    await selectHongKong(tester);
    app.nodes.value = [tokyo, singapore];
    await tester.pumpAndSettle();
    expect(find.text('Tokyo node'), findsOneWidget);
    expect(find.text('Singapore node'), findsOneWidget);
  });

  testWidgets('valid selected region survives a profile refresh', (
    tester,
  ) async {
    await selectHongKong(tester);
    app.nodes.value = [hongKong, tokyo, singapore];
    await tester.pumpAndSettle();
    expect(find.text('Hong Kong node'), findsOneWidget);
    expect(find.text('Tokyo node'), findsNothing);
    expect(find.text('Singapore node'), findsNothing);
  });

  testWidgets('empty loading state clears a removed region when data returns', (
    tester,
  ) async {
    await selectHongKong(tester);
    app.nodes.value = [];
    await tester.pump();
    app.nodes.value = [tokyo];
    await tester.pumpAndSettle();
    expect(find.text('Tokyo node'), findsOneWidget);
  });

  testWidgets('empty loading state preserves a still-valid region', (
    tester,
  ) async {
    await selectHongKong(tester);
    app.nodes.value = [];
    await tester.pump();
    app.nodes.value = [hongKong, tokyo];
    await tester.pumpAndSettle();
    expect(find.text('Hong Kong node'), findsOneWidget);
    expect(find.text('Tokyo node'), findsNothing);
  });

  testWidgets('nodes without region metadata remain reachable', (tester) async {
    await selectHongKong(tester);
    app.nodes.value = [
      node('unknown', 'Unknown region node', null),
      node('empty', 'Empty region node', ''),
    ];
    await tester.pumpAndSettle();
    expect(find.text('Unknown region node'), findsOneWidget);
    expect(find.text('Empty region node'), findsOneWidget);
  });

  testWidgets('clearing a removed region preserves the search query', (
    tester,
  ) async {
    await selectHongKong(tester);
    await tester.enterText(find.byType(EditableText), 'Tokyo');
    await tester.pumpAndSettle();
    expect(find.text(S.current('noMatches')), findsOneWidget);
    app.nodes.value = [tokyo, singapore];
    await tester.pumpAndSettle();
    expect(find.text('Tokyo node'), findsOneWidget);
    expect(find.text('Singapore node'), findsNothing);
    expect(
      tester.widget<EditableText>(find.byType(EditableText)).controller.text,
      'Tokyo',
    );
  });

  testWidgets('a removed region is not silently selected again on return', (
    tester,
  ) async {
    await selectHongKong(tester);
    app.nodes.value = [tokyo];
    await tester.pumpAndSettle();
    app.nodes.value = [hongKong, tokyo];
    await tester.pumpAndSettle();
    expect(find.text('Hong Kong node'), findsOneWidget);
    expect(find.text('Tokyo node'), findsOneWidget);
  });
}
