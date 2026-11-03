// Message center (handoff §8), a page of the main window as decided on
// #387, opened from the bell. The list: 「N 条未读」 / 「全部已读」 with
// 「全部标为已读」, then rows with the severity bar (critical, important),
// the unread dot, the type icon, title and relative time, type · severity,
// two lines of body, 「查看详情 ↗」 when there is a link and 「标记为已读」
// on hover. Pages of 20 load near the list's end. A skeleton until the
// first page, an empty state, an error banner over the kept list. The
// detail replaces the list in the same page (back, 「1 / 46」, previous and
// next, the full text, 「标为未读」) and marks the message read; Esc goes
// back, the list keeps its scroll position. A notification click opens
// its message's detail, or the push itself read-only when the inbox does
// not have it (MessagesState.requestOpen). Not in the prototype: the row's
// context menu.

import 'dart:async';

import 'package:flutter/services.dart';
import 'package:flutter/widgets.dart';
import 'package:forui/forui.dart';

import '../app_state.dart';
import '../messages_state.dart';
import '../src/rust/api.dart' as rust;
import '../strings.dart';
import '../theme.dart';
import '../widgets.dart';

class MessagesPage extends StatefulWidget {
  const MessagesPage({super.key, required this.app});

  final AppState app;

  @override
  State<MessagesPage> createState() => _MessagesPageState();
}

class _MessagesPageState extends State<MessagesPage> {
  final _scroll = ScrollController();
  final _selection = FocusNode();

  /// The message the detail shows, else the list.
  int? _open;

  /// A clicked notification's push the detail shows read-only, else the
  /// list.
  rust.UiMessage? _push;

  /// Relative times move on.
  Timer? _tick;

  MessagesState get messages => widget.app.messages;

  @override
  void initState() {
    super.initState();
    _scroll.addListener(_nearEnd);
    _tick = Timer.periodic(const Duration(minutes: 1), (_) => setState(() {}));
    messages.openRequests.addListener(_onOpenRequest);
    unawaited(messages.reload());
    _onOpenRequest();
  }

  @override
  void dispose() {
    messages.openRequests.removeListener(_onOpenRequest);
    _tick?.cancel();
    _scroll.dispose();
    _selection.dispose();
    super.dispose();
  }

  void _nearEnd() {
    if (_scroll.hasClients && _scroll.position.extentAfter <= Tokens.loadMoreDistance) {
      unawaited(messages.loadMore());
    }
  }

  void _show(int index) {
    final items = messages.view.value.items;
    if (index < 0 || index >= items.length) return;
    setState(() {
      _open = index;
      _push = null;
    });
    unawaited(messages.setRead(items[index], true));
    // Next is there before it is pressed.
    if (index >= items.length - 2) unawaited(messages.loadMore());
  }

  void _back() => setState(() {
    _open = null;
    _push = null;
  });

  void _onOpenRequest() => unawaited(_openRequested());

  /// A notification click's message: found in the list (loading pages as
  /// needed), else its push, read-only.
  Future<void> _openRequested() async {
    final request = messages.takeOpen();
    if (request == null) return;
    final account = messages.accountGeneration;
    final sequence = messages.openRequests.value;
    final id = request.messageId;
    final index = id == null ? null : await messages.locate(id);
    if (!mounted || account != messages.accountGeneration || sequence != messages.openRequests.value) return;
    if (index != null) {
      _show(index);
    } else {
      setState(() {
        _open = null;
        _push = request.push;
      });
    }
  }

