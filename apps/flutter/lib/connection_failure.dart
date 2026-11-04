// Stable codes only cross into the visible explanation or copied report.
// Service details can contain local paths, hostnames or credentials.
String? serviceFailureCode(String? detail) {
  final code = RegExp(r'^([A-Z][A-Z0-9_]{0,63})(?=:|$)').firstMatch(detail ?? '')?.group(1);
  const known = {
    'CORE_OPERATION_FAILED',
    'CORE_PANICKED',
    'ENGINE_FATAL',
    'ENGINE_SHUT_DOWN',
    'CORE_IPC_TIMEOUT',
    'CORE_SCHEMA_CAPABILITY_MISSING',
    'PROFILE_SCHEMA_UNSUPPORTED',
    'PROFILE_INVALID',
    'PROFILE_EXPIRED',
    'WINTUN_UNAVAILABLE',
    'PERMISSION_DENIED',
    'TUN_INSTANCE_EXISTS',
    'TUN_NAME_TAKEN',
    'STATE_DIR_IN_USE',
  };
  return known.contains(code) ? code : null;
}

String enhancedFailureMessage(String? code, String? detail) => switch (code) {
  'ServiceInstallCancelled' => 'fr_auth',
  'ServiceInstallFailed' => 'fr_install',
  'ServiceUnavailable' || 'ServiceIncompatible' => 'fr_service',
  'ServiceClientRejected' => 'fr_clientRejected',
  'NetworkPathContended' => 'fr_contended',
  'Timeout' => 'fr_timeout',
  'ConnectHealthCheckFailed' => 'fr_health',
  _ => switch (serviceFailureCode(detail)) {
    'WINTUN_UNAVAILABLE' => 'fr_driver',
    'PERMISSION_DENIED' => 'fr_permission',
    'TUN_INSTANCE_EXISTS' || 'TUN_NAME_TAKEN' || 'STATE_DIR_IN_USE' => 'fr_tunBusy',
    _ => 'fr_connect',
  },
};

String enhancedFailureReport({required String version, required String platform, String? code, String? detail}) {
  String safe(String? value) =>
      value != null && RegExp(r'^[A-Za-z0-9._+\-]{1,64}$').hasMatch(value) ? value : 'unknown';
  final stage = RegExp(r'\[start_stage=(create|version|profile|start)\]$').firstMatch(detail ?? '')?.group(1);
  return [
    'PPVPN ${safe(version)} (${safe(platform)})',
    'mode=enhanced',
    'error=${safe(code)}',
    if (serviceFailureCode(detail) case final reason?) 'reason=$reason',
    if (stage != null) 'stage=$stage',
  ].join('\n');
}
