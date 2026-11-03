// The phone layout (#448 step 5, to the canvas's phone boards in #535): on
// phones and tablets, and below Tokens.phoneBreakpoint wide elsewhere.
// Three tabs at the bottom (概览 · 节点 · 我的) on a translucent bar, each
// page under a top bar (the brand on 概览, a large title on the others),
// the bell on 概览 and 我的. Messages, logs and choices are pushed full
// screen with a back control. Signed out: the phone sign-in.

import 'dart:async';
import 'dart:io' show Platform;
import 'dart:ui' show ImageFilter;

import 'package:flutter/cupertino.dart' show CupertinoPageRoute;
import 'package:flutter/widgets.dart';
import 'package:flutter_svg/flutter_svg.dart';
import 'package:forui/forui.dart';

import '../app_state.dart';
import '../pages/logs.dart';
import '../pages/messages.dart';
import '../sample.dart' show samplePage;
import '../src/rust/api.dart' as rust;
import '../strings.dart';
import '../theme.dart';
import '../widgets.dart';
import 'me.dart';
import 'nodes.dart';
import 'overview.dart';
import 'sign_in.dart';

enum _Tab { overview, nodes, me }

class PhoneShell extends StatefulWidget {
  const PhoneShell({super.key, required this.app});

  final AppState app;

  @override
  State<PhoneShell> createState() => _PhoneShellState();
}

class _PhoneShellState extends State<PhoneShell> {
  // The screenshot mode's page (sample.dart): 节点, or 我的 for settings.
  _Tab _tab = switch (samplePage) {
    'nodes' => _Tab.nodes,
    'settings' => _Tab.me,
    _ => _Tab.overview,
  };

  @override
  void initState() {
    super.initState();
    widget.app.pageRequests.addListener(_showRequested);
    // The screenshot mode's pushed pages: the messages, the logs.
    if (samplePage case 'messages' || 'logs') {
      WidgetsBinding.instance.addPostFrameCallback((_) {
        if (!mounted) return;
        if (samplePage == 'messages') return openMessages(context, widget.app);
        final title = S.current('logs');
        unawaited(pushPhonePage<void>(context, PhoneScaffold(title: title, back: true, child: const LogsPage())));
      });
    }
  }

  @override
  void dispose() {
    widget.app.pageRequests.removeListener(_showRequested);
    super.dispose();
  }

  /// A notification click (the messages) or a settings request (我的).
  /// Signed out the sign-in stays.
  void _showRequested() {
    final request = widget.app.pageRequests.value;
    if (request == null || widget.app.snapshot.value.auth != rust.UiAuth.signedIn) return;
    switch (request.page) {
      case WindowPage.messages:
        openMessages(context, widget.app);
      case WindowPage.settings:
        setState(() => _tab = _Tab.me);
    }
  }

  @override
  Widget build(BuildContext context) => ValueListenableBuilder(
    valueListenable: widget.app.snapshot,
    builder: (context, snapshot, _) {
      final app = widget.app;
      if (snapshot.auth != rust.UiAuth.signedIn) {
        return MediaQuery.withClampedTextScaling(maxScaleFactor: Tokens.phoneTextMax, child: PhoneSignIn(app: app));
      }
      final s = S.current;
      final bell = PhoneBell(unread: snapshot.unread, onPress: () => openMessages(context, app));
      final page = switch (_tab) {
        _Tab.overview => PhoneOverview(app: app, bell: bell, onNodes: () => setState(() => _tab = _Tab.nodes)),
        _Tab.nodes => PhoneNodes(app: app),
        _Tab.me => PhoneScaffold(title: s('me'), trailing: bell, child: PhoneMe(app: app)),
      };
      final media = MediaQuery.of(context);
      return MediaQuery.withClampedTextScaling(
        maxScaleFactor: Tokens.phoneTextMax,
        child: Stack(
        children: [
          // The pages scroll under the translucent tab bar: their bottom
          // padding includes it.
          Positioned.fill(
            child: MediaQuery(
              data: media.copyWith(padding: media.padding.copyWith(bottom: media.padding.bottom + Tokens.phoneTabBar)),
              // The cap inside: this MediaQuery is built from the one above.
              child: MediaQuery.withClampedTextScaling(
                maxScaleFactor: Tokens.phoneTextMax,
                child: _TabSwitch(tab: _tab, child: page),
              ),
            ),
          ),
          Positioned(
            left: 0,
            right: 0,
            bottom: 0,
            child: MediaQuery.withNoTextScaling(
              child: _TabBar(tab: _tab, onTab: (tab) => setState(() => _tab = tab)),
            ),
          ),
        ],
        ),
      );
    },
  );
}

