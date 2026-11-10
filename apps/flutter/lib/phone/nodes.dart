// The phone nodes page (#448 step 5), to the canvas's 手机 · 节点 (#535):
// the test method and 全部测速 in the top bar, the search field, the region
// chips (the selected one solid accent), then the current node in its own
// highlighted card and the others grouped by region under captions, 62 a
// row (flag, name, the tier tag and the lines below, latency) with 0.5 px
// separators. A tap makes a node the current one; pulling down refreshes
// the list.

import 'dart:async';

import 'package:flutter/cupertino.dart' show CupertinoSliverRefreshControl;
import 'package:flutter/services.dart' show TextInputAction;
import 'package:flutter/widgets.dart';
import 'package:forui/forui.dart';

import '../app_state.dart';
import '../pages/nodes.dart';
import '../pages/overview.dart';
import '../src/rust/api.dart' as rust;
import '../strings.dart';
import '../theme.dart';
import '../widgets.dart';
import 'phone.dart';

class PhoneNodes extends StatefulWidget {
  const PhoneNodes({super.key, required this.app});

  final AppState app;

  @override
  State<PhoneNodes> createState() => _PhoneNodesState();
}

class _PhoneNodesState extends State<PhoneNodes> {
  rust.UiProbeMethod _method = rust.UiProbeMethod.tcp;
  bool _testing = false;
  String _query = '';

  /// The region chip; null for 全部.
  String? _region;

  AppState get app => widget.app;

  Future<void> _testAll() async {
    setState(() => _testing = true);
    await app.testAll(_method);
    if (mounted) setState(() => _testing = false);
  }

