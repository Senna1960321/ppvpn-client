// 我的 (#448 step 5), to the canvas's 手机 · 我的 (#535): the account card
// (radius 18 on the brand gradient, the expiry and the days left; a press
// switches teams), then 50-high rows with 17 titles and the accent-tinted
// icon tiles in two groups: 本机 (routing, logs, appearance, language,
// diagnostics; native) and 账户与帮助 (the web pages ↗, account deletion
// among them), 退出登录 in red after a confirmation, the version last. A
// row with a value pushes its choices.

import 'dart:async';

import 'package:flutter/widgets.dart';
import 'package:flutter_svg/flutter_svg.dart';
import 'package:forui/forui.dart';

import '../app_state.dart';
import '../distribution.dart';
import '../links.dart';
import '../pages/diagnostics.dart';
import '../pages/logs.dart';
import '../pages/settings.dart';
import '../settings.dart';
import '../src/rust/api.dart' as rust;
import '../strings.dart';
import '../theme.dart';
import '../updates.dart';
import '../widgets.dart';
import 'phone.dart';

class PhoneMe extends StatelessWidget {
  const PhoneMe({super.key, required this.app});

  final AppState app;

  @override
  Widget build(BuildContext context) => ValueListenableBuilder(
    valueListenable: app.snapshot,
    builder: (context, snapshot, _) {
      final s = S.current;
      return ListView(
        padding: phoneScrollPadding(context),
        children: [
          _AccountCard(app: app, snapshot: snapshot),
          PhoneCaption(text: s('thisDevice')),
          PhoneCard(
            inset: 58,
            children: [
              _Row(
                icon: FLucideIcons.split,
                title: s('routing'),
                value: snapshot.routingMode == rust.UiRoutingMode.rules ? s('rules') : s('global'),
                onPress: () => pushPhoneChoice<rust.UiRoutingMode>(
                  context,
                  title: s('routing'),
                  options: [(rust.UiRoutingMode.rules, s('rules')), (rust.UiRoutingMode.global, s('global'))],
                  value: snapshot.routingMode,
                  onChanged: (mode) => unawaited(app.setRoutingMode(mode)),
                ),
              ),
              _Row(
                icon: FLucideIcons.list,
                title: s('logs'),
                onPress: () => unawaited(
                  pushPhonePage<void>(context, PhoneScaffold(title: s('logs'), back: true, child: const LogsPage())),
                ),
              ),
              ValueListenableBuilder(
                valueListenable: Settings.appearance,
                builder: (context, appearance, _) {
                  final options = [
                    (rust.UiAppearance.system, s('sys')),
                    (rust.UiAppearance.light, s('light')),
                    (rust.UiAppearance.dark, s('dark')),
                  ];
                  return _Row(
                    icon: FLucideIcons.moon,
                    title: s('appearance'),
                    value: _name(options, appearance),
                    onPress: () => pushPhoneChoice(
                      context,
                      title: s('appearance'),
                      options: options,
                      value: appearance,
                      onChanged: Settings.setAppearance,
                    ),
                  );
                },
              ),
              ValueListenableBuilder(
                valueListenable: Settings.language,
                builder: (context, language, _) {
                  // Each language in its own name.
                  final options = [
                    (rust.UiLanguage.system, s('sys')),
                    (rust.UiLanguage.chinese, '中文'),
                    (rust.UiLanguage.english, 'English'),
                  ];
                  return _Row(
                    icon: FLucideIcons.languages,
                    title: s('language'),
                    value: _name(options, language),
                    onPress: () => pushPhoneChoice(
                      context,
                      title: s('language'),
                      options: options,
                      value: language,
                      onChanged: Settings.setLanguage,
                    ),
                  );
                },
              ),
              // Network diagnostics (#521): its page says the DoH resolvers
              // are asked only when the user presses Run.
              _Row(
                icon: FLucideIcons.globe,
                title: s('diagnostics'),
                onPress: () => unawaited(
                  pushPhonePage<void>(
                    context,
                    PhoneScaffold(
                      title: s('diagnostics'),
                      back: true,
                      child: const SingleChildScrollView(
                        padding: EdgeInsets.fromLTRB(16, 8, 16, 24),
                        child: DiagnosticsView(),
                      ),
                    ),
                  ),
                ),
              ),
            ],
          ),
          PhoneCaption(text: s('accountHelp'), trailing: s('openInWeb')),
          PhoneCard(
            inset: 58,
            children: [
              // The website's console (ppvpn-web apps/client, under
              // /dashboard): subscriptions, and tickets for help. It has no
              // device page. Not on iOS: a way to buy a plan outside in-app
              // purchase is refused by App Review (guideline 3.1.1, #473); a
              // plan bought elsewhere still works in the app.
              if (externalPurchases)
                _Row(
                  icon: FLucideIcons.creditCard,
                  title: s('planWeb'),
                  web: true,
                  onPress: () => _open('/dashboard/subscriptions'),
                ),
              _Row(
                icon: FLucideIcons.megaphone,
                title: s('helpWeb'),
                web: true,
                onPress: () => _open('/dashboard/tickets'),
              ),
              // The site's deletion page (signed out, it signs in first, then
              // comes back): it confirms again and deletes the account.
              // App Store and Play need it reachable from the app (#473).
              _Row(
                icon: FLucideIcons.userX,
                title: s('deleteAccountWeb'),
                web: true,
                onPress: () => _open('/account/delete'),
              ),
              if (!playDistribution)
                _Row(icon: FLucideIcons.info, title: s('about'), web: true, onPress: () => _open('')),
              if (Updates.installs)
                _Row(
                  icon: FLucideIcons.download,
                  title: s('checkUpdates'),
                  onPress: () => unawaited(
                    pushPhonePage<void>(
                      context,
                      PhoneScaffold(
                        title: s('checkUpdates'),
                        back: true,
                        child: const Padding(padding: EdgeInsets.all(16), child: VersionSettings()),
                      ),
                    ),
                  ),
                ),
            ],
          ),
          const SizedBox(height: 18),
          _SignOut(onPress: () => unawaited(_confirmSignOut(context))),
          if (Settings.version.isNotEmpty)
            Padding(
              padding: const EdgeInsets.only(top: 14),
              child: Text(
                '${s('app')} ${Settings.version}',
                textAlign: TextAlign.center,
                style: Tokens.of(context).monoText(11, color: Tokens.of(context).t3, height: 16),
              ),
            ),
        ],
      );
    },
  );