  @override
  Widget build(BuildContext context) => ValueListenableBuilder(
    valueListenable: widget.app.snapshot,
    builder: (context, snapshot, _) => ValueListenableBuilder(
      valueListenable: messages.view,
      builder: (context, view, _) {
        // A short list does not scroll: load on until it does or ends.
        WidgetsBinding.instance.addPostFrameCallback((_) {
          if (mounted) _nearEnd();
        });
        final unread = widget.app.fixed ? view.items.where((message) => !message.read).length : snapshot.unread;
        final open = _open != null && _open! < view.items.length ? _open : null;
        final push = _push;
        return Padding(
          padding: Tokens.messagesPadding,
          child: Center(
            child: ConstrainedBox(
              constraints: const BoxConstraints(maxWidth: Tokens.pageMaxWidth),
              child: IndexedStack(
                index: open == null && push == null ? 0 : 1,
                sizing: StackFit.expand,
                children: [
                  _List(
                    view: view,
                    unread: unread,
                    scroll: _scroll,
                    messages: messages,
                    onOpen: _show,
                  ),
                  if (open != null)
                    _Detail(
                      message: view.items[open],
                      position: (index: open, total: view.total > view.items.length ? view.total : view.items.length),
                      selection: _selection,
                      onBack: _back,
                      onPrevious: open > 0 ? () => _show(open - 1) : null,
                      onNext: open + 1 < view.items.length ? () => _show(open + 1) : null,
                      onToggleRead: () => unawaited(messages.setRead(view.items[open], !view.items[open].read)),
                      onLink: () => messages.openLink(view.items[open]),
                    )
                  else if (push != null)
                    _Detail(message: push, selection: _selection, onBack: _back)
                  else
                    const SizedBox(),
                ],
              ),
            ),
          ),
        );
      },
    ),
  );
}

// --- the list ---------------------------------------------------------------

class _List extends StatelessWidget {
  const _List({
    required this.view,
    required this.unread,
    required this.scroll,
    required this.messages,
    required this.onOpen,
  });

  final MessagesView view;
  final int unread;
  final ScrollController scroll;
  final MessagesState messages;
  final ValueChanged<int> onOpen;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final s = S.current;
    final Widget body;
    if (!view.loaded) {
      body = view.failed ? const SizedBox() : const _Skeleton();
    } else if (view.items.isEmpty) {
      body = const _Empty();
    } else {
      final now = DateTime.now();
      body = ListView.builder(
        controller: scroll,
        itemCount: view.items.length + 1,
        itemBuilder: (context, index) => index < view.items.length
            ? _Row(
                message: view.items[index],
                first: index == 0,
                now: now,
                onOpen: () => onOpen(index),
                onMarkRead: () => unawaited(messages.setRead(view.items[index], true)),
                onLink: () => messages.openLink(view.items[index]),
              )
            : _ListEnd(view: view),
      );
    }
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        SizedBox(
          height: Tokens.messagesBarHeight,
          child: Row(
            children: [
              Expanded(
                child: Semantics(
                  liveRegion: true,
                  child: Text(
                    unread > 0 ? s('unreadShort', {'n': '$unread'}) : s('allRead'),
                    maxLines: 1,
                    overflow: TextOverflow.ellipsis,
                    style: t.sub,
                  ),
                ),
              ),
              Button(
                kind: ButtonKind.secondary,
                height: 28,
                fontSize: 12,
                icon: FLucideIcons.checkCheck,
                label: s('markAllRead'),
                onPress: unread > 0 && view.loaded ? () => unawaited(messages.markAllRead()) : null,
              ),
            ],
          ),
        ),
        const SizedBox(height: 4),
        Expanded(
          child: CardBox(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                if (view.failed) _ErrorBanner(onRetry: () => unawaited(messages.retry())),
                Expanded(child: body),
              ],
            ),
          ),
        ),
      ],
    );
  }
}

/// The severity's colour and its soft fill; none below important.
(Color, Color)? _tone(Tokens t, rust.UiMessageSeverity severity) => switch (severity) {
  rust.UiMessageSeverity.critical => (t.bad, t.badSoft),
  rust.UiMessageSeverity.important => (t.warn, t.warnSoft),
  _ => null,
};

String? _severityLabel(rust.UiMessageSeverity severity) => switch (severity) {
  rust.UiMessageSeverity.critical => S.current('sev_critical'),
  rust.UiMessageSeverity.important => S.current('sev_important'),
  _ => null,
};

