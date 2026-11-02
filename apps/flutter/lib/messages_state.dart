// The message center's list (#401): pages of [MessagesState.pageSize]
// from the client, newest first. Kept while the app runs, so the page
// reopens on what it showed and a failed reload keeps the list. A read
// state change shows at once and goes back if the call fails. The unread
// count is the snapshot's (the server's total), not counted here. A
// notification click asks the page to open its message ([requestOpen]).

import 'dart:async';

import 'package:flutter/foundation.dart';

import 'links.dart';
import 'src/rust/api.dart' as rust;

/// What the message center shows.
class MessagesView {
  const MessagesView({
    this.items = const [],
    this.total = 0,
    this.loaded = false,
    this.loading = false,
    this.loadingMore = false,
    this.failed = false,
  });

  /// Loaded so far, newest first.
  final List<rust.UiMessage> items;

  /// All messages of the account.
  final int total;

  /// A page arrived at least once: until then the skeleton shows.
  final bool loaded;

  /// The first page is being (re)loaded.
  final bool loading;
  final bool loadingMore;

  /// The latest load failed: the error banner shows.
  final bool failed;

  bool get hasMore => items.length < total;

  MessagesView copyWith({
    List<rust.UiMessage>? items,
    int? total,
    bool? loaded,
    bool? loading,
    bool? loadingMore,
    bool? failed,
  }) => MessagesView(
    items: items ?? this.items,
    total: total ?? this.total,
    loaded: loaded ?? this.loaded,
    loading: loading ?? this.loading,
    loadingMore: loadingMore ?? this.loadingMore,
    failed: failed ?? this.failed,
  );
}

/// What a notification click opens: inbox message [messageId], else (or
/// when the inbox does not have it) the push itself, read-only.
typedef OpenRequest = ({int? messageId, rust.UiMessage push});

class MessagesState {
  MessagesState({required this.fixed, required this.onError});

  static const pageSize = 20;

  /// The most pages [locate] reads (as the native apps).
  static const searchPages = 10;

  /// Sample data with no client behind it (sample.dart): nothing is
  /// loaded, read changes stay local, links do not open.
  final bool fixed;

  /// A read change failed (the message went back).
  final void Function(String error) onError;

  final ValueNotifier<MessagesView> view = ValueNotifier(const MessagesView());

  int _pages = 0;

  /// Bumped by [reload]: answers to an older load are dropped.
  int _generation = 0;

  /// The failed load was a further page (retry loads it again).
  bool _failedMore = false;

  /// The loads in flight, for [locate] to wait on.
  Future<void>? _reloading;
  Future<void>? _loadingMore;

  OpenRequest? _openRequest;

  /// Counts the notification clicks' requests; the page takes the latest
  /// with [takeOpen].
  final ValueNotifier<int> openRequests = ValueNotifier(0);

  void requestOpen(OpenRequest request) {
    _openRequest = request;
    openRequests.value++;
  }

  /// The latest request, once.
  OpenRequest? takeOpen() {
    final request = _openRequest;
    _openRequest = null;
    return request;
  }

  /// Inbox message [id]'s index in the list: the first page loaded again
  /// (or the load in flight), then further pages, at most [searchPages].
  /// Null when it is not there or a load fails.
  Future<int?> locate(int id) async {
    if (fixed) return null;
    final reloading = _reloading;
    await (view.value.loading && reloading != null ? reloading : reload());
    for (var pages = 1; ; pages++) {
      final current = view.value;
      final index = current.items.indexWhere((message) => message.id == id);
      if (index >= 0) return index;
      if (current.failed || !current.hasMore || pages >= searchPages) return null;
      await loadMore();
    }
  }

  /// Loads the first page again; the list stays while it loads.
  Future<void> reload() => _reloading = _reload();

  Future<void> _reload() async {
    if (fixed) return;
    final generation = ++_generation;
    view.value = view.value.copyWith(loading: true, loadingMore: false, failed: false);
    try {
      final page = await rust.notifications(page: 1, pageSize: pageSize);
      if (generation != _generation) return;
      _pages = 1;
      view.value = MessagesView(items: page.items, total: page.total, loaded: true);
    } catch (error) {
      if (generation != _generation) return;
      debugPrint('notifications: $error');
      _failedMore = false;
      view.value = view.value.copyWith(loading: false, failed: true);
    }
  }

  /// The next page, when there is one and nothing else loads (the one
  /// loading, when that is the next page). Not after a failure (the list's
  /// end would ask again at once): [retry] does.
  Future<void> loadMore() {
    final current = view.value;
    if (current.loadingMore) return _loadingMore ?? Future.value();
    if (fixed || !current.loaded || current.loading || current.failed || !current.hasMore) {
      return Future.value();
    }
    return _loadingMore = _loadMore(current);
  }

  Future<void> _loadMore(MessagesView current) async {
    final generation = _generation;
    view.value = current.copyWith(loadingMore: true);
    try {
      final page = await rust.notifications(page: _pages + 1, pageSize: pageSize);
      if (generation != _generation) return;
      _pages++;
      // New messages shift the pages: skip what is already shown.
      final shown = {for (final message in view.value.items) message.id};
      final items = [...view.value.items, ...page.items.where((message) => !shown.contains(message.id))];
      view.value = view.value.copyWith(
        items: items,
        // An empty page: the total was stale, stop asking.
        total: page.items.isEmpty ? items.length : page.total,
        loadingMore: false,
      );
    } catch (error) {
      if (generation != _generation) return;
      debugPrint('notifications: $error');
      _failedMore = true;
      view.value = view.value.copyWith(loadingMore: false, failed: true);
    }
  }

  /// 「重试」 on the error banner.
  Future<void> retry() {
    if (_failedMore && view.value.loaded) {
      view.value = view.value.copyWith(failed: false);
      return loadMore();
    }
    return reload();
  }

  Future<void> setRead(rust.UiMessage message, bool read) async {
    if (message.read == read) return;
    _replace(message.id, read);
    if (fixed) return;
    try {
      if (read) {
        await rust.markNotificationRead(id: message.id);
      } else {
        await rust.markNotificationUnread(id: message.id);
      }
    } catch (error) {
      _replace(message.id, !read);
      onError('$error');
    }
  }

  Future<void> markAllRead() async {
    final before = view.value.items;
    view.value = view.value.copyWith(items: [for (final message in before) _withRead(message, true)]);
    if (fixed) return;
    try {
      await rust.markAllNotificationsRead();
    } catch (error) {
      view.value = view.value.copyWith(items: before);
      onError('$error');
    }
  }

  /// 「查看详情」: opens the link in the browser and marks the message read.
  void openLink(rust.UiMessage message) {
    final link = message.deepLink;
    if (link == null || !canOpenUrl(link)) return;
    if (!fixed) unawaited(openUrl(link));
    unawaited(setRead(message, true));
  }

  void _replace(int id, bool read) {
    view.value = view.value.copyWith(
      items: [for (final message in view.value.items) message.id == id ? _withRead(message, read) : message],
    );
  }

  static rust.UiMessage _withRead(rust.UiMessage message, bool read) => rust.UiMessage(
    id: message.id,
    title: message.title,
    content: message.content,
    category: message.category,
    severity: message.severity,
    deepLink: message.deepLink,
    read: read,
    createdAt: message.createdAt,
  );
}
