// Sign-in (design DesktopLogin): the brand panel on the left (see
// SignInBrand), the content on the right with 56 on either side. Signed
// out; waiting for the browser (the device code in 40×52 boxes, the wait
// ring with the countdown, the three steps); and the three errors.

import 'dart:async';

import 'package:flutter/widgets.dart';
import 'package:forui/forui.dart';

import '../app_state.dart';
import '../links.dart';
import '../src/rust/api.dart' as rust;
import '../strings.dart';
import '../theme.dart';
import '../widgets.dart';
import 'diagnostics.dart';

class SignInPage extends StatelessWidget {
  const SignInPage({super.key, required this.app, this.inset = Tokens.signInPadding});

  final AppState app;

  /// Either side: 56 beside the brand panel, the page padding on a phone.
  final double inset;

  @override
  Widget build(BuildContext context) => ValueListenableBuilder(
    valueListenable: app.snapshot,
    builder: (context, snapshot, _) {
      final code = snapshot.deviceCode;
      final Widget body;
      if (snapshot.auth == rust.UiAuth.restoring) {
        body = Spinner(color: Tokens.of(context).accent, size: 20);
      } else if (snapshot.auth == rust.UiAuth.awaitingBrowser && code != null) {
        body = _Waiting(app: app, code: code);
      } else {
        body = _SignedOut(app: app, error: signInError(snapshot.error));
      }
      return Align(
        alignment: Alignment.centerLeft,
        child: SingleChildScrollView(
          padding: EdgeInsets.symmetric(horizontal: inset, vertical: 20),
          child: body,
        ),
      );
    },
  );
}

/// Why the last sign-in ended, when it failed (the phone's sign-in too).
enum SignInError { expired, denied, network }

SignInError? signInError(rust.UiError? error) => switch (error?.code) {
  'AuthExpired' => SignInError.expired,
  'AuthDenied' => SignInError.denied,
  'NetworkUnreachable' || 'ServerUnavailable' => SignInError.network,
  _ => null,
};

/// 「登录」 over the heading, then the heading and the paragraph.
class _Heading extends StatelessWidget {
  const _Heading({required this.title, required this.text, this.icon});

  final String title;
  final String text;
  final Widget? icon;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        if (icon != null) ...[icon!, const SizedBox(height: 12)],
        Text(
          S.current('signInOver'),
          style: t.note.copyWith(fontWeight: FontWeight.w600, color: t.accent, letterSpacing: 0.24),
        ),
        Padding(
          padding: const EdgeInsets.only(top: 6, bottom: 8),
          child: Semantics(header: true, child: Text(title, style: t.signInTitle)),
        ),
        ConstrainedBox(
          constraints: const BoxConstraints(maxWidth: 420),
          child: Text(text, style: t.body.copyWith(color: t.t2)),
        ),
      ],
    );
  }
}

class _SignedOut extends StatelessWidget {
  const _SignedOut({required this.app, required this.error});

  final AppState app;
  final SignInError? error;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final s = S.current;
    final (title, text) = switch (error) {
      null => (s('loginTitle'), s('loginSub')),
      SignInError.expired => (s('errExpiredT'), s('errExpiredD')),
      SignInError.denied => (s('errDeniedT'), s('errDeniedD')),
      SignInError.network => (s('errNetT'), s('errNetD')),
    };
    final button = switch (error) {
      null => s('loginBtn'),
      SignInError.network => s('tryAgain'),
      _ => s('signInAgain'),
    };
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      mainAxisSize: MainAxisSize.min,
      children: [
        _Heading(
          title: title,
          text: text,
          icon: error == null
              ? null
              : Icon(FLucideIcons.circleAlert, size: 32, color: error == SignInError.network ? t.warn : t.bad),
        ),
        const SizedBox(height: 32),
        Button(kind: ButtonKind.primary, label: button, fontSize: 14, onPress: app.signIn),
        const SizedBox(height: 8),
        const DiagnosticsButton(),
      ],
    );
  }
}

class _Waiting extends StatefulWidget {
  const _Waiting({required this.app, required this.code});

  final AppState app;
  final rust.UiDeviceCode code;

  @override
  State<_Waiting> createState() => _WaitingState();
}

class _WaitingState extends State<_Waiting> {
  late DateTime _expires;
  Timer? _tick;

  @override
  void initState() {
    super.initState();
    _start();
  }

  @override
  void didUpdateWidget(_Waiting old) {
    super.didUpdateWidget(old);
    if (old.code.userCode != widget.code.userCode) _start();
  }