String _typeLabel(rust.UiMessageCategory category) => S.current(switch (category) {
  rust.UiMessageCategory.subscriptionExpiring => 't_expiring',
  rust.UiMessageCategory.subscriptionExpired => 't_expired',
  rust.UiMessageCategory.billing => 't_bill',
  rust.UiMessageCategory.order => 't_order',
  rust.UiMessageCategory.route => 't_route',
  rust.UiMessageCategory.announcement => 't_broadcast',
  rust.UiMessageCategory.other => 't_other',
});

IconData _typeIcon(rust.UiMessageCategory category) => switch (category) {
  rust.UiMessageCategory.subscriptionExpiring => FLucideIcons.calendarClock,
  rust.UiMessageCategory.subscriptionExpired => FLucideIcons.calendarX,
  rust.UiMessageCategory.billing => FLucideIcons.receipt,
  rust.UiMessageCategory.order => FLucideIcons.shoppingBag,
  rust.UiMessageCategory.route => FLucideIcons.route,
  rust.UiMessageCategory.announcement => FLucideIcons.megaphone,
  rust.UiMessageCategory.other => FLucideIcons.info,
};

/// The 32 round type icon, in the severity's colour on its soft fill.
class _TypeIcon extends StatelessWidget {
  const _TypeIcon({required this.message});

  final rust.UiMessage message;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final tone = _tone(t, message.severity);
    return ExcludeSemantics(
      child: Container(
        width: Tokens.rowIconSize,
        height: Tokens.rowIconSize,
        decoration: BoxDecoration(color: tone?.$2 ?? t.sunk, shape: BoxShape.circle),
        child: Icon(_typeIcon(message.category), size: 16, color: tone?.$1 ?? t.t2),
      ),
    );
  }
}

/// The severity bar: 3 wide, rounded on the right; clear below important.
class _SeverityBar extends StatelessWidget {
  const _SeverityBar({required this.severity, this.radius = const BorderRadius.horizontal(right: Radius.circular(2))});

  final rust.UiMessageSeverity severity;
  final BorderRadius radius;

  @override
  Widget build(BuildContext context) {
    final tone = _tone(Tokens.of(context), severity);
    return Container(
      width: Tokens.severityBarWidth,
      decoration: BoxDecoration(color: tone?.$1, borderRadius: radius),
    );
  }
}

class _Row extends StatelessWidget {
  const _Row({
    required this.message,
    required this.first,
    required this.now,
    required this.onOpen,
    required this.onMarkRead,
    required this.onLink,
  });