  @override
  Widget build(BuildContext context) => ValueListenableBuilder(
    valueListenable: app.snapshot,
    builder: (context, snapshot, _) => PhoneScaffold(
      title: S.current('nodes'),
      trailing: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          _MethodButton(
            method: _method,
            onPress: () => setState(
              () => _method = switch (_method) {
                rust.UiProbeMethod.icmp => rust.UiProbeMethod.tcp,
                rust.UiProbeMethod.tcp => rust.UiProbeMethod.connect,
                rust.UiProbeMethod.connect => rust.UiProbeMethod.icmp,
              },
            ),
          ),
          const SizedBox(width: 8),
          _TestAllButton(testing: _testing, onPress: _testing || !snapshot.standardReady ? null : _testAll),
        ],
      ),
      child: _body(context, snapshot),
    ),
  );

  Widget _body(BuildContext context, rust.UiSnapshot snapshot) {
    final limited = LimitedAccess.of(snapshot);
    if (limited != null) return limited;
    return ValueListenableBuilder(
      valueListenable: app.nodes,
      builder: (context, nodes, _) {
        if (nodes.isEmpty) {
          return snapshot.profile == rust.UiProfile.invalid
              ? Center(
                  child: Padding(
                    padding: const EdgeInsets.all(Tokens.phonePadding),
                    child: ConfigWarning(previous: snapshot.profilePrevious),
                  ),
                )
              : const LoadingNodes();
        }
        final regions = <String>{
          for (final node in nodes)
            if ((node.exitRegion ?? '').isNotEmpty) node.exitRegion!,
        }.toList();
        // A refreshed profile can remove the selected region. Clear it before
        // the single-region layout hides the controls for changing it.
        if (_region != null && !regions.contains(_region)) _region = null;
        final query = _query.trim().toLowerCase();
        final shown = [
          for (final node in nodes)
            if ((_region == null || node.exitRegion == _region) &&
                (query.isEmpty ||
                    nodeTitle(node).toLowerCase().contains(query) ||
                    (node.exitRegion ?? '').toLowerCase().contains(query)))
              node,
        ];
        final padding = phoneScrollPadding(context);
        final current = shown.where((node) => node.id == snapshot.selectedNodeId).firstOrNull;
        // The others by region, in the list's order; a group's count is
        // the region's.
        final groups = <String, List<rust.UiNode>>{};
        final counts = <String, int>{};
        for (final node in shown) {
          final region = node.exitRegion ?? '';
          counts[region] = (counts[region] ?? 0) + 1;
          if (node.id != current?.id) (groups[region] ??= []).add(node);
        }
        final s = S.current;
        return Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            MediaQuery.withClampedTextScaling(
              maxScaleFactor: Tokens.phoneChromeTextMax,
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  Padding(
                    padding: const EdgeInsets.fromLTRB(Tokens.phonePadding, 4, Tokens.phonePadding, 12),
                    child: _SearchField(onChanged: (value) => setState(() => _query = value)),
                  ),
                  if (regions.length > 1)
                    _RegionChips(
                      regions: regions,
                      selected: _region,
                      onSelect: (region) => setState(() => _region = region),
                    ),
                ],
              ),
            ),
            Expanded(
              child: CustomScrollView(
                physics: const BouncingScrollPhysics(parent: AlwaysScrollableScrollPhysics()),
                slivers: [
                  CupertinoSliverRefreshControl(onRefresh: app.refreshNodes),
                  SliverPadding(
                    padding: padding.copyWith(top: 0),
                    sliver: shown.isEmpty
                        ? SliverToBoxAdapter(
                            child: Padding(
                              padding: const EdgeInsets.only(top: 40),
                              child: Text(
                                s('noMatches'),
                                textAlign: TextAlign.center,
                                style: Tokens.of(context).phoneNote,
                              ),
                            ),
                          )
                        : ValueListenableBuilder(
                            valueListenable: app.latency,
                            builder: (context, latency, _) {
                              Widget rowOf(rust.UiNode node) => _NodeRow(
                                node: node,
                                current: node.id == snapshot.selectedNodeId,
                                latency: latency[node.id],
                                onPress: () => unawaited(app.selectNode(node.id)),
                              );
                              return SliverList.list(
                                children: [
                                  if (current != null) ...[
                                    PhoneCaption(text: s('currentNode')),
                                    PhoneCard(
                                      decoration: _currentDecoration(Tokens.of(context)),
                                      children: [rowOf(current)],
                                    ),
                                  ],
                                  for (final MapEntry(key: region, value: nodes) in groups.entries) ...[
                                    PhoneCaption(
                                      text: region.isEmpty ? s('allRegions') : region,
                                      trailing: s('nodesCount', {'n': '${counts[region]}'}),
                                    ),
                                    PhoneCard(inset: 56, children: [for (final node in nodes) rowOf(node)]),
                                  ],
                                ],
                              );
                            },
                          ),
                  ),
                ],
              ),
            ),
          ],
        );
      },
    );
  }

  /// The current node's card: the accent ring (1.5) and glow over a
  /// faint accent wash at the top.
  static BoxDecoration _currentDecoration(Tokens t) => BoxDecoration(
    gradient: LinearGradient(
      begin: Alignment.topCenter,
      end: Alignment.bottomCenter,
      stops: const [0, 0.7],
      colors: [Color.alphaBlend(t.accent.withValues(alpha: 0.05), t.card), t.card],
    ),
    borderRadius: BorderRadius.circular(Tokens.phoneCardRadius),
    border: Border.all(color: t.accent, width: 1.5),
    boxShadow: [BoxShadow(color: t.accent.withValues(alpha: 0.14), offset: const Offset(0, 6), blurRadius: 18)],
  );
}

/// The test method as a 36 pill in the accent colour on accent-soft (Ping,
/// TCP, Connect); a press moves to the next.
class _MethodButton extends StatelessWidget {
  const _MethodButton({required this.method, required this.onPress});

  final rust.UiProbeMethod method;
  final VoidCallback onPress;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final name = switch (method) {
      rust.UiProbeMethod.icmp => 'Ping',
      rust.UiProbeMethod.tcp => 'TCP',
      rust.UiProbeMethod.connect => 'Connect',
    };
    return Pressable(
      key: const ValueKey('probe-method'),
      onPress: onPress,
      label: '${S.current('probe')}, $name',
      radius: Tokens.phoneSmallButton / 2,
      builder: (context, _) => SizedBox(
        height: Tokens.phoneControl,
        child: Center(
          child: Container(
            height: Tokens.phoneSmallButton,
            padding: const EdgeInsets.symmetric(horizontal: 12),
            alignment: Alignment.center,
            decoration: BoxDecoration(
              color: t.accentSoft,
              borderRadius: BorderRadius.circular(Tokens.phoneSmallButton / 2),
            ),
            child: Row(
              mainAxisSize: MainAxisSize.min,
              children: [
                Text(name, style: t.phoneBodyStrong.copyWith(fontSize: 14, height: 20 / 14, color: t.accent)),
                const SizedBox(width: 5),
                Icon(FLucideIcons.chevronDown, size: 12, color: t.accent),
              ],
            ),
          ),
        ),
      ),
    );
  }
}