  void _start() {
    _expires = DateTime.now().add(Duration(seconds: widget.code.expiresInSecs));
    _tick?.cancel();
    _tick = Timer.periodic(const Duration(seconds: 1), (_) => setState(() {}));
  }

  @override
  void dispose() {
    _tick?.cancel();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final s = S.current;
    final left = _expires.difference(DateTime.now());
    final seconds = left.isNegative ? 0 : left.inSeconds;
    final countdown = '${seconds ~/ 60}:${(seconds % 60).toString().padLeft(2, '0')}';
    // The page opened in the browser counts as step 1 done.
    Widget step(int n, String text, {bool done = false, bool now = false}) => Padding(
      padding: const EdgeInsets.only(top: 10),
      child: Row(
        children: [
          Container(
            width: 20,
            height: 20,
            alignment: Alignment.center,
            decoration: BoxDecoration(
              shape: BoxShape.circle,
              color: done ? t.ok : (now ? t.accent : t.sunk),
              border: done || now ? null : Border.all(color: t.line),
              boxShadow: now ? [BoxShadow(color: t.accentSoft, spreadRadius: 4)] : null,
            ),
            child: Text(
              done ? '✓' : '$n',
              style: t.label.copyWith(
                fontWeight: FontWeight.w700,
                color: done ? const Color(0xFFFFFFFF) : (now ? t.onAccent : t.t2),
              ),
            ),
          ),
          const SizedBox(width: 10),
          Text(
            text,
            style: t.sub.copyWith(color: now ? t.t1 : t.t2, fontWeight: now ? FontWeight.w500 : FontWeight.w400),
          ),
        ],
      ),
    );
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      mainAxisSize: MainAxisSize.min,
      children: [
        _Heading(title: s('waitTitle'), text: s('waitSub')),
        const SizedBox(height: 28),
        // Shrinks rather than overflows in a narrow window (the code row is
        // about 460 wide).
        FittedBox(
          fit: BoxFit.scaleDown,
          alignment: Alignment.centerLeft,
          child: Row(
            mainAxisSize: MainAxisSize.min,
            children: [
              Semantics(label: widget.code.userCode, child: ExcludeSemantics(child: _CodeBoxes(code: widget.code.userCode))),
              const SizedBox(width: 10),
              CopyButton(text: widget.code.userCode, kind: ButtonKind.soft, height: Tokens.controlHeight),
            ],
          ),
        ),
        const SizedBox(height: 20),
        Row(
          children: [
            Spinner(color: t.accent, track: t.accentSoft, size: 18),
            const SizedBox(width: 10),
            if (seconds == 0)
              Text(s('errExpiredT'), style: t.sub.copyWith(color: t.bad))
            else
              Text.rich(
                TextSpan(
                  style: t.sub,
                  children: [
                    TextSpan(text: '${s('st_authorizing')} · '),
                    TextSpan(text: s('expiresIn', {'t': countdown}), style: t.monoText(13, color: t.t2)),
                  ],
                ),
              ),
          ],
        ),
        const SizedBox(height: 18),
        step(1, s('step1'), done: widget.code.browserOpened),
        step(2, s('step2'), now: true),
        step(3, s('step3')),
        const SizedBox(height: 32),
        Row(
          mainAxisSize: MainAxisSize.min,
          children: [
            Button(
              kind: ButtonKind.secondary,
              height: Tokens.primaryHeight,
              fontSize: 14,
              label: s('cancel'),
              onPress: widget.app.cancelSignIn,
            ),
            const SizedBox(width: 8),
            Button(
              kind: ButtonKind.primary,
              fontSize: 14,
              label: s('reopen'),
              onPress: () => unawaited(openUrl(widget.code.verificationUrl)),
            ),
          ],
        ),
      ],
    );
  }
}

/// The device code, a 40×52 box per character (radius 10, sunk, line
/// outline, mono 26/600), 6 apart; the hyphen is a 12×2 dash.
class _CodeBoxes extends StatelessWidget {
  const _CodeBoxes({required this.code});

  final String code;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final cells = <Widget>[];
    for (final char in code.characters) {
      if (cells.isNotEmpty) cells.add(const SizedBox(width: Tokens.codeGap));
      cells.add(
        char == '-'
            ? Container(
                width: 12,
                height: 2,
                margin: const EdgeInsets.symmetric(horizontal: 4),
                decoration: BoxDecoration(color: t.t3, borderRadius: BorderRadius.circular(1)),
              )
            : Container(
                width: Tokens.codeBox.width,
                height: Tokens.codeBox.height,
                alignment: Alignment.center,
                decoration: BoxDecoration(
                  color: t.sunk,
                  borderRadius: BorderRadius.circular(10),
                  border: Border.all(color: t.line),
                ),
                child: Text(char, style: t.monoText(Tokens.codeFontSize, weight: FontWeight.w600)),
              ),
      );
    }
    return Row(mainAxisSize: MainAxisSize.min, children: cells);
  }
}

