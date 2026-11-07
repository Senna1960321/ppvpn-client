"""Exercise the real macOS packager with a small, ad-hoc signed app bundle."""

import hashlib
import json
import os
from pathlib import Path
import platform
import plistlib
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[3]
# The APNs profile preparer is shipped with the new macOS packaging pipeline.
# Build its current app layout when present, including when these ZIP tests
# are combined with the APNs migration.
APNS_LAYOUT = (ROOT / "apps/flutter/scripts/macos-push-profile.py").is_file()


@unittest.skipUnless(platform.system() == "Darwin", "requires Apple's packaging tools")
class MacOSPackage(unittest.TestCase):
    def test_zip_preserves_the_signed_app_and_metadata_names_its_bytes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in ("apps/macos/scripts/package-app.sh", "apps/macos/packaging/postinstall"):
                target = root / name
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(ROOT / name, target)

            def run(*args, **kwargs):
                result = subprocess.run(args, text=True, capture_output=True, **kwargs)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                return result

            def info(path, executable, identifier):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(plistlib.dumps({
                    "CFBundleExecutable": executable, "CFBundleIdentifier": identifier,
                    "CFBundlePackageType": "APPL", "CFBundleShortVersionString": "0.7.5",
                    "CFBundleVersion": "0.7.5.123", "LSMinimumSystemVersion": "13.0",
                }))

            app = root / "PPVPN.app"
            info(app / "Contents/Info.plist", "PPVPN", "com.peakpassvpn.fixture")
            binaries = app / "Contents/MacOS"
            binaries.mkdir()
            source = root / "main.c"
            source.write_text("int main(void) { return 0; }\n")
            run("clang", str(source), "-o", str(binaries / "PPVPN"))
            for name in ("service", "service-install", "service-uninstall"):
                shutil.copy2(binaries / "PPVPN", binaries / f"ppvpn-{name}")
            def make_framework(name):
                framework = app / f"Contents/Frameworks/{name}.framework"
                version = framework / "Versions/A"
                version.mkdir(parents=True)
                (framework / "Versions/Current").symlink_to("A")
                (framework / name).symlink_to(f"Versions/Current/{name}")
                (framework / "Resources").symlink_to("Versions/Current/Resources")
                info(version / "Resources/Info.plist", name,
                     f"com.peakpassvpn.fixture.{name.lower()}")
                source.write_text("int fixture(void) { return 0; }\n")
                library = version / name
                run("clang", "-dynamiclib", str(source), "-o", str(library), "-install_name",
                    f"@rpath/{name}.framework/Versions/A/{name}")
                return library

            framework_name = "Sparkle" if APNS_LAYOUT else "PPVPNClientFFI"
            library = make_framework(framework_name)
            agent = app / "Contents/Library/LoginItems/PPVPN Agent.app"
            if not APNS_LAYOUT:
                info(agent / "Contents/Info.plist", "PPVPN Agent", "com.peakpassvpn.fixture.agent")
                (agent / "Contents/MacOS").mkdir()
                source.write_text("extern int fixture(void); int main(void) { return fixture(); }\n")
                run("clang", str(source), str(library), "-o", str(agent / "Contents/MacOS/PPVPN Agent"))

            # Do not inherit release credentials from the developer's shell.
            env = {key: value for key, value in os.environ.items()
                   if not key.startswith(("PPVPN_", "SPARKLE_"))}
            env["PATH"] = "/usr/bin:/bin:" + env["PATH"]
            run("/bin/bash", str(root / "apps/macos/scripts/package-app.sh"), str(app),
                platform.machine(), env=env)
            out = root / "dist/macos"
            meta = json.loads(next(out.glob("release-meta-*.json")).read_text())
            self.assertTrue(meta["file"].endswith(".zip"), meta)
            self.assertTrue(meta["install_file"].endswith(".pkg"), meta)
            self.assertFalse(list(out.glob("*.dmg")))
            for prefix in ("", "install_"):
                data = (out / meta[prefix + "file"]).read_bytes()
                self.assertEqual(len(data), meta[prefix + "length"])
                self.assertEqual(hashlib.sha256(data).hexdigest(), meta[prefix + "sha256"])
            unpacked = root / "unpacked"
            run("ditto", "-x", "-k", str(out / meta["file"]), str(unpacked))
            copy = unpacked / "PPVPN.app"
            run("codesign", "--verify", "--deep", "--strict", str(copy))
            self.assertEqual((app / "Contents/Info.plist").read_bytes(),
                             (copy / "Contents/Info.plist").read_bytes())
            self.assertTrue(os.access(copy / "Contents/MacOS/PPVPN", os.X_OK))
            self.assertEqual(os.readlink(copy / f"Contents/Frameworks/{framework_name}.framework/Versions/Current"), "A")
            package = root / "package"
            run("pkgutil", "--expand-full", str(out / meta["install_file"]), str(package))
            run("codesign", "--verify", "--deep", "--strict", str(package / "Payload/PPVPN.app"))
            self.assertEqual((package / "Scripts/postinstall").read_bytes(),
                             (ROOT / "apps/macos/packaging/postinstall").read_bytes())
            if APNS_LAYOUT:
                for extracted in (copy, package / "Payload/PPVPN.app"):
                    self.assertFalse((extracted / "Contents/Library/LoginItems/PPVPN Agent.app").exists())
                    self.assertFalse((extracted / "Contents/Frameworks/PPVPNClientFFI.framework").exists())
                # Each retired component must be rejected independently. Keep
                # the production guard exercised alongside the valid fixture.
                for component in ("agent", "framework"):
                    with self.subTest(retired_component=component):
                        if component == "agent":
                            info(agent / "Contents/Info.plist", "PPVPN Agent", "com.peakpassvpn.fixture.agent")
                            (agent / "Contents/MacOS").mkdir()
                            shutil.copy2(binaries / "PPVPN", agent / "Contents/MacOS/PPVPN Agent")
                            retired = agent
                        else:
                            make_framework("PPVPNClientFFI")
                            retired = app / "Contents/Frameworks/PPVPNClientFFI.framework"
                        result = subprocess.run(
                            ["/bin/bash", str(root / "apps/macos/scripts/package-app.sh"),
                             str(app), platform.machine()], env=env, text=True, capture_output=True)
                        self.assertNotEqual(result.returncode, 0)
                        self.assertIn("retired push-agent code is still bundled", result.stderr)
                        shutil.rmtree(retired)


if __name__ == "__main__":
    unittest.main()
