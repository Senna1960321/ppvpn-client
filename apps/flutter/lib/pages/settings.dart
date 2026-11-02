// Settings (handoff "6. 设置", #401 step 3): a page of the main window, in
// the overview's column, with three grouped cards. General: appearance,
// language, routing mode, launch at login (step 4, the system's state), the
// version with the update check and the automatic-check switch where the
// app installs updates, or the Linux notice (step 5, updates.dart). Account: user, team, service expiry,
// sign out with a confirmation. Advanced: the API override (next launch)
// and the system service's status (install and uninstall disabled:
// enhanced mode is step 2). Appearance, language and the override are the
// app's own (settings.dart); the routing mode is the client's.

import 'dart:async';
import 'dart:io';

import 'package:flutter/widgets.dart';
import 'package:forui/forui.dart';
import 'package:window_manager/window_manager.dart';

import '../app_state.dart';
import '../settings.dart';
import '../src/rust/api.dart' as rust;
import '../strings.dart';
import '../theme.dart';
import '../updates.dart';
import '../widgets.dart';
import 'diagnostics.dart';

class SettingsPage extends StatelessWidget {
  const SettingsPage({super.key, required this.app});

  final AppState app;

  @override
  Widget build(BuildContext context) => ValueListenableBuilder(
    valueListenable: app.snapshot,
    builder: (context, snapshot, _) => SingleChildScrollView(
      padding: Tokens.pagePadding,
      child: Center(
        child: ConstrainedBox(
          constraints: const BoxConstraints(maxWidth: Tokens.pageMaxWidth),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              _General(app: app, snapshot: snapshot),
              const SizedBox(height: Tokens.groupGap),
              _Account(app: app, snapshot: snapshot),
              const SizedBox(height: Tokens.groupGap),
              _Advanced(snapshot: snapshot),
            ],
          ),
        ),
      ),
    ),
  );
}

/// A group: its heading, then its rows in one card.
class _Group extends StatelessWidget {
  const _Group({required this.title, required this.children});

  final String title;
  final List<Widget> children;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Padding(
          padding: Tokens.groupLabelPadding,
          child: Semantics(header: true, child: Text(title, style: t.sectionTitle.copyWith(color: t.t2))),
        ),
        CardBox(child: Column(children: children)),
      ],
    );
  }
}

/// A row's value: one line of secondary text.
class _Value extends StatelessWidget {
  const _Value(this.text, {this.mono = false});

  final String text;
  final bool mono;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    return ConstrainedBox(
      constraints: const BoxConstraints(maxWidth: 320),
      child: Text(
        text,
        maxLines: 1,
        overflow: TextOverflow.ellipsis,
        style: mono ? t.monoText(13, color: t.t2) : t.sub,
      ),
    );
  }
}

class _General extends StatelessWidget {
  const _General({required this.app, required this.snapshot});

  final AppState app;
  final rust.UiSnapshot snapshot;

  @override
  Widget build(BuildContext context) {
    final s = S.current;
    return _Group(
      title: s('general'),
      children: [
        ValueListenableBuilder(
          valueListenable: Settings.appearance,
          builder: (context, appearance, _) => SettingRow(
            icon: FLucideIcons.sunMoon,
            title: s('appearance'),
            trailing: Segmented(
              label: s('appearance'),
              options: [
                (rust.UiAppearance.system, s('sys')),
                (rust.UiAppearance.light, s('light')),
                (rust.UiAppearance.dark, s('dark')),
              ],
              value: appearance,
              onChanged: Settings.setAppearance,
            ),
          ),
        ),
        ValueListenableBuilder(
          valueListenable: Settings.language,
          builder: (context, language, _) => SettingRow(
            icon: FLucideIcons.languages,
            title: s('language'),
            divider: true,
            trailing: Segmented(
              label: s('language'),
              // Each language in its own name.
              options: [
                (rust.UiLanguage.system, s('sys')),
                (rust.UiLanguage.chinese, '中文'),
                (rust.UiLanguage.english, 'English'),
              ],
              value: language,
              onChanged: Settings.setLanguage,
            ),
          ),
        ),
        SettingRow(
          icon: FLucideIcons.split,
          title: s('routing'),
          divider: true,
          trailing: Segmented(
            label: s('routing'),
            options: [(rust.UiRoutingMode.rules, s('rules')), (rust.UiRoutingMode.global, s('global'))],
            value: snapshot.routingMode,
            onChanged: app.setRoutingMode,
          ),
        ),
        const _LaunchAtLogin(),
        const _Version(),
      ],
    );
  }
}

