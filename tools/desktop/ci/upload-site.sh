#!/usr/bin/env bash
# Uploads the update site, as release.yml's site job builds it, to the R2 bucket served at
# https://pkg.peakpassvpn.com, through R2's S3 API (the AWS CLI).
#
#   upload-site.sh <site dir>           upload
#   upload-site.sh --plan <site dir>    print what would be uploaded, in order; touches nothing
#   upload-site.sh --check <site dir>   with the bucket's keys, read only: the plan, the
#                                       immutable files checked against the bucket as an upload
#                                       would, and the files an upload would overwrite; writes
#                                       nothing (the rehearsal runs it)
#
# The site is uploaded over what the bucket holds, while clients read it, so in an order in
# which every file a client can fetch names only files that are already there:
#
#   1  immutable: linux/apt/pool/…, the apt indexes' by-hash copies, linux/rpm/…/repodata/
#      except repomd.xml(.asc) (createrepo_c names them by their checksum), the versioned
#      setup packages linux/setup/*.{deb,rpm}, the macOS and Windows installers the Sparkle
#      feeds fetch, desktop/<channel>/<version>/<build>/<file>
#   2  the key, the setup packages' signatures (made anew on every build: gpg signatures
#      carry their time) and their stable aliases: linux/ppvpn.asc, linux/setup/*.asc,
#      linux/ppvpn-archive-keyring.deb, linux/ppvpn-release.rpm (+ .asc)
#   3  the apt package indexes: dists/<suite>/main/binary-amd64/Packages{,.gz,.xz}
#   4  the signed indexes: dists/<suite>/{Release,Release.gpg,InRelease}, repomd.xml(.asc)
#   5  the update checks: linux/<channel>/latest.json, desktop/<channel>/appcast-*.xml
#   6  the channel pointers, desktop/channels/*.json, which the release workflow reads back
#
# Anything else is uploaded with tier 5. Nothing is deleted: an index a client fetched before
# this upload names files that stay where they are (old packages, old by-hash indexes).
#
# An immutable file is never replaced: one already in the bucket with other content (its
# x-amz-meta-sha256, which this script sets, or else its content) is refused, before
# anything is uploaded; one with the same content is left as it is. Cache-Control is
# "public, max-age=31536000, immutable" for tier 1 and "no-cache" for the rest.
#
# Environment (upload and --check):
#   R2_ACCOUNT_ID          the Cloudflare account: https://<id>.r2.cloudflarestorage.com
#   R2_BUCKET              the bucket
#   AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY   an R2 API token with write access to it
#   AWS_DEFAULT_REGION     auto
set -euo pipefail

die() { echo "upload-site: $*" >&2; exit 1; }

PLAN_ONLY=0 CHECK_ONLY=0
case "${1:-}" in
  --plan) PLAN_ONLY=1; shift ;;
  --check) CHECK_ONLY=1; shift ;;
esac
[[ $# -eq 1 ]] || die "usage: upload-site.sh [--plan|--check] <site dir>"
SITE="${1%/}"
[[ -d "$SITE" ]] || die "$SITE is not a directory"

IMMUTABLE_CACHE="public, max-age=31536000, immutable"
MUTABLE_CACHE="no-cache"

tier() {   # path relative to the site
  case "$1" in
    linux/setup/*.asc) echo 2 ;;
    linux/apt/pool/*|linux/apt/dists/*/by-hash/*|linux/setup/*) echo 1 ;;
    linux/rpm/*/repodata/repomd.xml|linux/rpm/*/repodata/repomd.xml.asc) echo 4 ;;
    linux/rpm/*/repodata/*) echo 1 ;;
    desktop/*/[0-9]*.[0-9]*.[0-9]*/[0-9]*/*|android/*/[0-9]*.[0-9]*.[0-9]*/[0-9]*/*) echo 1 ;;
    linux/ppvpn.asc|linux/ppvpn-archive-keyring.deb|linux/ppvpn-archive-keyring.deb.asc|\
    linux/ppvpn-release.rpm|linux/ppvpn-release.rpm.asc) echo 2 ;;
    linux/apt/dists/*/Packages|linux/apt/dists/*/Packages.gz|linux/apt/dists/*/Packages.xz) echo 3 ;;
    linux/apt/dists/*/Release|linux/apt/dists/*/Release.gpg|linux/apt/dists/*/InRelease) echo 4 ;;
    desktop/channels/*|android/*/latest.json) echo 6 ;;
    *) echo 5 ;;
  esac
}

content_type() {
  case "$1" in
    */InRelease|*/Release|*/Packages) echo "text/plain; charset=utf-8" ;;
    *.deb) echo "application/vnd.debian.binary-package" ;;
    *.rpm) echo "application/x-rpm" ;;
    *.apk) echo "application/vnd.android.package-archive" ;;
    *.asc|*.gpg) echo "application/pgp-signature" ;;
    *.json) echo "application/json" ;;
    *.xml) echo "application/xml" ;;
    *.gz) echo "application/gzip" ;;
    *.xz) echo "application/x-xz" ;;
    *.zst) echo "application/zstd" ;;
    *.dmg) echo "application/x-apple-diskimage" ;;
    *.exe) echo "application/vnd.microsoft.portable-executable" ;;
    *) echo "application/octet-stream" ;;
  esac
}

