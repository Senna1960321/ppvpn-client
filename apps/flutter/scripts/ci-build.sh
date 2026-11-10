#!/usr/bin/env bash
# Builds the Flutter app for the runner's platform (ci.yml, job flutter;
# release.yml's installer jobs), or with FLUTTER_TARGET=android the Android
# app on Linux (ci.yml, job android; #448), or with FLUTTER_TARGET=ios the
# iOS app on macOS (ci.yml, job ios): the platform runner (flutter create, with
# our files from runner/<os> over it and the native app's identity), the
# bridge glue (flutter_rust_bridge_codegen), analysis, the Rust library
# (release), the app (release), the library put into the bundle. The
# bundle's path goes to $GITHUB_OUTPUT as `bundle`.
# Nothing generated here is committed (see apps/flutter/.gitignore).
#
# Optional, as release.yml passes them:
#   PPVPN_VERSION        x.y.z (default: the workspace's, Cargo.toml)
#   PPVPN_BUILD_NUMBER   the release's one counter, DESKTOP_BUILD_OFFSET plus
#                        its run number, 0-65535 (default 0): the fourth part
#                        of the Windows file version, and CFBundleVersion
#                        <version>.<build> on macOS, as for the native apps
#   FLUTTER_TARGET       android: the APK (needs ANDROID_NDK_HOME and the
#                        Android SDK), ios: the iOS app, unsigned, instead of
#                        the runner's platform
#   ANDROID_FORMAT       android: apk (the default; arm64 and x86_64, for a
#                        phone or the emulator) or aab (the App Bundle for
#                        Google Play: release.yml)
#   ANDROID_KEYSTORE_FILE, ANDROID_KEYSTORE_PASSWORD, ANDROID_KEY_ALIAS,
#   ANDROID_KEY_PASSWORD
#                        android: signed with this upload key (the Play
#                        upload key, release.yml); without them with the
#                        template's debug key, which installs by hand but
#                        which Play refuses
#   PPVPN_TUNNEL_ANDROID android: the tunnel host already built, a directory
#                        of <abi>/libppvpn_tunnel.so as crates/tunnel-host/
#                        scripts/build-android.sh leaves it (default: built
#                        here, into build/tunnel-android)
#   IOS_SDK              ios: simulator (a debug build for Apple silicon's
#                        simulator, ci.yml) or device (release; the default)
#   IOS_TEAM_ID, IOS_APP_PROFILE, IOS_TUNNEL_PROFILE
#                        ios device: signed for the App Store by this team
#                        with these provisioning profiles (the app's, the
#                        packet tunnel's; installed, their certificate in a
#                        keychain: tools/desktop/ci/ios-signing.sh), and
#                        exported as an IPA, the bundle; without them the
#                        app is unsigned (#470)
#   PPVPN_TUNNEL_XCFRAMEWORK, PPVPN_TUNNEL_LIBS
#                        ios: a tunnel host built already and its linker
#                        flags, linked into the packet tunnel extension
#                        (scripts/ios-tunnel-target.rb). Without them it is
#                        built here for the SDK's slice (crates/tunnel-host
#                        scripts/build-xcframework.sh); a tree without
#                        crates/tunnel-host gets an extension that cannot
#                        start
#   MACOS_ARCHS          macOS: the library's slices, "arm64 x86_64" joined
#                        into one, or one of them (release.yml builds each
#                        architecture's DMG in a job of its own, #459)
#                        (default: the runner's architecture only)
#   PPVPN_SKIP_ANALYZE   1: no flutter analyze (ci.yml analyzes on Linux)
#   and what rust/src/update.rs and bridge.rs bake into the library:
#   PPVPN_UPDATE_SITE, PPVPN_RELEASE_CHANNEL, PPVPN_SPARKLE_PUBLIC_KEY,
#   PPVPN_API_BASE_DEFAULT (required: the backend, an https origin; ci.yml
#   and release.yml pass the GitHub variable PPVPN_API_BASE, a dev release
#   the secret of that name, #517)
set -euo pipefail

if [ -z "${PPVPN_API_BASE_DEFAULT:-}" ]; then
  echo "ci-build: PPVPN_API_BASE_DEFAULT (the backend) is empty: see docs/release.md (后端地址)" >&2
  exit 1
fi
export PPVPN_API_BASE_DEFAULT

app=$(cd "$(dirname "$0")/.." && pwd)
root=$(cd "$app/../.." && pwd)
cd "$app"

case "${FLUTTER_TARGET:-}-$(uname -s)" in
  android-Linux) os=android ;;
  ios-Darwin) os=ios ;;
  -Linux) os=linux ;;
  -Darwin) os=macos ;;
  -MINGW* | -MSYS*) os=windows ;;
  *) echo "unsupported: ${FLUTTER_TARGET:-} on $(uname -s)" >&2; exit 1 ;;
esac

