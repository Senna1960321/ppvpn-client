#!/usr/bin/env bash
# Packages the Flutter app (#401 step 5) as the native macOS release did
# (#522 removed it): the same PPVPN.app layout, signing, DMG, .pkg and
# release-meta.
#
# Usage: apps/flutter/scripts/package-macos.sh <PPVPN.app> arm64|x86_64
#   <PPVPN.app> is what scripts/ci-build.sh built; it is copied, not changed.
#   PPVPN_BINARIES_DIR         the service helpers, <bin>-universal-apple-darwin or
#                              <bin>-<triple> (default build/binaries, as
#                              tools/desktop/build-service.sh stages them)
#   PPVPN_UPDATE_FEED_URL      Sparkle appcast. Unset: the channel's feed on the update
#                              site, <PPVPN_UPDATE_SITE>/desktop/<channel>/appcast-<platform>.xml,
#                              when both the site and the channel are given; otherwise
#                              updates are off
#   PPVPN_SPARKLE_PUBLIC_KEY   Sparkle EdDSA public key
#   and package-app.sh's: PPVPN_CODESIGN_IDENTITY, PPVPN_CODESIGN_TEAM,
#   PPVPN_INSTALLER_IDENTITY, PPVPN_NOTARY_*, PPVPN_RELEASE_CHANNEL,
#   SPARKLE_PRIVATE_KEY_FILE.
#   PPVPN_MACOS_APNS_PROFILE   Developer ID provisioning profile for the desktop
#                              bundle, required when PPVPN_CODESIGN_TEAM is set.
#
# Into the copy, at the native app's paths:
#   Contents/Library/LaunchAgents/com.peakpassvpn.ppvpn.desktop.agent.plist
#       migration only: unregister the previous job, without shipping its agent
#   Contents/MacOS/ppvpn-service{,-install,-uninstall}  what rust/src/hooks.rs
#       runs for enhanced mode
#   Contents/Frameworks/Sparkle.framework  resolved by packaging/macos/SparkleArtifacts,
#       which the runner loads (MainFlutterWindow.swift,
#       Updater), and the Info.plist keys it reads: PPVPNUpdateFeedURL,
#       SUPublicEDKey, SUEnableAutomaticChecks
# then apps/macos/scripts/package-app.sh: thinning, inside-out signing with
# packaging/macos/PPVPN.entitlements, and dist/macos/PPVPN-<version>-<platform>.dmg,
# .pkg and release-meta-<platform>.json.
set -euo pipefail

src="${1:-}"
arch="${2:-}"
case "$arch" in
  arm64) triple=aarch64-apple-darwin platform=macos-arm64 ;;
  x86_64) triple=x86_64-apple-darwin platform=macos-x64 ;;
  *) echo "usage: $0 <PPVPN.app> arm64|x86_64" >&2; exit 2 ;;
esac
[ -x "$src/Contents/MacOS/PPVPN" ] || { echo "no PPVPN.app at '$src'" >&2; exit 2; }

app_dir=$(cd "$(dirname "$0")/.." && pwd)
root=$(cd "$app_dir/../.." && pwd)
macos="$root/apps/macos"
bins="${PPVPN_BINARIES_DIR:-$root/build/binaries}"
build="$app_dir/build/macos-package-$arch"

plist() { /usr/libexec/PlistBuddy -c "Print :$1" "$src/Contents/Info.plist"; }
[ "$(plist CFBundleIdentifier)" = com.peakpassvpn.ppvpn.desktop ] ||
  { echo "$src is not com.peakpassvpn.ppvpn.desktop" >&2; exit 2; }
version=$(plist CFBundleShortVersionString)
bundle_version=$(plist CFBundleVersion)

# Resolve the public Sparkle binary independently of the retired agent and
# its generated Swift/Rust client. No login-keychain access is needed.
packages="$build/SourcePackages"
swift package --package-path "$app_dir/packaging/macos/SparkleArtifacts" \
  --scratch-path "$packages" --disable-keychain resolve