# tier <tab> path, in upload order.
PLAN="$(mktemp)"
trap 'rm -f "$PLAN" "$PLAN.objects"' EXIT
(cd "$SITE" && find . -type f -print | sed 's#^\./##') | while IFS= read -r path; do
  [[ "$path" =~ ^[A-Za-z0-9._+~/-]+$ ]] || die "$path: unexpected characters in a path"
  printf '%s\t%s\n' "$(tier "$path")" "$path"
done | sort -t $'\t' -k1,1n -k2,2 >"$PLAN"
[[ -s "$PLAN" ]] || die "$SITE is empty"
grep -q $'^6\tdesktop/channels/stable.json$' "$PLAN" || die "$SITE has no desktop/channels/stable.json"

while IFS=$'\t' read -r t path; do
  cache="$MUTABLE_CACHE"
  [[ "$t" == 1 ]] && cache="$IMMUTABLE_CACHE"
  printf '%s  %-44s %s\n' "$t" "$cache" "$path"
done <"$PLAN"
[[ "$PLAN_ONLY" == 0 ]] || exit 0

: "${R2_ACCOUNT_ID:?R2_ACCOUNT_ID (the variable R2_ACCOUNT_ID) is not set}"
: "${R2_BUCKET:?R2_BUCKET (the variable PPVPN_PKG_R2_BUCKET) is not set}"
: "${AWS_ACCESS_KEY_ID:?AWS_ACCESS_KEY_ID (the secret PPVPN_PKG_R2_ACCESS_KEY_ID) is not set}"
: "${AWS_SECRET_ACCESS_KEY:?AWS_SECRET_ACCESS_KEY (the secret PPVPN_PKG_R2_SECRET_ACCESS_KEY) is not set}"
[[ "$R2_ACCOUNT_ID" =~ ^[0-9a-f]{32}$ ]] || die "R2_ACCOUNT_ID is not a 32-digit hex account id"
command -v aws >/dev/null 2>&1 || die "the AWS CLI is required"
s3api() { aws s3api --endpoint-url "https://$R2_ACCOUNT_ID.r2.cloudflarestorage.com" "$@"; }
sha256() { sha256sum "$1" | cut -d' ' -f1; }

# --- 1. the immutable files already in the bucket: the same, or refused -------------------
# One listing of each prefix, rather than a request per file. A listing that fails (no
# access) stops the run: nothing is uploaded blind.
for prefix in linux/ desktop/ android/; do
  listing=$(s3api list-objects-v2 --bucket "$R2_BUCKET" --prefix "$prefix" --query 'Contents[].Key' --output text) \
    || die "cannot list $prefix in bucket $R2_BUCKET"
  printf '%s\n' "$listing" | tr '\t' '\n' | grep -v '^None$' || true
done >"$PLAN.objects"
s3api head-bucket --bucket "$R2_BUCKET" >/dev/null || die "cannot read the bucket $R2_BUCKET"
declare -A SKIP=()
while IFS=$'\t' read -r t path; do
  [[ "$t" == 1 ]] || continue
  grep -qxF "$path" "$PLAN.objects" || continue
  want="$(sha256 "$SITE/$path")"
  have="$(s3api head-object --bucket "$R2_BUCKET" --key "$path" --query 'Metadata.sha256' --output text)"
  if [[ "$have" == None || -z "$have" ]]; then
    # Uploaded some other way: compare the content itself.
    s3api get-object --bucket "$R2_BUCKET" --key "$path" "$PLAN.object" >/dev/null
    have="$(sha256 "$PLAN.object")"
    rm -f "$PLAN.object"
  fi
  [[ "$have" == "$want" ]] || die "$path is in the bucket with other content (sha256 $have, not $want): an immutable file is never replaced"
  SKIP["$path"]=1
done <"$PLAN"

if [[ "$CHECK_ONLY" == 1 ]]; then
  echo "bucket $R2_BUCKET: $(wc -l <"$PLAN.objects" | tr -d ' ') objects under linux/, desktop/ and android/"
  echo "immutable files already there with the same content: ${#SKIP[@]}"
  while IFS=$'\t' read -r t path; do
    [[ "$t" != 1 ]] && grep -qxF "$path" "$PLAN.objects" && echo "would overwrite  $path"
  done <"$PLAN" || true
  echo "check only: wrote nothing"
  exit 0
fi

# --- 2. the upload, in order -----------------------------------------------------------------
uploaded=0 kept=0
while IFS=$'\t' read -r t path; do
  if [[ -n "${SKIP[$path]:-}" ]]; then
    echo "keep    $path"
    kept=$((kept + 1))
    continue
  fi
  cache="$MUTABLE_CACHE"
  [[ "$t" == 1 ]] && cache="$IMMUTABLE_CACHE"
  s3api put-object --bucket "$R2_BUCKET" --key "$path" --body "$SITE/$path" \
    --content-type "$(content_type "$path")" --cache-control "$cache" \
    --metadata "sha256=$(sha256 "$SITE/$path")" >/dev/null
  echo "upload  $path"
  uploaded=$((uploaded + 1))
done <"$PLAN"
echo "uploaded $uploaded files to $R2_BUCKET, kept $kept already there; deleted nothing"