version=${PPVPN_VERSION:-$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml")}
build=${PPVPN_BUILD_NUMBER:-0}
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "version must be x.y.z, got '$version'" >&2; exit 1; }
[[ $build =~ ^[0-9]+$ ]] && [ "$build" -le 65535 ] ||
  { echo "build number must be 0-65535, got '$build'" >&2; exit 1; }
# A build for a channel updates from that channel's feed (as the native
# builds require it): without the site its updates would silently be off.
if [ -n "${PPVPN_RELEASE_CHANNEL:-}" ] && [ -z "${PPVPN_UPDATE_SITE:-}" ]; then
  echo "PPVPN_UPDATE_SITE is required for a channel build" >&2
  exit 1
fi
if [ -n "${PPVPN_UPDATE_SITE:-}" ] && [[ ! $PPVPN_UPDATE_SITE =~ ^https://[^/[:space:]]+(/[^[:space:]]*)?$ ]]; then
  echo "PPVPN_UPDATE_SITE must be an https URL" >&2
  exit 1
fi

# Replaces the template's $2 (a basic regex) with $3 in $1. A template
# without it fails the build rather than keeping the generated value.
replace() {
  grep -q -- "$2" "$1" || { echo "$1: no '$2' to replace" >&2; exit 1; }
  sed -i.orig "s|$2|$3|" "$1"
  rm "$1.orig"
}

# The runner for this platform, then without the counter sample's test.
flutter create --platforms="$os" --org com.peakpassvpn --project-name ppvpn_flutter . >/dev/null
rm -rf test
# The native app's icon replaces the template's (other file names).
if [ "$os" = macos ]; then
  rm -rf macos/Runner/Assets.xcassets/AppIcon.appiconset
fi
# iOS: our launch screen's image (runner/ios, the brand mark as a vector)
# and app icon (scripts/app-icons.swift) replace the template's PNGs; on
# Android the adaptive icon (runner/android, mipmap-anydpi-v26) the
# template's launcher PNGs (#535).
if [ "$os" = ios ]; then
  rm -rf ios/Runner/Assets.xcassets/LaunchImage.imageset ios/Runner/Assets.xcassets/AppIcon.appiconset
fi
if [ "$os" = android ]; then
  rm -f android/app/src/main/res/mipmap-*/ic_launcher.png
fi
# Our changes to the generated runner (macOS: the activation channel; the
# icons, from apps/macos and apps/windows; Android: the activity in our
# package).
if [ -d "runner/$os" ]; then
  cp -R "runner/$os/." "$os/"
fi
flutter pub get
cc=linux/runner/my_application.cc
[ -f "$cc" ] || cc=linux/my_application.cc

# The native apps' identity (#401 step 5): what the installers, updates,
# the service, login items and the push agent know the app by.
case $os in
  macos)
    # PPVPN.app, as apps/macos/project.yml (target PPVPN).
    f=macos/Runner/Configs/AppInfo.xcconfig
    replace "$f" '^PRODUCT_NAME = .*' 'PRODUCT_NAME = PPVPN'
    replace "$f" '^PRODUCT_BUNDLE_IDENTIFIER = .*' 'PRODUCT_BUNDLE_IDENTIFIER = com.peakpassvpn.ppvpn.desktop'
    replace "$f" '^PRODUCT_COPYRIGHT = .*' 'PRODUCT_COPYRIGHT = © 2026 PeakPass Labs LLC'
    # LSMinimumSystemVersion is the deployment target: macOS 13, as native.
    replace macos/Runner.xcodeproj/project.pbxproj \
      'MACOSX_DEPLOYMENT_TARGET = [0-9.]*;' 'MACOSX_DEPLOYMENT_TARGET = 13.0;'
    # The application menu's "About", "Hide" and "Quit" items.
    xib=macos/Runner/Base.lproj/MainMenu.xib
    grep -qE 'APP_NAME|ppvpn_flutter' "$xib" || { echo "$xib: no app name to replace" >&2; exit 1; }
    sed -i.orig -e '/title=/s/APP_NAME/PPVPN/g' -e '/title=/s/ppvpn_flutter/PPVPN/g' "$xib"
    rm "$xib.orig"
    plist=macos/Runner/Info.plist
    for key in CFBundleDisplayName LSApplicationCategoryType LSUIElement CFBundleLocalizations; do
      /usr/libexec/PlistBuddy -c "Delete :$key" "$plist" 2>/dev/null || true
    done
    /usr/libexec/PlistBuddy \
      -c 'Add :CFBundleDisplayName string PPVPN' \
      -c 'Add :LSApplicationCategoryType string public.app-category.utilities' \
      -c 'Add :LSUIElement bool false' \
      -c 'Add :CFBundleLocalizations array' \
      -c 'Add :CFBundleLocalizations:0 string zh-Hans' \
      -c 'Add :CFBundleLocalizations:1 string en' \
      -c 'Set :CFBundleVersion $(FLUTTER_BUILD_NAME).$(FLUTTER_BUILD_NUMBER)' \
      "$plist"
    # The prototype writes the system proxy and its own files: no App Sandbox
    # (the native app has none either).
    for f in macos/Runner/DebugProfile.entitlements macos/Runner/Release.entitlements; do
      /usr/libexec/PlistBuddy -c 'Set :com.apple.security.app-sandbox false' "$f"
    done
    # The ppvpn:// scheme: the push agent opens ppvpn://push/<id> for a click
    # on a notification.
    /usr/libexec/PlistBuddy -c 'Delete :CFBundleURLTypes' "$plist" 2>/dev/null || true
    /usr/libexec/PlistBuddy \
      -c 'Add :CFBundleURLTypes array' \
      -c 'Add :CFBundleURLTypes:0 dict' \
      -c 'Add :CFBundleURLTypes:0:CFBundleURLName string com.peakpassvpn.ppvpn.desktop' \
      -c 'Add :CFBundleURLTypes:0:CFBundleURLSchemes array' \
      -c 'Add :CFBundleURLTypes:0:CFBundleURLSchemes:0 string ppvpn' \
      "$plist" ;;
  android)
    # The Play package, as the user decided it (#448; it cannot change
    # after the first upload), with our activity in it (runner/android)
    # instead of the template's; Android 8 and later.
    gradle=android/app/build.gradle.kts
    replace "$gradle" 'namespace = "[^"]*"' 'namespace = "com.peakpassvpn.ppvpn"'
    replace "$gradle" 'applicationId = "[^"]*"' 'applicationId = "com.peakpassvpn.ppvpn"'
    replace "$gradle" 'minSdk = flutter.minSdkVersion' 'minSdk = 26'
    rm -r android/app/src/main/kotlin/com/peakpassvpn/ppvpn_flutter
    manifest=android/app/src/main/AndroidManifest.xml
    replace "$manifest" 'android:label="[^"]*"' 'android:label="PPVPN"'
    # The template asks for the network only in its debug and profile
    # manifests: the release app talks to the API too.
    replace "$manifest" '^    <application' '    <uses-permission android:name="android.permission.INTERNET"/>\n    <application'
    # TunnelService follows the best non-VPN network: reading it, and on
    # Android 8 to 11 requestNetwork, which needs the change permission.
    # Both are normal permissions, granted at install.
    replace "$manifest" '^    <application' '    <uses-permission android:name="android.permission.ACCESS_NETWORK_STATE"/>\n    <uses-permission android:name="android.permission.CHANGE_NETWORK_STATE"/>\n    <application'
    # The system VPN (runner/android TunnelService), in its own process:
    # only the system binds it.
    replace "$manifest" '^    </application>' '        <service android:name=".TunnelService" android:process=":tunnel"\n            android:exported="false" android:permission="android.permission.BIND_VPN_SERVICE">\n            <intent-filter>\n                <action android:name="android.net.VpnService"/>\n            </intent-filter>\n        </service>\n    </application>'
    # The release build's signature: the upload key when one is given (its
    # path and secrets read by Gradle from the environment, never written
    # into the project), else the template's debug key.
    if [ -n "${ANDROID_KEYSTORE_FILE:-}" ]; then
      for name in ANDROID_KEYSTORE_PASSWORD ANDROID_KEY_ALIAS ANDROID_KEY_PASSWORD; do
        [ -n "${!name:-}" ] || { echo "$name is required with ANDROID_KEYSTORE_FILE" >&2; exit 1; }
      done
      [ -f "$ANDROID_KEYSTORE_FILE" ] || { echo "no keystore at $ANDROID_KEYSTORE_FILE" >&2; exit 1; }
      replace "$gradle" '^    buildTypes {' '    signingConfigs {\n        create("upload") {\n            storeFile = file(System.getenv("ANDROID_KEYSTORE_FILE"))\n            storePassword = System.getenv("ANDROID_KEYSTORE_PASSWORD")\n            keyAlias = System.getenv("ANDROID_KEY_ALIAS")\n            keyPassword = System.getenv("ANDROID_KEY_PASSWORD")\n        }\n    }\n    buildTypes {'
      replace "$gradle" 'signingConfig = signingConfigs.getByName("debug")' 'signingConfig = signingConfigs.getByName("upload")'
    fi ;;
  ios)
    # The App Store id, as the user decided it (#448), and iOS 15 and later.
    # The app's channels (runner/ios PPVPNPlugin.swift) go into the
    # generated AppDelegate.swift, which the Runner target compiles, and
    # register next to the generated plugins.
    python3 - <<'PY'