  /// The site's pages behind the ↗ rows, in the system browser for now (the
  /// in-app browser and the one-time sign-in link come with the web pages,
  /// #387), on the configured backend's site (#517).
  void _open(String path) {
    try {
      unawaited(openUrl(rust.siteUrl(path: path)));
    } catch (error) {
      debugPrint('siteUrl: $error');
    }
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

String _name<T>(List<(T, String)> options, T value) =>
    options.firstWhere((option) => option.$1 == value, orElse: () => options.first).$2;

/// The account card: the avatar, the name, the team (个人 for the personal
/// space) with the days left, white on the brand gradient. A press lists
/// the teams to switch to (as the desktop's account menu, #443).
class _AccountCard extends StatelessWidget {
  const _AccountCard({required this.app, required this.snapshot});

  final AppState app;
  final rust.UiSnapshot snapshot;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final s = S.current;
    const white = Color(0xFFFFFFFF);
    const soft = Color(0xC7FFFFFF);
    final name = snapshot.accountName ?? '';
    final initial = name.isEmpty ? '?' : name.characters.first.toUpperCase();
    final days = _daysLeft(snapshot.expiresAt);
    final until = snapshot.expiresAt == null ? null : DateTime.tryParse(snapshot.expiresAt!)?.toLocal();
    final team = snapshot.teamName ?? '—';
    String two(int n) => n.toString().padLeft(2, '0');
    return Pressable(
      key: const ValueKey('account'),
      onPress: () => _teams(context),
      label: '${s('account')}, $name, $team',
      radius: Tokens.phoneAccountRadius,
      builder: (context, _) => Container(
        clipBehavior: Clip.antiAlias,
        decoration: BoxDecoration(
          gradient: const LinearGradient(
            begin: Alignment(-0.64, -0.77),
            end: Alignment(0.64, 0.77),
            stops: [0, 0.5, 1],
            colors: [Color(0xFF2C56B4), Color(0xFF1F4390), Color(0xFF152F6A)],
          ),
          borderRadius: BorderRadius.circular(Tokens.phoneAccountRadius),
          boxShadow: [
            BoxShadow(
              color: t.brightness == Brightness.light ? const Color(0x381F4390) : const Color(0x73000000),
              offset: const Offset(0, 10),
              blurRadius: 28,
            ),
          ],
        ),
        child: Stack(
          children: [
            // The brand's mountains in the bottom right corner.
            Positioned(
              right: -10,
              bottom: -6,
              child: ExcludeSemantics(
                child: SvgPicture.string(
                  '<svg viewBox="0 0 200 90" xmlns="http://www.w3.org/2000/svg">'
                  '<path d="M0 90 L60 30 L95 62 L135 14 L200 80 L200 90 Z" fill="#ffffff" fill-opacity="0.07"/>'
                  '<path d="M40 90 L95 50 L130 74 L165 44 L200 70 L200 90 Z" fill="#ffffff" fill-opacity="0.08"/></svg>',
                  width: 200,
                  height: 90,
                ),
              ),
            ),
            Padding(
              padding: const EdgeInsets.fromLTRB(18, 16, 18, 16),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  Row(
                    children: [
                      Container(
                        width: 46,
                        height: 46,
                        alignment: Alignment.center,
                        decoration: BoxDecoration(
                          color: const Color(0x2EFFFFFF),
                          shape: BoxShape.circle,
                          border: Border.all(color: const Color(0x40FFFFFF)),
                        ),
                        child: Text(initial, style: t.phoneBodyStrong.copyWith(fontSize: 19, color: white)),
                      ),
                      const SizedBox(width: 12),
                      Expanded(
                        child: Column(
                          crossAxisAlignment: CrossAxisAlignment.start,
                          children: [
                            Text(
                              name.isEmpty ? '—' : name,
                              maxLines: 1,
                              overflow: TextOverflow.ellipsis,
                              style: t.phoneRowTitle.copyWith(fontWeight: FontWeight.w600, color: white),
                            ),
                            Text(
                              team,
                              maxLines: 1,
                              overflow: TextOverflow.ellipsis,
                              style: t.phoneNote.copyWith(color: soft),
                            ),
                          ],
                        ),
                      ),
                      const Icon(FLucideIcons.chevronRight, size: 16, color: soft),
                    ],
                  ),
                  if (until != null) ...[
                    const SizedBox(height: 14),
                    // Side by side, one under the other at large text sizes.
                    Wrap(
                      alignment: WrapAlignment.spaceBetween,
                      spacing: 12,
                      runSpacing: 2,
                      children: [
                        Text(
                          s('validUntil', {'d': '${until.year}-${two(until.month)}-${two(until.day)}'}),
                          style: t.phoneNote.copyWith(fontSize: 12, color: soft),
                        ),
                        if (days != null)
                          Text(s('daysLeft', {'n': '$days'}), style: t.phoneNote.copyWith(fontSize: 12, color: soft)),
                      ],
                    ),
                  ],
                ],
              ),
            ),
          ],
        ),
      ),
    );
  }

  /// The teams, pushed: the current one checked, a disabled one greyed
  /// with 已停用; a press switches and goes back.
  void _teams(BuildContext context) {
    unawaited(app.refreshTeams());
    unawaited(
      pushPhonePage<void>(
        context,
        PhoneScaffold(
          title: S.current('switchTeam'),
          back: true,
          child: ValueListenableBuilder(
            valueListenable: app.teams,
            builder: (context, teams, _) {
              final t = Tokens.of(context);
              final current = app.snapshot.value.teamId;
              if (teams.isEmpty) return Center(child: Spinner(color: t.accent, size: 20));
              return ListView(
                padding: phoneScrollPadding(context),
                children: [
                  Container(
                    clipBehavior: Clip.antiAlias,
                    decoration: t.phoneCardDecoration(),
                    child: Column(
                      children: [
                        for (final (i, team) in teams.indexed)
                          Pressable(
                            key: ValueKey('team-${team.id}'),
                            radius: 0,
                            selected: team.id == current,
                            label: team.personal ? S.current('personal') : team.name,
                            onPress: !team.active || team.id == current
                                ? null
                                : () {
                                    unawaited(app.switchTeam(team.id));
                                    unawaited(Navigator.of(context).maybePop());
                                  },
                            builder: (context, pressed) => Container(
                              constraints: const BoxConstraints(minHeight: Tokens.phoneRow),
                              padding: const EdgeInsets.symmetric(horizontal: 16),
                              decoration: BoxDecoration(
                                color: pressed ? t.phonePress : null,
                                border: i == 0 ? null : Border(top: BorderSide(color: t.line, width: 0.5)),
                              ),
                              child: Row(
                                children: [
                                  Expanded(
                                    child: Text(
                                      team.personal ? S.current('personal') : team.name,
                                      maxLines: 1,
                                      overflow: TextOverflow.ellipsis,
                                      style: t.phoneRowTitle,
                                    ),
                                  ),
                                  if (!team.active) Text(S.current('teamDisabled'), style: t.phoneNote),
                                  if (team.id == current) Icon(FLucideIcons.check, size: 18, color: t.accent),
                                ],
                              ),
                            ),
                          ),
                      ],
                    ),
                  ),
                ],
              );
            },
          ),
        ),
      ),
    );
  }
}