  final rust.UiMessage message;
  final bool first;
  final DateTime now;
  final VoidCallback onOpen;
  final VoidCallback onMarkRead;
  final VoidCallback onLink;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final s = S.current;
    final unread = !message.read;
    final tone = _tone(t, message.severity);
    final severity = _severityLabel(message.severity);
    final type = _typeLabel(message.category);
    final time = relativeTime(_parse(message.createdAt), now);
    return Pressable(
      radius: 0,
      onPress: onOpen,
      label: [if (unread) s('unread'), message.title, type, ?severity, time].join(', '),
      builder: (context, hovered) => AnimatedContainer(
        duration: Tokens.motion(context, Tokens.motionFast),
        curve: Tokens.motionFastCurve,
        decoration: BoxDecoration(
          color: hovered ? t.sunk : null,
          border: first ? null : Border(top: BorderSide(color: t.hair)),
        ),
        child: Stack(
          children: [
            Positioned(left: 0, top: 8, bottom: 8, child: _SeverityBar(severity: message.severity)),
            Padding(
              padding: Tokens.messageRowPadding,
              child: Row(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  SizedBox(
                    width: Tokens.unreadDot,
                    height: Tokens.rowIconSize,
                    child: Center(
                      child: AnimatedContainer(
                        duration: Tokens.motion(context, Tokens.motionBase),
                        curve: Tokens.motionCurve,
                        width: Tokens.unreadDot,
                        height: Tokens.unreadDot,
                        decoration: BoxDecoration(color: unread ? t.accent : null, shape: BoxShape.circle),
                      ),
                    ),
                  ),
                  const SizedBox(width: 8),
                  _TypeIcon(message: message),
                  const SizedBox(width: 10),
                  Expanded(
                    child: Column(
                      crossAxisAlignment: CrossAxisAlignment.stretch,
                      children: [
                        Row(
                          crossAxisAlignment: CrossAxisAlignment.baseline,
                          textBaseline: TextBaseline.alphabetic,
                          children: [
                            Expanded(
                              child: Text(
                                message.title,
                                maxLines: 1,
                                overflow: TextOverflow.ellipsis,
                                style: t.rowTitle.copyWith(fontWeight: unread ? FontWeight.w700 : FontWeight.w400),
                              ),
                            ),
                            const SizedBox(width: 8),
                            Text(time, maxLines: 1, style: t.note.copyWith(fontSize: 11, color: t.t2)),
                          ],
                        ),
                        const SizedBox(height: 2),
                        Text.rich(
                          TextSpan(
                            children: [
                              TextSpan(text: type),
                              if (severity != null) ...[
                                const TextSpan(text: ' · '),
                                TextSpan(
                                  text: severity,
                                  style: TextStyle(color: tone?.$1, fontWeight: FontWeight.w600),
                                ),
                              ],
                            ],
                          ),
                          maxLines: 1,
                          overflow: TextOverflow.ellipsis,
                          style: t.note.copyWith(fontSize: 11, color: t.t2),
                        ),
                        const SizedBox(height: 4),
                        Text(message.content, maxLines: 2, overflow: TextOverflow.ellipsis, style: t.preview),
                        const SizedBox(height: 6),
                        SizedBox(
                          height: Tokens.smallButtonHeight,
                          child: Row(
                            children: [
                              if (message.deepLink != null)
                                Button(
                                  kind: ButtonKind.secondary,
                                  height: Tokens.smallButtonHeight,
                                  fontSize: 12,
                                  label: '${s('viewDetails')} ↗',
                                  onPress: onLink,
                                ),
                              const Spacer(),
                              if (unread && hovered)
                                Button(
                                  kind: ButtonKind.text,
                                  height: Tokens.smallButtonHeight,
                                  fontSize: 12,
                                  icon: FLucideIcons.check,
                                  label: s('markRead'),
                                  onPress: onMarkRead,
                                ),
                              const SizedBox(width: 4),
                              Icon(FLucideIcons.chevronRight, size: 16, color: t.t3),
                            ],
                          ),
                        ),
                      ],
                    ),
                  ),
                ],
              ),
            ),
          ],
        ),
      ),
    );
  }
}

/// Under the list: 「正在加载…」 while a page loads, 「没有更多消息了」 at
/// the end.
class _ListEnd extends StatelessWidget {
  const _ListEnd({required this.view});

  final MessagesView view;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final s = S.current;
    return SizedBox(
      height: Tokens.listFooterHeight,
      child: Center(
        child: view.loadingMore
            ? Row(
                mainAxisSize: MainAxisSize.min,
                children: [
                  Spinner(color: t.t2, size: 12),
                  const SizedBox(width: 6),
                  Text(s('loadingMore'), style: t.note.copyWith(color: t.t2)),
                ],
              )
            : view.hasMore
            ? const SizedBox()
            : Text(s('noMore'), style: t.note.copyWith(fontSize: 11)),
      ),
    );
  }
}

/// 「消息加载失败 · 请检查网络连接后重试。」 + 「重试」 over the list.
class _ErrorBanner extends StatelessWidget {
  const _ErrorBanner({required this.onRetry});