import re
path = "ios/Runner.xcodeproj/project.pbxproj"
text = open(path).read()
generated = "PRODUCT_BUNDLE_IDENTIFIER = com.peakpassvpn.ppvpnFlutter;"
if text.count(generated) != 3:
    raise SystemExit(f"{path}: not the three Runner configurations' bundle id")
# Without the debug dylib: the app's code in the Runner executable itself,
# which flutter test's log predicate needs (PPVPNPlugin's VMServiceAnnouncer).
text = re.sub(r"\n\t*ENABLE_DEBUG_DYLIB = \w+;", "", text)
text = text.replace(generated, "PRODUCT_BUNDLE_IDENTIFIER = com.peakpassvpn.ppvpn;\n\t\t\t\tENABLE_DEBUG_DYLIB = NO;")
text, targets = re.subn(r"IPHONEOS_DEPLOYMENT_TARGET = [0-9.]+;", "IPHONEOS_DEPLOYMENT_TARGET = 15.0;", text)
if not targets:
    raise SystemExit(f"{path}: no deployment target")
open(path, "w").write(text)

delegate = "ios/Runner/AppDelegate.swift"
code = open(delegate).read()
plugin = open("runner/ios/Runner/PPVPNPlugin.swift").read()
imports = [line for line in plugin.splitlines() if line.startswith("import ")]
body = "\n".join(line for line in plugin.splitlines() if not line.startswith("import "))
code, registered = re.subn(
    r"^([ \t]*)GeneratedPluginRegistrant\.register\(with: ([^)]+)\)\n",
    lambda m: m.group(0) + f'{m.group(1)}PPVPNPlugin.register(with: {m.group(2)}.registrar(forPlugin: "PPVPNPlugin")!)\n',
    code, count=1, flags=re.M)
