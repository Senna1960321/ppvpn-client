# PPVPN for macOS: push agent and packaging

The macOS app is the Flutter one (`apps/flutter`, #401); the native SwiftUI app
is gone (#522). What stays here:

- `PPVPNAgent`, the Swift push agent: a UI-less helper that launchd keeps
  running at login (`LaunchAgents/`, registered by the app through
  `SMAppService.agent`). It long-polls the backend through the client crate's
  `PushAgent` (`crates/desktop`, UniFFI) and shows each push as a
  notification; a click opens `ppvpn://push/<id>` in the app.
- `AppLogic`, the SwiftPM package `PPVPNAppLogic` the agent uses beyond the
  bindings: its log and the app's directories. No AppKit, so it builds and
  tests on Linux too.
- `project.yml` (XcodeGen), whose `PPVPNAgent` target also resolves Sparkle,
  which the Flutter app's package takes from that build.
- `scripts/package-app.sh` and `packaging/postinstall`: thinning, signing,
  the ZIP, the .pkg and the release metadata, run by
  `apps/flutter/scripts/package-macos.sh`.

Paths and commands here are relative to the repository root.

## Build and test

```bash
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
`bootstrap.sh` after `project.yml` or the ppvpn-client interface changes.

## Release ZIP and package

`apps/flutter/scripts/package-macos.sh` puts the agent, the shared
`PPVPNClientFFI.framework`, Sparkle and the service helpers into the Flutter
app, then `package-app.sh` thins every Mach-O to the one architecture and
re-signs inside out, checks each binary's architecture, that the agent links
the single framework in `Contents/Frameworks`, and the signature. It writes
the ZIP, the package and `release-meta-macos-<arm64|x64>.json` (schema 1:
platform, channel, version, build, file, length, sha256, ed_signature, min_os,
published_at, all of the ZIP, plus install_file, install_length,
install_sha256 and install_ed_signature of the package), from which the
update site's feed is written.
The package (`pkgbuild`) installs only to /Applications, is not relocatable,
and runs `packaging/postinstall`, which hands the bundle to the console user
so that Sparkle updates it without an administrator prompt
(docs/security.md). `SPARKLE_PRIVATE_KEY_FILE` fills `ed_signature` and
`install_ed_signature`: the privileged service checks the package with the
same key before it installs it silently (#548). CI
(`tools/desktop/ci/build-macos-native.sh`) builds both architectures on an
Apple silicon runner into one artifact.

## Signing and installing

Releases are signed with our Apple Developer ID (Team ID in
`crates/service/src/macos_team_id`, #471) and notarized. CI
(`tools/desktop/ci/build-macos-native.sh`) imports the Developer ID
Application and Installer identities into a temporary keychain from the
`MACOS_SIGNING_P12_*` and `MACOS_INSTALLER_P12_*` secrets, refuses
identities of any other kind or team, and passes the App Store Connect API
key (`ASC_KEY_ID`, `ASC_ISSUER_ID`, `ASC_KEY_P8`) to notarytool.
`package-app.sh` then:

- signs every piece of code inside out with the hardened runtime and a
  secure timestamp, keeping codesign's own designated requirements
  (identifier, Apple's anchor, the Team ID), which carry across builds and
  certificate renewals (keychain access, the push agent's SMAppService
  registration, the notification permission);
- takes `com.apple.security.cs.disable-library-validation` out of the
  entitlements: every embedded library carries the same Team ID;
- notarizes the app and staples its ticket, then archives it with
  `ditto -c -k --keepParent`; extracts the ZIP and verifies the app's signature
  and stapled ticket. The ZIP needs no separate notarization;
- builds the pkg (`productsign`, Developer ID Installer), notarizes and
  staples it, and checks Gatekeeper's verdict on the app and package;
- signs the final ZIP and pkg for Sparkle and writes their exact lengths,
  hashes and signatures to the release metadata.

A stable build without the identities or the API key fails. Without them
(local builds, pull requests) signing falls back to ad-hoc; CI's packaging
check signs with a throwaway self-signed identity, where each designated
requirement pins that certificate and the entitlements keep
disable-library-validation (no Team ID).

The privileged service accepts only a client with our Developer ID signature
(`crates/service/src/macsign.rs`). A service installed by a release before
the Developer ID (0.7.x and older pin the old self-signed certificate)
refuses the new app, which then reinstalls it once, behind the usual
administrator prompt.

Installing: open the downloaded .pkg; Installer puts the app in
/Applications. The installed app has no quarantine attribute, opens without
asking and is not translocated. The ZIP is the Sparkle update archive;
first installs continue to use the pkg. Previously published DMGs are kept
unchanged. Updates installed by Sparkle keep the same signature.

## Logs

- App and push agent: `~/Library/Logs/PPVPN/`
- Privileged service and Enhanced Mode engine: `/Library/Logs/PPVPN/` (`ppvpn-service.log`,
  `ppvpn-core.log`, launchd's `ppvpn-service.out.log` / `.err.log`; readable without root;
  5 MB per file, 3 files kept). Uninstalling or reinstalling the service leaves them in place;
  builds up to 18 wrote them inside the helper bundle, where the uninstall deleted them.
