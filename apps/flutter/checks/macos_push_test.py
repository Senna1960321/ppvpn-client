"""APNs provisioning and native cold-launch contracts, without Apple credentials."""
import datetime as dt
import hashlib
import importlib.util
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch

FLUTTER = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("macos_push_profile", FLUTTER / "scripts/macos-push-profile.py")
profile = importlib.util.module_from_spec(spec)
spec.loader.exec_module(profile)
TEAM = "TESTTEAM01"
CERT = b"test certificate DER"
SHA1 = hashlib.sha1(CERT).hexdigest()


def valid_profile():
    return {
        "Platform": ["OSX"], "ProvisionsAllDevices": True,
        "ExpirationDate": dt.datetime(2099, 1, 1),
        "TeamIdentifier": [TEAM], "ApplicationIdentifierPrefix": ["OLDPREFIX1"],
        "DeveloperCertificates": [CERT],
        "Entitlements": {
            profile.APP_ID: "OLDPREFIX1." + profile.BUNDLE_ID,
            profile.TEAM_ID: TEAM, profile.APS: "production",
        },
    }


class ProvisioningTest(unittest.TestCase):
    def test_explicit_production_profile_preserves_app_id_prefix(self):
        granted = profile.validate(valid_profile(), TEAM, SHA1.upper())
        self.assertEqual(granted, valid_profile()["Entitlements"])

    def test_packaging_inputs_trigger_the_ci_packaging_check(self):
        # Exercise the classifier with one changed file, without GitHub access.
        with tempfile.TemporaryDirectory() as tmp:
            gh = Path(tmp) / "gh"
            gh.write_text('#!/bin/sh\nprintf "%s\\n" "$CHANGED_PATH"\n')
            gh.chmod(0o755)
            env = dict(os.environ, PATH=tmp + os.pathsep + os.environ["PATH"],
                       GITHUB_EVENT_NAME="pull_request", PR_BASE_SHA="base",
                       GITHUB_SHA="head", GITHUB_REPOSITORY="test/repo")
            for changed, expected in [
                ("apps/flutter/packaging/macos/SparkleArtifacts/Package.swift", "true"),
                ("apps/flutter/packaging/macos/SparkleArtifacts/Package.resolved", "true"),
                ("apps/flutter/packaging/macos/PPVPN.entitlements", "true"),
                ("apps/flutter/packaging/linux/nfpm.yaml", "false"),
                ("docs/release.md", "false"),
            ]:
                with self.subTest(changed=changed):
                    result = subprocess.run(
                        ["bash", str(FLUTTER.parents[1] / "tools/ci-changes.sh")],
                        env=dict(env, CHANGED_PATH=changed), check=True,
                        capture_output=True, text=True,
                    )
                    flags = dict(line.split("=", 1) for line in result.stdout.splitlines())
                    self.assertEqual(flags["macpkg"], expected)

    def test_rejects_expired_missing_or_invalid_expiry(self):
        for value in [dt.datetime(2000, 1, 1), None, "2099-01-01"]:
            with self.subTest(value=value):
                data = valid_profile()
                data["ExpirationDate"] = value
                with self.assertRaisesRegex(ValueError, "expiry|expired"):
                    profile.validate(data, TEAM, SHA1)

    def test_rejects_other_distribution_methods(self):
        for key, value in [("Platform", ["iOS"]), ("ProvisionsAllDevices", False)]:
            data = valid_profile()
            data[key] = value
            with self.assertRaisesRegex(ValueError, "Developer ID"):
                profile.validate(data, TEAM, SHA1)

    def test_rejects_wrong_team_or_certificate(self):
        with self.assertRaisesRegex(ValueError, "another team"):
            profile.validate(valid_profile(), "OTHERTEAM1", SHA1)
        with self.assertRaisesRegex(ValueError, "certificate"):
            profile.validate(valid_profile(), TEAM, "00" * 20)

    def test_rejects_wrong_bundle_wildcard_sandbox_and_debugging(self):
        for key, value in [
            (profile.APP_ID, "OLDPREFIX1.*"),
            (profile.APP_ID, "OLDPREFIX1.com.peakpassvpn.ppvpn"),
            (profile.TEAM_ID, "OTHERTEAM1"),
            (profile.APS, "development"),
            ("get-task-allow", True),
            ("com.apple.security.get-task-allow", True),
        ]:
            with self.subTest(key=key, value=value):
                data = valid_profile()
                data["Entitlements"][key] = value
                with self.assertRaises(ValueError):
                    profile.validate(data, TEAM, SHA1)

    def fixture(self, directory):
        app = Path(directory) / "PPVPN.app"
        (app / "Contents").mkdir(parents=True)
        (app / "Contents/Info.plist").write_bytes(plistlib.dumps({"CFBundleIdentifier": profile.BUNDLE_ID}))
        source = Path(directory) / "input.plist"
        source.write_bytes(plistlib.dumps({"com.apple.security.cs.disable-library-validation": True}))
        return app, Path(directory) / "output.plist", source

    def test_ad_hoc_build_removes_restricted_profile_and_disables_registration(self):
        with tempfile.TemporaryDirectory() as tmp:
            app, output, source = self.fixture(tmp)
            embedded = app / "Contents/embedded.provisionprofile"
            embedded.write_bytes(b"stale profile")
            source.write_bytes(plistlib.dumps(valid_profile()["Entitlements"]))
            profile.prepare(app, output, source, "", "", "-")
            self.assertFalse(embedded.exists())
            self.assertEqual(plistlib.loads(output.read_bytes()), {})
            self.assertIs(plistlib.loads((app / "Contents/Info.plist").read_bytes())["PPVPNAPNsEnabled"], False)

    def test_signed_build_cannot_silently_omit_the_profile(self):
        with tempfile.TemporaryDirectory() as tmp:
            app, output, source = self.fixture(tmp)
            for missing in ["", str(Path(tmp) / "missing")]:
                with self.assertRaisesRegex(ValueError, "require PPVPN_MACOS_APNS_PROFILE"):
                    profile.prepare(app, output, source, TEAM, missing, "Developer ID Application: Test")
            self.assertFalse(output.exists())

    def test_profile_cannot_be_used_with_ad_hoc_signing(self):
        with tempfile.TemporaryDirectory() as tmp:
            app, output, source = self.fixture(tmp)
            with self.assertRaisesRegex(ValueError, "without a Developer ID"):
                profile.prepare(app, output, source, "", "some-profile", "-")

    def test_prepare_embeds_validated_profile_and_grants_only_required_entitlements(self):
        with tempfile.TemporaryDirectory() as tmp:
            app, output, source = self.fixture(tmp)
            cms = Path(tmp) / "profile.provisionprofile"
            cms.write_bytes(b"CMS input")
            decoded = valid_profile()
            decoded["Entitlements"]["unrelated-capability"] = True
            def run(args, **kwargs):
                if args[:3] == ["security", "cms", "-D"]:
                    return subprocess.CompletedProcess(args, 0, plistlib.dumps(decoded))
                if args[:2] == ["security", "find-certificate"]:
                    return subprocess.CompletedProcess(args, 0, b"PEM certificate")
                self.assertEqual(args, ["openssl", "x509", "-outform", "DER"])
                self.assertEqual(kwargs["input"], b"PEM certificate")
                return subprocess.CompletedProcess(args, 0, CERT)
            with patch.object(profile.subprocess, "run", side_effect=run):
                profile.prepare(app, output, source, TEAM, cms, "Developer ID Application: Test")
            granted = plistlib.loads(output.read_bytes())
            self.assertEqual(granted[profile.APS], "production")
            self.assertNotIn("unrelated-capability", granted)
            self.assertEqual((app / "Contents/embedded.provisionprofile").read_bytes(), b"CMS input")
            self.assertIs(plistlib.loads((app / "Contents/Info.plist").read_bytes())["PPVPNAPNsEnabled"], True)

    def test_cms_failure_does_not_modify_the_app(self):
        with tempfile.TemporaryDirectory() as tmp:
            app, output, source = self.fixture(tmp)
            cms = Path(tmp) / "bad.profile"
            cms.write_bytes(b"invalid")
            before = (app / "Contents/Info.plist").read_bytes()
            with patch.object(profile.subprocess, "run", side_effect=subprocess.CalledProcessError(1, "security")):
                with self.assertRaises(subprocess.CalledProcessError):
                    profile.prepare(app, output, source, TEAM, cms, "identity")
            self.assertFalse(output.exists())
            self.assertFalse((app / "Contents/embedded.provisionprofile").exists())
            self.assertEqual((app / "Contents/Info.plist").read_bytes(), before)


