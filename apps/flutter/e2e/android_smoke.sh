#!/usr/bin/env bash
# The Android smoke test (#448 steps 3 and 5) on a running emulator (ci.yml,
# job android, after ci-build.sh with FLUTTER_TARGET=android):
# integration_test/mobile_smoke_test.dart twice, its debug APK carrying the
# release bridge and tunnel host ci-build.sh put in jniLibs. First a
# tablet's screen in landscape (1280 dp across: the desktop layout), then
# a phone's, about 390 dp across in portrait (the phone layout below 600).
set -euo pipefail
cd "$(dirname "$0")/.."

adb wait-for-device
device=$(adb devices | awk 'NR > 1 && $2 == "device" { print $1; exit }')
[ -n "$device" ] || { echo "no emulator" >&2; exit 1; }
# Both shapes come from the screen size (wm size), at rotation 0: a
# rotation (user_rotation) applied a minute or more after the settings
# change, so the tablet leg ran in portrait and the rotation landed in the
# phone leg, which then had the desktop layout in a phone's height (run
# 40828057797).
adb shell settings put system accelerometer_rotation 0
adb shell settings put system user_rotation 0
# The test's system VPN, allowed as soon as flutter test has installed the
# app (it installs it for each leg and uninstalls it after), long before
# the test gets to it: no consent dialog, which the test cannot answer.
(
  while true; do
    adb shell appops set com.peakpassvpn.ppvpn ACTIVATE_VPN allow >/dev/null 2>&1 || true
    sleep 0.5
  done
) &
grant=$!
adb logcat -c || true
# The emulator can be gone by the time this runs, after both legs passed,
# and adb then waits for it without end ("- waiting for device -": the
# step's 30 minutes ran out, job 124559462808): every adb call here has a
# deadline. TunnelService's lines (tag PPVPN), for a failed leg, also kept
# for the job's artifact.
trap 'kill "$grant" 2>/dev/null || true
  timeout 20 adb logcat -d -s PPVPN:V >build/smoke-logcat.txt 2>&1 || true
  cat build/smoke-logcat.txt
  timeout 20 adb shell wm size reset || true; timeout 20 adb shell wm density reset || true' EXIT

# A size applies some time after wm size; a leg that started before then
# would lay out for the other shape. Waits until the app area the window
# manager reports is wider than tall (landscape) or taller than wide
# (portrait) on three reads in a row, up to 60 s.
wait_for() { # landscape|portrait
  local size w h seen=0
  for _ in $(seq 60); do
    size=$(adb shell dumpsys window displays | grep -oE ' app=[0-9]+x[0-9]+' | head -1 | cut -d= -f2) || true
    if [ -n "$size" ] && { w=${size%x*} h=${size#*x}; if [ "$1" = landscape ]; then [ "$w" -gt "$h" ]; else [ "$h" -gt "$w" ]; fi; }; then
      seen=$((seen + 1))
      if [ "$seen" -ge 3 ]; then
        echo "$1: app area $size"
        return
      fi
    else
      seen=0
    fi
    sleep 1
  done
  echo "the screen did not turn $1 in 60 s (app area '${size:-unknown}')" >&2
  exit 1
}

# One leg, judged by its tests, not by flutter test's exit status: after
# the tests, flutter test uninstalls the app, and that fails when the
# emulator has gone offline in the meantime, after both legs had passed
# (job 124155824780). The JSON report (one event per line): every test
# that started finished, none failed or errored, and at least one that
# was not hidden (loading) or skipped passed. A failed, errored or
# unfinished test fails the leg.
leg() { # name
  local report="$PWD/build/smoke-$1.json" status=0 unfinished ran bad
  rm -f "$report"
  flutter test integration_test/mobile_smoke_test.dart -d "$device" \
    --flavor apk --dart-define=PPVPN_ANDROID_CHANNEL=apk \
    --file-reporter "json:$report" || status=$?
  [ "$status" -eq 0 ] && return
  [ -s "$report" ] || { echo "$1: flutter test exited $status, no report" >&2; exit 1; }
  unfinished=$(jq -s '([.[] | select(.type == "testStart") | .test.id] - [.[] | select(.type == "testDone") | .testID]) | length' "$report")
  ran=$(jq -s '[.[] | select(.type == "testDone" and .hidden != true and .skipped != true and .result == "success")] | length' "$report")
  bad=$(jq -s '[.[] | select(.type == "testDone" and .result != "success")] | length' "$report")
  if [ "$bad" -eq 0 ] && [ "$ran" -gt 0 ] && [ "$unfinished" -eq 0 ]; then
    echo "$1: flutter test exited $status after its $ran test(s) passed (the uninstall afterwards); the leg passed"
    return
  fi
  echo "$1: flutter test exited $status: $ran passed, $bad failed, $unfinished unfinished" >&2
  # What the report says went wrong (the whole report is the job's
  # artifact).
  jq -r 'select(.type == "error") | "error: \(.error)"' "$report" | head -n 20 >&2 || true
  jq -c 'select(.type == "testDone" and .result != "success")' "$report" | head -n 10 >&2 || true
  exit 1
}

echo "--- tablet, landscape"
adb shell wm size 2560x1800
adb shell wm density 320
wait_for landscape
leg tablet

echo "--- phone, portrait"
adb shell wm size 1080x2340
adb shell wm density 440
wait_for portrait
leg phone
