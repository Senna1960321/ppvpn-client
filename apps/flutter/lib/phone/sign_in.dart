// The phone sign-in (#448 step 5), to the canvas's 手机 · 登录 (#535): the
// brand on top (the brand gradient under the status bar with its
// mountains, the mark, 「PPVPN」, the title and the line), then a panel
// rising over it with the three steps and 「登录」. The authorization page
// opens over the app as a sheet (links.dart: SFSafariViewController as a
// page sheet, a Custom Tab on Android) with the code in its address, so
// nothing is compared by hand; it closes once signed in (AppState).

import 'dart:async';

import 'package:flutter/widgets.dart';
import 'package:flutter_svg/flutter_svg.dart';
import 'package:forui/forui.dart';

import '../app_state.dart';
import '../links.dart';
import '../pages/sign_in.dart' show SignInError, signInError;
import '../pages/diagnostics.dart';
import '../src/rust/api.dart' as rust;
import '../strings.dart';
import '../theme.dart';
import '../widgets.dart';

class PhoneSignIn extends StatelessWidget {
  const PhoneSignIn({super.key, required this.app});

  final AppState app;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final s = S.current;
    const white = Color(0xFFFFFFFF);
    final top = MediaQuery.paddingOf(context).top;
    final bottom = MediaQuery.paddingOf(context).bottom;
    // The brand, then the panel filling the rest of the screen, or
    // scrolling when the text is large (SliverFillRemaining).
    return ColoredBox(
      color: t.card,
      child: CustomScrollView(
        slivers: [
          SliverToBoxAdapter(
            child: Container(
              decoration: const BoxDecoration(gradient: Tokens.brandGradient),
              child: Stack(
                children: [
                  Positioned(
                    left: 0,
                    right: 0,
                    bottom: 0,
                    child: ExcludeSemantics(
                      child: SvgPicture.string(_mountains, height: 210, fit: BoxFit.cover),
                    ),
                  ),
                  // The panel's rounded top, rising 28 over the brand (a
                  // viewport paints its earlier slivers on top, so the
                  // panel itself cannot reach up here).
                  Positioned(
                    left: 0,
                    right: 0,
                    bottom: 0,
                    height: 28,
                    child: DecoratedBox(
                      decoration: BoxDecoration(
                        color: t.card,
                        borderRadius: const BorderRadius.vertical(top: Radius.circular(28)),
                      ),
                    ),
                  ),
                  Padding(
                    padding: EdgeInsets.fromLTRB(24, top + 40, 24, 120),
                    child: Column(
                      crossAxisAlignment: CrossAxisAlignment.start,
                      children: [
                        Row(
                          children: [
                            const BrandMark(size: 48, onBrand: true),
                            const SizedBox(width: 12),
                            Text(
                              s('app'),
                              style: t.phoneTitle.copyWith(fontSize: 22, height: 28 / 22, color: white),
                            ),
                          ],
                        ),
                        const SizedBox(height: 36),
                        Semantics(
                          header: true,
                          child: Text(s('signInTitle'), style: t.phoneTitle.copyWith(color: white)),
                        ),
                        const SizedBox(height: 10),
                        ConstrainedBox(
                          constraints: const BoxConstraints(maxWidth: 300),
                          child: Text(
                            s('phoneSignInLine'),
                            style: t.phoneBody.copyWith(fontSize: 15, color: const Color(0xCCFFFFFF)),
                          ),
                        ),
                      ],
                    ),
                  ),
                ],
              ),
            ),
          ),
          SliverFillRemaining(
            hasScrollBody: false,
            child: Padding(
              padding: EdgeInsets.fromLTRB(Tokens.phonePadding, 0, Tokens.phonePadding, bottom + 6),
              child: ValueListenableBuilder(
                valueListenable: app.snapshot,
                builder: (context, snapshot, _) => _Panel(app: app, snapshot: snapshot),
              ),
            ),
          ),
        ],
      ),
    );
  }

  static const _mountains =
      '<svg viewBox="0 0 390 210" xmlns="http://www.w3.org/2000/svg" preserveAspectRatio="xMidYMax slice">'
      '<path d="M0 120 L72 66 L124 104 L196 30 L258 92 L308 62 L390 118 L390 210 L0 210 Z" fill="#fff" fill-opacity="0.08"/>'
      '<path d="M0 158 L62 120 L114 146 L176 86 L238 134 L298 100 L390 152 L390 210 L0 210 Z" fill="#fff" fill-opacity="0.10"/>'
      '<path d="M196 30 L211 47 L202 53 L190 45 Z" fill="#fff" fill-opacity="0.35"/></svg>';
}

/// The steps, then by state: 「登录」 (or why the last try failed and
/// 「重试」); while the page is up 「等待授权…」 with 「重新打开授权页面」
/// and 「取消」; while the saved sign-in is restored a spinner. The legal
/// line at the bottom.
class _Panel extends StatelessWidget {
  const _Panel({required this.app, required this.snapshot});