if not registered:
    raise SystemExit(f"{delegate}: no GeneratedPluginRegistrant.register to follow")
missing = [line for line in imports if line not in code.splitlines()]
open(delegate, "w").write("\n".join(missing) + "\n" + code + "\n" + body + "\n")
PY
    rm ios/Runner/PPVPNPlugin.swift
    plist=ios/Runner/Info.plist
    # The export compliance answer (#470): only the system's and standard
    # encryption, so no question per TestFlight build.
    /usr/libexec/PlistBuddy -c 'Set :CFBundleDisplayName PPVPN' -c 'Set :CFBundleName PPVPN' \
      -c 'Add :ITSAppUsesNonExemptEncryption bool false' "$plist"
    # #535: the iPhone in portrait only, the iPad in all four (multitasking
    # needs them); the system's own text (the in-app browser's controls,
    # permission prompts) in Chinese or English, as the app's.
    /usr/libexec/PlistBuddy \
      -c 'Delete :UISupportedInterfaceOrientations' \
      -c 'Add :UISupportedInterfaceOrientations array' \
      -c 'Add :UISupportedInterfaceOrientations:0 string UIInterfaceOrientationPortrait' \
      -c 'Delete :UISupportedInterfaceOrientations~ipad' \
      -c 'Add :UISupportedInterfaceOrientations~ipad array' \
      -c 'Add :UISupportedInterfaceOrientations~ipad:0 string UIInterfaceOrientationPortrait' \
      -c 'Add :UISupportedInterfaceOrientations~ipad:1 string UIInterfaceOrientationPortraitUpsideDown' \
      -c 'Add :UISupportedInterfaceOrientations~ipad:2 string UIInterfaceOrientationLandscapeLeft' \
      -c 'Add :UISupportedInterfaceOrientations~ipad:3 string UIInterfaceOrientationLandscapeRight' \
      -c 'Add :CFBundleLocalizations array' \
      -c 'Add :CFBundleLocalizations:0 string zh-Hans' \
      -c 'Add :CFBundleLocalizations:1 string en' "$plist" ;;
  windows)
    # ppvpn.exe and its version resource, as apps/windows
    # (PPVPN.Windows.csproj, Directory.Build.props).
    replace windows/CMakeLists.txt '^set(BINARY_NAME ".*")' 'set(BINARY_NAME "ppvpn")'
    replace windows/runner/main.cpp 'window.Create(L"[^"]*"' 'window.Create(L"PPVPN"'
    rc=windows/runner/Runner.rc
    # UTF-8 for the copyright sign.
    if ! grep -qF '#pragma code_page(65001)' "$rc"; then
      { printf '#pragma code_page(65001)\n'; cat "$rc"; } >"$rc.new"
      mv "$rc.new" "$rc"
    fi
    for kv in 'CompanyName=PeakPass Labs LLC' 'FileDescription=PPVPN' 'InternalName=ppvpn' \
        'LegalCopyright=© PeakPass Labs LLC' 'OriginalFilename=ppvpn.exe' 'ProductName=PPVPN'; do
      replace "$rc" "VALUE \"${kv%%=*}\", \"[^\"]*\"" "VALUE \"${kv%%=*}\", \"${kv#*=}\""
    done ;;
  linux)
    # /usr/lib/ppvpn/ppvpn and the GApplication id, as apps/linux
    # (PPVPNApplication.Id): the desktop entry and the window's class.
    replace linux/CMakeLists.txt '^set(BINARY_NAME ".*")' 'set(BINARY_NAME "ppvpn")'
    replace linux/CMakeLists.txt '^set(APPLICATION_ID ".*")' 'set(APPLICATION_ID "com.peakpassvpn.ppvpn.desktop")'
    replace "$cc" 'gtk_header_bar_set_title(header_bar, "[^"]*");' 'gtk_header_bar_set_title(header_bar, "PPVPN");'
    replace "$cc" 'gtk_window_set_title(window, "[^"]*");' 'gtk_window_set_title(window, "PPVPN");'
    # The program name, so the X11 WM_CLASS, is the application id: the
    # desktop entry's StartupWMClass (packaging/linux).
    grep -qF 'g_set_prgname(APPLICATION_ID);' "$cc" ||
      { echo "$cc: the program name is no longer the application id" >&2; exit 1; } ;;
