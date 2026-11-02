# Flutter app

The desktop and mobile Flutter app, backed by the Rust client through
flutter_rust_bridge. Desktop releases use this app. Android builds have
separate Google Play and website APK distributions; the mobile tunnel host
and current device-validation scope are described in
[Mobile integration](../../docs/mobile.md).

## Layout

- `rust/`: the bridge crate `ppvpn-flutter` (a workspace member). `api.rs` is
  everything Dart can call; `bridge.rs` holds the one `Client`, its listener
  and the conversions; `hooks.rs` holds the platform hooks; `push_agent.rs`
  starts the push agent.
- `push-agent/`: `ppvpn-push-agent` (a workspace member), which shows
  backend pushes as notifications while the app is closed, on Windows and
  Linux (macOS keeps the Swift agent). It replaces the .NET agents and
  reads the app's data directory (`rust/src/dirs.rs`, compiled into both).
- `lib/theme.dart`: every design token in one place: the unified design
  signed off on #387 (2026-10-20). It covers colours for light and dark,
  shadows, type, sizes and radii, with pixel values from the design sources.
  Latin text uses Inter (bundled by forui), numbers and addresses JetBrains
  Mono (`assets/fonts`, OFL), and Chinese the system font.
- `lib/widgets.dart`: the shared pieces drawn to the design: buttons, the
  three-state switch, the latency pill, flags, the brand mark. Each is
  keyboard-operable and announced to screen readers.
- `lib/strings.dart`: the handoff's `strings.json` (zh and en) for the keys
  the prototype shows.
- `lib/shell.dart`: the window frame, following the navigation decided on
  #387. It has a left sidebar (200 wide, 34-high items: overview, nodes,
  logs) and the account card and settings at the bottom. Messages open from
  a bell with the unread badge at the top right of the content area. There
  is no system title bar. On macOS the traffic lights sit at the top of the
  sidebar; on Windows and Linux the caption buttons are at the top right of
  the content area, right of the bell.
- `lib/pages/`: sign-in, overview, nodes, logs, settings, and the message
  center (opened from the bell; `lib/messages_state.dart` holds its pages).
  `lib/tray.dart`: the tray icon and its menu. `lib/settings.dart`: the
  app's own settings.
- `lib/activation.dart` with `rust/src/instance.rs`: one instance, and
  clicks on the push agent's notifications (#401 step 4), handled as the
  native apps do. A second launch hands its arguments to the running
  instance and exits; the running one shows its window. On Linux the lock
  and socket are `instance.lock` and `instance.sock` in the data
  directory. On Windows it is a named pipe, plus the toast's COM activator
  (`--toast-activated`, registered by the push agent). On macOS the push
  agent opens `ppvpn://push/<id>`. A click opens the message's link in the
  browser, or the message center on that message.
- `runner/<os>/`: our files over the generated runner (`flutter create`).
  On macOS that is `MainFlutterWindow.swift`, for the `ppvpn://` links and
  reopens; `scripts/ci-build.sh` also registers the scheme in its
  Info.plist. The app icons are the native apps' (`AppIcon.appiconset`
  from `apps/macos`, `app_icon.ico` from `apps/windows`).