/// 全部测速: the primary pill (36, the gradient, the bolt and the label);
/// the spinner while the test runs.
class _TestAllButton extends StatelessWidget {
  const _TestAllButton({required this.testing, required this.onPress});

  final bool testing;
  final VoidCallback? onPress;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final light = t.brightness == Brightness.light;
    final label = S.current('testAll');
    return Pressable(
      key: const ValueKey('test-all'),
      onPress: onPress,
      label: label,
      radius: Tokens.phoneSmallButton / 2,
      builder: (context, _) => SizedBox(
        height: Tokens.phoneControl,
        child: Center(
          child: Container(
            height: Tokens.phoneSmallButton,
            padding: const EdgeInsets.symmetric(horizontal: 14),
            decoration: BoxDecoration(
              gradient: light ? Tokens.primaryGradient : null,
              color: light ? null : t.accent,
              borderRadius: BorderRadius.circular(Tokens.phoneSmallButton / 2),
              boxShadow: light
                  ? const [BoxShadow(color: Color(0x471F4390), offset: Offset(0, 4), blurRadius: 12)]
                  : null,
            ),
            child: Row(
              mainAxisSize: MainAxisSize.min,
              children: [
                if (testing)
                  Spinner(color: t.onAccent, size: 14)
                else
                  Icon(FLucideIcons.zap, size: 14, color: t.onAccent),
                const SizedBox(width: 6),
                Text(label, style: t.phoneBodyStrong.copyWith(fontSize: 14, height: 20 / 14, color: t.onAccent)),
              ],
            ),
          ),
        ),
      ),
    );
  }
}

/// The search field: 44 high on the phone's sunk fill, radius 12.
class _SearchField extends StatefulWidget {
  const _SearchField({required this.onChanged});

  final ValueChanged<String> onChanged;

  @override
  State<_SearchField> createState() => _SearchFieldState();
}

class _SearchFieldState extends State<_SearchField> {
  final TextEditingController _controller = TextEditingController();
  final FocusNode _focus = FocusNode();