esac

# The Windows and Linux runners show the window on Flutter's first frame;
# here it stays hidden until main.dart shows it, so that a `--background`
# start (launch at login) keeps to the tray. A template without the line
# fails the build rather than showing the window after all.
case $os in
  windows) replace windows/runner/flutter_window.cpp 'this->Show();' '' ;;
  linux)
    if grep -qF 'first_frame_cb' "$cc"; then
      replace "$cc" 'gtk_widget_show(gtk_widget_get_toplevel(GTK_WIDGET(view)));' ''
    else
      replace "$cc" 'gtk_widget_show(GTK_WIDGET(window));' 'gtk_widget_realize(GTK_WIDGET(window));'
    fi ;;
esac

flutter_rust_bridge_codegen generate
grep -q '^mod frb_generated;' rust/src/lib.rs || echo 'mod frb_generated;' >> rust/src/lib.rs
# Analysis once per pull request, in ci.yml's Linux flutter job: the
# others set PPVPN_SKIP_ANALYZE=1 (#459).
if [ "${PPVPN_SKIP_ANALYZE:-}" != 1 ]; then
  flutter test checks/probe_batch_test.dart
  flutter analyze --no-fatal-infos
fi

lib="$root/target/release"
if [ "$os" = android ]; then
  # The bridge for the APK's two ABIs (arm64 phones, x86_64 emulators),
  # linked by the NDK's clang for API 26, in jniLibs.
  ndk=${ANDROID_NDK_HOME:?ANDROID_NDK_HOME must name the Android NDK}
  bin=$ndk/toolchains/llvm/prebuilt/linux-x86_64/bin
  for pair in arm64-v8a:aarch64-linux-android x86_64:x86_64-linux-android; do
    abi=${pair%%:*}
    triple=${pair#*:}
    upper=$(echo "$triple" | tr 'a-z-' 'A-Z_')
    (cd "$root" && env "CARGO_TARGET_${upper}_LINKER=$bin/${triple}26-clang" \
      "CC_${triple//-/_}=$bin/${triple}26-clang" "AR_${triple//-/_}=$bin/llvm-ar" \
      "BINDGEN_EXTRA_CLANG_ARGS_${triple//-/_}=--target=${triple}26" \
      cargo build --release --locked -p ppvpn-flutter --target "$triple")
    so=$root/target/$triple/release/libppvpn_flutter.so
    mkdir -p "android/app/src/main/jniLibs/$abi"
    cp "$so" "android/app/src/main/jniLibs/$abi/"
    # A C++ dependency linked against the shared libc++ needs it beside.
    if "$bin/llvm-readelf" -d "$so" | grep -qF 'libc++_shared.so'; then
      cp "$ndk/toolchains/llvm/prebuilt/linux-x86_64/sysroot/usr/lib/$triple/libc++_shared.so" \
        "android/app/src/main/jniLibs/$abi/"
    fi
  done
  # The tunnel host for TunnelService (runner/android, the :tunnel
  # process), beside the bridge: libppvpn_tunnel.so (and libc++_shared.so
  # if it needs it, the same file as the bridge's).
  tunnel=${PPVPN_TUNNEL_ANDROID:-}
  if [ -z "$tunnel" ]; then
    tunnel=$app/build/tunnel-android
    "$root/crates/tunnel-host/scripts/build-android.sh" "$tunnel" arm64-v8a x86_64
  fi
  for abi in arm64-v8a x86_64; do
    test -f "$tunnel/$abi/libppvpn_tunnel.so" || { echo "no $tunnel/$abi/libppvpn_tunnel.so" >&2; exit 1; }
    cp "$tunnel/$abi/"*.so "android/app/src/main/jniLibs/$abi/"
  done
elif [ "$os" = ios ]; then
  # iOS takes no loose dynamic library: the bridge is a static library the
  # Runner links whole (-force_load), its symbols exported for the Dart
  # side's lookup (ExternalLibrary.process), with the system libraries it
  # needs as rustc prints them (the lib is touched so that it prints).
  case ${IOS_SDK:-device} in
    simulator) triple=aarch64-apple-ios-sim ;;
    device) triple=aarch64-apple-ios ;;
    *) echo "IOS_SDK must be simulator or device, got '$IOS_SDK'" >&2; exit 1 ;;
  esac
  touch rust/src/lib.rs
  mkdir -p build
  if ! (cd "$root" && IPHONEOS_DEPLOYMENT_TARGET=15.0 cargo rustc --color never --release --locked -p ppvpn-flutter --lib \
      --target "$triple" --crate-type staticlib -- --print native-static-libs) 2>build/ios-rustc.log; then
    cat build/ios-rustc.log >&2
    exit 1
  fi
  native=$(sed -n 's/.*native-static-libs: //p' build/ios-rustc.log | tail -1)
  [ -n "$native" ] || { cat build/ios-rustc.log >&2; echo "rustc printed no native-static-libs" >&2; exit 1; }
  echo "native-static-libs: $native"
  mkdir -p ios/rust
  cp "$root/target/$triple/release/libppvpn_flutter.a" ios/rust/
  NATIVE="$native" python3 - <<'PY'
