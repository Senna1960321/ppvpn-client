import 'dart:async';

import 'package:flutter_test/flutter_test.dart';
import 'package:ppvpn_flutter/app_state.dart';
import 'package:ppvpn_flutter/src/rust/api.dart';
import 'package:ppvpn_flutter/src/rust/frb_generated.dart';

UiSnapshot state(UiAuth auth, [String? name, String? team]) => UiSnapshot(
  auth: auth,
  deviceCode: null,
  accountName: name,
  teamName: null,
  teamId: team,
  profile: UiProfile.loading,
  profilePrevious: false,
  profileRevision: null,
  expiresAt: null,
  phase: UiPhase.off,
  lineKey: null,
  latencyMs: null,
  standardReady: false,
  selectedNodeId: null,
  error: null,
  unread: 0,
  routingMode: UiRoutingMode.rules,
  connectionMode: UiConnectionMode.compatible,
  serviceInstalled: false,
  canTakeOver: false,
  suggestCompatible: false,
  apiUnreachable: false,
);
const oldTeam = UiTeam(id: 'old', name: 'Previous account private team', personal: false, active: true);
const newTeam = UiTeam(id: 'new', name: 'Current account team', personal: false, active: true);

class TeamApi implements RustLibApi {
  final requests = <Completer<List<UiTeam>>>[];
  Completer<UiEvents>? events;
  Completer<void>? signOut;
  @override
  Future<void> crateApiSignOut() {
    signOut = Completer<void>();
    return signOut!.future;
  }

  @override
  Future<UiEvents> crateApiNextEvents() {
    events = Completer<UiEvents>();
    return events!.future;
  }

  @override
  Future<List<UiTeam>> crateApiTeams() {
    final request = Completer<List<UiTeam>>();
    requests.add(request);
    return request.future;
  }

  Future<void> publish(UiSnapshot snapshot) async {
    events!.complete(UiEvents(snapshot: snapshot, probes: []));
    await Future<void>.delayed(Duration.zero);
  }

  @override
  void crateApiShutdown() => events?.complete(const UiEvents(probes: []));
  @override
  dynamic noSuchMethod(Invocation invocation) => super.noSuchMethod(invocation);
}

void main() {
  late TeamApi api;
  late AppState app;
  setUp(() {
    api = TeamApi();
    RustLib.initMock(api: api);
    app = AppState(state(UiAuth.signedIn, 'First account'));
  });
  tearDown(() async {
    app.stop();
    await Future<void>.delayed(Duration.zero);
    RustLib.dispose();
  });
  test('loaded team cache is discarded on sign out', () async {
    final request = app.refreshTeams();
    api.requests.single.complete([oldTeam]);
    await request;
    expect(app.teams.value.single.id, 'old');
    await api.publish(state(UiAuth.signedOut));
    expect(app.teams.value, isEmpty);
  });
  test('late old account response cannot replace current account teams', () async {
    final old = app.refreshTeams();
    await api.publish(state(UiAuth.signedOut));
    await api.publish(state(UiAuth.signedIn, 'Second account'));
    final current = app.refreshTeams();
    api.requests.last.complete([newTeam]);
    await current;
    api.requests.first.complete([oldTeam]);
    await old;
    expect(app.teams.value.map((team) => team.id), ['new']);
  });
  test('a coalesced sign out and same-name login invalidates before Rust replies', () async {
    final request = app.refreshTeams();
    api.requests.single.complete([oldTeam]);
    await request;
    final pending = app.signOut();
    expect(app.teams.value, isEmpty);
    api.signOut!.complete();
    await pending;
  });
  test('account replacement without an observed signed-out event clears choices', () async {
    final request = app.refreshTeams();
    api.requests.single.complete([oldTeam]);
    await request;
    await api.publish(state(UiAuth.signedIn, 'Second account'));
    expect(app.teams.value, isEmpty);
  });
  test('a newer refresh wins within the same account', () async {
    final first = app.refreshTeams();
    final second = app.refreshTeams();
    api.requests.last.complete([newTeam]);
    await second;
    api.requests.first.complete([oldTeam]);
    await first;
    expect(app.teams.value.map((team) => team.id), ['new']);
  });
  test('a stale failed refresh cannot raise an error in the next account', () async {
    final old = app.refreshTeams();
    await api.publish(state(UiAuth.signedOut));
    await api.publish(state(UiAuth.signedIn, 'Second account'));
    api.requests.single.completeError(StateError('previous account request failed'));
    await old;
    expect(app.actionError.value, isNull);
  });
  test('switching teams within one account keeps its choices', () async {
    final request = app.refreshTeams();
    api.requests.single.complete([oldTeam, newTeam]);
    await request;
    await api.publish(state(UiAuth.signedIn, 'First account', 'new'));
    expect(app.teams.value.map((team) => team.id), ['old', 'new']);
  });
  test('same account refresh remains functional', () async {
    final request = app.refreshTeams();
    api.requests.single.complete([newTeam]);
    await request;
    expect(app.teams.value.map((team) => team.id), ['new']);
  });
  test('initial account metadata does not discard an in-flight team refresh', () async {
    await api.publish(state(UiAuth.signedOut));
    await api.publish(state(UiAuth.signedIn));
    final request = app.refreshTeams();
    await api.publish(state(UiAuth.signedIn, 'First account'));
    api.requests.single.complete([newTeam]);
    await request;
    expect(app.teams.value.map((team) => team.id), ['new']);
  });

  test('explicit logout rejects the old response even when auth snapshots are coalesced', () async {
    final request = app.refreshTeams();
    final logout = app.signOut();
    // The UI still holds the first SignedIn snapshot: the pump may only see
    // SignedIn again after login to an account with the same display name.
    api.requests.single.complete([oldTeam]);
    await request;
    expect(app.teams.value, isEmpty);
    api.signOut!.complete();
    await logout;
  });

  test('a current failed refresh preserves current choices and reports its error', () async {
    final first = app.refreshTeams();
    api.requests.single.complete([newTeam]);
    await first;
    final refresh = app.refreshTeams();
    api.requests.last.completeError(StateError('current request failed'));
    await refresh;
    expect(app.teams.value.map((team) => team.id), ['new']);
    expect(app.actionError.value, contains('current request failed'));
  });

  test('refresh after logout does not make an unauthenticated bridge call', () async {
    await api.publish(state(UiAuth.signedOut));
    await app.refreshTeams();
    expect(api.requests, isEmpty);
  });

  test('shutdown ignores a pending successful team response', () async {
    final request = app.refreshTeams();
    app.stop();
    api.requests.single.complete([oldTeam]);
    await request;
    expect(app.teams.value, isEmpty);
  });
}