/// A tab's page coming in (the motion board, #535): it fades in rising
/// [Tokens.pageRise] over [Tokens.motionPage]; the page it replaces fades
/// out over [Tokens.motionFast].
class _TabSwitch extends StatelessWidget {
  const _TabSwitch({required this.tab, required this.child});

  final _Tab tab;
  final Widget child;

  @override
  Widget build(BuildContext context) => AnimatedSwitcher(
    duration: Tokens.motion(context, Tokens.motionPage),
    reverseDuration: Tokens.motion(context, Tokens.motionFast),
    switchInCurve: Tokens.motionCurve,
    switchOutCurve: Tokens.motionFastCurve,
    layoutBuilder: (current, previous) =>
        Stack(fit: StackFit.expand, children: [...previous, ?current]),
    transitionBuilder: (child, animation) => FadeTransition(
      opacity: animation,
      child: AnimatedBuilder(
        animation: animation,
        builder: (context, child) =>
            Transform.translate(offset: Offset(0, Tokens.pageRise * (1 - animation.value)), child: child),
        child: child,
      ),
    ),
    child: KeyedSubtree(key: ValueKey(tab), child: child),
  );
}

/// The tab bar: 50 plus the safe area, translucent over a blur with a
/// 0.5 px line on top (the canvas's phone boards). Icons 24, the selected
/// one filled and in the accent colour, labels 11.
class _TabBar extends StatelessWidget {
  const _TabBar({required this.tab, required this.onTab});

  final _Tab tab;
  final ValueChanged<_Tab> onTab;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final s = S.current;
    final bottom = MediaQuery.paddingOf(context).bottom;
    Widget item(_Tab value, String label) {
      final on = value == tab;
      final color = on ? t.accent : t.t3;
      return Expanded(
        child: Pressable(
          key: ValueKey('tab-${value.name}'),
          onPress: () => onTab(value),
          selected: on,
          label: label,
          radius: 0,
          builder: (context, _) => Column(
            mainAxisAlignment: MainAxisAlignment.center,
            children: [
              // Outline and filled cross-fade as the tab is chosen.
              AnimatedSwitcher(
                duration: Tokens.motion(context, Tokens.motionFast),
                child: TabIcon(key: ValueKey(on), tab: value.name, filled: on, color: color, size: 24),
              ),
              const SizedBox(height: 3),
              Text(
                label,
                style: t.label.copyWith(color: color, fontWeight: on ? FontWeight.w600 : FontWeight.w500),
              ),
            ],
          ),
        ),
      );
    }

    return ClipRect(
      child: BackdropFilter(
        // The canvas's blur(24px): a deviation of 24.
        filter: ImageFilter.blur(sigmaX: 24, sigmaY: 24),
        child: Container(
          height: Tokens.phoneTabBar + bottom,
          padding: EdgeInsets.only(bottom: bottom, left: 8, right: 8),
          decoration: BoxDecoration(
            color: t.phoneBarFill,
            border: Border(top: BorderSide(color: t.phoneBarLine, width: 0.5)),
          ),
          child: Row(
            children: [
              item(_Tab.overview, s('overview')),
              item(_Tab.nodes, s('nodes')),
              item(_Tab.me, s('me')),
            ],
          ),
        ),
      ),
    );
  }
}

/// The tab icons of the canvas (shield, globe, person): 2 px outlines, or
/// filled for the selected tab (Lucide has no filled icons).
class TabIcon extends StatelessWidget {
  const TabIcon({super.key, required this.tab, required this.filled, required this.color, this.size = 24});

  /// overview, nodes or me.
  final String tab;
  final bool filled;
  final Color color;
  final double size;