import json, os
path = "ios/Runner.xcodeproj/project.pbxproj"
text = open(path).read()
flags = ["$(inherited)", "-force_load", "$(PROJECT_DIR)/rust/libppvpn_flutter.a", "-Wl,-export_dynamic"]
flags += os.environ["NATIVE"].split()
setting = "OTHER_LDFLAGS = (" + "".join(json.dumps(flag) + ", " for flag in flags) + ");"
# The Dart side finds the bridge's functions by name (dlsym): an archive
# strips every symbol by default ("all"), the export trie included, and
# the TestFlight build of 0.7.2 stayed white. An archive strips locals
# only.
setting += "\n\t\t\t\tSTRIP_STYLE = \"non-global\";"
bundle = "PRODUCT_BUNDLE_IDENTIFIER = com.peakpassvpn.ppvpn;"
if text.count(bundle) != 3:
    raise SystemExit(f"{path}: not the three Runner configurations")
open(path, "w").write(text.replace(bundle, bundle + "\n\t\t\t\t" + setting))
PY
  if [ -n "${IOS_TEAM_ID:-}" ]; then
    : "${IOS_APP_PROFILE:?IOS_APP_PROFILE is required with IOS_TEAM_ID}"
    [ "${IOS_SDK:-device}" = device ] || { echo "IOS_TEAM_ID signs device builds only" >&2; exit 1; }
    [ "$build" -gt 0 ] || { echo "an App Store build needs PPVPN_BUILD_NUMBER" >&2; exit 1; }
    # Manual signing of the Runner target (its three configurations), with
    # the entitlements from runner/ios: the system VPN and the App Group.
    python3 - <<'PY'
import json, os
path = "ios/Runner.xcodeproj/project.pbxproj"
text = open(path).read()
settings = {
    "CODE_SIGN_STYLE": "Manual",
    "DEVELOPMENT_TEAM": os.environ["IOS_TEAM_ID"],
    "CODE_SIGN_IDENTITY": "Apple Distribution",
    '"CODE_SIGN_IDENTITY[sdk=iphoneos*]"': "Apple Distribution",
    "PROVISIONING_PROFILE_SPECIFIER": os.environ["IOS_APP_PROFILE"],
    "CODE_SIGN_ENTITLEMENTS": "Runner/Runner.entitlements",
}
lines = "".join(f"\n\t\t\t\t{key} = {json.dumps(value)};" for key, value in settings.items())
bundle = "PRODUCT_BUNDLE_IDENTIFIER = com.peakpassvpn.ppvpn;"
if text.count(bundle) != 3:
    raise SystemExit(f"{path}: not the three Runner configurations")