/// Whole days until [rfc3339], none when unknown or past.
int? _daysLeft(String? rfc3339) {
  final time = rfc3339 == null ? null : DateTime.tryParse(rfc3339);
  if (time == null) return null;
  final days = time.difference(DateTime.now()).inDays;
  return days < 0 ? null : days;
}

/// A 50 row: the 32 icon tile, the 17 title, the value in t3 (14), then
/// the chevron, or ↗ for a web page.
class _Row extends StatelessWidget {
  const _Row({required this.icon, required this.title, required this.onPress, this.value, this.web = false});

  final IconData icon;
  final String title;
  final String? value;
  final bool web;
  final VoidCallback onPress;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    return Pressable(
      radius: 0,
      onPress: onPress,
      label: value == null ? title : '$title, $value',
      builder: (context, pressed) => AnimatedContainer(
        duration: Tokens.motion(context, pressed ? Duration.zero : Tokens.motionFast),
        color: pressed ? t.phonePress : t.phonePress.withValues(alpha: 0),
        constraints: const BoxConstraints(minHeight: Tokens.phoneRow),
        padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 6),
        child: Row(
          children: [
            PhoneTile(icon: icon),
            const SizedBox(width: 12),
            Expanded(
              child: Text(
                title,
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
                style: t.phoneRowTitle.copyWith(letterSpacing: -0.34),
              ),
            ),
            if (value case final text?) ...[
              const SizedBox(width: 8),
              Text(text, style: t.phoneBody.copyWith(fontSize: 14, height: 20 / 14, color: t.t3)),
            ],
            const SizedBox(width: 6),
            Icon(web ? FLucideIcons.arrowUpRight : FLucideIcons.chevronRight, size: 16, color: t.t3),
          ],
        ),
      ),
    );
  }
}

/// 退出登录: a 50 card, the text red (15 · 600) and centred.
class _SignOut extends StatelessWidget {
  const _SignOut({required this.onPress});

  final VoidCallback onPress;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    return Pressable(
      key: const ValueKey('sign-out'),
      onPress: onPress,
      label: S.current('signOut'),
      radius: Tokens.phoneCardRadius,
      builder: (context, pressed) => Container(
        constraints: const BoxConstraints(minHeight: Tokens.phoneRow),
        padding: const EdgeInsets.symmetric(vertical: 8),
        alignment: Alignment.center,
        decoration: t.phoneCardDecoration(color: pressed ? Color.alphaBlend(t.phonePress, t.card) : null),
        child: Text(
          S.current('signOut'),
          style: t.phoneBodyStrong.copyWith(fontSize: 15, height: 20 / 15, color: t.bad),
        ),
      ),
    );
  }
}
