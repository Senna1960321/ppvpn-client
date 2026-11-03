#!/usr/bin/env bash
# Thin a built PPVPN.app to one architecture, re-sign it inside out, and wrap
# it in the DMG, the .pkg and the release metadata: the second half of
# apps/flutter/scripts/package-macos.sh, with the artifacts the native app
# shipped.
#
# Usage: apps/macos/scripts/package-app.sh <PPVPN.app> arm64|x86_64
#   PPVPN_CODESIGN_IDENTITY    signing identity in the keychain search list, e.g.
#                              "Developer ID Application: … (<team>)" (default: ad-hoc "-")
#   PPVPN_CODESIGN_TEAM        with a Developer ID identity, its Team ID (#471): secure
#                              timestamps, Apple's designated requirements, no
#                              disable-library-validation, and the DMG signed too.
#                              Without it (a self-signed identity, CI) each designated
#                              requirement pins the certificate instead
#   PPVPN_INSTALLER_IDENTITY   "Developer ID Installer: … (<team>)": productsign the pkg
#                              (needs PPVPN_CODESIGN_TEAM)
#   PPVPN_NOTARY_KEY_FILE      App Store Connect API key (.p8) for notarytool, with
#   PPVPN_NOTARY_KEY_ID        its key id and issuer: notarize and staple the app, then
#   PPVPN_NOTARY_ISSUER        the DMG and the pkg (needs PPVPN_CODESIGN_TEAM)
#   PPVPN_APP_ENTITLEMENTS     sign the app itself with this file (default: keep
#                              the entitlements Xcode signed it with)
#   PPVPN_RELEASE_CHANNEL      dev | stable, recorded in release-meta (unset: null)
#   SPARKLE_PRIVATE_KEY_FILE   when set, sign the DMG for Sparkle and the pkg for
#                              the service's silent updates (sign_update, #548)
#   PPVPN_SOURCE_PACKAGES      the Xcode build's SourcePackages, where Sparkle's
#                              sign_update is (needed with SPARKLE_PRIVATE_KEY_FILE)
#
# The app is changed in place. It must hold the service helpers in
# Contents/MacOS. The retired push agent and PPVPNClientFFI are forbidden.
#
# Output (dist/macos), <platform> = macos-arm64 | macos-x64:
#   PPVPN-<version>-<platform>.dmg   Sparkle's updates and manual installs
#   PPVPN-<version>-<platform>.pkg   first installs: Installer puts the app in
#                                    /Applications (packaging/postinstall); also
#                                    what the service installs silently (#548)
#   release-meta-<platform>.json   what the update site's feed and pointer are written from;
#                                  file/length/sha256/ed_signature are the DMG's,
#                                  install_file/install_length/install_sha256/
#                                  install_ed_signature the pkg's
set -Eeuo pipefail
# Name the failing command: some (a pipeline in an assignment) fail silently.
trap 'echo "error: ${BASH_SOURCE[0]}:$LINENO: \"$BASH_COMMAND\" exited $?" >&2' ERR

APP="${1:-}"
ARCH="${2:-}"
case "$ARCH" in
  arm64) PLATFORM=macos-arm64 ;;
  x86_64) PLATFORM=macos-x64 ;;
  *) echo "usage: $0 <PPVPN.app> arm64|x86_64" >&2; exit 2 ;;
esac
[[ -f "$APP/Contents/Info.plist" ]] || { echo "error: no app at $APP" >&2; exit 2; }
APP="$(cd "$APP" && pwd)"
SIGN_IDENTITY="${PPVPN_CODESIGN_IDENTITY:--}"
TEAM="${PPVPN_CODESIGN_TEAM:-}"
INSTALLER_IDENTITY="${PPVPN_INSTALLER_IDENTITY:-}"
NOTARIZE=0
[[ -z "${PPVPN_NOTARY_KEY_FILE:-}" ]] || NOTARIZE=1
TIMESTAMP=--timestamp=none
CERT_SHA1=""
if [[ -n "$TEAM" ]]; then
  [[ "$TEAM" =~ ^[A-Z0-9]{10}$ ]] || { echo "error: PPVPN_CODESIGN_TEAM is not a Team ID: $TEAM" >&2; exit 2; }
  [[ "$SIGN_IDENTITY" == "Developer ID Application: "*" ($TEAM)" ]] \
    || { echo "error: \"$SIGN_IDENTITY\" is not a Developer ID Application identity of $TEAM" >&2; exit 2; }
  TIMESTAMP=--timestamp