/// Launch at login: the switch, then, while the system holds the entry
/// back, the native apps' hint with the way to the system's settings (macOS
/// Login Items, Windows Startup apps), and the error when the system
/// refused a change. Read again when the page shows and when the window
/// gains focus: the user may have changed it in the system's settings.
class _LaunchAtLogin extends StatefulWidget {
  const _LaunchAtLogin();

  @override
  State<_LaunchAtLogin> createState() => _LaunchAtLoginState();
}

class _LaunchAtLoginState extends State<_LaunchAtLogin> with WindowListener {
  String? _error;

  @override
  void initState() {
    super.initState();
    windowManager.addListener(this);
    unawaited(_refresh());
  }

  @override
  void dispose() {
    windowManager.removeListener(this);
    super.dispose();
  }

  @override
  void onWindowFocus() => unawaited(_refresh());

  Future<void> _refresh() => _show(Settings.refreshLaunchAtLogin());

  Future<void> _set(bool enabled) => _show(Settings.setLaunchAtLogin(enabled));

  /// The call's error, once it ends, unless the page has closed.
  Future<void> _show(Future<String?> call) async {
    final error = await call;
    if (mounted) setState(() => _error = error);
  }

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final s = S.current;
    return ValueListenableBuilder(
      valueListenable: Settings.launchAtLogin,
      builder: (context, state, _) => Column(
        children: [
          SettingRow(
            icon: FLucideIcons.power,
            title: s('launchAtLogin'),
            divider: true,
            trailing: TriSwitch(
              label: s('launchAtLogin'),
              state: state.enabled ? SwitchState.on : SwitchState.off,
              onChanged: (enabled) => unawaited(_set(enabled)),
            ),
          ),
          if (state.needsApproval)
            _SubRow(
              children: [
                Icon(FLucideIcons.circleAlert, size: 14, color: t.warn),
                const SizedBox(width: 8),
                Expanded(
                  child: Text(s(Platform.isWindows ? 'winLaunchApprove' : 'launchApprove'), style: t.note),
                ),
                const SizedBox(width: 8),
                Button(
                  kind: ButtonKind.text,
                  height: Tokens.smallButtonHeight,
                  fontSize: 12,
                  label: s(Platform.isWindows ? 'openWinStartup' : 'openSysSettings'),
                  onPress: () => unawaited(_show(Settings.openLaunchAtLoginSettings())),
                ),
              ],
            ),
          if (_error case final error?)
            _SubRow(
              children: [
                Expanded(
                  child: Semantics(
                    liveRegion: true,
                    child: Text(error, style: t.note.copyWith(color: t.bad)),
                  ),
                ),
              ],
            ),
        ],
      ),
    );
  }
}

/// The version, with 「检查更新…」 where the app checks; the 「自动更新」
/// switch where it installs updates (#548); on Linux the newer
/// build's notice with the upgrade command, and how a check ended.
class _Version extends StatelessWidget {
  const _Version();

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final s = S.current;
    return Column(
      children: [
        SettingRow(
          icon: FLucideIcons.info,
          title: s('versionT'),
          divider: true,
          trailing: Row(
            mainAxisSize: MainAxisSize.min,
            children: [
              _Value(Settings.version.isEmpty ? '—' : Settings.version, mono: true),
              if (Updates.installs || Updates.notices) ...[
                const SizedBox(width: 8),
                Button(
                  kind: ButtonKind.text,
                  height: Tokens.smallButtonHeight,
                  fontSize: 12,
                  label: s('checkUpdates'),
                  onPress: () => unawaited(Updates.check()),
                ),
              ],
            ],
          ),
        ),
        ValueListenableBuilder(
          valueListenable: Updates.notice,
          builder: (context, notice, _) => notice == null
              ? ValueListenableBuilder(
                  valueListenable: Updates.checked,
                  builder: (context, checked, _) => switch (checked) {
                    CheckResult.upToDate => _SubRow(children: [Text(s('upToDate'), style: t.note)]),
                    CheckResult.failed => _SubRow(
                      children: [Text(s('updCheckFailT'), style: t.note.copyWith(color: t.bad))],
                    ),
                    _ => const SizedBox.shrink(),
                  },
                )
              : _SubRow(
                  children: [
                    Icon(FLucideIcons.arrowDown, size: 14, color: t.accent),
                    const SizedBox(width: 8),
                    Expanded(
                      child: Column(
                        crossAxisAlignment: CrossAxisAlignment.start,
                        children: [
                          Text(s('updAvailT', {'v': notice.version}), style: t.note),
                          Text(s('updAvailD'), style: t.note),
                          Text(notice.command, style: t.monoText(12, color: t.t2)),
                        ],
                      ),
                    ),
                    const SizedBox(width: 8),
                    CopyButton(text: notice.command, label: s('copyCommand')),
                  ],
                ),
        ),
        ValueListenableBuilder(
          valueListenable: Updates.autoUpdate,
          builder: (context, autoUpdate, _) => autoUpdate == null
              ? const SizedBox.shrink()
              : SettingRow(
                  icon: FLucideIcons.calendarClock,
                  title: s('autoUpdate'),
                  description: s('autoUpdateD'),
                  divider: true,
                  trailing: TriSwitch(
                    label: s('autoUpdate'),
                    state: autoUpdate ? SwitchState.on : SwitchState.off,
                    onChanged: (enabled) => unawaited(Updates.setAutoUpdate(enabled)),
                  ),
                ),
        ),
      ],
    );
  }
}

