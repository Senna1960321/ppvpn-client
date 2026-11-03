# PPVPN for macOS: packaging and legacy helpers

The macOS app is the Flutter one (`apps/flutter`, #401); the native SwiftUI app
is gone (#522). What stays here:

- `PPVPNAgent`, the retired Swift push agent, and its `LaunchAgents/`
  plist. The executable is no longer built or bundled by the packager;
  only the plist remains in the app for unregistering an installed job.
  The Flutter runner now receives APNs directly (#539).
- `AppLogic`, the legacy SwiftPM package `PPVPNAppLogic`: its log and app
  directory helpers still have Linux CI tests and contain no AppKit.
- `project.yml` (XcodeGen), retained for the legacy agent. Shipping
  packages resolve Sparkle independently through
  `apps/flutter/packaging/macos/SparkleArtifacts/Package.resolved`.
- `scripts/package-app.sh` and `packaging/postinstall`: thinning, signing,
  the DMG, the .pkg and the release metadata, run by
  `apps/flutter/scripts/package-macos.sh`.

Paths and commands here are relative to the repository root.

## Build and test

```bash
# Legacy agent only: not needed to package the Flutter app
# PPVPNClient Swift package + PPVPN.xcodeproj
apps/macos/scripts/bootstrap.sh            # --release for an optimised crate
xcodebuild -project apps/macos/PPVPN.xcodeproj -scheme PPVPNAgent -configuration Debug build

# AppLogic's tests: macOS, after bootstrap.sh
swift test --package-path apps/macos/AppLogic
# Linux (CI pins Swift 6.4.0): builds the crate and a Linux PPVPNClient package
apps/macos/scripts/build-swift-linux.sh
swift test --package-path apps/macos/AppLogic
```

`PPVPN.xcodeproj` and `PPVPNClient/` are generated and git-ignored; re-run
`bootstrap.sh` after `project.yml` or the ppvpn-client interface changes
when working on that legacy code. Current app builds and APNs tests are
documented in [apps/flutter](../flutter/README.md) and
[testing](../../docs/testing.md#macos-apns-与打包检查).

## Release DMG and package

`apps/flutter/scripts/package-macos.sh` puts Sparkle, the service helpers
and the legacy job's migration plist into the Flutter app. It removes
`PPVPN Agent.app` and `PPVPNClientFFI.framework`, then `package-app.sh`
thins every Mach-O to one architecture, re-signs inside out, and checks
each binary's architecture and signature. It writes
the DMG, the package and `release-meta-macos-<arm64|x64>.json` (schema 1:
platform, channel, version, build, file, length, sha256, ed_signature, min_os,
published_at, all of the DMG, plus install_file, install_length,
install_sha256 and install_ed_signature of the package), from which the
update site's feed is written.
The package (`pkgbuild`) installs only to /Applications, is not relocatable,
and runs `packaging/postinstall`, which hands the bundle to the console user
so that Sparkle updates it without an administrator prompt
(docs/security.md). `SPARKLE_PRIVATE_KEY_FILE` fills `ed_signature` and
`install_ed_signature`: the privileged service checks the package with the
same key before it installs it silently (#548). CI
(`tools/desktop/ci/build-macos-native.sh`) packages each requested
architecture on Apple silicon; release.yml gives each its own artifact.

## Signing and installing

Releases are signed with our Apple Developer ID (Team ID in
`crates/service/src/macos_team_id`, #471) and notarized. CI
(`tools/desktop/ci/build-macos-native.sh`) imports the Developer ID
Application and Installer identities into a temporary keychain from the
`MACOS_SIGNING_P12_*` and `MACOS_INSTALLER_P12_*` secrets, refuses
identities of any other kind or team, and passes the App Store Connect API
key (`ASC_KEY_ID`, `ASC_ISSUER_ID`, `ASC_KEY_P8`) to notarytool.
Developer ID builds also require `MACOS_APNS_PROFILE_BASE64`: the Developer
ID profile for `com.peakpassvpn.ppvpn.desktop`, authorizing production APNs
for that team and signing certificate. The Flutter packager validates and
embeds it before signing; setup is in
[release prerequisites](../../docs/release.md#macos-apns-签名与配置).
`package-app.sh` then:

- signs every piece of code inside out with the hardened runtime and a
  secure timestamp, keeping codesign's own designated requirements
  (identifier, Apple's anchor, the Team ID), which carry across builds and
  certificate renewals (keychain access and notification permission);
- takes `com.apple.security.cs.disable-library-validation` out of the
  entitlements: every embedded library carries the same Team ID;
- notarizes the app and staples its ticket, builds the DMG (signed) and the
  pkg (`productsign`, Developer ID Installer) from it, notarizes and staples
  both, and checks Gatekeeper's verdict on all three;
- only then signs the DMG for Sparkle and writes the release metadata.

A stable build without the identities, APNs profile or API key fails.
Any Developer ID build without its matching APNs profile fails. Unsigned
local and merge builds use ad-hoc signing; CI's packaging
check signs with a throwaway self-signed identity, where each designated
requirement pins that certificate and the entitlements keep
disable-library-validation (no Team ID). These builds omit the embedded
profile and restricted APNs entitlements and do not register with APNs.

The privileged service accepts only a client with our Developer ID signature
(`crates/service/src/macsign.rs`). A service installed by a release before
the Developer ID (0.7.x and older pin the old self-signed certificate)
refuses the new app, which then reinstalls it once, behind the usual
administrator prompt.

Installing: open the downloaded .pkg; Installer puts the app in
/Applications. The installed app has no quarantine attribute, opens without
asking and is not translocated. From the DMG instead, drag the app to
Applications in Finder. Updates installed by Sparkle keep the same
signature.

## Logs

- App: `~/Library/Logs/PPVPN/`; there is no new macOS push-agent process.
- Privileged service and Enhanced Mode engine: `/Library/Logs/PPVPN/` (`ppvpn-service.log`,
  `ppvpn-core.log`, launchd's `ppvpn-service.out.log` / `.err.log`; readable without root;
  5 MB per file, 3 files kept). Uninstalling or reinstalling the service leaves them in place;
  builds up to 18 wrote them inside the helper bundle, where the uninstall deleted them.
