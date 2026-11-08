import 'dart:async';

import 'package:flutter_test/flutter_test.dart';
import 'package:ppvpn_flutter/messages_state.dart';
import 'package:ppvpn_flutter/src/rust/api.dart';
import 'package:ppvpn_flutter/src/rust/frb_generated.dart';

UiMessage message(int id, {bool read = false, String title = 'original'}) =>
    UiMessage(
      id: id,
      title: title,
      content: 'message $id',
      category: UiMessageCategory.other,
      severity: UiMessageSeverity.normal,
      read: read,
      createdAt: '2026-11-08T00:00:00Z',
    );

class FakeApi implements RustLibApi {
  final all = <Completer<void>>[];
  final single = <({int id, bool read, Completer<void> response})>[];
  final pages = <int>[];
  bool failLoad = false;
  bool refreshed = false;

  @override
  Future<UiMessagePage> crateApiNotifications({
    required int page,
    required int pageSize,
  }) async {
    pages.add(page);
    if (failLoad) throw StateError('load failed');
    return UiMessagePage(
      items: page > 2
          ? []
          : List.generate(
              20,
              (index) => message(
                (page - 1) * 20 + index + 1,
                read: refreshed,
                title: refreshed ? 'refreshed' : 'original',
              ),
            ),
      total: 40,
    );
  }

  @override
  Future<void> crateApiMarkAllNotificationsRead() {
    final response = Completer<void>();
    all.add(response);
    return response.future;
  }

  Future<void> _single(int id, bool read) {
    final response = Completer<void>();
    single.add((id: id, read: read, response: response));
    return response.future;
  }

  @override
  Future<void> crateApiMarkNotificationRead({required int id}) =>
      _single(id, true);

  @override
  Future<void> crateApiMarkNotificationUnread({required int id}) =>
      _single(id, false);

  @override
  dynamic noSuchMethod(Invocation invocation) => super.noSuchMethod(invocation);
}

Future<void> fail(Completer<void> response, Future<void> operation) async {
  response.completeError(StateError('mark failed'));
  await operation;
}

Future<void> succeed(Completer<void> response, Future<void> operation) async {
  response.complete();
  await operation;
}

