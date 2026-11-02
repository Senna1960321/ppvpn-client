// The silent update's two notices (#548, updates.dart):
// - UpdatedNotice: 「已更新到 PPVPN {v}」 at the top of the overview, once,
//   after the service installed this build; closed by its button or after
//   10 seconds.
// - InstallingNotice: the window of an app opened while the service is
//   installing an update (docs/host-integration.md section 16.5); the app
//   quits on 「好」 or after 10 seconds, and connects nothing meanwhile.

import 'dart:async';

import 'package:flutter/widgets.dart';
import 'package:forui/forui.dart';

import 'strings.dart';
import 'theme.dart';
import 'updates.dart';
import 'widgets.dart';

/// How long either notice stays.
const _shown = Duration(seconds: 10);

class UpdatedNotice extends StatelessWidget {
  const UpdatedNotice({super.key});

  @override
  Widget build(BuildContext context) => ValueListenableBuilder(
    valueListenable: Updates.updatedTo,
    builder: (context, version, _) => Reveal(
      child: version == null
          ? null
          : Padding(
              padding: const EdgeInsets.only(bottom: Tokens.cardGap),
              child: _Updated(version: version),
            ),
    ),
  );
}

class _Updated extends StatefulWidget {
  const _Updated({required this.version});

  final String version;

  @override
  State<_Updated> createState() => _UpdatedState();
}

class _UpdatedState extends State<_Updated> {
  late final Timer _timer;

  @override
  void initState() {
    super.initState();
    _timer = Timer(_shown, _close);
  }

  @override
  void dispose() {
    _timer.cancel();
    super.dispose();
  }

  void _close() => Updates.updatedTo.value = null;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final s = S.current;
    return Semantics(
      liveRegion: true,
      child: Container(
        padding: const EdgeInsetsDirectional.fromSTEB(16, 6, 6, 6),
        decoration: BoxDecoration(
          color: t.okSoft,
          borderRadius: BorderRadius.circular(Tokens.cardRadius),
          border: Border.all(color: t.hair),
        ),
        child: Row(
          children: [
            Icon(FLucideIcons.circleCheck, size: 16, color: t.ok),
            const SizedBox(width: 10),
            Expanded(child: Text(s('updatedTo', {'v': widget.version}), style: t.sectionTitle)),
            Button(
              kind: ButtonKind.icon,
              icon: FLucideIcons.x,
              semanticsLabel: s('close'),
              onPress: _close,
            ),
          ],
        ),
      ),
    );
  }
}

/// The whole window while the service installs an update; [onDone] quits.
class InstallingNotice extends StatefulWidget {
  const InstallingNotice({super.key, required this.onDone});

  final VoidCallback onDone;

  @override
  State<InstallingNotice> createState() => _InstallingNoticeState();
}

class _InstallingNoticeState extends State<InstallingNotice> {
  late final Timer _timer;

  @override
  void initState() {
    super.initState();
    _timer = Timer(_shown, widget.onDone);
  }

  @override
  void dispose() {
    _timer.cancel();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final s = S.current;
    return ColoredBox(
      color: t.bg,
      child: Center(
        child: Padding(
          padding: Tokens.pagePadding,
          child: Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              Icon(FLucideIcons.download, size: 28, color: t.accent),
              const SizedBox(height: 14),
              Text(s('updInstalling'), textAlign: TextAlign.center, style: t.sectionTitle),
              const SizedBox(height: 18),
              Button(kind: ButtonKind.primary, label: s('ok'), onPress: widget.onDone),
            ],
          ),
        ),
      ),
    );
  }
}
