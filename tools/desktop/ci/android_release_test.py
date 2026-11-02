"""Website APK verification and its real update-site integration."""

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import textwrap
import unittest

import android_release as apk
import update_site as site
from update_site_test import DOWNLOAD, SITE, release


SIGNER = "ab" * 32
TOOL = r'''#!/usr/bin/env python3
import json, os, pathlib, sys
config = json.loads(pathlib.Path(os.environ["APK_TEST_CONFIG"]).read_text())
with open(os.environ["APK_TEST_CALLS"], "a") as log:
    log.write(json.dumps(sys.argv[1:]) + "\n")
if sys.argv[1] == "verify":
    if config.get("signature_error"):
        sys.exit("APK signature invalid")
    for i, signer in enumerate(config["signers"], 1):
        print(f"Signer #{i} certificate SHA-256 digest: {signer}")
elif sys.argv[1] == "manifest":
    print(config[sys.argv[2]])
else:
    sys.exit("Unexpected Android tool invocation")
'''


class AndroidRelease(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.input = self.root / "input.apk"
        self.input.write_bytes(b"APK fixture whose signature tools are controlled by this test")
        self.out = self.root / "release"
        self.tool = self.root / "android-tool"
        self.tool.write_text(TOOL)
        self.tool.chmod(0o755)
        self.config_path = self.root / "tool-config.json"
        self.calls = self.root / "tool-calls.jsonl"
        self.config = {"signers": [SIGNER], "application-id": apk.PACKAGE,
                       "version-name": "0.4.0", "version-code": "1234"}
        self.old_env = {key: os.environ.get(key) for key in ("APK_TEST_CONFIG", "APK_TEST_CALLS")}
        os.environ.update(APK_TEST_CONFIG=str(self.config_path), APK_TEST_CALLS=str(self.calls))
        self.addCleanup(self.restore_env)

    def restore_env(self):
        for key, value in self.old_env.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value

    def package(self, **options):
        self.config_path.write_text(json.dumps(self.config))
        apk.package(self.input, self.out, options.get("version", "0.4.0"),
                    options.get("code", 1234), options.get("signer", SIGNER),
                    str(self.tool), str(self.tool))

    def metadata(self):
        return json.loads((self.out / "release-meta-android-apk.json").read_text())

    def test_verifies_signature_and_all_manifest_fields_before_packaging(self):
        self.package(signer=":".join(["AB"] * 32))
        meta = self.metadata()
        self.assertEqual(meta, {"schema": 1, "distribution": "apk", "package": apk.PACKAGE,
                               "version": "0.4.0", "version_code": 1234, "length": self.input.stat().st_size,
                               "sha256": hashlib.sha256(self.input.read_bytes()).hexdigest(),
                               "signer_sha256": SIGNER, "file": "PPVPN-0.4.0-1234-android.apk"})
        self.assertEqual((self.out / meta["file"]).read_bytes(), self.input.read_bytes())
        self.assertEqual([json.loads(line)[:-1] for line in self.calls.read_text().splitlines()],
                         [["verify", "--print-certs"], ["manifest", "application-id"],
                          ["manifest", "version-name"], ["manifest", "version-code"]])

    def test_invalid_signature_or_pin_publishes_nothing(self):
        for changes in ({"signature_error": True}, {"signers": []}, {"signers": ["cd" * 32]},
                        {"signers": [SIGNER, "cd" * 32]}):
            with self.subTest(changes=changes):
                before = dict(self.config)
                self.config.update(changes)
                with self.assertRaises((ValueError, subprocess.CalledProcessError)):
                    self.package()
                self.assertFalse(self.out.exists())
                self.config = before

    def test_manifest_mismatch_publishes_nothing(self):
        for field, value in (("application-id", "other.package"), ("version-name", "0.4.1"),
                             ("version-code", "1235")):
            with self.subTest(field=field):
                before = self.config[field]
                self.config[field] = value
                with self.assertRaisesRegex(ValueError, field):
                    self.package()
                self.assertFalse(self.out.exists())
                self.config[field] = before

    def test_package_limits_reject_before_creating_output(self):
        for code in (0, 2_100_000_001):
            self.config["version-code"] = str(code)
            with self.subTest(code=code), self.assertRaisesRegex(ValueError, "version_code"):
                self.package(code=code)
            self.assertFalse(self.out.exists())
        self.config["version-code"] = "1234"
        self.input.write_bytes(b"")
        with self.assertRaisesRegex(ValueError, "length"):
            self.package()
        self.assertFalse(self.out.exists())

    def test_metadata_rejects_unsafe_names_types_sizes_and_hashes(self):
        self.package()
        valid = self.metadata()
        cases = {"distribution": "play", "package": "other.package", "version": "../bad",
                 "version_code": True, "length": apk.MAX_APK_BYTES + 1, "sha256": "x" * 64,
                 "signer_sha256": "ABC", "file": "../elsewhere.apk", "schema": 2}
        for key, value in cases.items():
            with self.subTest(key=key), self.assertRaises(ValueError):
                apk.validate({**valid, key: value})

    def test_valid_apk_is_copied_and_linked_by_real_site_builder(self):
        self.package()
        release(self.out)
        web = self.root / "site"
        files = site.build(str(self.out), "v0.4.0", "stable", DOWNLOAD, SITE + "/prefix", str(web),
                           installers_on_site=True)
        relative = "android/stable/0.4.0/1234/PPVPN-0.4.0-1234-android.apk"
        self.assertIn(relative, files)
        self.assertIn("android/stable/latest.json", files)
        pointer = json.loads((web / "android/stable/latest.json").read_text())
        self.assertEqual(pointer, {**self.metadata(), "url": SITE + "/prefix/" + relative})
        self.assertEqual((web / relative).read_bytes(), self.input.read_bytes())
        # Exercise the actual workflow's rehearsal copy step as well: normal
        # release downloads and rehearsal staging must carry the same APK.
        workflow = (Path(__file__).resolve().parents[3] / ".github/workflows/release.yml").read_text()
        step = workflow.split("      - name: The rehearsal's release\n", 1)[1]
        body = step.split("        run: |\n", 1)[1]
        lines = []
        for line in body.splitlines():
            if line.strip() and not line.startswith("          "):
                break
            lines.append(line)
        script = textwrap.dedent("\n".join(lines))
        self.assertTrue(script.strip(), "Missing rehearsal shell block")
        staged = self.root / "rehearsal-release"
        shutil.copytree(self.out, staged)
        (staged / "SHA256SUMS").write_text("fixture sums\n")
        result = subprocess.run(["bash", "-c", script], cwd=self.root, text=True, capture_output=True,
                                env={**os.environ, "SITE_BASE": SITE, "TAG": "v0.4.0",
                                     "GITHUB_ENV": str(self.root / "github-env")})
        self.assertEqual(result.returncode, 0, result.stderr)
        rehearsal = self.root / "rehearsal-site"
        site.build(str(self.root / "newest"), "v0.4.0", "stable", DOWNLOAD, SITE, str(rehearsal),
                   installers_on_site=True)
        self.assertEqual((rehearsal / relative).read_bytes(), self.input.read_bytes())

    def test_bad_apk_prevents_all_desktop_and_android_site_writes(self):
        self.package()
        release(self.out)
        meta_path = self.out / "release-meta-android-apk.json"
        valid = self.metadata()
        source = self.out / valid["file"]
        cases = [{"version": "0.4.1", "file": "PPVPN-0.4.1-1234-android.apk"},
                 {"version_code": 1235, "file": "PPVPN-0.4.0-1235-android.apk"},
                 {"length": valid["length"] + 1}, {"sha256": "0" * 64},
                 {"signer_sha256": "not-a-pin"}]
        for change in cases:
            with self.subTest(change=change):
                meta_path.write_text(json.dumps({**valid, **change}))
                web = self.root / "refused-site"
                with self.assertRaises(ValueError):
                    site.build(str(self.out), "v0.4.0", "stable", DOWNLOAD, SITE, str(web),
                               installers_on_site=True)
                self.assertFalse(web.exists())
        meta_path.write_text(json.dumps(valid))
        source.unlink()
        with self.assertRaises(FileNotFoundError):
            site.build(str(self.out), "v0.4.0", "stable", DOWNLOAD, SITE, str(web), installers_on_site=True)
        self.assertFalse(web.exists())

    def test_historical_release_without_apk_and_orphan_apk(self):
        self.out.mkdir()
        self.assertEqual(apk.site_files(self.out, SITE, "stable", "0.4.0", 1234), ({}, {}))
        (self.out / "PPVPN-0.4.0-1234-android.apk").write_bytes(b"orphan")
        with self.assertRaisesRegex(ValueError, "no release metadata"):
            apk.site_files(self.out, SITE, "stable", "0.4.0", 1234)

    def test_site_origin_refusals_before_writes(self):
        self.package()
        release(self.out)
        for origin in ("http://updates.example.com", "https://user:password@updates.example.com",
                       "https://updates.example.com?query=1", "https://updates.example.com#fragment"):
            with self.subTest(origin=origin), self.assertRaises((ValueError, site.Refused)):
                site.build(str(self.out), "v0.4.0", "stable", DOWNLOAD, origin, str(self.root / "site"))
            self.assertFalse((self.root / "site").exists())


if __name__ == "__main__":
    unittest.main()