/// A line under a row, aligned with the row's title.
class _SubRow extends StatelessWidget {
  const _SubRow({required this.children});

  final List<Widget> children;

  @override
  Widget build(BuildContext context) => Padding(
    padding: const EdgeInsets.fromLTRB(20 + Tokens.rowIconSize + 14, 0, 20, 12),
    child: Row(children: children),
  );
}

/// RFC 3339 as the local date, YYYY-MM-DD.
String _date(String? rfc3339) {
  final time = rfc3339 == null ? null : DateTime.tryParse(rfc3339)?.toLocal();
  if (time == null) return '—';
  String two(int n) => n.toString().padLeft(2, '0');
  return '${time.year}-${two(time.month)}-${two(time.day)}';
}

class _Account extends StatelessWidget {
  const _Account({required this.app, required this.snapshot});

  final AppState app;
  final rust.UiSnapshot snapshot;

  @override
  Widget build(BuildContext context) {
    final s = S.current;
    return _Group(
      title: s('account'),
      children: [
        SettingRow(icon: FLucideIcons.user, title: s('user'), trailing: _Value(snapshot.accountName ?? '—')),
        SettingRow(
          icon: FLucideIcons.users,
          title: s('team'),
          divider: true,
          trailing: _Value(snapshot.teamName ?? '—'),
        ),
        SettingRow(
          icon: FLucideIcons.calendar,
          title: s('expires'),
          divider: true,
          trailing: _Value(_date(snapshot.expiresAt), mono: true),
        ),
        SettingRow(
          icon: FLucideIcons.logOut,
          title: s('signOut'),
          divider: true,
          trailing: Button(
            kind: ButtonKind.secondary,
            label: s('signOutBtn'),
            onPress: () => _confirmSignOut(context),
          ),
        ),
      ],
    );
  }

  Future<void> _confirmSignOut(BuildContext context) async {
    final s = S.current;
    final confirmed = await showFDialog<bool>(
      context: context,
      builder: (context, style, animation) => FDialog(
        animation: animation,
        semanticsLabel: s('signOutQ'),
        builder: (context, style) => ConfirmContent(title: s('signOutQ'), body: s('signOutD'), confirm: s('signOut')),
      ),
    );
    if (confirmed ?? false) await app.signOut();
  }
}

/// A confirmation's content: title, text, Cancel and the action. Pops
/// true for the action.
class ConfirmContent extends StatelessWidget {
  const ConfirmContent({required this.title, required this.body, required this.confirm});

  final String title;
  final String body;
  final String confirm;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    return Padding(
      padding: Tokens.dialogPadding,
      child: Column(
        mainAxisSize: MainAxisSize.min,
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Semantics(header: true, child: Text(title, style: t.bodyStrong.copyWith(fontSize: 15))),
          const SizedBox(height: 6),
          Text(body, style: t.sub),
          const SizedBox(height: 20),
          Row(
            mainAxisAlignment: MainAxisAlignment.end,
            children: [
              Button(
                kind: ButtonKind.secondary,
                label: S.current('cancel'),
                onPress: () => Navigator.of(context).pop(false),
              ),
              const SizedBox(width: 8),
              Button(
                kind: ButtonKind.primary,
                height: Tokens.controlHeight,
                label: confirm,
                onPress: () => Navigator.of(context).pop(true),
              ),
            ],
          ),
        ],
      ),
    );
  }
}