- The native apps' identity (#401 step 5), set by `scripts/ci-build.sh` in
  the generated runner:
  - macOS: `PPVPN.app`, bundle id `com.peakpassvpn.ppvpn.desktop`, macOS
    13 or later, with the native Info.plist's display name, category,
    copyright (PeakPass Labs LLC) and localizations;
  - Windows: `ppvpn.exe`, window title `PPVPN`, and the version resource's
    company (PeakPass Labs LLC), product and copyright;
  - Linux: `ppvpn`, application id `com.peakpassvpn.ppvpn.desktop` (the
    desktop entry's name), window title `PPVPN`.

  The version is `PPVPN_VERSION` (default: the workspace's, `Cargo.toml`)
  and the build number `PPVPN_BUILD_NUMBER` (default 0), the names
  release.yml gives the installer jobs. macOS's `CFBundleVersion` is
  `<version>.<build>`, as the native app's. The Dart package stays
  `ppvpn_flutter`, and so does the bridge library (`libppvpn_flutter`).
- `assets/tray/`: the shipped tray icons, copied from the native apps: the
  macOS template images (`menubar-*`, from `apps/macos` `MenuBar-*`) and
  the Windows and Linux colour icons (`tray-*`, from `apps/assets/icons`)
  for not connected, in progress, connected and error.

## What it does and does not do

- Signs in with the device code, picks a node, and connects with the
  **system proxy** (compatible mode, on the in-process standard core) or in
  **enhanced mode** (TUN through the privileged service, #401). Enhanced
  mode installs the service on first use, after an explanation, behind the
  system's admin prompt, exactly as the native apps do (same paths and
  helpers: `rust/src/hooks.rs`). The helpers (`ppvpn-service-install`,
  `-uninstall`, and on macOS `ppvpn-service`) must sit next to the app's
  executable; `scripts/ci-build.sh` does not put them there. On Linux,
  `scripts/install-linux-layout.sh` does, on macOS
  `scripts/package-macos.sh` (see below).
- Quitting from the tray puts the system proxy back (`Client::shutdown`).
  Closing the window only hides it.
- The app takes over the native apps' data in place (#401 step 5): the
  same data and log directories (`rust/src/dirs.rs`), so the client's own
  files there (node, connection and routing mode, push agent state) carry
  over both ways, and an upgrade from the native app, or a rollback to it,
  needs no copy:
  - macOS: `~/Library/Application Support/PPVPN` (0700), logs in
    `~/Library/Logs/PPVPN`;
  - Linux: `$XDG_DATA_HOME/ppvpn`, logs in `$XDG_STATE_HOME/ppvpn/logs`;
  - Windows: `%LOCALAPPDATA%\PPVPN`, logs in its `logs` folder.
- The sign-in is the native apps' own too, the same bytes in the same
  store (`rust/src/credentials.rs`):
  - macOS: the 0600 file `credentials` in the data directory, excluded
    from Time Machine (the native app's release name; its Debug builds
    use `credentials.debug`);
  - Windows: the Credential Manager's generic credential
    `PPVPN/desktop.credentials` (user `PPVPN`);
  - Linux: the Secret Service item the native app stores through
    libsecret (schema `com.peakpassvpn.ppvpn.desktop`, attributes
    `service`, `account` = `credentials`, `xdg:schema`), reached over
    D-Bus; the 0600 file `credentials` in the data directory when the
    session has no Secret Service.
- The settings page (#401 step 3) sets the appearance, the language and
  the routing mode, and shows the account, the version and the system
  service's status. Update controls are available when the build has an
  updater or notice feed (see [Updates](#updates)). Website Android builds
  expose the same version/update section from the sign-in page and Me.
- Launch at login (#401 step 4) is the native apps' own, off by default,
  its state read from the system: `SMAppService.mainApp` on macOS, in the
  runner's Swift (`LoginItem` in `MainFlutterWindow.swift`, channel
  `ppvpn/login-item`; with the approval hint and System Settings › Login
  Items); in `rust/src/autostart.rs` the `PPVPN` value under HKCU's
  `Run` key on Windows (with the hint when Settings › Apps › Startup
  turned it off; the value is pointed at the current executable at every
  start), and `$XDG_CONFIG_HOME/autostart/com.peakpassvpn.ppvpn.desktop`
  on Linux. The
  Windows and Linux entries pass `--background`: the app starts in the
  tray with no window. For that, `scripts/ci-build.sh` takes the
  first-frame show out of those two runners, and `main.dart` shows the
  window otherwise.
- Appearance and the API override are the native apps' settings
  (`rust/src/settings.rs`): on macOS the user defaults of
  `com.peakpassvpn.ppvpn.desktop` (`appearance`, `apiBaseOverride`), on
  Windows `app-settings.json` in the data directory and on Linux
  `$XDG_CONFIG_HOME/ppvpn/settings.json` (`Appearance`,
  `ApiBaseOverride`; the native apps' other keys are kept). The native
  apps have no language setting: this app keeps its own next to those
  (`language` or `Language`), which they ignore. The API override
  (Settings → Advanced) takes effect on the next launch; `PPVPN_API_BASE`
  wins over it. Only a debug build of the Rust library shows and honours
  the override (#519); a release build, what `scripts/ci-build.sh` and
  release.yml make, keeps a saved one but ignores it. Without either the
  backend is the one compiled in from `PPVPN_API_BASE_DEFAULT` (required by
  `scripts/ci-build.sh`; CI and releases pass it, docs/release.md 后端地址,
  #517). No backend address is written in the code.
- Quit the native app before trying it: both use the same data, sign-in
  and system proxy setting, and both have the same identity (bundle id,
  executable names).
- Flags are 4:3 images from flag-icons (MIT, `assets/flags/LICENSE`), only
  for the countries we expect; any other country gets its code on a tile.
  There are no emoji flags: platforms draw them differently, and Windows
  draws them as letters.
- Not yet (to-dos on #387 and #401): the node search field and context
  menu, and the message rows' context menu.

## Building

CI builds the desktop app in the `flutter` job and mobile apps in `android`
and `ios`. `tools/ci-changes.sh` selects the affected jobs, including the
Android job when its updater test harness changes.
`scripts/ci-build.sh` does the following:

1. generates the platform runner (`flutter create`), copies `runner/<os>/`
   over it, gives it the native apps' identity (above), and keeps the window
   hidden until `main.dart` shows it on Windows and Linux;
2. generates the bridge glue (`flutter_rust_bridge_codegen generate`);
3. analyses the Dart code and runs `flutter test checks/` (both are skipped
   when `PPVPN_SKIP_ANALYZE=1`);
4. builds the Rust library and the app in release mode, with the version
   and build number;
5. copies the library into the bundle.

`scripts/smoke.sh` then starts the app and checks that it is still running
after 20 s. It takes a screenshot and notes the resident memory and the
bundle's sizes. The job summary shows those numbers, and the artifact
`flutter-<os>` holds the screenshot. Nothing generated is committed (see
`.gitignore`).

`e2e/run.sh` runs the signed-in flow end to end on each platform (#401
step 1). It covers device sign-in, choosing a node, connecting in standard
mode, a request through the node, disconnecting and signing out. The
setup is:

- the real client and core, against `e2e/mock_backend.py` (Python standard
  library, on loopback) and `test/fakenode` as the node;
- `integration_test/signed_in_test.dart` drives the UI;
- `PPVPN_FLUTTER_HOME` keeps the app's data apart (its sign-in and
  settings too, as plain files in that home: never the keyring or the
  native apps' settings), and
  `PPVPN_TEST_TRUST_LOCAL_BACKEND=1` lets the client accept the loopback
  backend.

Pinned versions:

| Component | Version | Pinned in |
| --- | --- | --- |
| Flutter | 3.47.7 | `scripts/install-flutter.sh` |
| flutter_rust_bridge | 2.13.0 | crate, Dart package and codegen |
| forui | 0.27.6 | `pubspec.yaml` |
| tray_manager | 0.8.0 | `pubspec.yaml` |
| nativeapi | 0.5.2 | `pubspec.yaml` |
| window_manager | 0.5.2 | `pubspec.yaml` |

## Android distributions

On Linux, set `FLUTTER_TARGET=android`, the Android SDK/NDK paths and
`PPVPN_API_BASE_DEFAULT`, then run `scripts/ci-build.sh` with the Rust and
Flutter toolchains used by CI. The script generates the runner and bridge,
then selects both the native flavor and Dart's `PPVPN_ANDROID_CHANNEL`:

| Build inputs | Distribution | Output under `apps/flutter/` |
| --- | --- | --- |
| `ANDROID_FORMAT=apk` (default) | `apk` by default | `build/app/outputs/flutter-apk/app-apk-release.apk` |
| `ANDROID_FORMAT=aab` | `play` by default | `build/app/outputs/bundle/playRelease/app-play-release.aab` |
| `ANDROID_FORMAT=apk ANDROID_DISTRIBUTION=play` | Play behavior in an APK for testing | `build/app/outputs/flutter-apk/app-play-release.apk` |

`ANDROID_DISTRIBUTION` can explicitly be `play` or `apk`; AAB builds require
`play`. Both use application ID `com.peakpassvpn.ppvpn`, minimum API 26 and
arm64-v8a/x86_64. `PPVPN_BUILD_NUMBER` is the versionCode, with local zero
mapped to 1. `scripts/android-distribution.py` generates the flavors and
embeds `<PPVPN_UPDATE_SITE>/android/<PPVPN_RELEASE_CHANNEL>/latest.json`
only in `apk`; without both values its updater is unavailable.

Play's source set has no APK downloader, installer provider or install
permission. Its UI hides external purchase and About links; message and
notification links pass the same restricted origin/path policy in
`lib/distribution.dart`. Missing or unknown Dart distribution metadata uses
the Play policy. `apk` retains website behavior and the native updater.

`ANDROID_KEYSTORE_FILE`, `ANDROID_KEYSTORE_PASSWORD`, `ANDROID_KEY_ALIAS`
and `ANDROID_KEY_PASSWORD` select the signing key; without a key the local
build uses the template's debug key. Play AABs use the upload key. Published
website APKs require the app-signing identity and pinned certificate
described in [Android release configuration](../../docs/release.md#android-分发与签名).
`scripts/check-android-signature.sh` checks AAB JAR signatures with `keytool`
and APK signatures with `apksigner`, including APKs without a v1 signature.

The native and workflow tests have a separate
[runner and prerequisites](../../tools/desktop/ci/android-update-tests/README.md).
Flutter's `checks/` tests cover Play link policy and the phone update UI;
the emulator VPN smoke test selects `--flavor apk` and the matching Dart
distribution explicitly.

## macOS packaging

`scripts/package-macos.sh <PPVPN.app> arm64|x86_64` packages the app that
`scripts/ci-build.sh` built as the native release packaged its app
(#401 step 5; #522 removed it), into the same artifacts in `dist/macos`. Into a
copy of the bundle, at the native app's paths, it puts:

- the Swift push agent (`apps/macos`, target `PPVPNAgent`, built with the
  app's version) at `Contents/Library/LoginItems/PPVPN Agent.app`;
- its launchd job at `Contents/Library/LaunchAgents`;
- the client framework the agent links at `Contents/Frameworks`;
- the service helpers in `Contents/MacOS`, where `rust/src/hooks.rs` looks
  for them;
- Sparkle (resolved by the agent's build, `apps/macos/project.yml`) at
  `Contents/Frameworks`, and the Info.plist keys the native build set: the appcast (`PPVPN_UPDATE_FEED_URL`,
  else the channel's feed from `PPVPN_UPDATE_SITE` and
  `PPVPN_RELEASE_CHANNEL`) and the EdDSA key (`PPVPN_SPARKLE_PUBLIC_KEY`).

Then `apps/macos/scripts/package-app.sh` does the rest. It thins the bundle
to the one architecture and signs it inside out with the hardened runtime
and `packaging/macos/PPVPN.entitlements`. It signs with the fixed certificate the service
pins when `PPVPN_CODESIGN_IDENTITY` is set, ad-hoc otherwise. It then writes
`PPVPN-<version>-<platform>.dmg` (with the Sparkle signature when
`SPARKLE_PRIVATE_KEY_FILE` is set), the `.pkg` (id
`com.peakpassvpn.ppvpn.desktop.pkg`, with the native postinstall, and the
same signature for the service's silent updates, #548) and
`release-meta-<platform>.json`.

An installed app keeps the agent registered as the native app does
(`runner/macos/Runner/MainFlutterWindow.swift`, `PushAgentService`): it
uses `SMAppService.agent` from `/Applications`, registers again once per
build, and checks the heartbeat every five minutes, with `launchctl
kickstart` and a rate-limited re-registration.

CI (job `flutter`, macOS) packages the arm64 slice ad-hoc with the client
and the helpers in debug. It checks the bundle, the signatures, the DMG,
the expanded `.pkg` and release-meta. release.yml's `macos` job builds the
app universal (`MACOS_ARCHS="arm64 x86_64"`) and packages both
architectures through `tools/desktop/ci/build-macos-native.sh`
(`PPVPN_FLUTTER_BUNDLE`), with the release's signing and Sparkle keys.

## Windows installer

`scripts/package-windows.ps1` (Windows, after `scripts/ci-build.sh`) builds
the installer the native Windows release ships, from this app: the same
NSIS script (`packaging/windows/ppvpn.nsi`), install directory
(`C:\Program Files\PPVPN`), service handling, shortcuts, uninstall entry
and `/PURGE`, and the same artifacts,
`dist/windows/PPVPN-<version>-windows-x64-setup.exe` (+ `.sha256`) and
`release-meta-windows-x64.json`. It stages the release bundle (`ppvpn.exe`,
`flutter_windows.dll`, `ppvpn_flutter.dll`, the plugins' DLLs, `data\`), the
Rust push agent, `ppvpn-service`, `-install` and `-uninstall`, `wintun.dll`,
`WinSparkle.dll` (the native app's WinSparkle version) and the
notifications' icon (`Assets\AppLogo.png`). `install-files.txt` lists what
it installs, so an upgrade from the native app removes the native app's
files, and the next upgrade this app's. Authenticode signing and the
update feed's EdDSA signature take the native build's environment
(`WINDOWS_SIGN_COMMAND`, `PPVPN_WINDOWS_PUBLISHER_EKU`, `SPARKLE_PRIVATE_KEY`);
the version is `PPVPN_VERSION` and `PPVPN_BUILD_NUMBER`, as for
`ci-build.sh`, and the bundle's `ppvpn.exe` must carry it.

What the installer expects of the app, as of the native one:

- the quit event `Local\PPVPN.Desktop.Quit`: the running instance creates
  it (`rust/src/instance.rs`); set, the app quits as the tray's Quit does.
  The installer and uninstaller set it before replacing or removing the
  files;
- `ppvpn.exe --set-shortcut-aumid <.lnk>` stamps the AppUserModelID
  `PeakPass.PPVPN` on the shortcut the installer created and exits
  (`rust/src/aumid.rs`). At every start the app also sets it as the
  process's AppUserModelID and writes its notification registration
  (display name, icon, `ppvpn.exe --toast-activated` as the click
  activator), as the push agent does; the uninstaller removes it.

In CI (`ci.yml`, job `flutter` on Windows) the installer is built
unsigned, then `scripts/smoke-installer.ps1` installs it over the native
app of the latest release, checks the installation, sets the quit event,
and uninstalls with `/PURGE`. The artifact `flutter-installer-windows`
holds the installer.

## Updates

From the native apps' feeds, as they update (`lib/updates.dart`):

- macOS: Sparkle, which the runner loads from `Contents/Frameworks` when
  `scripts/package-macos.sh` bundled it (`MainFlutterWindow.swift`,
  `Updater`). It unregisters the push agent before it replaces the app.
- Windows: WinSparkle (`rust/src/update.rs`) with the feed and key baked
  into the bridge when it is built (`PPVPN_UPDATE_FEED_URL`, else
  `PPVPN_UPDATE_SITE` and `PPVPN_RELEASE_CHANNEL`; `PPVPN_SPARKLE_PUBLIC_KEY`),
  its state under the native app's registry key and the automatic-check
  switch as `AutoCheckUpdates` in `app-settings.json`. Once its installer
  runs, the app hides and waits for the installer's quit event, then
  quits on its own after 30 s.
- Linux: a notice in the settings page with the upgrade command, when the
  repository's `latest.json` (`PPVPN_UPDATE_FEED`, else the channel's, else
  stable's on the update site) has a newer build than `PPVPN_VERSION` and
  `PPVPN_BUILD_NUMBER` baked into the bridge. A build without a build number
  or outside a package has no notice.
- Website Android APK: initialized before the phone UI starts, including
  while signed out. It checks after 15 seconds and every 24 hours, or from
  Check for Updates. Download and Install opens Android's install permission
  settings when needed; return and tap again after granting permission.
  The native plugin checks the HTTPS origin, bytes, package/version and
  installed signer before sharing a private-cache file with Android's
  installer. The user confirms installation; Play builds use store updates.

Builds without a feed have updates off and no
「检查更新…」.

## Linux layout and the enhanced-mode end-to-end test

`scripts/install-linux-layout.sh <bundle> <service-bin-dir>` (root, Linux
only) installs the bundle as the native packages do
(`packaging/linux/nfpm.yaml`). The executable becomes
`/usr/lib/ppvpn/ppvpn`, root-owned and 0755, because that is the only
client the privileged service accepts. `lib/` and `data/` sit beside it,
and so do `ppvpn-service`, `ppvpn-service-install` and
`ppvpn-service-uninstall`, which is where `rust/src/hooks.rs` looks for
them. To remove it, run
`/usr/lib/ppvpn/ppvpn-service-uninstall` and then `rm -rf /usr/lib/ppvpn`.

`scripts/package-linux.sh <bundle>` makes the deb and rpm in that layout
(`packaging/linux/nfpm.yaml`): the files the native release makes
(`PPVPN-<version>-linux-x64.{deb,rpm}` and their `release-meta-linux-x64-*.json`),
under the native package's name `ppvpn`, version and release (the build
number), with its maintainer scripts and push agent autostart entry
(`packaging/linux`), so it upgrades the native package in place. The
dependencies are GTK 3, X11 and XInput, EGL, xdg-utils and pkexec instead of
GTK 4, libadwaita and .NET's. It builds the service and the push agent
itself. CI builds both in the Linux `flutter` job and checks each in a fresh
container (`scripts/package-smoke.sh`, ubuntu:24.04 and fedora:latest): the
libraries resolve from the dependencies, the app starts, it is removed
cleanly, and the deb upgrades the native release in place.

`e2e/linux_enhanced.sh` is the enhanced-mode test (#401 step 2), the
counterpart of `crates/desktop/src/e2e_linux.rs`. CI runs it in the Linux
`flutter` job after the build as
`sudo test/netns/run.sh --script e2e/linux_enhanced.sh <fakenode>`, which
puts it inside run.sh's network namespace and checks that the host is left
as it was. Before that, the job installs the layout and the service with
`ppvpn-service-install`, then stops the unit, because a systemd unit runs in
the host's namespace. The script then:

1. starts `test/fakenode` in the uplink namespace,
   `/usr/lib/ppvpn-service/ppvpn-service` in the test namespace and
   `e2e/linux_enhanced_backend.py` on loopback;
2. writes a saved sign-in (`data/credentials`) to a temporary
   `PPVPN_FLUTTER_HOME`;
3. starts the installed release app under its own Xvfb and D-Bus session,
   with `PPVPN_FLUTTER_E2E=enhanced`;
4. checks the TUN, the policy rules (9091–9101) and table 2091 while
   connected;
5. checks that `GET http://192.0.2.10:8080/` answers through the node;
6. checks that the routing state is gone after a disconnect and after the
   app quits.

`PPVPN_FLUTTER_E2E=enhanced` (`lib/e2e.dart`) is a test-only switch and is
off unless the variable is set. Once the app is signed in with a profile,
it turns enhanced mode on. It prints `E2E ready`, `E2E phase <phase>` and
`E2E action-error <error>` to stdout. It reads `connect`, `disconnect` and
`quit` lines from stdin. On `quit` or SIGTERM it disconnects and then quits
as the tray's Quit does.
