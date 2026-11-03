#!/usr/bin/env python3
"""Validate and embed the Developer ID profile used for macOS APNs (#539)."""
import datetime as dt
import hashlib
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import sys

BUNDLE_ID = "com.peakpassvpn.ppvpn.desktop"
APS = "com.apple.developer.aps-environment"
APP_ID = "com.apple.application-identifier"
TEAM_ID = "com.apple.developer.team-identifier"


def validate(profile, team, certificate_sha1, now=None):
    now = now or dt.datetime.now(dt.timezone.utc)
    expiry = profile.get("ExpirationDate")
    if not isinstance(expiry, dt.datetime) or expiry.replace(tzinfo=dt.timezone.utc) <= now:
        raise ValueError("the macOS APNs profile has expired or has no expiry")
    if "OSX" not in profile.get("Platform", []) or profile.get("ProvisionsAllDevices") is not True:
        raise ValueError("macOS APNs needs a Developer ID profile, not a development or App Store profile")
    if team not in profile.get("TeamIdentifier", []):
        raise ValueError("the macOS APNs profile belongs to another team")
    entitlements = profile.get("Entitlements", {})
    app_id = entitlements.get(APP_ID, "")
    prefixes = profile.get("ApplicationIdentifierPrefix", [])
    if app_id not in [f"{prefix}.{BUNDLE_ID}" for prefix in prefixes]:
        raise ValueError("the macOS APNs profile must name the explicit desktop app id")
    if entitlements.get(TEAM_ID) != team or entitlements.get(APS) != "production":
        raise ValueError("the macOS APNs profile must grant production push to the signing team")
    if entitlements.get("get-task-allow") or entitlements.get("com.apple.security.get-task-allow"):
        raise ValueError("a release profile must not allow debugging")
    certificates = profile.get("DeveloperCertificates", [])
    if certificate_sha1.lower() not in [hashlib.sha1(cert).hexdigest() for cert in certificates]:
        raise ValueError("the Developer ID signing certificate is not in the APNs profile")
    return {APP_ID: app_id, TEAM_ID: team, APS: "production"}


def prepare(app, output, source_entitlements, team, profile_path, identity):
    info_path = app / "Contents/Info.plist"
    info = plistlib.loads(info_path.read_bytes())
    if info.get("CFBundleIdentifier") != BUNDLE_ID:
        raise ValueError("not the desktop PPVPN app")
    entitlements = plistlib.loads(source_entitlements.read_bytes())
    embedded = app / "Contents/embedded.provisionprofile"
    if team:
        if not profile_path or not Path(profile_path).is_file():
            raise ValueError("Developer ID builds require PPVPN_MACOS_APNS_PROFILE (MACOS_APNS_PROFILE_BASE64 in CI)")
        decoded = subprocess.run(
            ["security", "cms", "-D", "-i", str(profile_path)], check=True, capture_output=True
        ).stdout
        certificate = subprocess.run(
            ["security", "find-certificate", "-c", identity, "-p"], check=True, capture_output=True
        ).stdout
        der = subprocess.run(
            ["openssl", "x509", "-outform", "DER"], input=certificate, check=True, capture_output=True
        ).stdout
        entitlements.update(validate(plistlib.loads(decoded), team, hashlib.sha1(der).hexdigest()))
        shutil.copyfile(profile_path, embedded)
    else:
        if profile_path:
            raise ValueError("an APNs profile cannot be used without a Developer ID signing team")
        # CI and local builds stay runnable without restricted entitlements.
        for key in [APS, APP_ID, TEAM_ID]:
            entitlements.pop(key, None)
        embedded.unlink(missing_ok=True)
    info["PPVPNAPNsEnabled"] = bool(team)
    output.write_bytes(plistlib.dumps(entitlements))
    info_path.write_bytes(plistlib.dumps(info))


if __name__ == "__main__":
    try:
        prepare(Path(sys.argv[1]), Path(sys.argv[2]), Path(sys.argv[3]),
                os.environ.get("PPVPN_CODESIGN_TEAM", ""),
                os.environ.get("PPVPN_MACOS_APNS_PROFILE", ""),
                os.environ.get("PPVPN_CODESIGN_IDENTITY", ""))
    except (ValueError, OSError, subprocess.CalledProcessError, plistlib.InvalidFileException) as error:
        # Never print decoded provisioning contents or certificate command output.
        raise SystemExit(f"macOS APNs provisioning failed: {error}") from None