  @override
  void dispose() {
    _controller.dispose();
    _focus.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final hint = S.current('searchNodes');
    final style = t.phoneBody;
    return GestureDetector(
      onTap: _focus.requestFocus,
      child: Semantics(
        label: hint,
        textField: true,
        child: Container(
          height: Tokens.phoneControl,
          padding: const EdgeInsets.symmetric(horizontal: 12),
          decoration: BoxDecoration(
            color: t.phoneSunk,
            borderRadius: BorderRadius.circular(Tokens.phoneControlRadius),
          ),
          child: Row(
            children: [
              Icon(FLucideIcons.search, size: 18, color: t.t3),
              const SizedBox(width: 8),
              Expanded(
                child: Stack(
                  alignment: Alignment.centerLeft,
                  children: [
                    ValueListenableBuilder(
                      valueListenable: _controller,
                      builder: (context, value, _) => value.text.isEmpty
                          ? ExcludeSemantics(child: Text(hint, maxLines: 1, style: style.copyWith(color: t.t3)))
                          : const SizedBox(),
                    ),
                    EditableText(
                      controller: _controller,
                      focusNode: _focus,
                      style: style,
                      cursorColor: t.accent,
                      backgroundCursorColor: t.t3,
                      selectionColor: t.accentSoft,
                      maxLines: 1,
                      autocorrect: false,
                      textInputAction: TextInputAction.search,
                      onChanged: widget.onChanged,
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

/// The regions as chips (34 high), scrolling sideways; 全部 first.
class _RegionChips extends StatelessWidget {
  const _RegionChips({required this.regions, required this.selected, required this.onSelect});

  final List<String> regions;
  final String? selected;
  final ValueChanged<String?> onSelect;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    Widget chip(String? region) {
      final on = region == selected;
      final text = region ?? S.current('allRegions');
      return Pressable(
        onPress: () => onSelect(region),
        selected: on,
        label: text,
        radius: 17,
        builder: (context, _) => AnimatedContainer(
          duration: Tokens.motion(context, Tokens.motionFast),
          curve: Tokens.motionFastCurve,
          height: 34,
          padding: const EdgeInsets.symmetric(horizontal: 15),
          alignment: Alignment.center,
          decoration: BoxDecoration(color: on ? t.accent : t.phoneSunk, borderRadius: BorderRadius.circular(17)),
          child: Text(
            text,
            style: t.phoneBody.copyWith(
              fontSize: 14,
              height: 20 / 14,
              fontWeight: on ? FontWeight.w600 : FontWeight.w500,
              color: on ? t.onAccent : t.t2,
            ),
          ),
        ),
      );
    }

    return SizedBox(
      height: 34,
      child: ListView(
        scrollDirection: Axis.horizontal,
        padding: const EdgeInsets.symmetric(horizontal: Tokens.phonePadding),
        children: [
          for (final (i, region) in <String?>[null, ...regions].indexed) ...[
            if (i > 0) const SizedBox(width: 8),
            chip(region),
          ],
        ],
      ),
    );
  }
}

class _NodeRow extends StatelessWidget {
  const _NodeRow({required this.node, required this.current, required this.latency, required this.onPress});

  final rust.UiNode node;
  final bool current;
  final Latency? latency;
  final VoidCallback onPress;

  @override
  Widget build(BuildContext context) {
    final t = Tokens.of(context);
    final lines = linesText(node);
    final tier = (node.entryLabel ?? '').isNotEmpty;
    return Pressable(
      radius: 0,
      selected: current,
      label: nodeTitle(node),
      onPress: onPress,
      builder: (context, pressed) => AnimatedContainer(
        duration: Tokens.motion(context, pressed ? Duration.zero : Tokens.motionFast),
        color: pressed ? t.phonePress : t.phonePress.withValues(alpha: 0),
        constraints: const BoxConstraints(minHeight: Tokens.phoneRowTwoLines),
        padding: const EdgeInsetsDirectional.fromSTEB(16, 8, 14, 8),
        child: Row(
          children: [
            Flag(countryCode: node.exitCountryCode, size: Tokens.phoneFlagSize),
            const SizedBox(width: 12),
            Expanded(
              child: Column(
                mainAxisAlignment: MainAxisAlignment.center,
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  // Two lines at large text sizes before it is cut.
                  Text(
                    nodeTitle(node),
                    maxLines: 2,
                    overflow: TextOverflow.ellipsis,
                    style: t.phoneBody.copyWith(
                      fontWeight: current ? FontWeight.w600 : FontWeight.w500,
                      letterSpacing: -0.16,
                    ),
                  ),
                  if (tier || lines.isNotEmpty) ...[
                    const SizedBox(height: 3),
                    Row(
                      children: [
                        if (tier) TierTag(label: node.entryLabel, entryKey: node.entryKey),
                        if (tier && lines.isNotEmpty) const SizedBox(width: 6),
                        if (lines.isNotEmpty)
                          Flexible(
                            child: Text(
                              lines,
                              maxLines: 1,
                              overflow: TextOverflow.ellipsis,
                              style: t.monoText(12, color: t.t3, height: 16),
                            ),
                          ),
                      ],
                    ),
                  ],
                ],
              ),
            ),
            const SizedBox(width: 8),
            LatencyPill(latency: latency),
            // The check grows in from 0.6 as the node becomes current.
            AnimatedSwitcher(
              duration: Tokens.motion(context, Tokens.motionBase),
              switchInCurve: Tokens.motionCurve,
              transitionBuilder: (child, animation) => FadeTransition(
                opacity: animation,
                child: ScaleTransition(scale: Tween(begin: 0.6, end: 1.0).animate(animation), child: child),
              ),
              child: current
                  ? Padding(
                      key: const ValueKey('check'),
                      padding: const EdgeInsetsDirectional.only(start: 8),
                      child: Container(
                        width: 22,
                        height: 22,
                        decoration: BoxDecoration(color: t.accent, shape: BoxShape.circle),
                        child: Icon(FLucideIcons.check, size: 13, color: t.onAccent),
                      ),
                    )
                  : const SizedBox.shrink(),
            ),
          ],
        ),
      ),
    );
  }
}
