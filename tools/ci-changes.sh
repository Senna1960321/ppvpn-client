#!/usr/bin/env bash
# What a pull request or a main push changes: prints `name=true|false` lines
# for $GITHUB_OUTPUT.
#   code  anything outside docs/ and Markdown. ci.yml's jobs skip their
#         build and test steps when it is false (the jobs still run and
#         report, so a docs-only pull request passes quickly).
#   flutter  apps/flutter/ (the Flutter desktop app, #401, with its
#         packaging), the crates it links (crates/{desktop,account,core,
#         engine-host}) and the service its installers ship
#         (crates/service), what its macOS packaging takes from apps/macos
#         (the scripts, the push agent and what it builds from, the
#         LaunchAgent, the .pkg's postinstall), the dependencies or the
#         workflows. ci.yml's flutter job builds only when it is true.
#         Outside apps/flutter/rust/ it does not set code: a
#         change to the Flutter app's Dart, scripts or assets runs only the
#         flutter job (and lint's sensitive check, which always runs). Its
#         Rust bridge (apps/flutter/rust/) and push agent
#         (apps/flutter/push-agent/) are workspace members, so they still
#         set code for the workspace's clippy and tests.
#   deps  Cargo.toml, Cargo.lock, rust-toolchain.toml, the shared actions or
#         the two workflows: what the build caches are keyed on or built by.
#         ci.yml's advisories job fails on an advisory only when it is true.
#   lock  what rust-cache's keys hash (#459): a Cargo.toml, Cargo.lock,
#         rust-toolchain.toml or .cargo/config.toml. release.yml looks the
#         caches up on main only when it is true (or one is missing): a
#         change to them makes new keys.
#   macpkg  what the flutter job's Package (macOS) step builds and checks
#         beyond the app the Build step made (#459): apps/macos/ (its
#         scripts, packaging, project, client package and agent), the
#         packaging and build scripts in apps/flutter/scripts/, the macOS
#         runner (apps/flutter/runner/macos/), the Flutter plugins (their
#         frameworks are signed in the bundle: pubspec), the client crate,
#         the service, tools/desktop/, or ci.yml, the shared actions and the
#         dependencies.
#         ~5 min on a cache hit, ~15 on a miss, on the scarce macOS runners;
#         a Dart-only change, for one, does not set it.
#   tunnel  the mobile tunnel host (crates/tunnel-host/, #448),
#         crates/engine-host/, the dependencies (Cargo.toml, crates/core's
#         target-specific ones, Cargo.lock) or ci.yml, and the only core code
#         that iOS compiles and macOS does not (the fallbacks of
#         hostipv6/mod.rs and localdns/source.rs; the rest builds the same
#         as on macOS, which the macos job's clippy covers): ci.yml's
#         `tunnel (ios)` builds it for iOS only then (~25 min cold, on the
#         scarce macOS runners), and `tunnel (android)` for Android.
#   android  flutter or tunnel: the Android app packs the tunnel host for
#         its VpnService (#448), so ci.yml's android job builds and tests
#         when either changed.
#   memory  the memory harness (ne-mem, tools/perf/{ne_mem,dhat_top}.py, test/perf/)
#         or Cargo.lock (a sail upgrade): ci.yml's macos and linux jobs
#         then build it in release and measure (#388).
# Every other event (workflow_dispatch, a new branch), a compare that fails,
# or one with 300 files or more (the API's page limit) counts as changing
# everything.
set -euo pipefail

all() { printf '%s=true\n' code flutter deps lock macpkg memory tunnel android; exit 0; }

case "${GITHUB_EVENT_NAME:-}" in
  pull_request) base=${PR_BASE_SHA:-} ;;
  push) base=${PUSH_BEFORE:-} ;;
  *) all ;;
esac
[[ -n $base && $base != 0000000000000000000000000000000000000000 ]] || all

files=$(gh api "repos/$GITHUB_REPOSITORY/compare/$base...$GITHUB_SHA" \
          --jq '.files[].filename') || all
n=$(grep -c . <<<"$files" || true)
(( n > 0 && n < 300 )) || all

code=false flutter=false deps=false lock=false macpkg=false memory=false tunnel=false
while IFS= read -r f; do
  case $f in
    *.md) ;;
    Cargo.toml | Cargo.lock | rust-toolchain.toml | .github/actions/* | .github/workflows/ci.yml | \
    apps/macos/* | apps/flutter/scripts/* | apps/flutter/packaging/macos/* | \
    apps/flutter/runner/macos/* | apps/flutter/pubspec.* | \
    crates/desktop/* | crates/service/* | tools/desktop/*) macpkg=true ;;
  esac
  case $f in
    Cargo.toml | */Cargo.toml | Cargo.lock | rust-toolchain.toml | .cargo/config.toml) lock=true ;;
  esac
  case $f in
    Cargo.lock | crates/core-lab/src/bin/ne-mem.rs | tools/perf/ne_mem.py | tools/perf/dhat_top.py | \
    test/perf/*) memory=true ;;
  esac
  case $f in
    *.md) ;;
    Cargo.toml | Cargo.lock | rust-toolchain.toml | .github/workflows/ci.yml | crates/core/Cargo.toml | \
    crates/core/src/hostipv6/mod.rs | crates/core/src/localdns/source.rs | \
    crates/engine-host/* | crates/tunnel-host/*) tunnel=true ;;
  esac
  case $f in
    docs/* | *.md) ;;
    Cargo.toml | Cargo.lock | rust-toolchain.toml | .github/actions/* | \
    .github/workflows/ci.yml | .github/workflows/release.yml)
      deps=true flutter=true code=true ;;
    apps/flutter/rust/* | apps/flutter/push-agent/*) flutter=true code=true ;;
    apps/flutter/*) flutter=true ;;
    crates/desktop/* | crates/engine-host/* | crates/service/*) flutter=true code=true ;;
    crates/account/* | crates/core/*) flutter=true code=true ;;
    apps/macos/scripts/* | apps/macos/PPVPNAgent/* | apps/macos/AppLogic/* | \
    apps/macos/LaunchAgents/* | apps/macos/packaging/* | apps/macos/project.yml)
      flutter=true code=true ;;
    apps/* | tools/desktop/*) code=true ;;
    *) code=true ;;
  esac
done <<<"$files"
echo "code=$code"
echo "flutter=$flutter"
echo "deps=$deps"
echo "lock=$lock"
echo "macpkg=$macpkg"
echo "memory=$memory"
echo "tunnel=$tunnel"
if [ "$flutter" = true ] || [ "$tunnel" = true ]; then
  echo "android=true"
else
  echo "android=false"
fi