elif [[ -n "$INSTALLER_IDENTITY" || "$NOTARIZE" == 1 ]]; then
  echo "error: signing the pkg or notarizing needs a Developer ID signature (PPVPN_CODESIGN_TEAM)" >&2
  exit 2
elif [[ "$SIGN_IDENTITY" != "-" ]]; then
  CERT_SHA1="$(security find-certificate -c "$SIGN_IDENTITY" -Z | awk '/^SHA-1 hash:/ { print $NF; exit }')"
  [[ -n "$CERT_SHA1" ]] || { echo "error: no certificate for \"$SIGN_IDENTITY\" in the keychain search list" >&2; exit 2; }
fi
CHANNEL="${PPVPN_RELEASE_CHANNEL:-}"
case "$CHANNEL" in ''|dev|stable) ;; *) echo "error: PPVPN_RELEASE_CHANNEL must be dev or stable" >&2; exit 2 ;; esac
ENTITLEMENTS="${PPVPN_APP_ENTITLEMENTS:-}"
[[ -z "$ENTITLEMENTS" || -f "$ENTITLEMENTS" ]] || { echo "error: no entitlements at $ENTITLEMENTS" >&2; exit 2; }
[[ -z "$INSTALLER_IDENTITY" || "$INSTALLER_IDENTITY" == "Developer ID Installer: "*" ($TEAM)" ]] \
  || { echo "error: \"$INSTALLER_IDENTITY\" is not a Developer ID Installer identity of $TEAM" >&2; exit 2; }
if [[ "$NOTARIZE" == 1 ]]; then
  : "${PPVPN_NOTARY_KEY_ID:?PPVPN_NOTARY_KEY_ID is required with PPVPN_NOTARY_KEY_FILE}"
  : "${PPVPN_NOTARY_ISSUER:?PPVPN_NOTARY_ISSUER is required with PPVPN_NOTARY_KEY_FILE}"
  [[ -f "$PPVPN_NOTARY_KEY_FILE" ]] || { echo "error: no notary key at $PPVPN_NOTARY_KEY_FILE" >&2; exit 2; }
fi

APP_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO_DIR="$(cd "$APP_DIR/../.." && pwd)"
OUT_DIR="$REPO_DIR/dist/macos"

# --- Thin every Mach-O to $ARCH -------------------------------------------
MACHO="$(mktemp)"
WORK="$(mktemp -d)"
trap 'rm -rf "$MACHO" "$WORK"' EXIT
find "$APP" -type f -print0 | while IFS= read -r -d '' file; do
  # Not Mach-O: no archs (and no failing command for the ERR trap to report).
  archs="$(lipo -archs "$file" 2>/dev/null || true)"
  [[ -n "$archs" ]] || continue
  echo "$file" >> "$MACHO"
  [[ "$archs" == "$ARCH" ]] && continue
  [[ " $archs " == *" $ARCH "* ]] || { echo "error: $file lacks $ARCH ($archs)" >&2; exit 1; }
  lipo -thin "$ARCH" "$file" -output "$file.thin"
  chmod "$(stat -f %Lp "$file")" "$file.thin"
  mv "$file.thin" "$file"
done

# --- Re-sign inside out ----------------------------------------------------
# Bundles (deepest first) and the main executable each one owns.
BUNDLES="$(find "$APP/Contents" -type d \( -name '*.framework' -o -name '*.xpc' -o -name '*.app' \) \
  | awk -F/ '{ print NF "\t" $0 }' | sort -rn | cut -f2-)"