  @override
  Widget build(BuildContext context) {
    final c = _hex(color);
    final inner = _hex(Tokens.of(context).brightness == Brightness.light ? const Color(0xFFFFFFFF) : Tokens.dark.bg);
    final body = switch ((tab, filled)) {
      ('overview', true) =>
        '<path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z" fill="$c"/>'
            '<path d="m8.5 12 2.5 2.5 4.5-5" fill="none" stroke="$inner" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"/>',
      ('overview', false) =>
        '<g fill="none" stroke="$c" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">'
            '<path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z"/><path d="m8.5 12 2.5 2.5 4.5-5"/></g>',
      ('nodes', true) =>
        '<g fill="none" stroke="$c" stroke-width="2.2" stroke-linecap="round"><circle cx="12" cy="12" r="9" '
            'fill="$c" fill-opacity="0.14"/><path d="M3 12h18M12 3a14 14 0 0 1 0 18M12 3a14 14 0 0 0 0 18"/></g>',
      ('nodes', false) =>
        '<g fill="none" stroke="$c" stroke-width="2" stroke-linecap="round"><circle cx="12" cy="12" r="9"/>'
            '<path d="M3 12h18M12 3a14 14 0 0 1 0 18M12 3a14 14 0 0 0 0 18"/></g>',
      (_, true) => '<g fill="$c"><circle cx="12" cy="8" r="4.5"/><path d="M3.5 21a8.5 8.5 0 0 1 17 0z"/></g>',
      _ =>
        '<g fill="none" stroke="$c" stroke-width="2" stroke-linecap="round"><circle cx="12" cy="8" r="4"/>'
            '<path d="M4 21a8 8 0 0 1 16 0"/></g>',
    };
    return ExcludeSemantics(
      child: SvgPicture.string(
        '<svg viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg">$body</svg>',
        width: size,
        height: size,
      ),
    );
  }
}

String _hex(Color color) =>
    '#${(color.toARGB32() & 0xFFFFFF).toRadixString(16).padLeft(6, '0')}';

/// A page under the top bar (56 plus the safe area): the large title (28)
/// on the left, or with [brand] the mark and 「PPVPN」 (the overview);
/// [trailing] on the right (the bell, the nodes page's tools); with [back]
/// a pushed page's back control and a 17 title instead. [background]
/// paints under the bar and the page (the overview's status tint).
class PhoneScaffold extends StatelessWidget {
  const PhoneScaffold({
    super.key,
    required this.title,
    required this.child,
    this.trailing,
    this.back = false,
    this.brand = false,
    this.background,
  });

  final String title;
  final Widget child;
  final Widget? trailing;
  final bool back;
  final bool brand;
  final Decoration? background;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final top = MediaQuery.paddingOf(context).top;
    final Widget heading = brand
        ? Row(
            children: [
              const BrandMark(size: 28),
              const SizedBox(width: 10),
              Flexible(
                child: Text(
                  S.current('app'),
                  maxLines: 1,
                  style: t.phoneTitle.copyWith(fontSize: 20, height: 26 / 20),
                ),
              ),
            ],
          )
        : Text(
            title,
            maxLines: 1,
            overflow: TextOverflow.ellipsis,
            style: back ? t.phoneBodyStrong.copyWith(fontSize: 17) : t.phoneTitle,
          );
    // The overview's tint follows the status (#535).
    return AnimatedContainer(
      duration: Tokens.motion(context, Tokens.motionBase),
      curve: Tokens.motionCurve,
      decoration: background ?? BoxDecoration(color: t.bg),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Container(
            height: Tokens.phoneTopBar + top,
            padding: EdgeInsets.fromLTRB(back ? 4 : Tokens.phoneBarStart, top, Tokens.phoneBarEnd, 0),
            child: MediaQuery.withClampedTextScaling(
              maxScaleFactor: Tokens.phoneChromeTextMax,
              child: Row(
              children: [
                if (back) const PhoneBack(),
                Expanded(child: Semantics(header: true, label: brand ? title : null, child: heading)),
                if (trailing case final trailing?) ...[const SizedBox(width: 8), trailing],
              ],
              ),
            ),
          ),
          Expanded(child: MediaQuery.removePadding(context: context, removeTop: true, child: child)),
        ],
      ),
    );
  }
}

