import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'package:flutter_test/flutter_test.dart';

import '../lib/update_feed.dart';

const _body = '{"version":"0.7.5","build":"0.7.5.1"}';
const _timeout = Duration(milliseconds: 200);
const _guard = Duration(seconds: 2);

/// Raw loopback HTTP keeps partial headers/bodies and peer closure observable.
class _Server {
  _Server(this.listener);

  final ServerSocket listener;
  final connections = <Socket>[];
  final requests = <String>[];
  final closed = <Completer<void>>[];
  final timers = <Timer>[];

  Uri get uri => Uri.parse('http://127.0.0.1:${listener.port}/latest.json');

  static Future<_Server> start(
    void Function(_Server, Socket, int) respond,
  ) async {
    final server = _Server(
      await ServerSocket.bind(InternetAddress.loopbackIPv4, 0),
    );
    server.listener.listen((socket) {
      final index = server.connections.length;
      server.connections.add(socket);
      final closed = Completer<void>();
      server.closed.add(closed);
      var head = '';
      var answered = false;
      socket.listen(
        (bytes) {
          head += ascii.decode(bytes);
          if (!answered && head.contains('\r\n\r\n')) {
            answered = true;
            server.requests.add(head);
            respond(server, socket, index);
          }
        },
        onError: (Object _) {
          if (!closed.isCompleted) closed.complete();
        },
        onDone: () {
          if (!closed.isCompleted) closed.complete();
        },
      );
    });
    addTearDown(() async {
      for (final timer in server.timers) {
        timer.cancel();
      }
      for (final socket in server.connections) {
        socket.destroy();
      }
      await server.listener.close();
    });
    return server;
  }
}

void _headers(Socket socket, {int? length}) {
  socket.write(
    'HTTP/1.1 200 OK\r\nContent-Length: ${length ?? _body.length}\r\n\r\n',
  );
}

Future<Object?> _fetch(_Server server, {Duration timeout = _timeout}) =>
    fetchUpdateFeed(
      server.uri,
      userAgent: 'PPVPN/0.7.4.1 (linux)',
      timeout: timeout,
    );

/// The outer watchdog must fail distinctly from the production deadline.
Future<Object?> _bounded(Future<Object?> request) => request.timeout(
  _guard,
  onTimeout: () => throw StateError('feed request did not reach its deadline'),
);

void main() {
  test('reads a complete feed and sends the build user agent', () async {
    final server = await _Server.start((_, socket, __) {
      _headers(socket);
      socket.write(_body);
    });
    expect(await _fetch(server, timeout: _guard), jsonDecode(_body));
    expect(server.requests.single, contains('GET /latest.json HTTP/1.1'));
    expect(
      server.requests.single.toLowerCase(),
      contains('user-agent: ppvpn/0.7.4.1 (linux)'),
    );
    await server.closed.single.future.timeout(_guard);
  });

  test('rejects an HTTP error and closes its connection', () async {
    final server = await _Server.start((_, socket, __) {
      socket.write(
        'HTTP/1.1 503 Service Unavailable\r\nContent-Length: 100\r\n\r\n',
      );
    });
    await expectLater(_bounded(_fetch(server)), throwsA(isA<HttpException>()));
    await server.closed.single.future.timeout(_guard);
  });

  test('rejects malformed JSON and closes its connection', () async {
    final server = await _Server.start((_, socket, __) {
      _headers(socket, length: 1);
      socket.write('{');
    });
    await expectLater(
      _bounded(_fetch(server)),
      throwsA(isA<FormatException>()),
    );
    await server.closed.single.future.timeout(_guard);
  });

  test('times out when response headers never arrive', () async {
    final server = await _Server.start((_, __, ___) {});
    await expectLater(
      _bounded(_fetch(server)),
      throwsA(isA<TimeoutException>()),
    );
    await server.closed.single.future.timeout(_guard);
  });

  test(
    'times out a stalled body, releases the socket, and can retry',
    () async {
      final server = await _Server.start((_, socket, index) {
        _headers(socket);
        socket.write(index == 0 ? '{"version":' : _body);
      });
      await expectLater(
        _bounded(_fetch(server)),
        throwsA(isA<TimeoutException>()),
      );
      await server.closed.first.future.timeout(_guard);
      expect(await _fetch(server, timeout: _guard), jsonDecode(_body));
      expect(server.requests, hasLength(2));
      await server.closed.last.future.timeout(_guard);
    },
  );

  test(
    'a body that keeps sending bytes cannot extend the total deadline',
    () async {
      var chunks = 0;
      final server = await _Server.start((server, socket, _) {
        // Whitespace is legal JSON, so the old unbounded read would succeed.
        _headers(socket, length: 100 + _body.length);
        server.timers.add(
          Timer.periodic(const Duration(milliseconds: 10), (timer) {
            if (chunks++ < 100) {
              socket.write(' ');
            } else {
              socket.write(_body);
              timer.cancel();
            }
          }),
        );
      });
      await expectLater(
        _bounded(_fetch(server)),
        throwsA(isA<TimeoutException>()),
      );
      expect(chunks, greaterThan(1));
      await server.closed.single.future.timeout(_guard);
    },
  );

  test('headers and body share one deadline', () async {
    var headersSent = false;
    final server = await _Server.start((server, socket, _) {
      server.timers.add(
        Timer(const Duration(milliseconds: 200), () {
          _headers(socket);
          headersSent = true;
          server.timers.add(
            Timer(const Duration(milliseconds: 400), () => socket.write(_body)),
          );
        }),
      );
    });
    await expectLater(
      _bounded(_fetch(server, timeout: const Duration(milliseconds: 500))),
      throwsA(isA<TimeoutException>()),
    );
    expect(headersSent, isTrue);
    await server.closed.single.future.timeout(_guard);
  });
}