MAINS=""
while IFS= read -r bundle; do
  [[ -n "$bundle" ]] || continue
  if [[ "$bundle" == *.framework ]]; then
    name="$(basename "$bundle" .framework)"
    MAINS+="$(cd "$bundle/Versions/Current" && pwd -P)/$name"$'\n'
  else
    MAINS+="$bundle/Contents/MacOS/$(/usr/libexec/PlistBuddy -c 'Print CFBundleExecutable' "$bundle/Contents/Info.plist")"$'\n'
  fi
done <<< "$BUNDLES"
# With a Developer ID, codesign's own designated requirement (identifier,
# Apple's anchor, our Team ID) carries across builds and certificate
# renewals, and library validation needs no exception: every library carries
# the same Team ID, so disable-library-validation is taken out of whatever
# entitlements a file had. With a self-signed certificate, each designated
# requirement is pinned to the identifier and that certificate: it has no
# Team ID, and the default requirement would otherwise not carry across
# builds (keychain ACLs, SMAppService, notification permission).
# The service helpers get fixed identifiers (the linker's default carries a
# hash): the app and ppvpn-service-install check them before running them as
# root (MacPlatformHooks.swift, install.rs).
sign() {
  local id="" preserve=identifier,entitlements identifier=() entitlements=() file
  case "$1" in
    */Contents/MacOS/ppvpn-service | */Contents/MacOS/ppvpn-service-install | */Contents/MacOS/ppvpn-service-uninstall)
      id="com.peakpassvpn.ppvpn.${1##*/ppvpn-}"
      identifier=(--identifier "$id")
      preserve=entitlements
      ;;
  esac
  if [[ "$1" == "$APP" && -n "$ENTITLEMENTS" ]]; then
    entitlements=(--entitlements "$ENTITLEMENTS")
    preserve=identifier
  fi
  if [[ -n "$TEAM" ]]; then
    # The entitlements it has (or is given), without disable-library-validation.
    file="$(mktemp "$WORK/entitlements.XXXXXX")"
    if [[ "$1" == "$APP" && -n "$ENTITLEMENTS" ]]; then
      cp "$ENTITLEMENTS" "$file"
    else
      codesign -d --entitlements - --xml "$1" > "$file" 2>/dev/null || true
    fi
    entitlements=()
    if [[ -s "$file" ]]; then
      /usr/libexec/PlistBuddy -c 'Delete :com.apple.security.cs.disable-library-validation' "$file" >/dev/null 2>&1 || true
      entitlements=(--entitlements "$file")
    fi
    preserve=identifier
    [[ -z "$id" ]] || preserve=""
  fi
  codesign --force --sign "$SIGN_IDENTITY" --options runtime $TIMESTAMP \
    ${preserve:+--preserve-metadata=$preserve} ${identifier[@]+"${identifier[@]}"} ${entitlements[@]+"${entitlements[@]}"} "$1"
  [[ -n "$CERT_SHA1" ]] || return 0
  # Pin the requirement to the identifier this signature carries: read before
  # signing, it can differ (a linker signature, as the xcframework's
  # PPVPNClientFFI has, does not keep its identifier; x86_64 code has none).
  [[ -n "$id" ]] || id="$(codesign -dv "$1" 2>&1 | sed -n 's/^Identifier=//p')"
  [[ -n "$id" ]] || { echo "error: no identifier in the signature of $1" >&2; exit 1; }
  codesign --force --sign "$SIGN_IDENTITY" --options runtime $TIMESTAMP \
    --preserve-metadata=entitlements --identifier "$id" ${entitlements[@]+"${entitlements[@]}"} \
    --requirements "=designated => identifier \"$id\" and certificate leaf = H\"$CERT_SHA1\"" "$1"
}
# Loose helpers first (the service helpers, Sparkle's Autoupdate, ...), then bundles.
while IFS= read -r file; do
  real="$(cd "$(dirname "$file")" && pwd -P)/$(basename "$file")"
  grep -Fxq "$real" <<< "$MAINS" || [[ "$file" == "$APP/Contents/MacOS/PPVPN" ]] || sign "$file"
done < "$MACHO"
while IFS= read -r bundle; do [[ -n "$bundle" ]] && sign "$bundle"; done <<< "$BUNDLES"
sign "$APP"