/// The scroll padding of a phone page: the page padding, and at the bottom
/// the safe area (with the tab bar, PhoneShell) as well.
EdgeInsets phoneScrollPadding(BuildContext context) => EdgeInsets.fromLTRB(
  Tokens.phonePadding,
  4,
  Tokens.phonePadding,
  Tokens.phonePadding + MediaQuery.paddingOf(context).bottom,
);

/// A pushed page's back control (44, the chevron in the accent colour).
class PhoneBack extends StatelessWidget {
  const PhoneBack({super.key});

  @override
  Widget build(BuildContext context) => Pressable(
    key: const ValueKey('back'),
    onPress: () => unawaited(Navigator.of(context).maybePop()),
    label: S.current('back'),
    builder: (context, _) => SizedBox(
      width: Tokens.phoneControl,
      height: Tokens.phoneControl,
      child: Icon(FLucideIcons.chevronLeft, size: 24, color: Tokens.of(context).accent),
    ),
  );
}

/// Messages: a round 44 button (translucent fill, hair ring) with the
/// unread badge on its top right (18 high, none at 0, 「99+」 beyond 99).
class PhoneBell extends StatelessWidget {
  const PhoneBell({super.key, required this.unread, required this.onPress});

  final int unread;
  final VoidCallback onPress;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final s = S.current;
    return Pressable(
      key: const ValueKey('bell'),
      onPress: onPress,
      label: unread > 0 ? '${s('messages')}, ${s('unreadN', {'n': '$unread'})}' : s('messages'),
      radius: Tokens.phoneControl / 2,
      builder: (context, _) => Container(
        width: Tokens.phoneControl,
        height: Tokens.phoneControl,
        decoration: BoxDecoration(
          color: t.phoneRoundFill,
          shape: BoxShape.circle,
          border: Border.all(color: t.hair),
        ),
        child: Stack(
          clipBehavior: Clip.none,
          children: [
            Center(child: Icon(FLucideIcons.bell, size: 22, color: t.t1)),
            if (unread > 0)
              Positioned(
                top: 3,
                right: 2,
                child: Container(
                  constraints: const BoxConstraints(minWidth: 18),
                  height: 18,
                  padding: const EdgeInsets.symmetric(horizontal: 5),
                  alignment: Alignment.center,
                  decoration: BoxDecoration(
                    color: t.bad,
                    borderRadius: BorderRadius.circular(9),
                    boxShadow: [BoxShadow(color: t.bg, spreadRadius: 2)],
                  ),
                  child: Text(
                    unread > 99 ? '99+' : '$unread',
                    style: t.label.copyWith(height: 18 / 11, fontWeight: FontWeight.w700, color: t.onBad),
                  ),
                ),
              ),
          ],
        ),
      ),
    );
  }
}

/// Pushes [page] full screen. iOS: the platform's page route, which also
/// goes back with a swipe from the screen's left edge (#535). Elsewhere it
/// slides in from the end over [Tokens.motionPage]. Routes sit beside the
/// app's home, so the page gets forui's theme and the body text style here.
Future<T?> pushPhonePage<T>(BuildContext context, Widget page) => Navigator.of(context).push<T>(_phoneRoute<T>(context, page));

PageRoute<T> _phoneRoute<T>(BuildContext context, Widget page) {
  Widget themed(BuildContext context) {
    final t = Tokens.of(context);
    return FTheme(
      data: t.forui,
      child: DefaultTextStyle(style: t.body, child: page),
    );
  }

  if (Platform.isIOS) return CupertinoPageRoute<T>(builder: themed);
  return PageRouteBuilder<T>(
    transitionDuration: Tokens.motion(context, Tokens.motionPage),
    reverseTransitionDuration: Tokens.motion(context, Tokens.motionFast),
    pageBuilder: (context, _, _) => themed(context),
    transitionsBuilder: (context, animation, _, child) => SlideTransition(
      position: animation.drive(
        Tween(begin: const Offset(1, 0), end: Offset.zero).chain(CurveTween(curve: Tokens.motionCurve)),
      ),
      child: child,
    ),
  );
}

