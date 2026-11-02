#!/usr/bin/env bash
# Uploads a release's desktop installers to the private R2 bucket the backend serves downloads
# from, through R2's S3 API (the AWS CLI). The website's download buttons and the update feed
# of the applications before 0.6.0 go to the backend, which reads this bucket on every request:
#
#   clients/desktop/native/releases/<version>/<build>/<installer>                 immutable
#   clients/desktop/native/releases/<version>/<build>/release-meta-<platform>.json  immutable
#   clients/desktop/native/channels/<channel>/release-meta-<platform>.json        the pointer
#
# A pointer is the platform's release-meta with a "path", the installer's key. The backend
# checks it (schema 1, platform, channel, version, build, file, length, sha256, min_os,
# published_at, the path's form, and ed_signature for macOS and Windows), signs a short-lived
# URL of the path for /api/v1/desktop/native/releases/<platform>/download and renders the
# Sparkle feed /api/v1/desktop/releases/<platform>/appcast.xml from it. Nothing is registered
# with the backend: moving the pointers is the release.
#
# macOS also has a first-install package (.pkg) next to the DMG, which /download serves when
# the pointer names it: install_file, install_length and install_sha256 in the release-meta,
# install_path (its key) added to the pointer. All of them or none (the backend answers 503
# to a partial set; none means the DMG), and only on macOS. The DMG stays in file/path: the
# Sparkle feeds update from it. The pkg's EdDSA signature (install_ed_signature, for the
# service's silent updates, #548) is for the update site; the pointers leave it out, so the
# backend sees the same three fields as before.
#
#   upload-downloads.sh <release dir>           upload
#   upload-downloads.sh --plan <release dir>    verify the files and print the keys; no bucket
#   upload-downloads.sh --check <release dir>   with the bucket's keys, read only: the plan,
#                                               what is already there and what each pointer
#                                               names now; writes nothing (the rehearsal)
#
# <release dir> is the release's files in one directory (release.yml's artifact
# release-files): the five installers, the macOS packages and their release-meta-<platform>.json. Every installer
# is checked against its release-meta before the bucket is touched. A release object is never
# replaced: one already there with other content is refused before anything is uploaded, one
# with the same content is kept (a run that failed half way can be run again). The pointers
# move last, after every upload succeeded.
#
# Environment:
#   VERSION, BUILD, CHANNEL   x.y.z, the build number (digits), stable
#   R2_ACCOUNT_ID             the Cloudflare account: https://<id>.r2.cloudflarestorage.com
#   R2_BUCKET                 the bucket (upload and --check)
#   AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY   an R2 API token with object read and write
#                             access to it (upload and --check)
#   AWS_DEFAULT_REGION        auto
set -euo pipefail

die() { echo "upload-downloads: $*" >&2; exit 1; }

PLAN_ONLY=0 CHECK_ONLY=0
case "${1:-}" in
  --plan) PLAN_ONLY=1; shift ;;
  --check) CHECK_ONLY=1; shift ;;
esac
[[ $# -eq 1 ]] || die "usage: upload-downloads.sh [--plan|--check] <release dir>"
DIR="${1%/}"
[[ -d "$DIR" ]] || die "$DIR is not a directory"
: "${VERSION:?VERSION is not set}" "${BUILD:?BUILD is not set}" "${CHANNEL:?CHANNEL is not set}"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "VERSION is not x.y.z: $VERSION"
[[ "$BUILD" =~ ^[0-9]+$ ]] || die "BUILD is not a number: $BUILD"
[[ "$CHANNEL" == stable ]] || die "only the stable channel is published, not $CHANNEL"
command -v jq >/dev/null 2>&1 || die "jq is required"

PLATFORMS=(macos-arm64 macos-x64 windows-x64 linux-x64-deb linux-x64-rpm)
RELEASES="clients/desktop/native/releases/$VERSION/$BUILD"
POINTERS="clients/desktop/native/channels/$CHANNEL"
IMMUTABLE_CACHE="public, max-age=31536000, immutable"
POINTER_CACHE="no-cache, no-store, must-revalidate"

sha256() { sha256sum "$1" | cut -d' ' -f1; }
content_type() {
  case "$1" in
    *.dmg) echo "application/x-apple-diskimage" ;;
    *.pkg) echo "application/octet-stream" ;;
    *.exe) echo "application/vnd.microsoft.portable-executable" ;;
    *.deb) echo "application/vnd.debian.binary-package" ;;
    *.rpm) echo "application/x-rpm" ;;
    *.json) echo "application/json" ;;
    *) echo "application/octet-stream" ;;
  esac
}