open(path, "w").write(text.replace(bundle, bundle + lines))
PY
    IOS_EXPORT_OPTIONS="$app/build/ExportOptions.plist"
    rm -f "$IOS_EXPORT_OPTIONS"
    plist() { /usr/libexec/PlistBuddy -c "$1" "$IOS_EXPORT_OPTIONS" >/dev/null; }
    plist 'Add :method string app-store-connect'
    plist 'Add :destination string export'
    plist "Add :teamID string $IOS_TEAM_ID"
    plist 'Add :signingStyle string manual'
    plist 'Add :signingCertificate string Apple Distribution'
    plist 'Add :provisioningProfiles dict'
    plist "Add :provisioningProfiles:com.peakpassvpn.ppvpn string $IOS_APP_PROFILE"
    : "${IOS_TUNNEL_PROFILE:?IOS_TUNNEL_PROFILE is required with IOS_TEAM_ID}"
    plist "Add :provisioningProfiles:com.peakpassvpn.ppvpn.tunnel string $IOS_TUNNEL_PROFILE"
    # CFBundleVersion is the release's build number, as built.
    plist 'Add :manageAppVersionAndBuildNumber bool false'
    plist 'Add :uploadSymbols bool true'
  fi
  # The tunnel host for the packet tunnel extension only (the engine runs
  # in the tunnel's process, docs/mobile.md): its static library and the
  # system libraries rustc says it needs.
  tunnel_script="$root/crates/tunnel-host/scripts/build-xcframework.sh"
  if [ -z "${PPVPN_TUNNEL_XCFRAMEWORK:-}" ] && [ -x "$tunnel_script" ]; then
    slice=ios
    [ "${IOS_SDK:-device}" = simulator ] && slice=simulator
    IPHONEOS_DEPLOYMENT_TARGET=15.0 "$tunnel_script" "$app/build/tunnel" "$slice"
    export PPVPN_TUNNEL_XCFRAMEWORK="$app/build/tunnel/PPVPNTunnel.xcframework"
    PPVPN_TUNNEL_LIBS=$(cat "$app/build/tunnel/PPVPNTunnel.ldflags")
    export PPVPN_TUNNEL_LIBS
  fi
  # The packet tunnel extension, last: Xcodeproj rewrites the whole project
  # file, which the text edits above rely on the generated form of.
  # xcodeproj is CocoaPods' gem, on the runners; else this version.
  ruby -e 'require "xcodeproj"' 2>/dev/null || gem install --user-install --no-document xcodeproj -v 1.27.0
  ruby scripts/ios-tunnel-target.rb ios/Runner.xcodeproj "$version" "$build"
elif [ "$os" = macos ] && [ -n "${MACOS_ARCHS:-}" ]; then
  # Flutter's release app is universal; package-app.sh thins each DMG's copy.
  slices=()
  for arch in $MACOS_ARCHS; do
    case $arch in
      arm64) triple=aarch64-apple-darwin ;;
      x86_64) triple=x86_64-apple-darwin ;;
      *) echo "unsupported arch in MACOS_ARCHS: $arch" >&2; exit 1 ;;
    esac
    (cd "$root" && cargo build --release --locked -p ppvpn-flutter --target "$triple")
    slices+=("$root/target/$triple/release/libppvpn_flutter.dylib")
  done
  lib="$root/target/universal-apple-darwin/release"
  mkdir -p "$lib"
  if [ ${#slices[@]} = 1 ]; then
    cp "${slices[0]}" "$lib/libppvpn_flutter.dylib"
  else
    lipo -create "${slices[@]}" -output "$lib/libppvpn_flutter.dylib"
  fi
else
  # Linux and Windows ship the push agent: built in the same invocation, its
  # features unify with the bridge's (tokio's signal, the windows crate's),
  # so the dependencies they share compile once, and package-linux.sh's and
  # package-windows.ps1's build, the same command, finds them built.
  agent=()
  case $os in linux | windows) agent=(-p ppvpn-push-agent) ;; esac
  (cd "$root" && cargo build --release --locked -p ppvpn-flutter ${agent[@]+"${agent[@]}"})
fi
if [ "$os" = android ]; then
  # versionCode must be positive.
  case ${ANDROID_FORMAT:-apk} in
    apk) flutter build apk --release --target-platform android-arm64,android-x64 \
      --build-name "$version" --build-number "$((build > 0 ? build : 1))" ;;
    aab) flutter build appbundle --release --target-platform android-arm64,android-x64 \
      --build-name "$version" --build-number "$((build > 0 ? build : 1))" ;;
    *) echo "ANDROID_FORMAT must be apk or aab, got '$ANDROID_FORMAT'" >&2; exit 1 ;;
  esac
elif [ "$os" = ios ]; then
  # The simulator runs debug builds only.
  if [ "${IOS_SDK:-device}" = simulator ]; then
    flutter build ios --simulator --debug --build-name "$version" --build-number "$build"
  elif [ -n "${IOS_EXPORT_OPTIONS:-}" ]; then
    flutter build ipa --release --export-options-plist="$IOS_EXPORT_OPTIONS" \
      --build-name "$version" --build-number "$build"
  else
    flutter build ios --release --no-codesign --build-name "$version" --build-number "$build"
  fi
else
  flutter build "$os" --release --build-name "$version" --build-number "$build"
fi