  final AppState app;
  final rust.UiSnapshot snapshot;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final s = S.current;
    final code = snapshot.deviceCode;
    final waiting = snapshot.auth == rust.UiAuth.awaitingBrowser && code != null;
    final error = snapshot.auth == rust.UiAuth.signedOut ? signInError(snapshot.error) : null;
    Widget step(int n, String text) => Padding(
      padding: const EdgeInsets.only(bottom: 14),
      child: Row(
        children: [
          Container(
            width: 26,
            height: 26,
            alignment: Alignment.center,
            decoration: BoxDecoration(color: t.accentSoft, shape: BoxShape.circle),
            child: Text('$n', style: t.label.copyWith(fontSize: 12, color: t.accent)),
          ),
          const SizedBox(width: 12),
          Expanded(child: Text(text, style: t.phoneBody.copyWith(fontSize: 15, height: 20 / 15, color: t.t2))),
        ],
      ),
    );
    final Widget action;
    if (snapshot.auth == rust.UiAuth.restoring) {
      action = Center(child: Spinner(color: t.accent, size: 20));
    } else if (waiting) {
      action = Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Semantics(
            liveRegion: true,
            child: Row(
              mainAxisAlignment: MainAxisAlignment.center,
              children: [
                Spinner(color: t.accent, size: 16),
                const SizedBox(width: 10),
                Text('${s('st_authorizing')}…', style: t.phoneBody.copyWith(color: t.t2)),
              ],
            ),
          ),
          const SizedBox(height: 16),
          _Primary(label: s('reopenAuth'), onPress: () => unawaited(openUrl(code.verificationUrl))),
          const SizedBox(height: 4),
          Pressable(
            onPress: app.cancelSignIn,
            label: s('cancel'),
            radius: Tokens.phoneControlRadius,
            builder: (context, _) => SizedBox(
              height: Tokens.phoneControl,
              child: Center(child: Text(s('cancel'), style: t.phoneBody.copyWith(color: t.t2))),
            ),
          ),
        ],
      );
    } else {
      final (title, text) = switch (error) {
        null => (null, null),
        SignInError.expired => (s('errExpiredT'), s('errExpiredD')),
        SignInError.denied => (s('errDeniedT'), s('errDeniedD')),
        SignInError.network => (s('errNetT'), s('errNetD')),
      };
      action = Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          if (title != null && text != null) ...[
            Row(
              children: [
                Icon(FLucideIcons.circleAlert, size: 18, color: error == SignInError.network ? t.warn : t.bad),
                const SizedBox(width: 8),
                Expanded(child: Text(title, style: t.phoneBodyStrong)),
              ],
            ),
            const SizedBox(height: 4),
            Text(text, style: t.phoneNote.copyWith(color: t.t2)),
            const SizedBox(height: 16),
          ],
          _Primary(
            label: switch (error) {
              null => s('signInOver'),
              SignInError.network => s('tryAgain'),
              _ => s('signInAgain'),
            },
            onPress: app.signIn,
          ),
          const SizedBox(height: 8),
          const Center(child: DiagnosticsButton()),
        ],
      );
    }
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        step(1, s('phoneStep1')),
        step(2, s('phoneStep2')),
        step(3, s('phoneStep3')),
        const SizedBox(height: 14),
        Swap(id: (snapshot.auth, error), child: action),
        const Spacer(),
        const SizedBox(height: 24),
        Text(
          '© PeakPass Labs LLC',
          textAlign: TextAlign.center,
          style: t.label.copyWith(fontWeight: FontWeight.w400, color: t.t3),
        ),
        const SizedBox(height: 10),
      ],
    );
  }
}

/// The primary button: 50 high, radius 12, the gradient, 17 · 600.
class _Primary extends StatelessWidget {
  const _Primary({required this.label, required this.onPress});

  final String label;
  final VoidCallback onPress;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final light = t.brightness == Brightness.light;
    return Pressable(
      key: const ValueKey('sign-in'),
      onPress: onPress,
      label: label,
      radius: Tokens.phoneControlRadius,
      builder: (context, _) => Container(
        height: 50,
        alignment: Alignment.center,
        decoration: BoxDecoration(
          gradient: light ? Tokens.primaryGradient : null,
          color: light ? null : t.accent,
          borderRadius: BorderRadius.circular(Tokens.phoneControlRadius),
          boxShadow: light ? const [BoxShadow(color: Color(0x471F4390), offset: Offset(0, 6), blurRadius: 16)] : null,
        ),
        child: Text(label, style: t.phoneBodyStrong.copyWith(fontSize: 17, height: 22 / 17, color: t.onAccent)),
      ),
    );
  }
}
