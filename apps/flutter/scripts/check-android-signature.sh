#!/usr/bin/env bash
# Check the actual artifact format: apksigner supports APK v2/v3 signatures
# that keytool's JAR reader cannot see. AABs still use JAR signatures.
set -euo pipefail
artifact=${1:?artifact required}
format=${2:?apk or aab required}
case "$format" in
  aab)
    signer=$(keytool -printcert -jarfile "$artifact")
    grep -q 'Owner:' <<<"$signer" || { echo "$artifact is not signed" >&2; exit 1; }
    ;;
  apk)
    sdk=${ANDROID_HOME:-${ANDROID_SDK_ROOT:?the Android SDK is needed}}
    apksigner=$(find "$sdk/build-tools" -name apksigner -type f | sort -V | tail -1)
    [ -n "$apksigner" ] || { echo "no apksigner in $sdk/build-tools" >&2; exit 1; }
    signer=$("$apksigner" verify --print-certs "$artifact")
    grep -q 'Signer #1 certificate DN:' <<<"$signer" || { echo "$artifact has no signer" >&2; exit 1; }
    ;;
  *) echo "unknown Android format: $format" >&2; exit 1 ;;
esac
! grep -q 'CN=Android Debug' <<<"$signer" || { echo "$artifact is signed with the debug key" >&2; exit 1; }