# --- Checks ------------------------------------------------------------------
# Each bundle on its own first, so that a failure names it.
while IFS= read -r bundle; do
  [[ -z "$bundle" ]] || codesign --verify --strict --verbose=2 "$bundle"
done <<< "$BUNDLES"
codesign --verify --deep --strict --verbose=4 "$APP"
if [[ -n "$CERT_SHA1" ]]; then
  for bundle in "$APP"; do
    codesign -dr - "$bundle" 2>&1 | grep -qi "certificate leaf = H\"$CERT_SHA1\"" \
      || { echo "error: $bundle lacks the pinned designated requirement" >&2; exit 1; }
  done
fi
for helper in service service-install service-uninstall; do
  file="$APP/Contents/MacOS/ppvpn-$helper"
  [[ "$(codesign -dv "$file" 2>&1 | sed -n 's/^Identifier=//p')" == "com.peakpassvpn.ppvpn.$helper" ]] \
    || { echo "error: $file is not signed as com.peakpassvpn.ppvpn.$helper" >&2; exit 1; }
  [[ -z "$CERT_SHA1" ]] || codesign -dr - "$file" 2>&1 | grep -qi "certificate leaf = H\"$CERT_SHA1\"" \
    || { echo "error: $file lacks the pinned designated requirement" >&2; exit 1; }
done
while IFS= read -r file; do
  [[ "$(lipo -archs "$file")" == "$ARCH" ]] || { echo "error: $file is not $ARCH only" >&2; exit 1; }
done < "$MACHO"
# macOS notifications now use APNs. Neither the old executable nor its
# duplicate client framework may survive into an update artifact.
[[ ! -e "$APP/Contents/Library/LoginItems/PPVPN Agent.app" && \
   ! -e "$APP/Contents/Frameworks/PPVPNClientFFI.framework" ]] ||
  { echo "error: retired push-agent code is still bundled" >&2; exit 1; }
for bin in ppvpn-service ppvpn-service-install ppvpn-service-uninstall; do
  [[ -f "$APP/Contents/MacOS/$bin" ]] || { echo "error: $bin missing" >&2; exit 1; }
done
if [[ -n "$TEAM" ]]; then
  # What notarization asks of every piece of code, and the service of us.
  while IFS= read -r file; do
    info="$(codesign -dv --verbose=4 "$file" 2>&1)"
    grep -qx "TeamIdentifier=$TEAM" <<< "$info" || { echo "error: $file is not signed by $TEAM" >&2; exit 1; }
    grep -q '^CodeDirectory .*flags=.*runtime' <<< "$info" || { echo "error: $file lacks the hardened runtime" >&2; exit 1; }
    grep -q '^Timestamp=' <<< "$info" || { echo "error: $file has no secure timestamp" >&2; exit 1; }
    granted="$(codesign -d --entitlements - --xml "$file" 2>/dev/null || true)"
    for key in com.apple.security.cs.disable-library-validation com.apple.security.get-task-allow; do
      [[ "$granted" != *"<key>$key</key>"* ]] || { echo "error: $file has $key" >&2; exit 1; }
    done
  done < "$MACHO"
fi

