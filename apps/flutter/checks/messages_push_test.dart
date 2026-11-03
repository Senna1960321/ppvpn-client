import 'dart:async';

import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ppvpn_flutter/app_state.dart';
import 'package:ppvpn_flutter/main.dart';
import 'package:ppvpn_flutter/messages_state.dart';
import 'package:ppvpn_flutter/pages/messages.dart';
import 'package:ppvpn_flutter/phone/phone.dart';
import 'package:ppvpn_flutter/src/rust/api.dart' as rust;
import 'package:ppvpn_flutter/src/rust/frb_generated.dart';

class InboxApi implements RustLibApi {
  late Future<rust.UiMessagePage> Function() load;
  Future<void> Function() mark = () async {};

  @override
  Future<rust.UiMessagePage> crateApiNotifications({required int page, required int pageSize}) => load();
  @override
  Future<void> crateApiMarkAllNotificationsRead() => mark();
  @override
  Future<void> crateApiMarkNotificationRead({required int id}) => mark();
  @override
  dynamic noSuchMethod(Invocation invocation) => super.noSuchMethod(invocation);
}

rust.UiMessage message(int id) => rust.UiMessage(
  id: id,
  title: 'Private message $id',
  content: 'Private body $id',
  category: rust.UiMessageCategory.other,
  severity: rust.UiMessageSeverity.normal,
  read: false,
  createdAt: '2026-11-01T00:00:00Z',
);

rust.UiSnapshot snapshot(rust.UiAuth auth, String? name) => rust.UiSnapshot(
  auth: auth,
  accountName: name,
  profile: rust.UiProfile.ready,
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
);

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();
  final api = InboxApi();
  RustLib.initMock(api: api);
  late MessagesState messages;

  setUp(() {
    messages = MessagesState(fixed: false, onError: (_) {});
    api.load = () async => rust.UiMessagePage(items: [message(42)], total: 1);
    api.mark = () async {};
  });

  test('failed reload cannot locate a notification in stale cached bodies', () async {
    await messages.reload();
    api.load = () async => throw StateError('offline');
    expect(await messages.locate(42), isNull);
  });

  test('account reset drops old list, tap and an in-flight load', () async {
    await messages.reload();
    messages.requestOpen((messageId: 42, push: message(42)));
    final pending = Completer<rust.UiMessagePage>();
    api.load = () => pending.future;
    final locating = messages.locate(42);
    messages.clear();
    pending.complete(rust.UiMessagePage(items: [message(42)], total: 1));
    expect(await locating, isNull);
    expect(messages.view.value.items, isEmpty);
    expect(messages.takeOpen(), (messageId: null, push: null));
    api.load = () async => throw StateError('offline in next account');
    expect(await messages.locate(42), isNull);
    expect(messages.view.value.items, isEmpty);
  });

  test('failed read update from previous account cannot restore its list', () async {
    await messages.reload();
    final pending = Completer<void>();
    api.mark = () => pending.future;
    final marking = messages.markAllRead();
    messages.clear();
    api.load = () async => rust.UiMessagePage(items: [message(99)], total: 1);
    await messages.reload();
    pending.completeError(StateError('old account request failed'));
    await marking;
    expect(messages.view.value.items.single.id, 99);
  });

  test('AppState clears messages on sign-out and direct account changes', () {
    final app = AppState.fixed(snapshot(rust.UiAuth.signedIn, 'Alice'));
    app.messages.view.value = MessagesView(items: [message(42)], loaded: true);
    app.snapshot.value = snapshot(rust.UiAuth.signedOut, null);
    expect(app.messages.view.value.items, isEmpty);
    app.snapshot.value = snapshot(rust.UiAuth.signedIn, 'Alice');
    app.messages.view.value = MessagesView(items: [message(42)], loaded: true);
    app.snapshot.value = snapshot(rust.UiAuth.signedIn, 'Bob');
    expect(app.messages.view.value.items, isEmpty);
  });

  testWidgets('notification taps reuse an already open message page', (tester) async {
    tester.view.physicalSize = const Size(540, 960);
    tester.view.devicePixelRatio = 1;
    addTearDown(tester.view.resetPhysicalSize);
    addTearDown(tester.view.resetDevicePixelRatio);
    final app = AppState.fixed(snapshot(rust.UiAuth.signedIn, "Alice"));
    app.messages.view.value = const MessagesView(loaded: true);
    await tester.pumpWidget(PrototypeApp(app: app));
    await tester.pumpAndSettle();
    expect(find.byType(PhoneShell), findsOneWidget);
    app.showPage(WindowPage.messages);
    await tester.pumpAndSettle();
    final first = tester.state(find.byType(MessagesPage));
    for (final id in [42, 43]) {
      app.messages.requestOpen((messageId: id, push: message(id)));
      app.showPage(WindowPage.messages);
      await tester.pumpAndSettle();
      expect(find.byType(MessagesPage, skipOffstage: false), findsOneWidget);
      expect(tester.state(find.byType(MessagesPage)), same(first));
      expect(find.text('Private body $id'), findsOneWidget);
    }
    app.messages.requestOpen((messageId: null, push: null));
    app.showPage(WindowPage.messages);
    await tester.pumpAndSettle();
    expect(find.text('Private body 43'), findsNothing);
    await tester.pumpWidget(const SizedBox());
  });
}