class MacPushTest(unittest.TestCase):
    @unittest.skipUnless(shutil.which("swiftc"), "Swift compiler required")
    def test_actual_mailbox_cold_launch_rotation_payload_filtering_and_repeated_taps(self):
        with tempfile.TemporaryDirectory() as tmp:
            main = Path(tmp) / "main.swift"
            main.write_text('''import Foundation
let inbox = MacPushInbox()
var events: [(String, [String: String])] = []
inbox.receiveToken("first")
inbox.receiveToken("latest")
inbox.receiveTap(["extra": ["message_id": 42, "deep_link": "file:///ignored"], "body": "ignored"])
inbox.start { events.append(($0, $1)) }
precondition(events.count == 2)
precondition(events[0].0 == "token" && events[0].1 == ["token": "latest"])
precondition(events[1].0 == "opened" && events[1].1 == ["message_id": "42"])
inbox.receiveToken("rotated")
precondition(events.last!.1 == ["token": "rotated"])
for _ in 0..<2 { inbox.receiveTap(["message_id": "43"]) }
precondition(events.suffix(2).allSatisfy { $0.0 == "opened" && $0.1 == ["message_id": "43"] })
inbox.receiveTap(["body": "untrusted", "deep_link": "https://ignored.test"])
precondition(events.last!.0 == "opened" && events.last!.1.isEmpty)
events.removeAll()
inbox.start { events.append(($0, $1)) }
precondition(events.count == 1 && events[0].1 == ["token": "rotated"])
print("macOS native mailbox: cold launch, rotation, taps and payload filtering passed")
''')
            binary = Path(tmp) / "check"
            subprocess.run(["swiftc", str(FLUTTER / "runner/macos/PushInbox.swift"), str(main), "-o", str(binary)], check=True)
            subprocess.run([str(binary)], check=True)

    def test_launch_installs_receiver_and_does_not_call_missing_superclass_selector(self):
        source = (FLUTTER / "runner/macos/Runner/AppDelegate.swift").read_text()
        self.assertLess(source.index("UNUserNotificationCenter.current().delegate"), source.index("super.applicationWillFinishLaunching"))
        # Flutter declares the optional AppKit method but does not implement
        # it. Swift typechecking alone did not catch this launch crash.
        self.assertNotIn("super.applicationDidFinishLaunching", source)
        script = (FLUTTER / "scripts/ci-build.sh").read_text()
        self.assertIn("cat runner/macos/PushInbox.swift >> macos/Runner/AppDelegate.swift", script)


if __name__ == "__main__":
    unittest.main()