sparkle=$(find "$packages/artifacts" -type d -path '*/Sparkle.xcframework/macos-*/Sparkle.framework' -prune)
[ -n "$sparkle" ] && [ "$(printf '%s\n' "$sparkle" | wc -l | tr -d ' ')" = 1 ] ||
  { echo "expected one resolved macOS Sparkle.framework, found: ${sparkle:-none}" >&2; exit 1; }
feed=${PPVPN_UPDATE_FEED_URL:-}
if [ -z "$feed" ] && [ -n "${PPVPN_UPDATE_SITE:-}" ] && [ -n "${PPVPN_RELEASE_CHANNEL:-}" ]; then
  feed="${PPVPN_UPDATE_SITE%/}/desktop/$PPVPN_RELEASE_CHANNEL/appcast-$platform.xml"
fi

app="$build/PPVPN.app"
rm -rf "$app"
ditto "$src" "$app"
contents="$app/Contents"
mkdir -p "$contents/Library/LaunchAgents" "$contents/Frameworks"
# Keep only the old job's plist so SMAppService can unregister an existing
# installation. No agent executable or its Rust/Swift framework is shipped.
rm -rf "$contents/Library/LoginItems/PPVPN Agent.app" "$contents/Frameworks/PPVPNClientFFI.framework"
install -m 644 "$macos/LaunchAgents/com.peakpassvpn.ppvpn.desktop.agent.plist" "$contents/Library/LaunchAgents/"
rm -rf "$contents/Frameworks/Sparkle.framework"
ditto "$sparkle" "$contents/Frameworks/Sparkle.framework"
# As the native app had them; empty values leave updates off.
plist_set() {
  /usr/libexec/PlistBuddy -c "Delete :$1" "$contents/Info.plist" 2>/dev/null || true
  /usr/libexec/PlistBuddy -c "Add :$1 $2 $3" "$contents/Info.plist"
}
plist_set PPVPNUpdateFeedURL string "$feed"
plist_set SUPublicEDKey string "${PPVPN_SPARKLE_PUBLIC_KEY:-}"
plist_set SUEnableAutomaticChecks bool true
for bin in ppvpn-service ppvpn-service-install ppvpn-service-uninstall; do
  file="$bins/$bin-universal-apple-darwin"
  [ -f "$file" ] || file="$bins/$bin-$triple"
  [ -f "$file" ] || { echo "$bin missing in $bins (tools/desktop/build-service.sh macos)" >&2; exit 1; }
  install -m 755 "$file" "$contents/MacOS/$bin"
done

python3 "$app_dir/scripts/macos-push-profile.py" "$app" "$build/PPVPN.entitlements" \
  "$app_dir/packaging/macos/PPVPN.entitlements"
PPVPN_APP_ENTITLEMENTS="$build/PPVPN.entitlements" \
PPVPN_SOURCE_PACKAGES="$packages" \
  "$macos/scripts/package-app.sh" "$app" "$arch"

# What the native app holds besides its own code (package-app.sh checked the
# signatures and the helpers' identifiers).
for path in Library/LaunchAgents/com.peakpassvpn.ppvpn.desktop.agent.plist \
    Frameworks/libppvpn_flutter.dylib Frameworks/Sparkle.framework/Versions/B/Sparkle \
    MacOS/ppvpn-service MacOS/ppvpn-service-install \
    MacOS/ppvpn-service-uninstall; do
  [ -e "$contents/$path" ] || { echo "$path missing in $app" >&2; exit 1; }
done
[ ! -e "$contents/Library/LoginItems/PPVPN Agent.app" ] && \
  [ ! -e "$contents/Frameworks/PPVPNClientFFI.framework" ] ||
  { echo "the retired macOS push agent is still bundled" >&2; exit 1; }
echo "app=$app" >> "${GITHUB_OUTPUT:-/dev/stdout}"
