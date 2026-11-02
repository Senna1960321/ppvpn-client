"""Run the real Windows service handoff with cargo replaced by fixture binaries.

Requires pwsh (preinstalled on the Ubuntu CI runner). PPVPN_TEST_PWSH can point
to a portable PowerShell for local runs. No signing service or Windows SDK is used.
"""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[3]
PINS = ("PPVPN_WINDOWS_PUBLISHER_EKU", "PPVPN_WINDOWS_NEXT_PUBLISHER_EKU")
CURRENT = "1.3.6.1.4.1.311.97.100"
NEXT = "1.3.6.1.4.1.311.97.200"
WRAPPER = r'''
$ErrorActionPreference = "Stop"
# Only the expensive native build is replaced. The production script creates
# the handoff, reads it back, and performs all configuration/version checks.
function cargo {
  Add-Content -LiteralPath $env:PPVPN_TEST_CALLS -Value "cargo $args"
  $out = Join-Path $env:PPVPN_TEST_ROOT "target/x86_64-pc-windows-msvc/service"
  New-Item -ItemType Directory -Force -Path $out | Out-Null
  foreach ($name in @("ppvpn-service", "ppvpn-service-install", "ppvpn-service-uninstall")) {
    [IO.File]::WriteAllText((Join-Path $out "$name.exe"), "fixture executable")
  }
  $global:LASTEXITCODE = 0
}
function Invoke-WebRequest { throw "unexpected network access" }
try {
if ($env:PPVPN_TEST_MODE -eq "build") {
  & $env:PPVPN_TEST_SCRIPT -ServiceOnly -OutDir $env:PPVPN_TEST_HANDOFF
} else {
  # This missing path is reached only after all ServiceDir checks succeed.
  # It stops the full production script before staging, downloads or signing.
  & $env:PPVPN_TEST_SCRIPT -Bundle $env:PPVPN_TEST_BUNDLE -ServiceDir $env:PPVPN_TEST_HANDOFF `
    -PushAgent (Join-Path $env:PPVPN_TEST_ROOT "pin-check-complete")
}
} catch {
  # Host-dependent PowerShell error rendering inserts colour and wraps a
  # message at different widths. Assert the exception, not that rendering.
  [Console]::Error.WriteLine($_.Exception.Message)
  exit 1
}
'''


class WindowsServiceHandoff(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.pwsh = os.environ.get("PPVPN_TEST_PWSH") or shutil.which("pwsh")
        if not cls.pwsh:
            raise RuntimeError("Windows service release tests require pwsh; set PPVPN_TEST_PWSH for a portable copy")

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.script = self.root / "apps/flutter/scripts/package-windows.ps1"
        self.script.parent.mkdir(parents=True)
        shutil.copy2(ROOT / "apps/flutter/scripts/package-windows.ps1", self.script)
        self.handoff = self.root / "handoff"
        self.bundle = self.root / "bundle"
        self.bundle.mkdir()
        (self.bundle / "ppvpn.exe").write_text("fixture executable")
        self.calls = self.root / "calls"
        self.env = {
            **os.environ,
            "PPVPN_TEST_SCRIPT": str(self.script),
            "PPVPN_TEST_ROOT": str(self.root),
            "PPVPN_TEST_HANDOFF": str(self.handoff),
            "PPVPN_TEST_BUNDLE": str(self.bundle),
            "PPVPN_TEST_CALLS": str(self.calls),
            # FileVersionInfo of these non-PE fixtures is 0.0.0.0.
            "PPVPN_VERSION": "0.0.0", "PPVPN_BUILD_NUMBER": "0",
            "PPVPN_WINDOWS_ALLOW_UNSIGNED_CLIENT": "0",
            PINS[0]: CURRENT, PINS[1]: NEXT,
        }
        for key in ("WINDOWS_SIGN_COMMAND", "PPVPN_RELEASE_CHANNEL", "SPARKLE_PRIVATE_KEY",
                    "PPVPN_UPDATE_PRIVATE_KEY", "PPVPN_UPDATE_PRIVATE_KEY_FILE"):
            self.env.pop(key, None)

    def invoke(self, mode, **overrides):
        env = {**self.env, "PPVPN_TEST_MODE": mode, **overrides}
        if mode == "package":
            # Configuration only: execution must stop before Invoke-Sign.
            env.setdefault("WINDOWS_SIGN_COMMAND", "test signer")
        return subprocess.run([self.pwsh, "-NoProfile", "-NonInteractive", "-Command", WRAPPER],
                              cwd=self.root, env=env, capture_output=True, text=True)

    def build(self, **overrides):
        result = self.invoke("build", **overrides)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("cargo build --locked -p ppvpn-service", self.calls.read_text())
        self.calls.write_text("")

    def assert_accepted(self, result):
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("pin-check-complete", result.stderr, result.stdout + result.stderr)
        self.assertNotIn("unexpected network access", result.stderr)
        self.assertEqual(self.calls.read_text(), "", "packaging rebuilt the service")

    def test_signed_pin_records_round_trip_including_empty_next_pin(self):
        for next_pin in (NEXT, ""):
            with self.subTest(next_pin=next_pin):
                self.build(**{PINS[1]: next_pin})
                self.assertEqual((self.handoff / "allow-unsigned-client.txt").read_bytes(), b"0\n")
                self.assertEqual((self.handoff / (PINS[0] + ".txt")).read_text(), CURRENT + "\n")
                self.assertEqual((self.handoff / (PINS[1] + ".txt")).read_text(), next_pin + "\n")
                self.assert_accepted(self.invoke("package", **{PINS[1]: next_pin}))

    def test_either_changed_environment_pin_is_rejected_before_signing(self):
        self.build()
        for pin in PINS:
            with self.subTest(pin=pin):
                result = self.invoke("package", **{pin: "1.3.6.1.4.1.311.97.999"})
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(f"prebuilt service's {pin} differs", result.stderr)
                self.assertNotIn("pin-check-complete", result.stderr)
                self.assertEqual(self.calls.read_text(), "")

    def test_missing_pin_records_fail_before_signing(self):
        for pin in PINS:
            with self.subTest(pin=pin):
                self.build()
                (self.handoff / (pin + ".txt")).unlink()
                result = self.invoke("package")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(pin + ".txt", result.stderr)
                self.assertNotIn("pin-check-complete", result.stderr)
                self.assertEqual(self.calls.read_text(), "")

    def test_unsigned_mode_round_trip_and_changed_signing_mode_refused(self):
        unsigned = {"PPVPN_WINDOWS_ALLOW_UNSIGNED_CLIENT": "1", PINS[0]: "", PINS[1]: ""}
        self.build(**unsigned)
        self.assertEqual((self.handoff / "allow-unsigned-client.txt").read_text(), "1\n")
        self.assert_accepted(self.invoke("package", WINDOWS_SIGN_COMMAND="", **unsigned))
        result = self.invoke("package")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("built with PPVPN_WINDOWS_ALLOW_UNSIGNED_CLIENT=1", result.stderr)
        self.assertNotIn("pin-check-complete", result.stderr)
        self.assertEqual(self.calls.read_text(), "")

    def test_enforced_signing_without_current_pin_fails_before_compiling(self):
        for pin in ("", "invalid"):
            with self.subTest(pin=pin):
                result = self.invoke("build", **{PINS[0]: pin})
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("needs PPVPN_WINDOWS_PUBLISHER_EKU", result.stderr)
                self.assertFalse(self.calls.exists())
                self.assertFalse(self.handoff.exists())


if __name__ == "__main__":
    unittest.main()
