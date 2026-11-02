#!/usr/bin/env python3
"""Verified website APK metadata and its update-site feed (#448)."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
from urllib.parse import urlsplit

PACKAGE = "com.peakpassvpn.ppvpn"
MAX_APK_BYTES = 512 * 1024 * 1024


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def validate(meta):
    if meta.get("schema") != 1 or meta.get("distribution") != "apk" or meta.get("package") != PACKAGE:
        raise ValueError("not PPVPN website APK metadata")
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", str(meta.get("version", ""))):
        raise ValueError("invalid APK version")
    for key, maximum in (("version_code", 2_100_000_000), ("length", MAX_APK_BYTES)):
        if type(meta.get(key)) is not int or not 0 < meta[key] <= maximum:
            raise ValueError(f"invalid APK {key}")
    for key in ("sha256", "signer_sha256"):
        if not re.fullmatch(r"[0-9a-f]{64}", str(meta.get(key, ""))):
            raise ValueError(f"invalid APK {key}")
    expected = f"PPVPN-{meta['version']}-{meta['version_code']}-android.apk"
    if meta.get("file") != expected:
        raise ValueError("invalid APK file name")


def package(apk, out, version, code, signer, apksigner, apkanalyzer):
    """Verify the APK's platform signature and declared manifest, not an
    AAB upload key or a debug key. The pin is Play App Signing's app key."""
    def run(*args):
        return subprocess.check_output(args, text=True).strip()
    report = run(apksigner, "verify", "--print-certs", str(apk))
    certificates = re.findall(r"^Signer #\d+ certificate SHA-256 digest: ([0-9a-fA-F]+)$", report, re.M)
    signer = signer.lower().replace(":", "")
    if certificates != [signer]:
        raise ValueError("APK signer does not match ANDROID_APK_SIGNER_SHA256")
    for field, expected in (("application-id", PACKAGE), ("version-name", version), ("version-code", str(code))):
        if run(apkanalyzer, "manifest", field, str(apk)) != expected:
            raise ValueError(f"APK manifest {field} differs from the release")
    meta = dict(schema=1, distribution="apk", package=PACKAGE, version=version, version_code=code,
                length=apk.stat().st_size, sha256=digest(apk), signer_sha256=signer,
                file=f"PPVPN-{version}-{code}-android.apk")
    validate(meta)
    out.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(apk, out / meta["file"])
    (out / "release-meta-android-apk.json").write_text(json.dumps(meta, indent=2) + "\n")


def site_files(assets, site_base, channel, expected_version, expected_build):
    """Optional for historical desktop releases. Metadata, when present,
    must match the current release and its APK before any site is written."""
    path = Path(assets) / "release-meta-android-apk.json"
    if not path.exists():
        if list(Path(assets).glob("*-android.apk")):
            raise ValueError("APK has no release metadata")
        return {}, {}
    meta = json.loads(path.read_text())
    validate(meta)
    if meta["version"] != expected_version or meta["version_code"] != expected_build:
        raise ValueError("APK and desktop release versions differ")
    source = Path(assets) / meta["file"]
    if source.stat().st_size != meta["length"] or digest(source) != meta["sha256"]:
        raise ValueError("APK bytes differ from the signed release metadata")
    base = urlsplit(site_base)
    if base.scheme != "https" or not base.hostname or base.username or base.password or base.query or base.fragment:
        raise ValueError("APK site must be an HTTPS base URL")
    relative = f"android/{channel}/{meta['version']}/{meta['version_code']}/{meta['file']}"
    pointer = {**meta, "url": site_base.rstrip("/") + "/" + relative}
    return {f"android/{channel}/latest.json": json.dumps(pointer, indent=2) + "\n"}, {relative: str(source)}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--apk", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--version", required=True)
    parser.add_argument("--build", required=True, type=int)
    parser.add_argument("--signer-sha256", required=True)
    parser.add_argument("--apksigner", required=True)
    parser.add_argument("--apkanalyzer", required=True)
    args = parser.parse_args()
    package(args.apk, args.out, args.version, args.build, args.signer_sha256, args.apksigner, args.apkanalyzer)
