"""Execute the Android build/smoke shell blocks with only Flutter replaced."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[3]


class AndroidDistribution(unittest.TestCase):
    def test_build_and_smoke_select_matching_native_and_dart_distributions(self):
        build = (ROOT / "apps/flutter/scripts/ci-build.sh").read_text()
        start = build.index('if [ "$os" = android ]; then\n  # versionCode')
        end = build.index('elif [ "$os" = ios ]; then', start)
        compile_block = build[start:end] + "fi\n"
        start = build.index('case $os in\n  android)\n    # An App Bundle')
        end = build.index('    # Both libraries for both ABIs', start)
        artifact_block = build[start:end] + '    ;;\nesac\nprintf "%s\\n" "$bundle"\n'
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            template = 'android {\n    buildTypes {\n    }\n}\n'
            gradle = root / "build.gradle.kts"
            generator = ROOT / "apps/flutter/scripts/android-distribution.py"
            bad_sites = ["http://updates.example.test", "https://user:secret@updates.example.test",
                         "https://updates.example.test?query=1", "https://updates.example.test#fragment",
                         "https:///"]
            configurations = [(site, "stable", template) for site in bad_sites]
            configurations += [("https://updates.example.test", "other", template),
                               ("https://updates.example.test", "stable", "android {}\n"),
                               ("https://updates.example.test", "stable", template + template)]
            for site, channel, source in configurations:
                with self.subTest(site=site, channel=channel, template=source):
                    gradle.write_text(source)
                    result = subprocess.run([sys.executable, str(generator), str(gradle)],
                                            env={**os.environ, "PPVPN_UPDATE_SITE": site,
                                                 "PPVPN_RELEASE_CHANNEL": channel},
                                            text=True, capture_output=True)
                    self.assertNotEqual(result.returncode, 0, result.stdout)
                    self.assertEqual(gradle.read_text(), source, "Refused configuration changed the runner")
            bins = root / "bin"
            bins.mkdir()
            flutter = bins / "flutter"
            flutter.write_text('#!/usr/bin/env python3\nimport json, os, pathlib, sys\n'
                               'pathlib.Path(os.environ["FLUTTER_CALL"]).write_text(json.dumps(sys.argv[1:]))\n')
            flutter.chmod(0o755)
            log = root / "flutter-call.json"
            env = {**os.environ, "PATH": f"{bins}:{os.environ['PATH']}", "FLUTTER_CALL": str(log)}
            for key in ("ANDROID_FORMAT", "ANDROID_DISTRIBUTION"):
                env.pop(key, None)
            cases = [({}, "apk", "apk"), ({"ANDROID_FORMAT": "aab"}, "appbundle", "play"),
                     ({"ANDROID_DISTRIBUTION": "play"}, "apk", "play"),
                     ({"ANDROID_DISTRIBUTION": "apk"}, "apk", "apk"),
                     ({"ANDROID_FORMAT": "aab", "ANDROID_DISTRIBUTION": "apk"}, None, None),
                     ({"ANDROID_FORMAT": "other"}, None, None),
                     ({"ANDROID_DISTRIBUTION": "other"}, None, None)]
            for options, target, flavor in cases:
                with self.subTest(options=options):
                    log.unlink(missing_ok=True)
                    expected = (f"build/app/outputs/bundle/{flavor}Release/app-{flavor}-release.aab"
                                if target == "appbundle" else
                                f"build/app/outputs/flutter-apk/app-{flavor}-release.apk")
                    artifact = root / expected
                    artifact.parent.mkdir(parents=True, exist_ok=True)
                    artifact.touch()
                    result = subprocess.run(["bash", "-c", 'set -eu\nos=android version=0.4.0 build=0\n' +
                                             compile_block + artifact_block], cwd=root, env={**env, **options},
                                            text=True, capture_output=True)
                    artifact.unlink()
                    if target is None:
                        self.assertNotEqual(result.returncode, 0, result.stdout)
                        self.assertFalse(log.exists(), "Invalid distribution reached Flutter")
                    else:
                        self.assertEqual(result.returncode, 0, result.stderr)
                        self.assertEqual(result.stdout.strip(), expected)
                        args = json.loads(log.read_text())
                        self.assertEqual(args[:2], ["build", target])
                        self.assertEqual(args[args.index("--flavor") + 1], flavor)
                        self.assertIn(f"--dart-define=PPVPN_ANDROID_CHANNEL={flavor}", args)
                        self.assertEqual(args[args.index("--build-number") + 1], "1")
            smoke = (ROOT / "apps/flutter/e2e/android_smoke.sh").read_text()
            start = smoke.index("leg() {")
            end = smoke.index('\necho "--- tablet', start)
            result = subprocess.run(["bash", "-c", 'set -eu\ndevice=fixture\n' + smoke[start:end] +
                                     "\nleg tablet\n"], cwd=root, env=env, text=True, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            args = json.loads(log.read_text())
            self.assertEqual(args[:2], ["test", "integration_test/mobile_smoke_test.dart"])
            self.assertIn("--flavor", args, "The emulator test must build one explicit flavor")
            self.assertEqual(args[args.index("--flavor") + 1], "apk")
            self.assertIn("--dart-define=PPVPN_ANDROID_CHANNEL=apk", args)
            # A change to these tests must actually select the native CI job.
            gh = bins / "gh"
            gh.write_text('#!/bin/sh\nprintf "%s\\n" "$CHANGED_FILE"\n')
            gh.chmod(0o755)
            scope_env = {**env, "GITHUB_EVENT_NAME": "pull_request", "PR_BASE_SHA": "base",
                         "GITHUB_REPOSITORY": "fixture/repo", "GITHUB_SHA": "head"}
            for changed, selected in (("tools/desktop/ci/android-update-tests/apkTest.kt", "true"),
                                      ("apps/flutter/scripts/check-android-signature.sh", "true"),
                                      ("docs/release.md", "false")):
                result = subprocess.run(["bash", str(ROOT / "tools/ci-changes.sh")],
                                        env={**scope_env, "CHANGED_FILE": changed}, text=True, capture_output=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                outputs = dict(line.split("=", 1) for line in result.stdout.splitlines())
                self.assertEqual(outputs["android"], selected, changed)


if __name__ == "__main__":
    unittest.main()