# --- 1. every platform's release-meta and installer, as the backend will check them ---------
declare -A FILE=() INSTALL=()
for platform in "${PLATFORMS[@]}"; do
  meta="$DIR/release-meta-$platform.json"
  [[ -f "$meta" ]] || die "release-meta-$platform.json is missing"
  field() { jq -r "$1 // empty" "$meta"; }
  [[ "$(field .schema)" == 1 ]] || die "$platform: schema is not 1"
  [[ "$(field .platform)" == "$platform" ]] || die "$platform: the release-meta is of $(field .platform)"
  [[ "$(field .channel)" == "$CHANNEL" ]] || die "$platform: channel is $(field .channel), not $CHANNEL"
  [[ "$(field .version)" == "$VERSION" ]] || die "$platform: version is $(field .version), not $VERSION"
  [[ "$(field .build)" == "$VERSION.$BUILD" ]] || die "$platform: build is $(field .build), not $VERSION.$BUILD"
  [[ -n "$(field .min_os)" ]] || die "$platform: min_os is missing"
  date -u -d "$(field .published_at)" +%s >/dev/null 2>&1 || die "$platform: published_at is not a time: $(field .published_at)"
  file="$(field .file)"
  [[ "$file" =~ ^[A-Za-z0-9._+~-]+$ ]] || die "$platform: unexpected file name: $file"
  [[ -f "$DIR/$file" ]] || die "$platform: $file is missing"
  [[ "$(field .length)" == "$(stat -c %s "$DIR/$file")" ]] || die "$platform: $file does not have the release-meta's length"
  [[ "$(field .sha256)" == "$(sha256 "$DIR/$file")" ]] || die "$platform: $file does not have the release-meta's SHA-256"
  # macOS and Windows update through Sparkle, which verifies the EdDSA signature; the Linux
  # packages are signed by their repositories instead.
  case "$platform" in
    linux-*) ;;
    *) [[ -n "$(field .ed_signature)" ]] || die "$platform: ed_signature is missing" ;;
  esac
  FILE[$platform]="$file"
  # The first-install package: all three fields valid, or none, and its EdDSA signature when
  # the release has one; install_path is the upload's.
  [[ -z "$(field .install_path)" ]] || die "$platform: install_path is set by this upload, not by the release-meta"
  install_keys="$(jq -r '[keys[] | select(startswith("install_"))] | join(" ")' "$meta")"
  if [[ -n "$install_keys" ]]; then
    [[ "$platform" == macos-* ]] || die "$platform: install_* fields are for macOS only ($install_keys)"
    [[ "$install_keys" == "install_file install_length install_sha256" \
      || "$install_keys" == "install_ed_signature install_file install_length install_sha256" ]] \
      || die "$platform: install_* fields must be install_file, install_length and install_sha256 together (and install_ed_signature), not: $install_keys"
    [[ "$install_keys" != install_ed_signature* || -n "$(field .install_ed_signature)" ]] \
      || die "$platform: install_ed_signature is empty"
    install="$(field .install_file)"
    [[ "$install" =~ ^[A-Za-z0-9._+~-]+\.pkg$ ]] || die "$platform: unexpected install_file: $install"
    [[ -f "$DIR/$install" ]] || die "$platform: $install is missing"
    jq -e '(.install_length | type) == "number" and (.install_sha256 | type) == "string"' "$meta" >/dev/null \
      || die "$platform: install_length must be a number and install_sha256 a string"
    [[ "$(field .install_sha256)" =~ ^[0-9a-f]{64}$ ]] || die "$platform: install_sha256 is not 64 hex characters"
    [[ "$(field .install_length)" == "$(stat -c %s "$DIR/$install")" ]] || die "$platform: $install does not have the release-meta's install_length"
    [[ "$(field .install_sha256)" == "$(sha256 "$DIR/$install")" ]] || die "$platform: $install does not have the release-meta's install_sha256"
    INSTALL[$platform]="$install"
  fi
done

# key <tab> local file, in upload order: the release objects of every platform.
PLAN="$(mktemp)" WORK="$(mktemp -d)"
trap 'rm -rf "$PLAN" "$WORK"' EXIT
for platform in "${PLATFORMS[@]}"; do
  printf '%s\t%s\n' "$RELEASES/${FILE[$platform]}" "$DIR/${FILE[$platform]}"
  [[ -z "${INSTALL[$platform]:-}" ]] || printf '%s\t%s\n' "$RELEASES/${INSTALL[$platform]}" "$DIR/${INSTALL[$platform]}"
  printf '%s\t%s\n' "$RELEASES/release-meta-$platform.json" "$DIR/release-meta-$platform.json"
done >"$PLAN"
for platform in "${PLATFORMS[@]}"; do
  if [[ -n "${INSTALL[$platform]:-}" ]]; then
    jq --arg path "$RELEASES/${FILE[$platform]}" --arg install "$RELEASES/${INSTALL[$platform]}" \
      '. + {path: $path, install_path: $install} | del(.install_ed_signature)' "$DIR/release-meta-$platform.json"
  else
    jq --arg path "$RELEASES/${FILE[$platform]}" '. + {path: $path}' "$DIR/release-meta-$platform.json"
  fi >"$WORK/release-meta-$platform.json"
  # The pointer as the backend checks it: install_path, when there, is the
  # install_file of this release's directory, as path is the file's.
  pointer="$WORK/release-meta-$platform.json"
  [[ "$(jq -r .path "$pointer")" == "$RELEASES/${FILE[$platform]}" ]] || die "$platform: the pointer's path is not $RELEASES/${FILE[$platform]}"
  if [[ -n "${INSTALL[$platform]:-}" ]]; then
    [[ "$(jq -r .install_path "$pointer")" == "$RELEASES/$(jq -r .install_file "$pointer")" ]] \
      || die "$platform: the pointer's install_path is not $RELEASES/<install_file>"
  else
    [[ "$(jq '[keys[] | select(startswith("install_"))] | length' "$pointer")" == 0 ]] || die "$platform: the pointer has install_* fields"
  fi
