"""Exercise the CLI build/sign boundary without an Apple signing identity."""

import fnmatch
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[3]


class CliStages(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        (self.root / "tools/cli").mkdir(parents=True)
        shutil.copy(ROOT / "tools/cli/build-release.sh", self.root / "tools/cli")
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.log = self.root / "calls"
        self.command("cargo", '''
if [ "$1" = metadata ]; then
  echo '{"packages":[{"name":"ppvpn-cli","version":"0.7.4"}]}'
  exit 0
fi
echo "cargo-build $PPVPN_BUILD_PROFILE $PPVPN_VERSION" >> "$CALLS"
[ "$NO_BUILD" != 1 ] || exit 77
while [ "$1" != --target ]; do shift; done
mkdir -p "target/$2/release"
printf '#!/bin/sh\\necho "ppvpn 0.7.4"\\n' > "target/$2/release/ppvpn"
chmod 755 "target/$2/release/ppvpn"
''')
        # All calls beyond the unsigned build are tripwires. A package run
        # must reach lipo using the hand-off, without invoking cargo build.
        self.command("lipo", 'echo lipo >> "$CALLS"; exit 73')
        for cmd in ("security", "codesign", "xcrun"):
            self.command(cmd, 'echo forbidden-signing >> "$CALLS"; exit 74')
        # Avoid a new Python dependency in the existing unittest job.
        self.command("jq", "cat >/dev/null; echo 0.7.4")
        self.env = {**os.environ, "PATH": f"{self.bin}:{os.environ['PATH']}",
                    "CALLS": str(self.log), "CHANNEL": "stable", "VERSION": "0.7.4",
                    "PPVPN_API_BASE_DEFAULT": "https://api.example.test",
                    "NO_BUILD": "0"}
        for key in ("MACOS_SIGNING_P12_BASE64", "MACOS_SIGNING_P12_PASSWORD",
                    "ASC_KEY_ID", "ASC_ISSUER_ID", "ASC_KEY_P8"):
            self.env.pop(key, None)

    def command(self, name, body):
        path = self.bin / name
        path.write_text("#!/bin/sh\n" + body + "\n")
        path.chmod(0o755)

    def invoke(self, stage, *targets, **env):
        environment = {**self.env, **env}
        if stage is None:
            environment.pop("PPVPN_CLI_STAGE", None)
        else:
            environment["PPVPN_CLI_STAGE"] = stage
        return subprocess.run(
            ["bash", str(self.root / "tools/cli/build-release.sh"), "macos", *targets],
            env=environment,
            capture_output=True, text=True,
        )

    def calls(self):
        return self.log.read_text() if self.log.exists() else ""

    def test_unsigned_build_never_uses_signing_tools_or_creates_archives(self):
        result = self.invoke("build")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls().count("cargo-build"), 2)
        self.assertNotIn("lipo", self.calls())
        self.assertNotIn("forbidden-signing", self.calls())
        self.assertFalse((self.root / "dist").exists())

    def test_package_consumes_built_slices_without_compiling(self):
        self.assertEqual(self.invoke("build").returncode, 0)
        # Actions moves a tar between fresh jobs. Use that hand-off rather
        # than accidentally testing the producer's original target tree.
        archive = self.root / "unsigned.tar"
        subprocess.run(["tar", "-cf", str(archive), "-C", str(self.root / "target"),
                        "aarch64-apple-darwin/release/ppvpn",
                        "x86_64-apple-darwin/release/ppvpn"], check=True)
        shutil.rmtree(self.root / "target")
        (self.root / "target").mkdir()
        subprocess.run(["tar", "-xf", str(archive), "-C", str(self.root / "target")], check=True)
        for target in ("aarch64-apple-darwin", "x86_64-apple-darwin"):
            self.assertEqual((self.root / "target" / target / "release/ppvpn").stat().st_mode & 0o777, 0o755)
        self.log.write_text("")
        result = self.invoke("package", NO_BUILD="1")
        self.assertEqual(result.returncode, 73, result.stderr)
        self.assertEqual(self.calls(), "lipo\n")

    def test_missing_slice_does_not_trigger_a_build_after_approval(self):
        result = self.invoke("package", NO_BUILD="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("missing prebuilt CLI slice", result.stderr)
        self.assertEqual(self.calls(), "")

    def test_stable_build_still_requires_matching_version(self):
        result = self.invoke("build", VERSION="0.7.5")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("is not the CLI's", result.stderr)
        self.assertEqual(self.calls(), "")

    def test_stable_still_requires_both_architectures(self):
        result = self.invoke("build", "aarch64-apple-darwin")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("a stable build is universal", result.stderr)

    def test_unknown_stage_is_rejected(self):
        result = self.invoke("sign-maybe")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.calls(), "")

    def test_default_still_builds_and_then_packages(self):
        result = self.invoke(None)
        self.assertEqual(result.returncode, 73, result.stderr)
        self.assertEqual(self.calls().count("cargo-build"), 2)
        self.assertTrue(self.calls().endswith("lipo\n"))

    def test_dev_build_keeps_the_dev_identity(self):
        result = self.invoke("build", "aarch64-apple-darwin", CHANNEL="dev", VERSION="0.8.0")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls(), "cargo-build dev 0.8.0\n")


class ReleaseArtifacts(unittest.TestCase):
    def test_unsigned_cli_handoff_is_not_selected_for_publication(self):
        workflow = (ROOT / ".github/workflows/release.yml").read_text()
        # Read the actual producer, consumer and release collector. A name
        # such as cli-macos-unsigned also matches the final cli-* glob.
        jobs = dict(re.findall(r"^  ([\w-]+):\n(.*?)(?=^  [\w-]+:\n|\Z)",
                               workflow, re.MULTILINE | re.DOTALL))
        upload = re.search(r"uses: actions/upload-artifact@.*?\n          name: ([^\n]+)",
                           jobs["cli-macos-build"], re.DOTALL).group(1)
        download = re.search(r"uses: actions/download-artifact@.*?\n          name: ([^\n]+)",
                             jobs["cli-macos"], re.DOTALL).group(1)
        self.assertEqual(upload, download)
        patterns = re.findall(r"^          pattern: (.+)$", jobs["release-files"], re.MULTILINE)
        self.assertTrue(patterns)
        self.assertFalse(any(fnmatch.fnmatchcase(upload, pattern) for pattern in patterns),
                         "unsigned intermediate artifact is selected for publication")
        self.assertTrue(any(fnmatch.fnmatchcase("cli-macos", pattern) for pattern in patterns))


if __name__ == "__main__":
    unittest.main()