  final VoidCallback onRetry;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final s = S.current;
    return Semantics(
      liveRegion: true,
      container: true,
      child: Container(
        padding: const EdgeInsets.fromLTRB(14, 8, 10, 8),
        decoration: BoxDecoration(
          color: t.badSoft,
          border: Border(bottom: BorderSide(color: t.hair)),
        ),
        child: Row(
          children: [
            Icon(FLucideIcons.circleAlert, size: 16, color: t.bad),
            const SizedBox(width: 8),
            Text(s('msgErrT'), style: t.note.copyWith(fontWeight: FontWeight.w600, color: t.t1)),
            const SizedBox(width: 8),
            Expanded(
              child: Text(s('msgErrD'), maxLines: 1, overflow: TextOverflow.ellipsis, style: t.note.copyWith(color: t.t2)),
            ),
            const SizedBox(width: 8),
            Button(
              kind: ButtonKind.secondary,
              height: Tokens.smallButtonHeight,
              fontSize: 12,
              label: s('retry'),
              onPress: onRetry,
            ),
          ],
        ),
      ),
    );
  }
}

/// First load: six breathing placeholder rows.
class _Skeleton extends StatefulWidget {
  const _Skeleton();

  @override
  State<_Skeleton> createState() => _SkeletonState();
}

class _SkeletonState extends State<_Skeleton> with SingleTickerProviderStateMixin {
  late final AnimationController _pulse = AnimationController(vsync: this, duration: Tokens.skeletonPulse)
    ..repeat(reverse: true);
  late final Animation<double> _opacity = Tween<double>(
    begin: 0.45,
    end: 1,
  ).animate(CurvedAnimation(parent: _pulse, curve: Curves.easeInOut));

  @override
  void dispose() {
    _pulse.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    Widget bar(double height, double widthFactor) => FractionallySizedBox(
      alignment: Alignment.centerLeft,
      widthFactor: widthFactor,
      child: Container(
        height: height,
        decoration: BoxDecoration(color: t.line, borderRadius: BorderRadius.circular(4)),
      ),
    );
    const widths = [(0.62, 0.34), (0.48, 0.28), (0.70, 0.30), (0.55, 0.38), (0.66, 0.26), (0.44, 0.32)];
    return Semantics(
      label: S.current('loadingMore'),
      child: ClipRect(
        child: FadeTransition(
          opacity: _opacity,
          child: Column(
            children: [
              for (var i = 0; i < Tokens.skeletonRows; i++)
                Container(
                  padding: Tokens.messageRowPadding.copyWith(left: Tokens.messageRowPadding.left + Tokens.unreadDot + 8),
                  decoration: BoxDecoration(
                    border: i == 0 ? null : Border(top: BorderSide(color: t.hair)),
                  ),
                  child: Row(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      Container(
                        width: Tokens.rowIconSize,
                        height: Tokens.rowIconSize,
                        decoration: BoxDecoration(color: t.line, shape: BoxShape.circle),
                      ),
                      const SizedBox(width: 10),
                      Expanded(
                        child: Column(
                          children: [
                            const SizedBox(height: 2),
                            bar(11, widths[i % widths.length].$1),
                            const SizedBox(height: 7),
                            bar(9, widths[i % widths.length].$2),
                            const SizedBox(height: 7),
                            bar(9, 0.55),
                          ],
                        ),
                      ),
                    ],
                  ),
                ),
            ],
          ),
        ),
      ),
    );
  }
}

class _Empty extends StatelessWidget {
  const _Empty();

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final s = S.current;
    return Center(
      child: Padding(
        padding: const EdgeInsets.all(32),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Container(
              width: Tokens.emptyIconSize,
              height: Tokens.emptyIconSize,
              decoration: BoxDecoration(color: t.sunk, shape: BoxShape.circle),
              child: Icon(FLucideIcons.inbox, size: 30, color: t.t2),
            ),
            const SizedBox(height: 14),
            Semantics(
              header: true,
              child: Text(s('msgEmptyT'), style: t.pageTitle.copyWith(fontWeight: FontWeight.w600)),
            ),
            const SizedBox(height: 6),
            ConstrainedBox(
              constraints: const BoxConstraints(maxWidth: 300),
              child: Text(s('msgEmptyD'), style: t.sub, textAlign: TextAlign.center),
            ),
          ],
        ),
      ),
    );
  }
}