done

while IFS=$'\t' read -r key _; do
  printf 'release  %s\n' "$key"
done <"$PLAN"
for platform in "${PLATFORMS[@]}"; do
  printf 'pointer  %s\n' "$POINTERS/release-meta-$platform.json"
done
[[ "$PLAN_ONLY" == 0 ]] || exit 0

: "${R2_ACCOUNT_ID:?R2_ACCOUNT_ID (the variable R2_ACCOUNT_ID) is not set}"
: "${R2_BUCKET:?R2_BUCKET (the variable R2_CLIENT_BUCKET) is not set}"
: "${AWS_ACCESS_KEY_ID:?AWS_ACCESS_KEY_ID (the secret R2_ACCESS_KEY_ID) is not set}"
: "${AWS_SECRET_ACCESS_KEY:?AWS_SECRET_ACCESS_KEY (the secret R2_SECRET_ACCESS_KEY) is not set}"
[[ "$R2_ACCOUNT_ID" =~ ^[0-9a-f]{32}$ ]] || die "R2_ACCOUNT_ID is not a 32-digit hex account id"
command -v aws >/dev/null 2>&1 || die "the AWS CLI is required"
s3api() { aws s3api --endpoint-url "https://$R2_ACCOUNT_ID.r2.cloudflarestorage.com" "$@"; }

# Whether the bucket has the key. Anything but "it is there" and "not found" stops the run
# (no access, a wrong bucket): nothing is uploaded blind.
has() {
  if s3api head-object --bucket "$R2_BUCKET" --key "$1" >/dev/null 2>"$WORK/head.err"; then
    return 0
  fi
  grep -q -E '\(404\)|Not Found' "$WORK/head.err" || die "cannot read $1 in $R2_BUCKET: $(tr '\n' ' ' <"$WORK/head.err")"
  return 1
}

# --- 2. the release objects already in the bucket: the same, or refused ---------------------
declare -A SKIP=()
while IFS=$'\t' read -r key file; do
  has "$key" || continue
  want="$(sha256 "$file")"
  have="$(s3api head-object --bucket "$R2_BUCKET" --key "$key" --query 'Metadata.sha256' --output text)"
  if [[ "$have" == None || -z "$have" ]]; then
    # Uploaded some other way: compare the content itself.
    s3api get-object --bucket "$R2_BUCKET" --key "$key" "$WORK/object" >/dev/null
    have="$(sha256 "$WORK/object")"
    rm -f "$WORK/object"
  fi
  [[ "$have" == "$want" ]] || die "$key is in the bucket with other content (sha256 $have, not $want): a release object is never replaced"
  SKIP["$key"]=1
done <"$PLAN"

if [[ "$CHECK_ONLY" == 1 ]]; then
  echo "bucket $R2_BUCKET: release objects already there with the same content: ${#SKIP[@]}"
  for platform in "${PLATFORMS[@]}"; do
    key="$POINTERS/release-meta-$platform.json"
    if has "$key"; then
      s3api get-object --bucket "$R2_BUCKET" --key "$key" "$WORK/pointer" >/dev/null
      echo "would move  $key  from $(jq -r '"\(.version) (build \(.build)), \(.path)"' "$WORK/pointer") to $VERSION (build $VERSION.$BUILD)"
    else
      echo "would create  $key"
    fi
  done
  echo "check only: wrote nothing"
  exit 0
fi

# --- 3. the release objects, then the pointers ----------------------------------------------
uploaded=0 kept=0
while IFS=$'\t' read -r key file; do
  if [[ -n "${SKIP[$key]:-}" ]]; then
    echo "keep    $key"
    kept=$((kept + 1))
    continue
  fi
  s3api put-object --bucket "$R2_BUCKET" --key "$key" --body "$file" \
    --content-type "$(content_type "$key")" --cache-control "$IMMUTABLE_CACHE" \
    --metadata "sha256=$(sha256 "$file")" >/dev/null
  echo "upload  $key"
  uploaded=$((uploaded + 1))
done <"$PLAN"
for platform in "${PLATFORMS[@]}"; do
  key="$POINTERS/release-meta-$platform.json"
  s3api put-object --bucket "$R2_BUCKET" --key "$key" --body "$WORK/release-meta-$platform.json" \
    --content-type application/json --cache-control "$POINTER_CACHE" >/dev/null
  echo "pointer $key"
done
echo "uploaded $uploaded release objects to $R2_BUCKET, kept $kept already there; moved ${#PLATFORMS[@]} pointers to $VERSION (build $VERSION.$BUILD)"
