import 'package:flutter_test/flutter_test.dart';

import '../lib/distribution.dart';

void main() {
  const site = 'https://www.example.test';
  test('Play keeps device authorization, support, and account deletion', () {
    for (final path in ['/dashboard/device/authorize?user_code=ABC-123', '/dashboard/tickets', '/account/delete']) {
      expect(permittedPlayLink('$site$path', site), isTrue, reason: path);
    }
  });

  test('Play rejects payment routes and server-controlled redirect parameters', () {
    for (final path in [
      '/',
      '/dashboard/subscription',
      '/dashboard/device/authorize?redirect=https://payments.example.test',
      '/dashboard/device/authorize?user_code=A&return_url=/checkout',
      '/dashboard/tickets?next=/checkout',
      '/account/delete#payment',
      '/dashboard/tickets/../subscription',
      '/dashboard/tickets%2F..%2Fsubscription',
    ]) {
      expect(permittedPlayLink('$site$path', site), isFalse, reason: path);
    }
  });

  test('Play rejects other origins, credentials, and malformed links', () {
    for (final url in [
      'http://www.example.test/dashboard/tickets',
      'https://www.example.test:444/dashboard/tickets',
      'https://other.example.test/dashboard/tickets',
      'https://www.example.test.evil.test/dashboard/tickets',
      'https://name@www.example.test/dashboard/tickets',
      'javascript:alert(1)',
      '/dashboard/tickets',
      'https:///dashboard/tickets',
      'https://[invalid',
      '$site/dashboard/device/authorize?%FF=x',
      '$site/dashboard/device/authorize?user_code=%FF',
    ]) {
      expect(permittedPlayLink(url, site), isFalse, reason: url);
    }
    for (final invalidSite in ['', 'not a URL', 'http://www.example.test']) {
      expect(permittedPlayLink('$site/dashboard/tickets', invalidSite), isFalse);
    }
  });
}
