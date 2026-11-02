import 'dart:io';

// Missing build metadata fails closed. ci-build.sh supplies the native
// flavor and this Dart value together.
const androidDistribution = String.fromEnvironment('PPVPN_ANDROID_CHANNEL', defaultValue: 'play');
bool get playDistribution => Platform.isAndroid && androidDistribution != 'apk';
bool get externalPurchases => !Platform.isIOS && !playDistribution;

/// Play retains sign-in, support and account deletion. Server-provided
/// message links cannot add a payment or arbitrary storefront entry.
bool permittedPlayLink(String url, String site) {
  final uri = Uri.tryParse(url);
  final origin = Uri.tryParse(site);
  if (uri == null || uri.scheme != 'https' || uri.userInfo.isNotEmpty || uri.hasFragment) return false;
  if (origin == null || origin.scheme != 'https' || origin.host.isEmpty || origin.userInfo.isNotEmpty) return false;
  if (uri.host.isEmpty || uri.origin != origin.origin) return false;
  if (uri.path == '/dashboard/device/authorize') {
    try {
      return uri.queryParametersAll.keys.every((key) => key == 'user_code');
    } on FormatException {
      return false;
    }
  }
  return const ['/dashboard/tickets', '/account/delete'].contains(uri.path) && !uri.hasQuery;
}
