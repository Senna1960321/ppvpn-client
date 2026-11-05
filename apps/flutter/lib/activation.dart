// Notification clicks and further launches of the app (#401 step 4). The
// push agent shows the OS notifications; a click comes back with the push
// id it carries, which AppState.openPush resolves.
//
// - macOS: the agent opens ppvpn://push/<id>, as for the native app. The
//   runner (runner/macos/Runner/MainFlutterWindow.swift) passes it on the
//   channel below, as it does a reopen (the Dock icon, opening the app
//   again), and holds what arrives before Dart asks.
// - Windows and Linux: the bridge keeps the app to one instance and hands
//   over the toast activator's clicks and other launches' arguments
//   (rust/src/instance.rs). On Windows also the installer's request to
//   quit (its quit event), before it replaces the app's files, and the
//   update's installer starting (rust/src/update.rs).

import 'dart:async';
import 'dart:io';

import 'package:flutter/foundation.dart';
import 'package:flutter/services.dart';

import 'src/rust/api.dart' as rust;

const _channel = MethodChannel('ppvpn/activation');

/// For the app's life: [onShow] for another launch or a reopen, [onPush]
/// with a clicked notification's push id, [onQuit] when the installer asks
/// the app to quit (Windows), [onUserQuit] for the application menu's Quit
/// (macOS: the runner routes it here, so that the app quits as the tray's
/// Quit does), [onUpdate] when an update's installer has started and the
/// app should step out of sight until then (Windows).
void listenForActivations({
  required void Function() onShow,
  required void Function(int pushId) onPush,
  required void Function() onQuit,
  required void Function() onUserQuit,
  required void Function() onUpdate,
}) {
  if (Platform.isMacOS) {
    void take(Object? item) {
      if (item == 'show') {
        onShow();
      } else if (item == 'quit') {
        onUserQuit();
      } else if (item is String) {
        final id = pushLinkId(item);
        if (id != null) onPush(id);
      }
    }

    _channel.setMethodCallHandler((call) async {
      if (call.method == 'activate') take(call.arguments);
    });
    unawaited(
      _channel.invokeListMethod<String>('ready').then(
        (pending) => pending?.forEach(take),
        onError: (Object error) => debugPrint('activation: $error'),
      ),
    );
    return;
  }
  unawaited(() async {
    while (true) {
      final activation = await rust.nextActivation();
      final id = activation.pushId;
      if (activation.quit) {
        onQuit();
      } else if (activation.update) {
        onUpdate();
      } else if (id == null) {
        onShow();
      } else {
        onPush(id);
      }
    }
  }());
}

/// The push id in `ppvpn://push/<id>`, and nothing else (as the native
/// app's PushLink): no query, no fragment, the id in ASCII digits.
int? pushLinkId(String link) {
  final uri = Uri.tryParse(link);
  if (uri == null || uri.scheme != 'ppvpn' || uri.host != 'push' || uri.hasQuery || uri.hasFragment) return null;
  final segments = uri.pathSegments;
  if (segments.length != 1 || !RegExp(r'^[0-9]+$').hasMatch(segments.single)) return null;
  return int.tryParse(segments.single);
}