class _Advanced extends StatelessWidget {
  const _Advanced({required this.snapshot});

  final rust.UiSnapshot snapshot;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final s = S.current;
    final installed = snapshot.serviceInstalled;
    return _Group(
      title: s('advanced'),
      children: [
        // Development only (#519): release builds neither show nor honour it.
        if (Settings.apiOverrideAllowed) const _ApiOverride(),
        SettingRow(
          icon: FLucideIcons.server,
          title: s('service'),
          description: s('laterStep'),
          divider: true,
          trailing: Row(
            mainAxisSize: MainAxisSize.min,
            children: [
              Container(
                height: 22,
                padding: const EdgeInsets.symmetric(horizontal: 8),
                alignment: Alignment.center,
                decoration: BoxDecoration(
                  color: installed ? t.okSoft : t.sunk,
                  borderRadius: BorderRadius.circular(11),
                ),
                child: Text(
                  installed ? s('serviceOn') : s('serviceOff'),
                  style: t.note.copyWith(fontWeight: FontWeight.w500, color: installed ? t.ok : t.t3),
                ),
              ),
              const SizedBox(width: 10),
              // Enhanced mode is a later step: shown, not pressable.
              Button(
                kind: ButtonKind.secondary,
                label: installed ? s('uninstallBtn') : s('installBtn'),
                semanticsLabel: installed ? s('uninstall') : null,
                onPress: null,
              ),
            ],
          ),
        ),
        // Network diagnostics (#521): the DoH comparison only on the press.
        SettingRow(
          icon: FLucideIcons.globe,
          title: s('diagnostics'),
          description: s('diagnosticsSub'),
          divider: true,
          trailing: Button(
            kind: ButtonKind.secondary,
            label: s('diagnosticsRun'),
            onPress: () => unawaited(_showDiagnostics(context)),
          ),
        ),
      ],
    );
  }

  Future<void> _showDiagnostics(BuildContext context) => showFDialog<void>(
    context: context,
    builder: (context, style, animation) => FDialog(
      animation: animation,
      semanticsLabel: S.current('diagnostics'),
      builder: (context, style) => Padding(
        padding: Tokens.dialogPadding,
        child: SizedBox(
          width: 560,
          child: SingleChildScrollView(
            child: Column(
              mainAxisSize: MainAxisSize.min,
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                Semantics(
                  header: true,
                  child: Text(S.current('diagnostics'), style: Tokens.of(context).bodyStrong.copyWith(fontSize: 15)),
                ),
                const SizedBox(height: 6),
                const DiagnosticsView(),
                const SizedBox(height: 20),
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

/// The API override: a mono field under the title, the hint under it. It
/// saves on Enter and when focus leaves; the client reads it when it
/// starts, so the hint says it takes effect on the next launch.
class _ApiOverride extends StatefulWidget {
  const _ApiOverride();

  @override
  State<_ApiOverride> createState() => _ApiOverrideState();
}

class _ApiOverrideState extends State<_ApiOverride> {
  bool _invalid = false;
  bool _saved = false;

  void _submit(String text) {
    if (text.trim() == Settings.apiOverride.value) {
      setState(() => _invalid = false);
      return;
    }
    final ok = Settings.setApiOverride(text);
    setState(() {
      _invalid = !ok;
      _saved = ok;
    });
  }

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final s = S.current;
    final hint = _invalid ? s('apiInvalid') : (_saved ? s('apiSaved') : s('apiHintNext'));
    return Padding(
      padding: Tokens.fieldBlockPadding,
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          const RowIcon(icon: FLucideIcons.link),
          const SizedBox(width: 14),
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                SizedBox(
                  height: Tokens.rowIconSize,
                  child: Align(alignment: Alignment.centerLeft, child: Text(s('apiOverride'), style: t.rowTitle)),
                ),
                const SizedBox(height: 6),
                TextInput(
                  label: s('apiOverride'),
                  initial: Settings.apiOverride.value,
                  placeholder: s('apiPh'),
                  invalid: _invalid,
                  onSubmit: _submit,
                ),
                const SizedBox(height: 6),
                Semantics(
                  liveRegion: _invalid || _saved,
                  child: Text(hint, style: t.note.copyWith(color: _invalid ? t.bad : null)),
                ),
              ],
            ),
          ),
        ],
      ),
    );
  }
}
