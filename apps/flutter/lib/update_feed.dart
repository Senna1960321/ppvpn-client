import 'dart:convert';
import 'dart:io';

/// Reads the Linux repository's latest.json with the app's build identity.
/// The deadline covers connecting, headers, and the complete JSON body.
Future<Object?> fetchUpdateFeed(
  Uri feed, {
  required String userAgent,
  Duration timeout = const Duration(seconds: 30),
}) async {
  final client = HttpClient()..connectionTimeout = timeout;
  client.userAgent = userAgent;
  try {
    return await _readFeed(client, feed).timeout(timeout);
  } finally {
    // Future.timeout does not cancel its input: release a stalled request
    // or body as well as successful responses, so later checks can retry.
    client.close(force: true);
  }
}

Future<Object?> _readFeed(HttpClient client, Uri feed) async {
  final request = await client.getUrl(feed);
  final response = await request.close();
  if (response.statusCode != HttpStatus.ok) {
    throw HttpException('HTTP ${response.statusCode}');
  }
  return jsonDecode(await response.transform(utf8.decoder).join());
}
