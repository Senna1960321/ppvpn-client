"""Real APK v2 and JAR signatures, using only keys generated in the test workdir."""

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[4]


def check_signatures(app: Path, work: Path, env: dict):
    sdk = Path(env.get("ANDROID_HOME") or env["ANDROID_SDK_ROOT"])
    java = Path(env["JAVA_HOME"]) / "bin"
    apksigner = sorted(sdk.glob("build-tools/*/apksigner"))[-1]
    apkanalyzer = sorted(sdk.glob("cmdline-tools/*/bin/apkanalyzer"))[-1]
    env = {**env, "PATH": str(java) + os.pathsep + env["PATH"],
           "JAVA_TOOL_OPTIONS": "-Duser.language=en -Duser.country=US"}
    fixtures = work / "signature-fixtures"
    fixtures.mkdir(exist_ok=True)
    # This is AGP's real compiled manifest/resource APK, before signing. It
    # needs no Flutter engine or Rust libraries to test signatures/metadata.
    unsigned = fixtures / "unsigned.apk"
    shutil.copy2(app / "build/intermediates/linked_resources_binary_format/apkDebug/"
                 "processApkDebugResources/linked-resources-binary-format-apkDebug.ap_", unsigned)

    def run(args, success=True, message=None):
        result = subprocess.run(list(map(str, args)), env=env, capture_output=True, text=True)
        assert (result.returncode == 0) == success, (args, result.stdout, result.stderr)
        if message:
            assert message in result.stderr, result.stderr
        return result

    def helper(artifact, kind, success=True, message=None):
        return run(["bash", ROOT / "apps/flutter/scripts/check-android-signature.sh", artifact, kind],
                   success, message)

    pin = None
    for name, subject in (("release", "CN=PPVPN Test"), ("debug", "CN=Android Debug,O=Android,C=US")):
        key = fixtures / (name + ".p12")
        if not key.exists():
            run([java / "keytool", "-genkeypair", "-keystore", key, "-storepass", "test-fixture",
                 "-keypass", "test-fixture", "-alias", "fixture", "-keyalg", "RSA", "-keysize", "2048",
                 "-validity", "1", "-dname", subject])
        apk = fixtures / (name + ".apk")
        run([apksigner, "sign", "--ks", key, "--ks-pass", "pass:test-fixture", "--ks-key-alias", "fixture",
             "--v1-signing-enabled", "false", "--v2-signing-enabled", "true", "--out", apk, unsigned])
        if name == "release":
            verified = run([apksigner, "verify", "--verbose", apk]).stdout
            assert "Verified using v1 scheme (JAR signing): false" in verified, verified
            assert "Verified using v2 scheme (APK Signature Scheme v2): true" in verified, verified
            # Independently derive the pin from the keystore certificate,
            # rather than repeating android_release.py's output parser.
            certificate = subprocess.run([str(java / "keytool"), "-exportcert", "-keystore", str(key),
                                          "-storepass", "test-fixture", "-alias", "fixture"],
                                         env=env, capture_output=True, check=True).stdout
            pin = hashlib.sha256(certificate).hexdigest()
            helper(apk, "apk")
        else:
            helper(apk, "apk", False, "signed with the debug key")
        # AABs retain JAR signing. The signature helper reads the container
        # signature, so the same small ZIP suffices for that branch.
        bundle = fixtures / (name + ".aab")
        run([java / "jarsigner", "-keystore", key, "-storepass", "test-fixture",
             "-signedjar", bundle, unsigned, "fixture"])
        helper(bundle, "aab", name == "release", None if name == "release" else "signed with the debug key")
    helper(unsigned, "apk", False)
    helper(unsigned, "aab", False, "is not signed")

    # Exercise the entire production packager with Android's actual signature
    # and manifest tools. Nothing is published; all output stays under --work.
    package = [sys.executable, ROOT / "tools/desktop/ci/android_release.py", "--version", "0.4.0",
               "--build", "100", "--apksigner", apksigner, "--apkanalyzer", apkanalyzer]
    out = fixtures / "packaged"
    run([*package, "--apk", fixtures / "release.apk", "--out", out, "--signer-sha256", pin])
    meta = json.loads((out / "release-meta-android-apk.json").read_text())
    assert meta["package"] == "com.peakpassvpn.ppvpn" and meta["signer_sha256"] == pin, meta
    assert meta["sha256"] == hashlib.sha256((out / meta["file"]).read_bytes()).hexdigest(), meta
    for name, source, expected_pin in (("wrong-pin", fixtures / "release.apk", "00" * 32),
                                       ("unsigned", unsigned, pin)):
        refused = fixtures / ("refused-" + name)
        run([*package, "--apk", source, "--out", refused, "--signer-sha256", expected_pin], False)
        assert not refused.exists(), "Refused APK created publication output"
    print("Android signatures: v2 APK and JAR accepted; debug/unsigned refused; real package tools passed")