void main() {
  late FakeApi api;
  late MessagesState messages;
  late List<String> errors;

  setUp(() {
    api = FakeApi();
    RustLib.initMock(api: api);
    errors = [];
    messages = MessagesState(fixed: false, onError: errors.add);
  });

  tearDown(RustLib.dispose);

  test(
    'failed mark-all preserves new pages and their pagination position',
    () async {
      await messages.reload();
      final marking = messages.markAllRead();
      await messages.loadMore();
      expect(messages.view.value.items, hasLength(40));
      await fail(api.all.single, marking);
      expect(
        messages.view.value.items.map((item) => item.id),
        List.generate(40, (i) => i + 1),
      );
      expect(messages.view.value.items.every((item) => !item.read), isTrue);
      await messages.loadMore();
      expect(api.pages, [1, 2]);
      expect(messages.view.value.total, 40);
      expect(messages.view.value.hasMore, isFalse);
      expect(errors, hasLength(1));
    },
  );

  test(
    'a reload with the same IDs supersedes an older mark-all rollback',
    () async {
      await messages.reload();
      final marking = messages.markAllRead();
      api.refreshed = true;
      await messages.reload();
      await fail(api.all.single, marking);
      expect(
        messages.view.value.items.every(
          (item) => item.title == 'refreshed' && item.read,
        ),
        isTrue,
      );
    },
  );

  test('a failed reload still lets a pending read change roll back', () async {
    await messages.reload();
    final marking = messages.markAllRead();
    api.failLoad = true;
    await messages.reload();
    await fail(api.all.single, marking);
    expect(messages.view.value.failed, isTrue);
    expect(messages.view.value.items.every((item) => !item.read), isTrue);
  });

  test(
    'a later successful mark-all survives an older failed mark-all',
    () async {
      await messages.reload();
      final first = messages.markAllRead();
      final second = messages.markAllRead();
      await succeed(api.all[1], second);
      await fail(api.all[0], first);
      expect(messages.view.value.items.every((item) => item.read), isTrue);
    },
  );

  for (final firstFailsFirst in [true, false]) {
    test(
      'two failed mark-all calls restore the baseline, first failure first=$firstFailsFirst',
      () async {
        await messages.reload();
        final first = messages.markAllRead();
        final second = messages.markAllRead();
        if (firstFailsFirst) {
          await fail(api.all[0], first);
          expect(messages.view.value.items.every((item) => item.read), isTrue);
          await fail(api.all[1], second);
        } else {
          await fail(api.all[1], second);
          expect(messages.view.value.items.every((item) => item.read), isTrue);
          await fail(api.all[0], first);
        }
        expect(messages.view.value.items.every((item) => !item.read), isTrue);
      },
    );
  }

  test('failed single and bulk changes restore the baseline in user operation order', () async {
    await messages.reload();
    final bulk = messages.markAllRead();
    final single = messages.setRead(messages.view.value.items.first, false);
    await fail(api.all.single, bulk);
    expect(messages.view.value.items.first.read, isFalse);
    await fail(api.single.single.response, single);
    expect(messages.view.value.items.first.read, isFalse);
  });

  test(
    'later successful single changes survive an earlier bulk rollback',
    () async {
      await messages.reload();
      final bulk = messages.markAllRead();
      final unread = messages.setRead(messages.view.value.items.first, false);
      await succeed(api.single[0].response, unread);
      final read = messages.setRead(messages.view.value.items.first, true);
      await succeed(api.single[1].response, read);
      await fail(api.all.single, bulk);
      expect(messages.view.value.items.first.read, isTrue);
      expect(
        messages.view.value.items.skip(1).every((item) => !item.read),
        isTrue,
      );
    },
  );

  test(
    'a successful earlier change is the baseline for a later failed change',
    () async {
      await messages.reload();
      final bulk = messages.markAllRead();
      final single = messages.setRead(messages.view.value.items.first, false);
      await succeed(api.all.single, bulk);
      expect(messages.view.value.items.first.read, isFalse);
      await fail(api.single.single.response, single);
      expect(messages.view.value.items.every((item) => item.read), isTrue);
    },
  );

  test('old completions do not replace changes made after a reload', () async {
    await messages.reload();
    final old = messages.markAllRead();
    api.refreshed = true;
    await messages.reload();
    final current = messages.setRead(messages.view.value.items.first, false);
    await fail(api.all.single, old);
    expect(messages.view.value.items.first.title, 'refreshed');
    expect(messages.view.value.items.first.read, isFalse);
    await fail(api.single.single.response, current);
    expect(messages.view.value.items.first.read, isTrue);
  });

  test('successful mark-all keeps later pages and a subsequent rollback uses its result', () async {
    await messages.reload();
    final bulk = messages.markAllRead();
    await messages.loadMore();
    await succeed(api.all.single, bulk);
    expect(messages.view.value.items, hasLength(40));
    final single = messages.setRead(messages.view.value.items.first, false);
    await fail(api.single.single.response, single);
    expect(messages.view.value.items.first.read, isTrue);
    expect(messages.view.value.items.last.read, isFalse);
  });

  test(
    'a notification outside the loaded list still sends its read action',
    () async {
      final marking = messages.setRead(message(99), true);
      expect(api.single.single.id, 99);
      expect(api.single.single.read, isTrue);
      await succeed(api.single.single.response, marking);
      expect(messages.view.value.items, isEmpty);
    },
  );

  test('fixed sample read changes stay local', () async {
    final sample = MessagesState(fixed: true, onError: errors.add);
    sample.view.value = MessagesView(
      items: [message(1)],
      total: 1,
      loaded: true,
    );
    await sample.markAllRead();
    await sample.setRead(sample.view.value.items.single, false);
    expect(sample.view.value.items.single.read, isFalse);
    expect(api.all, isEmpty);
    expect(api.single, isEmpty);
  });
}