/// Pushes the choices of a row: one 50 row each, the current one checked;
/// a press sets it and goes back.
void pushPhoneChoice<T>(
  BuildContext context, {
  required String title,
  required List<(T, String)> options,
  required T value,
  required ValueChanged<T> onChanged,
}) => unawaited(
  pushPhonePage<void>(
    context,
    PhoneScaffold(
      title: title,
      back: true,
      child: Builder(
        builder: (context) {
          final t = Tokens.of(context);
          return ListView(
            padding: phoneScrollPadding(context),
            children: [
              PhoneCard(
                children: [
                  for (final (option, text) in options)
                    Pressable(
                      radius: 0,
                      selected: option == value,
                      label: text,
                      onPress: () {
                        onChanged(option);
                        unawaited(Navigator.of(context).maybePop());
                      },
                      builder: (context, pressed) => Container(
                        color: pressed ? t.phonePress : null,
                        constraints: const BoxConstraints(minHeight: Tokens.phoneRow),
                        padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 8),
                        child: Row(
                          children: [
                            Expanded(child: Text(text, style: t.phoneRowTitle)),
                            if (option == value) Icon(FLucideIcons.check, size: 18, color: t.accent),
                          ],
                        ),
                      ),
                    ),
                ],
              ),
            ],
          );
        },
      ),
    ),
  ),
);

/// A phone card (radius 16, the hair outline, two soft shadows) holding
/// [children] as rows with 0.5 px separators, inset by [inset] (past the
/// rows' icon or flag).
class PhoneCard extends StatelessWidget {
  const PhoneCard({super.key, required this.children, this.inset = 0, this.decoration});

  final List<Widget> children;
  final double inset;

  /// Instead of the plain card's (the current node's highlighted card).
  final Decoration? decoration;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    return Container(
      clipBehavior: Clip.antiAlias,
      decoration: decoration ?? t.phoneCardDecoration(),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          for (final (i, row) in children.indexed) ...[
            if (i > 0)
              Padding(
                padding: EdgeInsetsDirectional.only(start: inset),
                child: Container(height: 0.5, color: t.phoneSeparator),
              ),
            row,
          ],
        ],
      ),
    );
  }
}

/// A group's caption above its card: 13 · 600 in t2, [trailing] (「在网页中
/// 打开」) on the right in t3.
class PhoneCaption extends StatelessWidget {
  const PhoneCaption({super.key, required this.text, this.trailing});

  final String text;
  final String? trailing;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    return Padding(
      padding: const EdgeInsets.fromLTRB(4, 18, 4, 8),
      child: Row(
        children: [
          Expanded(
            child: Semantics(
              header: true,
              child: Text(text, style: t.phoneNote.copyWith(fontWeight: FontWeight.w600, color: t.t2)),
            ),
          ),
          if (trailing case final trailing?) Text(trailing, style: t.phoneNote.copyWith(fontSize: 12, color: t.t3)),
        ],
      ),
    );
  }
}

/// A row's icon tile on the phone: the accent icon on accent-soft, 28
/// (radius 8, the overview) or 32 (radius 9, 我的).
class PhoneTile extends StatelessWidget {
  const PhoneTile({super.key, required this.icon, this.size = 32});

  final IconData icon;
  final double size;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    return Container(
      width: size,
      height: size,
      decoration: BoxDecoration(color: t.accentSoft, borderRadius: BorderRadius.circular(size < 30 ? 8 : 9)),
      child: Icon(icon, size: 16, color: t.accent),
    );
  }
}

/// One message page per navigator: notification taps reuse its listener
/// instead of opening the detail underneath a second, empty inbox page.
final _messageRoutes = Expando<PageRoute<void>>();

void openMessages(BuildContext context, AppState app) {
  final navigator = Navigator.of(context);
  final existing = _messageRoutes[navigator];
  if (existing != null && existing.isActive) {
    navigator.popUntil((route) => identical(route, existing));
    return;
  }
  final route = _phoneRoute<void>(
    context,
    PhoneScaffold(title: S.current('msgCenter'), back: true, child: MessagesPage(app: app)),
  );
  _messageRoutes[navigator] = route;
  unawaited(navigator.push<void>(route).then((_) {
    if (identical(_messageRoutes[navigator], route)) _messageRoutes[navigator] = null;
  }));
}