# --- Notarization --------------------------------------------------------
# Apple's notary service, with an App Store Connect API key
# (tools/notarize.sh): a submission that is not Accepted fails the build.
notarize() {
  "$REPO_DIR/tools/notarize.sh" "$1" "$PPVPN_NOTARY_KEY_FILE" "$PPVPN_NOTARY_KEY_ID" "$PPVPN_NOTARY_ISSUER"
}
# stapler fetches each ticket from Apple's CloudKit in one request and
# gives up when the connection drops (exit 68, NSURLErrorDomain -1005), as
# it did right after notarization in release runs 40911475488 and
# 40929635549. Only that exit is tried again (three tries in all, 60 s and
# 180 s apart), an exception to fixing flakes at the root that was agreed for
# this outside service; any other exit, such as 65 (no ticket), fails at once.
# Run 40993849537 lost all three DMG tries within 80 s while the app's second
# try 15 s in had worked, so the drops come in bursts and the gaps are long.
staple() {
  local status
  for pause in 60 180 0; do
    status=0
    xcrun stapler staple "$1" || status=$?
    [[ "$status" != 0 ]] || return 0
    if [[ "$status" != 68 || "$pause" == 0 ]]; then
      echo "error: stapler staple $1 exited $status" >&2
      return "$status"
    fi
    echo "warning: $(date -u +%H:%M:%SZ) stapler staple $1 exited 68 (connection to Apple lost), again in ${pause}s" >&2
    sleep "$pause"
  done
}
# The app first, and its ticket stapled to it: a copy of the app outside the
# DMG and the pkg (and the one Sparkle unpacks) opens offline too.
if [[ "$NOTARIZE" == 1 ]]; then
  ditto -c -k --keepParent "$APP" "$WORK/PPVPN.zip"
  notarize "$WORK/PPVPN.zip"
  # staple validates what it stapled ("The staple and validate action
  # worked!"); a separate stapler validate asks Apple again and lost that
  # connection after a good staple in release run 41018796202.
  staple "$APP"
fi

plist() { /usr/libexec/PlistBuddy -c "Print $1" "$APP/Contents/Info.plist"; }
VERSION="$(plist CFBundleShortVersionString)"
BUILD="$(plist CFBundleVersion)"
MIN_OS="$(plist LSMinimumSystemVersion)"
DMG="$OUT_DIR/PPVPN-$VERSION-$PLATFORM.dmg"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE" "$MACHO" "$WORK"' EXIT

cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
mkdir -p "$OUT_DIR"
rm -f "$DMG"
# hdiutil create sometimes fails with "Resource busy" on CI runners (its
# temporary image is still attached, or a leftover volume is). Detach those
# and try again, three times at most.
for try in 1 2 3; do
  if hdiutil create -volname "PPVPN" -srcfolder "$STAGE" -fs HFS+ -format UDZO -imagekey zlib-level=9 "$DMG" >/dev/null; then
    break
  fi
  [[ "$try" -lt 3 ]] || { echo "error: hdiutil create failed $try times" >&2; exit 1; }
  echo "warning: hdiutil create failed (try $try), retrying" >&2
  for volume in /Volumes/PPVPN*; do
    [[ -d "$volume" ]] && hdiutil detach -force "$volume" >/dev/null 2>&1 || true
  done
  rm -f "$DMG"
  sleep $((try * 5))
done
if [[ -n "$TEAM" ]]; then
  codesign --force --sign "$SIGN_IDENTITY" --timestamp "$DMG"
else
  codesign --force --sign - "$DMG"
fi
echo "wrote $DMG"

# The first-install package: the same app, installed only to /Applications
# (not relocated to another copy of the bundle id Installer finds, such as
# the mounted DMG), with packaging/postinstall and nothing else. Signed with
# the Developer ID Installer identity when there is one; unsigned, Gatekeeper
# asks once for it. The app it installs has no quarantine.
PKG="$OUT_DIR/PPVPN-$VERSION-$PLATFORM.pkg"
PKG_STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE" "$MACHO" "$WORK" "$PKG_STAGE"' EXIT
mkdir "$PKG_STAGE/root" "$PKG_STAGE/scripts"
ditto "$APP" "$PKG_STAGE/root/PPVPN.app"
install -m 755 "$APP_DIR/packaging/postinstall" "$PKG_STAGE/scripts/postinstall"
pkgbuild --analyze --root "$PKG_STAGE/root" "$PKG_STAGE/components.plist" >/dev/null
plutil -replace 0.BundleIsRelocatable -bool NO "$PKG_STAGE/components.plist"
plutil -replace 0.BundleIsVersionChecked -bool NO "$PKG_STAGE/components.plist"
rm -f "$PKG"
pkgbuild --root "$PKG_STAGE/root" --component-plist "$PKG_STAGE/components.plist" \
  --scripts "$PKG_STAGE/scripts" --install-location /Applications \
  --identifier com.peakpassvpn.ppvpn.desktop.pkg --version "$BUILD" "$PKG" >/dev/null
