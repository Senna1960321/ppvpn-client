#!/usr/bin/env bash
# Build the macOS app as one DMG (and .pkg) per architecture, plus release
# metadata for each: macos-arm64 (Apple silicon) and macos-x64 (Intel), both
# built on an Apple silicon runner, from the Flutter app
# (apps/flutter/scripts/package-macos.sh, #401).
#
# Required:
#   CI_PROJECT_DIR
#   PPVPN_FLUTTER_BUNDLE       the universal PPVPN.app of apps/flutter/scripts/ci-build.sh
#                              (built with MACOS_ARCHS and this script's environment)
# Optional:
#   MACOS_ARCHS                "arm64 x86_64" (default), or one of them
#   RUST_TOOLCHAIN             default stable
#   PPVPN_BUILD_NUMBER         monotonic counter shared by the release
#   PPVPN_RELEASE_CHANNEL      dev | stable, recorded in release-meta
#   PPVPN_VERSION              x.y.z from the release tag (default: project version)
#   PPVPN_API_BASE_DEFAULT     the backend (required; release.yml sets it, docs/release.md 后端地址)
#   PPVPN_UPDATE_SITE          base address of the update site; with a channel, each
#                              build's Sparkle feed is
#                              <site>/desktop/<channel>/appcast-<platform>.xml;
#                              without either, the build has no update feed
#   PPVPN_SPARKLE_PUBLIC_KEY   EdDSA public key; empty disables updates
#   SPARKLE_PRIVATE_KEY        EdDSA private key (base64); signs the DMGs and pkgs
#   MACOS_SIGNING_P12_BASE64   the Developer ID Application certificate + key of
#   MACOS_SIGNING_P12_PASSWORD   our team (crates/service/src/macos_team_id, #471);
#                              without them builds are ad-hoc
#   MACOS_INSTALLER_P12_BASE64 the Developer ID Installer certificate + key: signs
#   MACOS_INSTALLER_P12_PASSWORD the pkg
#   ASC_KEY_ID, ASC_ISSUER_ID, App Store Connect API key (contents of the .p8):
#   ASC_KEY_P8                 notarizes the app, the DMG and the pkg
#   A stable build needs all three; anything else signs and notarizes with
#   what it has, and builds ad hoc (with a warning) when the certificate is
#   not our Developer ID Application (the dev environment's old one).
#   MACOS_STAGING_DIR          output directory (default staging/macos-native)
#   PPVPN_PREBUILT=1           the Rust is built already, by other jobs (release.yml):
#                              the service helpers in build/binaries
#                              (tools/desktop/build-service.sh macos) and
#                              apps/macos/PPVPNClient (crates/desktop/scripts/build-apple.sh);
#                              only the Xcode project is generated here
set -euo pipefail

: "${CI_PROJECT_DIR:?CI_PROJECT_DIR is required}"
: "${PPVPN_FLUTTER_BUNDLE:?PPVPN_FLUTTER_BUNDLE is required}"
: "${MACOS_ARCHS:=arm64 x86_64}"
: "${RUST_TOOLCHAIN:=stable}"
: "${PPVPN_BUILD_NUMBER:?PPVPN_BUILD_NUMBER is required}"
: "${PPVPN_API_BASE_DEFAULT:?PPVPN_API_BASE_DEFAULT (the backend) is required}"

for arch in $MACOS_ARCHS; do
  case "$arch" in arm64|x86_64) ;; *) echo "unsupported arch in MACOS_ARCHS: $arch" >&2; exit 2 ;; esac
done

cd "$CI_PROJECT_DIR"
command -v xcodegen >/dev/null || brew install xcodegen
if [[ "${PPVPN_PREBUILT:-}" == 1 ]]; then
  (cd apps/macos && xcodegen generate)
else
  rustup toolchain install "$RUST_TOOLCHAIN" --profile minimal
  rustup default "$RUST_TOOLCHAIN"
  # ppvpn-client's build-apple.sh builds both slices; each package is thinned.
  rustup target add aarch64-apple-darwin x86_64-apple-darwin
  # Universal service helpers; package-app.sh thins them per build.
  bash tools/desktop/build-service.sh macos
fi

# A build for a channel updates from that channel's feed; one without has no feed.
if [[ -n "${PPVPN_RELEASE_CHANNEL:-}" && -z "${PPVPN_UPDATE_SITE:-}" ]]; then
  echo "PPVPN_UPDATE_SITE is required for a channel build" >&2
  exit 3