case $os in
  android)
    # An App Bundle keeps the APK's files under base/ (lib/, and dex/ for
    # the classes).
    if [ "${ANDROID_FORMAT:-apk}" = aab ]; then
      bundle=build/app/outputs/bundle/release/app-release.aab
      inner=base/ dexes='base/dex/classes*.dex'
    else
      bundle=build/app/outputs/flutter-apk/app-release.apk
      inner='' dexes='classes*.dex'
    fi
    test -f "$bundle"
    # Both libraries for both ABIs, in the package.
    listing=$(unzip -l "$bundle")
    for abi in arm64-v8a x86_64; do
      for so in libppvpn_flutter.so libppvpn_tunnel.so; do
        grep -qF "${inner}lib/$abi/$so" <<<"$listing" || { echo "$bundle lacks ${inner}lib/$abi/$so" >&2; exit 1; }
      done
    done
    # Signed with the upload key when one was given, not the debug key
    # (Play refuses a debug-signed upload).
    if [ -n "${ANDROID_KEYSTORE_FILE:-}" ]; then
      signer=$(keytool -printcert -jarfile "$bundle")
      grep -q 'Owner:' <<<"$signer" || { echo "$bundle is not signed" >&2; exit 1; }
      ! grep -q 'CN=Android Debug' <<<"$signer" || { echo "$bundle is signed with the debug key" >&2; exit 1; }
    fi
    # What native code calls back by name, kept through R8
    # (runner/android/app/proguard-rules.pro).
    sdk=${ANDROID_HOME:-${ANDROID_SDK_ROOT:?the Android SDK is needed}}
    dexdump=$(find "$sdk/build-tools" -name dexdump -type f | sort -V | tail -1)
    [ -n "$dexdump" ] || { echo "no dexdump in $sdk/build-tools" >&2; exit 1; }
    dex=$app/build/apk-dex
    rm -rf "$dex"
    mkdir -p "$dex"
    unzip -q -o -j "$bundle" "$dexes" -d "$dex"
    "$dexdump" "$dex"/classes*.dex >"$dex/dump.txt"
    for kept in tunnel/TunnelHost\$Listener.protect tunnel/TunnelHost\$Listener.onExit \
        Credentials.seal Credentials.open; do
      awk -v class="Lcom/peakpassvpn/ppvpn/${kept%.*};" -v want="${kept##*.}" '
        /Class descriptor/ { inside = index($0, "'"'"'" class "'"'"'") > 0 }
        inside && /name +:/ && index($0, "'"'"'" want "'"'"'") { found = 1 }
        END { exit !found }' "$dex/dump.txt" ||
        { echo "$bundle: R8 renamed or dropped ${kept//\//.}" >&2; exit 1; }
    done ;;
  ios)
    if [ "${IOS_SDK:-device}" = simulator ]; then
      bundle=build/ios/iphonesimulator/Runner.app
      test -x "$bundle/Runner"
    elif [ -n "${IOS_EXPORT_OPTIONS:-}" ]; then
      bundle=$(ls build/ios/ipa/*.ipa)
      test -f "$bundle"
      # The Runner that ships, after the export's own processing.
      runner=build/ios/Runner.shipped
      unzip -p "$bundle" Payload/Runner.app/Runner >"$runner"
    else
      bundle=build/ios/iphoneos/Runner.app
      test -x "$bundle/Runner"
      runner=$bundle/Runner
    fi
    # The bridge's functions in the export trie, where ExternalLibrary.process
    # (dlsym) looks, by the names flutter_rust_bridge's Dart side uses:
    # without them the app starts to a white screen. Only an IPA (a stable
    # run, before the upload) shows the archive's stripping; the simulator's
    # debug build strips nothing, so its smoke test cannot tell.
    if [ -n "${runner:-}" ]; then
      test -s "$runner"
      exports=$(xcrun dyld_info -exports "$runner")
      for name in frb_pde_ffi_dispatcher_primary frb_pde_ffi_dispatcher_sync frb_get_rust_content_hash \
          frb_dart_fn_deliver_output frb_init_frb_dart_api_dl frb_create_shutdown_callback; do
        grep -qw "_$name" <<<"$exports" || { echo "$runner does not export the bridge's $name" >&2; exit 1; }
      done
      echo "$runner: $(grep -c '_frb_' <<<"$exports") bridge functions exported"
    fi ;;
  linux)
    bundle=build/linux/x64/release/bundle
    test -x "$bundle/ppvpn"
    cp "$lib/libppvpn_flutter.so" "$bundle/lib/" ;;
  windows)
    bundle=build/windows/x64/runner/Release
    test -f "$bundle/ppvpn.exe"
    cp "$lib/ppvpn_flutter.dll" "$bundle/" ;;
  macos)
    bundle=build/macos/Build/Products/Release/PPVPN.app
    test -x "$bundle/Contents/MacOS/PPVPN"
    # What the edits above should have given the bundle.
    for kv in CFBundleIdentifier=com.peakpassvpn.ppvpn.desktop LSMinimumSystemVersion=13.0 \
        CFBundleShortVersionString="$version" CFBundleVersion="$version.$build"; do
      got=$(/usr/libexec/PlistBuddy -c "Print :${kv%%=*}" "$bundle/Contents/Info.plist")
      [ "$got" = "${kv#*=}" ] || { echo "${kv%%=*} is '$got', not '${kv#*=}'" >&2; exit 1; }
    done
    cp "$lib/libppvpn_flutter.dylib" "$bundle/Contents/Frameworks/"
    codesign --force --deep --sign - "$bundle" ;;
esac
echo "bundle=$app/$bundle" >> "${GITHUB_OUTPUT:-/dev/stdout}"