// --- the detail -------------------------------------------------------------

/// A message of the list, or with only [message], [selection] and [onBack]
/// a clicked notification's push, read-only: no position, no previous and
/// next, no read toggle.
class _Detail extends StatelessWidget {
  const _Detail({
    required this.message,
    this.position,
    required this.selection,
    required this.onBack,
    this.onPrevious,
    this.onNext,
    this.onToggleRead,
    this.onLink,
  });

  final rust.UiMessage message;

  /// Its place in the list (「1 / 46」).
  final ({int index, int total})? position;

  /// The full text's selection.
  final FocusNode selection;
  final VoidCallback onBack;
  final VoidCallback? onPrevious;
  final VoidCallback? onNext;
  final VoidCallback? onToggleRead;
  final VoidCallback? onLink;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final s = S.current;
    final at = _parse(message.createdAt);
    final now = DateTime.now();
    final time = at == null ? '' : '${absoluteTime(at)} · ${relativeTime(at, now)}';
    return CallbackShortcuts(
      bindings: {const SingleActivator(LogicalKeyboardKey.escape): onBack},
      child: Focus(
        autofocus: true,
        child: CardBox(
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              Container(
                height: Tokens.messagesBarHeight,
                padding: const EdgeInsets.symmetric(horizontal: 8),
                decoration: BoxDecoration(border: Border(bottom: BorderSide(color: t.hair))),
                child: Row(
                  children: [
                    Pressable(
                      onPress: onBack,
                      label: s('backToList'),
                      radius: 6,
                      builder: (context, hovered) => AnimatedContainer(
                        duration: Tokens.motion(context, Tokens.motionFast),
                        curve: Tokens.motionFastCurve,
                        height: 26,
                        padding: const EdgeInsets.fromLTRB(4, 0, 10, 0),
                        decoration: BoxDecoration(
                          color: hovered ? t.accentSoft : null,
                          borderRadius: BorderRadius.circular(6),
                        ),
                        child: Row(
                          mainAxisSize: MainAxisSize.min,
                          children: [
                            Icon(FLucideIcons.chevronLeft, size: 18, color: t.t2),
                            const SizedBox(width: 2),
                            Text(s('msgCenter'), style: t.control.copyWith(color: t.t1)),
                          ],
                        ),
                      ),
                    ),
                    const Spacer(),
                    if (position case final position?) ...[
                      Text('${position.index + 1} / ${position.total}', style: t.note.copyWith(color: t.t2)),
                      const SizedBox(width: 4),
                      Button(
                        kind: ButtonKind.icon,
                        height: 26,
                        icon: FLucideIcons.chevronUp,
                        semanticsLabel: s('prevMsg'),
                        onPress: onPrevious,
                      ),
                      Button(
                        kind: ButtonKind.icon,
                        height: 26,
                        icon: FLucideIcons.chevronDown,
                        semanticsLabel: s('nextMsg'),
                        onPress: onNext,
                      ),
                    ],
                  ],
                ),
              ),
              Expanded(
                child: SingleChildScrollView(
                  padding: Tokens.messageDetailPadding,
                  child: Column(
                    crossAxisAlignment: CrossAxisAlignment.stretch,
                    children: [
                      SelectableRegion(
                        focusNode: selection,
                        selectionControls: emptyTextSelectionControls,
                        child: Column(
                          crossAxisAlignment: CrossAxisAlignment.stretch,
                          children: [
                            IntrinsicHeight(
                              child: Row(
                                crossAxisAlignment: CrossAxisAlignment.stretch,
                                children: [
                                  Padding(
                                    padding: const EdgeInsets.symmetric(vertical: 2),
                                    child: _SeverityBar(severity: message.severity, radius: BorderRadius.circular(2)),
                                  ),
                                  const SizedBox(width: 11),
                                  Expanded(child: _DetailHead(message: message, time: time)),
                                ],
                              ),
                            ),
                            const SizedBox(height: 16),
                            Container(height: 1, color: t.hair),
                            const SizedBox(height: 16),
                            Text(message.content, style: t.longText),
                          ],
                        ),
                      ),
                      if (message.deepLink != null && onLink != null) ...[
                        const SizedBox(height: 16),
                        Align(
                          alignment: Alignment.centerLeft,
                          child: Button(
                            kind: ButtonKind.primary,
                            height: 28,
                            label: '${s('viewDetails')} ↗',
                            onPress: onLink,
                          ),
                        ),
                        const SizedBox(height: 6),
                        Text(s('openLinkHint'), style: t.note),
                      ],
                    ],
                  ),
                ),
              ),
              if (onToggleRead != null)
                Container(
                  padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 8),
                  decoration: BoxDecoration(border: Border(top: BorderSide(color: t.hair))),
                  child: Row(
                    children: [
                      Button(
                        kind: ButtonKind.secondary,
                        height: 26,
                        fontSize: 12,
                        icon: message.read ? FLucideIcons.mail : FLucideIcons.mailOpen,
                        label: message.read ? s('markUnread') : s('markRead'),
                        onPress: onToggleRead,
                      ),
                    ],
                  ),
                ),
            ],
          ),
        ),
      ),
    );
  }
}