fi
if [[ -n "${PPVPN_UPDATE_SITE:-}" && ! "$PPVPN_UPDATE_SITE" =~ ^https://[^/[:space:]]+(/[^[:space:]]*)?$ ]]; then
  echo "PPVPN_UPDATE_SITE must be an https URL" >&2
  exit 3
fi
export PPVPN_API_BASE_DEFAULT
export PPVPN_SPARKLE_PUBLIC_KEY="${PPVPN_SPARKLE_PUBLIC_KEY:-}"
export PPVPN_BUILD_NUMBER
unset PPVPN_UPDATE_FEED_URL

key_dir="$(mktemp -d)"
keychain=""
original_keychains=()
cleanup() {
  if [[ -n "$keychain" ]]; then
    security list-keychains -d user -s ${original_keychains[@]+"${original_keychains[@]}"} || true
    security delete-keychain "$keychain" || true
  fi
  rm -rf "$key_dir"
}
trap cleanup EXIT

# Code signing with our Developer ID (#471): Apple's designated requirements
# (Team ID) carry across updates and certificate renewals, and the service
# accepts only clients signed by the team (crates/service/src/macsign.rs).
# A temporary keychain holds the identities for this build only.
team="$(tr -d '[:space:]' < crates/service/src/macos_team_id)"
if [[ "${PPVPN_RELEASE_CHANNEL:-}" == stable ]]; then
  for name in MACOS_SIGNING_P12_BASE64 MACOS_INSTALLER_P12_BASE64 ASC_KEY_ID ASC_ISSUER_ID ASC_KEY_P8; do
    [[ -n "${!name:-}" ]] || { echo "a stable build must be signed and notarized: $name is not set" >&2; exit 5; }
  done
fi
# The one identity "<kind>: … (<team>)" in the keychain, or none.
identity() {
  security find-identity -v "$keychain" \
    | sed -n 's/^ *[0-9][0-9]*) [0-9A-F]\{40\} "\(.*\)"$/\1/p' \
    | grep -x "$1: .* ($team)" || true
}
# Why `security import` refuses a .p12 whose password is right: it reads
# only the legacy PKCS#12 algorithms (3DES, SHA-1 MAC), and reports OpenSSL
# 3's defaults (PBES2/AES-256, SHA-256 MAC) as "MAC verification failed
# ... (wrong password?)".
p12_hint() { # <secret>
  echo "$1 could not be imported. If the password is right and the .p12 came from OpenSSL 3, re-export it with \`openssl pkcs12 -export -legacy\` or from Keychain Access." >&2
}
# Imports a .p12 and prints its identity: one "<kind>: … (<team>)", or fails.
import_identity() { # <kind> <p12 base64> <password> <secret>
  printf '%s' "$2" | base64 --decode > "$key_dir/identity.p12"
  if ! security import "$key_dir/identity.p12" -k "$keychain" -P "$3" \
    -T /usr/bin/codesign -T /usr/bin/productsign >/dev/null; then
    rm -f "$key_dir/identity.p12"
    p12_hint "$4"
    return 1
  fi
  rm -f "$key_dir/identity.p12"
  local found
  found="$(identity "$1")"
  if [[ -z "$found" || "$found" == *$'\n'* ]]; then
    echo "the certificate is not one \"$1: … ($team)\" identity; the keychain holds:" >&2
    security find-identity -v "$keychain" >&2
    return 1
  fi
  echo "$found"
}
signed=0
if [[ -n "${MACOS_SIGNING_P12_BASE64:-}" ]]; then
  : "${MACOS_SIGNING_P12_PASSWORD:?MACOS_SIGNING_P12_PASSWORD is required with MACOS_SIGNING_P12_BASE64}"
  keychain="$key_dir/signing.keychain-db"
  keychain_password="$(uuidgen)"
  while IFS= read -r line; do original_keychains+=("$(echo "$line" | tr -d ' "')"); done \
    < <(security list-keychains -d user)
  security create-keychain -p "$keychain_password" "$keychain"
  security set-keychain-settings -lut 21600 "$keychain"
  security unlock-keychain -p "$keychain_password" "$keychain"
  if PPVPN_CODESIGN_IDENTITY="$(import_identity "Developer ID Application" \
    "$MACOS_SIGNING_P12_BASE64" "$MACOS_SIGNING_P12_PASSWORD" MACOS_SIGNING_P12_BASE64)"; then
    signed=1
  elif [[ "${PPVPN_RELEASE_CHANNEL:-}" == stable ]]; then
    exit 5
  else
    # A dev build whose environment still holds another certificate (the
    # self-signed one of 0.7.x): built ad hoc, as without one. The service
    # it ships accepts only our Developer ID, so its enhanced mode needs
    # the dev environment to sign with it too.
    echo "::warning::MACOS_SIGNING_P12_BASE64 is not our Developer ID Application certificate; signing ad-hoc (not for release)"
  fi
fi
if [[ "$signed" == 1 ]]; then
  if [[ -n "${MACOS_INSTALLER_P12_BASE64:-}" ]]; then
    : "${MACOS_INSTALLER_P12_PASSWORD:?MACOS_INSTALLER_P12_PASSWORD is required with MACOS_INSTALLER_P12_BASE64}"
    PPVPN_INSTALLER_IDENTITY="$(import_identity "Developer ID Installer" \
      "$MACOS_INSTALLER_P12_BASE64" "$MACOS_INSTALLER_P12_PASSWORD" MACOS_INSTALLER_P12_BASE64)"
    export PPVPN_INSTALLER_IDENTITY
  else
    echo "warning: MACOS_INSTALLER_P12_BASE64 not set; the pkg is unsigned"
  fi
  security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$keychain_password" "$keychain" >/dev/null
  security list-keychains -d user -s "$keychain" ${original_keychains[@]+"${original_keychains[@]}"}
  export PPVPN_CODESIGN_IDENTITY
  export PPVPN_CODESIGN_TEAM="$team"
  if [[ -n "${ASC_KEY_P8:-}" ]]; then
    : "${ASC_KEY_ID:?ASC_KEY_ID is required with ASC_KEY_P8}"
    : "${ASC_ISSUER_ID:?ASC_ISSUER_ID is required with ASC_KEY_P8}"
    (umask 077 && printf '%s\n' "$ASC_KEY_P8" > "$key_dir/notary.p8")
    export PPVPN_NOTARY_KEY_FILE="$key_dir/notary.p8"
    export PPVPN_NOTARY_KEY_ID="$ASC_KEY_ID"
    export PPVPN_NOTARY_ISSUER="$ASC_ISSUER_ID"
  else
    echo "warning: ASC_KEY_P8 not set; signed but not notarized (Gatekeeper refuses a download)"
  fi
else
  unset PPVPN_CODESIGN_IDENTITY
  [[ -n "${MACOS_SIGNING_P12_BASE64:-}" ]] \
    || echo "warning: MACOS_SIGNING_P12_BASE64 not set; signing ad-hoc (not for release)"
fi
if [[ -n "${SPARKLE_PRIVATE_KEY:-}" ]]; then
  [[ -n "$PPVPN_SPARKLE_PUBLIC_KEY" ]] || { echo "SPARKLE_PRIVATE_KEY set without PPVPN_SPARKLE_PUBLIC_KEY" >&2; exit 3; }
  printf '%s' "$SPARKLE_PRIVATE_KEY" > "$key_dir/sparkle.key"
  export SPARKLE_PRIVATE_KEY_FILE="$key_dir/sparkle.key"
else
  echo "warning: SPARKLE_PRIVATE_KEY not set; release-meta ed_signature will be null (not publishable)"
fi

staging_dir="${MACOS_STAGING_DIR:-$CI_PROJECT_DIR/staging/macos-native}"
rm -rf "$staging_dir"
mkdir -p "$staging_dir"

bootstrap=0
[[ "${PPVPN_PREBUILT:-}" == 1 ]] && bootstrap=1
for arch in $MACOS_ARCHS; do
  platform=macos-arm64
  [[ "$arch" == x86_64 ]] && platform=macos-x64
  # The client package is built once (universal) and reused.
  SKIP_BOOTSTRAP=$bootstrap apps/flutter/scripts/package-macos.sh "$PPVPN_FLUTTER_BUNDLE" "$arch"
  app="apps/flutter/build/macos-package-$arch/PPVPN.app"
  bootstrap=1

  expected_signature='Signature=adhoc'
  [[ -n "${PPVPN_CODESIGN_IDENTITY:-}" ]] && expected_signature="Authority=$PPVPN_CODESIGN_IDENTITY"
  if ! codesign -dv --verbose=4 "$app" 2>&1 | grep -qx "$expected_signature"; then
    echo "macOS app ($arch) is not signed as expected ($expected_signature)" >&2
    exit 4
  fi
  feed="$(/usr/libexec/PlistBuddy -c 'Print PPVPNUpdateFeedURL' "$app/Contents/Info.plist")"
  expected=""
  if [[ -n "${PPVPN_UPDATE_SITE:-}" && -n "${PPVPN_RELEASE_CHANNEL:-}" ]]; then
    expected="${PPVPN_UPDATE_SITE%/}/desktop/$PPVPN_RELEASE_CHANNEL/appcast-$platform.xml"
  fi
  [[ "$feed" == "$expected" ]] || { echo "feed URL mismatch ($arch): $feed" >&2; exit 4; }

  cp "dist/macos/"*"-$platform.dmg" "dist/macos/"*"-$platform.pkg" "dist/macos/release-meta-$platform.json" "$staging_dir/"
done
ls -l "$staging_dir"