if [[ -n "$INSTALLER_IDENTITY" ]]; then
  mv "$PKG" "$WORK/unsigned.pkg"
  productsign --sign "$INSTALLER_IDENTITY" --timestamp "$WORK/unsigned.pkg" "$PKG" >/dev/null
  pkgutil --check-signature "$PKG" | grep -qF "Developer ID Installer: " \
    && pkgutil --check-signature "$PKG" | grep -qF "($TEAM)" \
    || { echo "error: $PKG is not signed by the Developer ID Installer of $TEAM" >&2; exit 1; }
fi
# Consume the complete payload listing: grep -q closes the pipe early,
# making pkgutil fail under pipefail even when the app is present.
pkgutil --payload-files "$PKG" | grep -x './PPVPN.app/Contents/MacOS/PPVPN' >/dev/null \
  || { echo "error: $PKG does not install PPVPN.app" >&2; exit 1; }
echo "wrote $PKG"

# Both notarized at once, then their tickets stapled: the release metadata's
# lengths and hashes (and Sparkle's signature) are of the stapled files.
if [[ "$NOTARIZE" == 1 ]]; then
  notarize "$DMG" & dmg_job=$!
  notarize "$PKG" & pkg_job=$!
  failed=0
  wait "$dmg_job" || failed=1
  wait "$pkg_job" || failed=1
  [[ "$failed" == 0 ]] || exit 1
  for file in "$DMG" "$PKG"; do
    staple "$file"
  done
  # Gatekeeper's verdicts, as a download gets them.
  spctl -a -vv -t exec "$APP"
  spctl -a -vv -t open --context context:primary-signature "$DMG"
  spctl -a -vv -t install "$PKG"
fi

ED_SIGNATURE="" INSTALL_ED_SIGNATURE=""
if [[ -n "${SPARKLE_PRIVATE_KEY_FILE:-}" ]]; then
  : "${PPVPN_SOURCE_PACKAGES:?PPVPN_SOURCE_PACKAGES is required with SPARKLE_PRIVATE_KEY_FILE}"
  SIGN_UPDATE="$(find "$PPVPN_SOURCE_PACKAGES/artifacts" -path '*/bin/sign_update' -type f | head -1)"
  [[ -x "$SIGN_UPDATE" ]] || { echo "error: sign_update not found" >&2; exit 1; }
  # Prints: sparkle:edSignature="<base64>" length="<bytes>"
  ed_signature() {
    local signature
    signature="$("$SIGN_UPDATE" --ed-key-file "$SPARKLE_PRIVATE_KEY_FILE" "$1" \
      | sed -n 's/.*sparkle:edSignature="\([^"]*\)".*/\1/p')"
    [[ -n "$signature" ]] || { echo "error: sign_update produced no signature for $1" >&2; exit 1; }
    printf '%s' "$signature"
  }
  ED_SIGNATURE="$(ed_signature "$DMG")"
  # The service checks the pkg with the same key before it installs it (#548).
  INSTALL_ED_SIGNATURE="$(ed_signature "$PKG")"
fi

# Release metadata (schema 1, agreed across platforms); ed_signature and
# install_ed_signature are null for unsigned local builds, which must not be
# published.
META="$OUT_DIR/release-meta-$PLATFORM.json"
python3 - "$META" <<PY
import json, sys
json.dump({
    "schema": 1,
    "platform": "$PLATFORM",
    "channel": "$CHANNEL" or None,
    "version": "$VERSION",
    "build": "$BUILD",
    "file": "$(basename "$DMG")",
    "length": $(stat -f %z "$DMG"),
    "sha256": "$(shasum -a 256 "$DMG" | cut -d' ' -f1)",
    "ed_signature": "$ED_SIGNATURE" or None,
    "install_file": "$(basename "$PKG")",
    "install_length": $(stat -f %z "$PKG"),
    "install_sha256": "$(shasum -a 256 "$PKG" | cut -d' ' -f1)",
    "install_ed_signature": "$INSTALL_ED_SIGNATURE" or None,
    "min_os": "$MIN_OS",
    "published_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
}, open(sys.argv[1], "w"), indent=2)
PY
echo "wrote $META"
