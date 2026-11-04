// Network diagnostics (#521 step A): what the system resolver and the
// domestic DoH resolvers (AliDNS, DNSPod) answer for the service's API
// host and the profile's node hosts, and whether each address completes
// TCP and TLS (rust/src/api.rs diagnose_reachability, ppvpn-account
// reachability). The DoH resolvers are asked only when the user presses
// the button here; the intro says so. The report is plain text, monospace,
// with a copy button for pasting into a ticket. With the VPN off, a verified
// alternate API address can enable DNS recovery for this app session; the
// introductory text and button explain that before the user starts it.

import 'dart:async';

import 'package:flutter/widgets.dart';
import 'package:forui/forui.dart';

import '../src/rust/api.dart' as rust;
import '../strings.dart';
import '../theme.dart';
import '../widgets.dart';

/// Available before sign-in, when the API may be unreachable. Opening the
/// explanation does not start any probes; the user must press Run inside.
class DiagnosticsButton extends StatelessWidget {
  const DiagnosticsButton({super.key});

  @override
  Widget build(BuildContext context) => Button(
    kind: ButtonKind.text,
    label: S.current('diagnostics'),
    onPress: () => unawaited(
      showFDialog<void>(
        context: context,
        builder: (context, _, animation) => FDialog(
          animation: animation,
          constraints: BoxConstraints(maxWidth: 640, maxHeight: MediaQuery.sizeOf(context).height * 0.8),
          builder: (context, _) => SingleChildScrollView(
            padding: Tokens.dialogPadding,
            child: Column(
              mainAxisSize: MainAxisSize.min,
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                Text(S.current('diagnostics'), style: Tokens.of(context).pageTitle),
                const SizedBox(height: 16),
                const DiagnosticsView(),
                const SizedBox(height: 16),
                Align(
                  alignment: Alignment.centerRight,
                  child: Button(
                    kind: ButtonKind.secondary,
                    label: S.current('close'),
                    onPress: () => Navigator.of(context).pop(),
                  ),
                ),
              ],
            ),
          ),
        ),
      ),
    ),
  );
}

class DiagnosticsView extends StatefulWidget {
  const DiagnosticsView({super.key});

  @override
  State<DiagnosticsView> createState() => _DiagnosticsViewState();
}

class _DiagnosticsViewState extends State<DiagnosticsView> {
  bool _running = false;
  String? _report;

  Future<void> _run() async {
    setState(() => _running = true);
    String report;
    try {
      report = await rust.diagnoseReachability();
    } catch (error) {
      report = '$error';
    }
    if (!mounted) return;
    setState(() {
      _running = false;
      _report = report;
    });
  }

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final s = S.current;
    final report = _report;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Text(s('diagnosticsIntro'), style: t.sub),
        const SizedBox(height: 12),
        Wrap(
          spacing: 12,
          runSpacing: 8,
          crossAxisAlignment: WrapCrossAlignment.center,
          children: [
            Button(
              kind: ButtonKind.primary,
              height: Tokens.controlHeight,
              label: _running ? s('diagnosticsRunning') : s('diagnosticsRun'),
              onPress: _running ? null : () => unawaited(_run()),
            ),
            if (report != null && !_running) CopyButton(text: report),
          ],
        ),
        if (report != null) ...[
          const SizedBox(height: 12),
          Container(
            padding: const EdgeInsets.all(12),
            decoration: BoxDecoration(color: t.sunk, borderRadius: BorderRadius.circular(8)),
            child: SingleChildScrollView(
              scrollDirection: Axis.horizontal,
              child: Text(report, style: t.logLine),
            ),
          ),
        ],
      ],
    );
  }
}