/// Type icon, type and the outlined severity tag; the whole title; when.
class _DetailHead extends StatelessWidget {
  const _DetailHead({required this.message, required this.time});

  final rust.UiMessage message;
  final String time;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final tone = _tone(t, message.severity)?.$1;
    final severity = _severityLabel(message.severity);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Row(
          children: [
            _TypeIcon(message: message),
            const SizedBox(width: 10),
            Text(_typeLabel(message.category), style: t.note.copyWith(color: t.t2)),
            if (severity != null) ...[
              const SizedBox(width: 8),
              Container(
                padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 1),
                decoration: BoxDecoration(
                  borderRadius: BorderRadius.circular(4),
                  border: Border.all(color: tone ?? t.t2),
                ),
                child: Text(severity, style: t.note.copyWith(fontWeight: FontWeight.w600, color: tone)),
              ),
            ],
          ],
        ),
        const SizedBox(height: 10),
        Semantics(header: true, child: Text(message.title, style: t.detailTitle)),
        const SizedBox(height: 10),
        Text(time, style: t.note.copyWith(color: t.t2)),
      ],
    );
  }
}

// --- time -------------------------------------------------------------------

DateTime? _parse(String rfc3339) => DateTime.tryParse(rfc3339)?.toLocal();

const _months = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];

String _hm(DateTime at) => '${at.hour.toString().padLeft(2, '0')}:${at.minute.toString().padLeft(2, '0')}';

String _date(DateTime at, {required bool year}) {
  if (S.current.zh) return year ? '${at.year}年${at.month}月${at.day}日' : '${at.month}月${at.day}日';
  final day = '${_months[at.month - 1]} ${at.day}';
  return year ? '$day, ${at.year}' : day;
}

/// 刚刚 · N 分钟前 · 今天 14:20 · 昨天 14:20 · 9月21日 (with the year when
/// it is not this year's).
String relativeTime(DateTime? at, DateTime now) {
  if (at == null) return '';
  final s = S.current;
  final minutes = now.difference(at).inMinutes;
  if (minutes < 1) return s('justNow');
  if (minutes < 60) return s('minAgo', {'n': '$minutes'});
  final today = DateTime(now.year, now.month, now.day);
  if (!at.isBefore(today)) return s('timeToday', {'t': _hm(at)});
  if (!at.isBefore(DateTime(now.year, now.month, now.day - 1))) return s('timeYesterday', {'t': _hm(at)});
  return _date(at, year: at.year != now.year);
}

/// 2026年9月29日 14:29 / Sep 29, 2026 14:29.
String absoluteTime(DateTime at) => '${_date(at, year: true)} ${_hm(at)}';