/// The sign-in brand panel, 380 wide on the brand gradient with a white
/// glow at 80 % / 12 %: the traffic lights' row, 36 below it the 40 mark
/// with 「PPVPN」 22/700, low in the panel the title (24/32 · 700) and 10
/// below it the line (13/20, white 78 %, max 280), 150 for the mountains,
/// the copyright (11, white 65 %).
class SignInBrand extends StatelessWidget {
  const SignInBrand({super.key});

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    const white = Color(0xFFFFFFFF);
    TextStyle text(double size, double height, FontWeight weight, Color color, [double tracking = 0]) =>
        t.body.copyWith(fontSize: size, height: height / size, fontWeight: weight, color: color, letterSpacing: tracking * size);
    return Container(
      width: Tokens.signInBrandWidth,
      decoration: const BoxDecoration(gradient: Tokens.brandGradient),
      child: Stack(
        children: [
          const Positioned.fill(
            child: DecoratedBox(
              decoration: BoxDecoration(
                gradient: RadialGradient(
                  center: Alignment(0.6, -0.76),
                  radius: 0.55,
                  colors: [Color(0x38FFFFFF), Color(0x00FFFFFF)],
                  stops: [0, 0.7],
                ),
              ),
            ),
          ),
          const Positioned(left: 0, right: 0, bottom: 0, height: 250, child: CustomPaint(painter: _Mountains())),
          // A window lower than the panel's content (a phone in landscape
          // gets this layout from 600 wide) scrolls it rather than
          // overflowing; a taller one keeps the Spacer's gap.
          LayoutBuilder(
            builder: (context, box) => SingleChildScrollView(
              padding: Tokens.signInBrandPadding,
              child: ConstrainedBox(
                constraints: BoxConstraints(minHeight: box.maxHeight - Tokens.signInBrandPadding.vertical),
                child: IntrinsicHeight(
                  child: Column(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      // The traffic lights' row (macOS draws them over it).
                      const SizedBox(height: Tokens.trafficLightsHeight),
                      const SizedBox(height: 36),
                      Row(
                        children: [
                          const BrandMark(size: 40, onBrand: true),
                          const SizedBox(width: 12),
                          Text(S.current('app'), style: text(22, 28, FontWeight.w700, white, -0.02)),
                        ],
                      ),
                      const Spacer(),
                      Semantics(
                        header: true,
                        child: Text(S.current('signInTitle'), style: text(24, 32, FontWeight.w700, white, -0.02)),
                      ),
                      const SizedBox(height: 10),
                      ConstrainedBox(
                        constraints: const BoxConstraints(maxWidth: 280),
                        child: Text(S.current('signInLine'), style: text(13, 20, FontWeight.w400, const Color(0xC7FFFFFF))),
                      ),
                      const SizedBox(height: Tokens.signInMountains),
                      Text('© PeakPass Labs LLC', style: text(11, 16, FontWeight.w400, const Color(0xA6FFFFFF))),
                    ],
                  ),
                ),
              ),
            ),
          ),
        ],
      ),
    );
  }
}

/// The mountains behind the panel's bottom (DesktopLogin's 380×250 drawing,
/// anchored to the bottom).
class _Mountains extends CustomPainter {
  const _Mountains();

  @override
  void paint(Canvas canvas, Size size) {
    canvas.scale(size.width / 380, size.height / 250);
    Path poly(List<double> xy) {
      final path = Path()..moveTo(xy[0], xy[1]);
      for (var i = 2; i < xy.length; i += 2) {
        path.lineTo(xy[i], xy[i + 1]);
      }
      return path..close();
    }

    void fill(List<double> xy, Color color) => canvas.drawPath(poly(xy), Paint()..color = color);
    fill([0, 150, 70, 90, 120, 130, 190, 50, 250, 110, 300, 80, 380, 140, 380, 250, 0, 250], const Color(0x14FFFFFF));
    fill([0, 190, 60, 150, 110, 175, 170, 110, 230, 160, 290, 125, 380, 180, 380, 250, 0, 250], const Color(0x1AFFFFFF));
    fill([190, 50, 205, 66, 196, 72, 184, 64], const Color(0x59FFFFFF));
    fill([0, 225, 80, 200, 150, 215, 220, 190, 300, 210, 380, 200, 380, 250, 0, 250], const Color(0x590A193C));
  }

  @override
  bool shouldRepaint(_Mountains old) => false;
}
