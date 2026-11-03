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
            framework = app / "Contents/Frameworks/PPVPNClientFFI.framework"
            version = framework / "Versions/A"
            version.mkdir(parents=True)
            (framework / "Versions/Current").symlink_to("A")
            (framework / "PPVPNClientFFI").symlink_to("Versions/Current/PPVPNClientFFI")
            (framework / "Resources").symlink_to("Versions/Current/Resources")
            info(version / "Resources/Info.plist", "PPVPNClientFFI", "com.peakpassvpn.fixture.ffi")
            source.write_text("int fixture(void) { return 0; }\n")
            library = version / "PPVPNClientFFI"
            run("clang", "-dynamiclib", str(source), "-o", str(library), "-install_name",
                "@rpath/PPVPNClientFFI.framework/Versions/A/PPVPNClientFFI")
            agent = app / "Contents/Library/LoginItems/PPVPN Agent.app"
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
            self.assertEqual(os.readlink(copy / "Contents/Frameworks/PPVPNClientFFI.framework/Versions/Current"), "A")
            package = root / "package"
            run("pkgutil", "--expand-full", str(out / meta["install_file"]), str(package))
            run("codesign", "--verify", "--deep", "--strict", str(package / "Payload/PPVPN.app"))
            self.assertEqual((package / "Scripts/postinstall").read_bytes(),
                             (ROOT / "apps/macos/packaging/postinstall").read_bytes())


if __name__ == "__main__":
    unittest.main()
