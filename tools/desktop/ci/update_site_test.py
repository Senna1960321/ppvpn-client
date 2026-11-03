#!/usr/bin/env python3
"""Tests of update_site.py: python3 -m unittest discover -s tools/desktop/ci -p '*_test.py'"""

import hashlib
import json
import os
import tempfile
import unittest
import xml.etree.ElementTree as ET

import update_site as site

SPARKLE_NS = "{http://www.andymatuschak.org/xml-namespaces/sparkle}"
FILES = {
    "macos-arm64": "PPVPN-0.4.0-macos-arm64.zip",
    "macos-x64": "PPVPN-0.4.0-macos-x64.zip",
    "windows-x64": "PPVPN-0.4.0-windows-x64-setup.exe",
    "linux-x64-deb": "ppvpn_0.4.0-1234_amd64.deb",
    "linux-x64-rpm": "ppvpn-0.4.0-1234.x86_64.rpm",
}
MIN_OS = {"macos-arm64": "13.0", "macos-x64": "13.0", "windows-x64": "10.0.17763",
          "linux-x64-deb": "ubuntu-22.04", "linux-x64-rpm": "fedora-36"}
DOWNLOAD = "https://github.com/peakpassvpn/ppvpn-client/releases/download"
SITE = "https://updates.example.com"


def package_name(platform):
    return FILES[platform].replace(".zip", ".pkg")


def release(directory, channel="stable", version="0.4.0", build="1234", skip=(), with_files=True,
            packages=None, **overrides):
    """Writes a release's metadata (and installers) into `directory`. packages:
    the macOS metas also describe their pkgs, "signed" with an EdDSA signature
    as from #548 on, "unsigned" as before."""
    for platform, name in FILES.items():
        if platform in skip:
            continue
        body = f"installer of {platform}".encode()
        if with_files:
            with open(os.path.join(directory, name), "wb") as f:
                f.write(body)
        meta = {
            "schema": 1, "platform": platform, "channel": channel, "version": version,
            "build": f"{version}.{build}", "file": name, "length": len(body),
            "sha256": hashlib.sha256(body).hexdigest(), "min_os": MIN_OS[platform],
            "published_at": "2026-11-01T08:00:00Z",
        }
        if platform in site.SPARKLE:
            meta["ed_signature"] = "c2lnbmF0dXJl"
        if platform == "windows-x64":
            meta["installer_arguments"] = "/S"
        if packages and platform in site.SILENT:
            package = f"package of {platform}".encode()
            if with_files:
                with open(os.path.join(directory, package_name(platform)), "wb") as f:
                    f.write(package)
            meta.update(install_file=package_name(platform), install_length=len(package),
                        install_sha256=hashlib.sha256(package).hexdigest(),
                        install_ed_signature="cGtnIHNpZ25hdHVyZQ==" if packages == "signed" else None)
        meta.update(overrides.get(platform, {}))
        with open(os.path.join(directory, f"release-meta-{platform}.json"), "w", encoding="utf-8") as f:
            json.dump(meta, f)


class Build(unittest.TestCase):
    def setUp(self):
        self.assets = tempfile.TemporaryDirectory()
        self.out = tempfile.TemporaryDirectory()
        self.addCleanup(self.assets.cleanup)
        self.addCleanup(self.out.cleanup)

    def build(self, tag="v0.4.0", channel="stable"):
        return site.build(self.assets.name, tag, channel, DOWNLOAD, SITE, self.out.name)

    def pointer(self, channel="stable"):
        with open(os.path.join(self.out.name, "desktop", "channels", f"{channel}.json"), encoding="utf-8") as f:
            return json.load(f)

    def written(self):
        return sorted(os.path.relpath(os.path.join(root, name), self.out.name)
                      for root, _, names in os.walk(self.out.name) for name in names)

    def test_stable_release(self):
        release(self.assets.name)
        self.assertEqual(self.build(), [
            "desktop/channels/stable.json",
            "desktop/stable/appcast-macos-arm64.xml",
            "desktop/stable/appcast-macos-x64.xml",
            "desktop/stable/appcast-windows-x64.xml",
        ])
        pointer = self.pointer()
        self.assertEqual((pointer["schema"], pointer["channel"], pointer["version"], pointer["build"], pointer["tag"]),
                         (2, "stable", "0.4.0", 1234, "v0.4.0"))
        self.assertEqual(pointer["published_at"], "2026-11-01T08:00:00Z")
        self.assertEqual(pointer["release_notes_url"], pointer["release_url"])
        self.assertEqual(pointer["sha256sums_url"], f"{DOWNLOAD}/v0.4.0/SHA256SUMS")
        self.assertEqual(set(pointer["platforms"]), set(site.PLATFORMS))
        windows = pointer["platforms"]["windows-x64"]
        self.assertEqual(windows["url"], f"{DOWNLOAD}/v0.4.0/{FILES['windows-x64']}")
        self.assertEqual(windows["installer_arguments"], "/S")
        self.assertEqual(windows["appcast_url"], f"{SITE}/desktop/stable/appcast-windows-x64.xml")
        deb = pointer["platforms"]["linux-x64-deb"]
        self.assertEqual(deb["min_os"], "ubuntu-22.04")
        self.assertNotIn("ed_signature", deb)
        self.assertNotIn("appcast_url", deb)
        site.check_pointer(pointer, "stable")

    def test_dev_release_tag_carries_the_build(self):
        release(self.assets.name, channel="dev")
        self.build(tag="v0.4.0-dev.1234", channel="dev")
        self.assertEqual(self.pointer("dev")["tag"], "v0.4.0-dev.1234")
        with self.assertRaises(site.Refused):
            self.build(tag="v0.4.0", channel="dev")

    def test_feeds(self):
        release(self.assets.name)
        self.build()
        for platform in site.SPARKLE:
            root = ET.parse(os.path.join(self.out.name, "desktop", "stable", f"appcast-{platform}.xml")).getroot()
            item = root.find("channel/item")
            self.assertEqual(item.find(f"{SPARKLE_NS}version").text, "0.4.0.1234")
            self.assertEqual(item.find(f"{SPARKLE_NS}shortVersionString").text, "0.4.0")
            self.assertEqual(item.find(f"{SPARKLE_NS}minimumSystemVersion").text, MIN_OS[platform])
            self.assertEqual(item.find("pubDate").text, "Sun, 01 Nov 2026 08:00:00 +0000")
            self.assertIsNone(item.find(f"{SPARKLE_NS}releaseNotesLink"))
            enclosure = item.find("enclosure")
            self.assertEqual(enclosure.get("url"), f"{DOWNLOAD}/v0.4.0/{FILES[platform]}")
            self.assertEqual(enclosure.get(f"{SPARKLE_NS}edSignature"), "c2lnbmF0dXJl")
            self.assertEqual(int(enclosure.get("length")), len(f"installer of {platform}"))
            if platform == "windows-x64":
                self.assertEqual(enclosure.get(f"{SPARKLE_NS}os"), "windows-x64")
                self.assertEqual(enclosure.get(f"{SPARKLE_NS}installerArguments"), "/S")
            else:
                self.assertIsNone(enclosure.get(f"{SPARKLE_NS}os"))

    def test_installers_on_site(self):
        release(self.assets.name)
        written = site.build(self.assets.name, "v0.4.0", "stable", DOWNLOAD, SITE, self.out.name,
                             installers_on_site=True)
        copies = [f"desktop/stable/0.4.0/1234/{FILES[platform]}" for platform in site.SPARKLE]
        self.assertEqual(written, sorted([
            "desktop/channels/stable.json",
            "desktop/stable/appcast-macos-arm64.xml",
            "desktop/stable/appcast-macos-x64.xml",
            "desktop/stable/appcast-windows-x64.xml",
            *copies,
        ]))
        self.assertEqual(self.written(), written)
        pointer = self.pointer()
        for platform in site.SPARKLE:
            relative = f"desktop/stable/0.4.0/1234/{FILES[platform]}"
            with open(os.path.join(self.out.name, relative), "rb") as f:
                body = f.read()
            # The copy is the bytes the metadata (and so SHA256SUMS and the signature) describe.
            self.assertEqual(hashlib.sha256(body).hexdigest(), pointer["platforms"][platform]["sha256"])
            self.assertEqual(pointer["platforms"][platform]["url"], f"{SITE}/{relative}")
            root = ET.parse(os.path.join(self.out.name, "desktop", "stable", f"appcast-{platform}.xml")).getroot()
            self.assertEqual(root.find("channel/item/enclosure").get("url"), f"{SITE}/{relative}")
        # Linux updates from the repositories; its addresses stay the release's.
        self.assertEqual(pointer["platforms"]["linux-x64-deb"]["url"], f"{DOWNLOAD}/v0.4.0/{FILES['linux-x64-deb']}")
        self.assertEqual(pointer["sha256sums_url"], f"{DOWNLOAD}/v0.4.0/SHA256SUMS")
        site.check_pointer(pointer, "stable")

    def test_signed_packages_go_in_the_pointer_and_the_site(self):
        release(self.assets.name, packages="signed")
        written = site.build(self.assets.name, "v0.4.0", "stable", DOWNLOAD, SITE, self.out.name,
                             installers_on_site=True)
        pointer = self.pointer()
        for platform in site.SILENT:
            relative = f"desktop/stable/0.4.0/1234/{package_name(platform)}"
            self.assertIn(relative, written)
            with open(os.path.join(self.out.name, relative), "rb") as f:
                body = f.read()
            entry = pointer["platforms"][platform]
            self.assertEqual({key: value for key, value in entry.items() if key.startswith("install_")}, {
                "install_file": package_name(platform),
                "install_url": f"{SITE}/{relative}",
                "install_length": len(body),
                "install_sha256": hashlib.sha256(body).hexdigest(),
                "install_ed_signature": "cGtnIHNpZ25hdHVyZQ==",
            })
            # The feed still updates from the ZIP.
            root = ET.parse(os.path.join(self.out.name, "desktop", "stable", f"appcast-{platform}.xml")).getroot()
            self.assertTrue(root.find("channel/item/enclosure").get("url").endswith(FILES[platform]))
        self.assertFalse([key for key in pointer["platforms"]["windows-x64"] if key.startswith("install_")])
        site.check_pointer(pointer, "stable")

    def test_packages_without_a_signature_stay_out(self):
        # Releases before #548: the pkg is in the metadata but not signed for the service.
        release(self.assets.name, packages="unsigned")
        written = site.build(self.assets.name, "v0.4.0", "stable", DOWNLOAD, SITE, self.out.name,
                             installers_on_site=True)
        self.assertFalse([path for path in written if path.endswith(".pkg")])
        for entry in self.pointer()["platforms"].values():
            self.assertFalse([key for key in entry if key.startswith("install_")])

    def test_signed_packages_without_the_site_point_at_the_release(self):
        release(self.assets.name, packages="signed", with_files=False)
        self.build()
        entry = self.pointer()["platforms"]["macos-x64"]
        self.assertEqual(entry["install_url"], f"{DOWNLOAD}/v0.4.0/{package_name('macos-x64')}")

    def test_installers_on_site_need_the_signed_packages(self):
        release(self.assets.name, packages="signed")
        os.remove(os.path.join(self.assets.name, package_name("macos-arm64")))
        with self.assertRaisesRegex(site.Refused, "macos-arm64: .*\\.pkg is missing"):
            site.build(self.assets.name, "v0.4.0", "stable", DOWNLOAD, SITE, self.out.name, installers_on_site=True)
        self.assertEqual(self.written(), [])

    def test_package_refusals(self):
        cases = {
            "wrong package hash": {"macos-x64": {"install_sha256": "0" * 64}},
            "wrong package length": {"macos-arm64": {"install_length": 1}},
            "not a pkg": {"macos-arm64": {"install_file": FILES["macos-arm64"]}},
            "path in install_file": {"macos-arm64": {"install_file": "../x.pkg"}},
            "length as text": {"macos-x64": {"install_length": "17"}},
            "no package hash": {"macos-x64": {"install_sha256": None}},
            "signature on windows": {"windows-x64": {"install_ed_signature": "c2ln"}},
        }
        for name, overrides in cases.items():
            with self.subTest(name), tempfile.TemporaryDirectory() as assets, tempfile.TemporaryDirectory() as out:
                release(assets, packages="signed", **overrides)
                with self.assertRaises(site.Refused):
                    site.build(assets, "v0.4.0", "stable", DOWNLOAD, SITE, out)
                self.assertEqual(os.listdir(out), [])

    def test_installers_on_site_need_the_installers(self):
        release(self.assets.name)
        os.remove(os.path.join(self.assets.name, FILES["windows-x64"]))
        with self.assertRaisesRegex(site.Refused, "windows-x64: .* is missing"):
            site.build(self.assets.name, "v0.4.0", "stable", DOWNLOAD, SITE, self.out.name, installers_on_site=True)
        self.assertEqual(self.written(), [])

    def test_a_missing_platform_writes_nothing(self):
        release(self.assets.name, skip=("linux-x64-rpm",))
        with self.assertRaises(site.Refused):
            self.build()
        self.assertEqual(self.written(), [])

    def test_metadata_without_the_installers(self):
        release(self.assets.name, with_files=False)
        self.build()
        self.assertEqual(self.pointer()["platforms"]["macos-arm64"]["length"], len("installer of macos-arm64"))

    def test_refusals(self):
        cases = {
            "wrong hash": {"macos-x64": {"sha256": "0" * 64}},
            "wrong length": {"windows-x64": {"length": 1}},
            "no update signature": {"windows-x64": {"ed_signature": None}},
            "another version": {"linux-x64-deb": {"version": "0.4.1"}},
            "another build": {"linux-x64-rpm": {"build": "0.4.0.1235"}},
            "another channel": {"macos-arm64": {"channel": "dev"}},
            "path in file": {"macos-arm64": {"file": "../x.zip"}},
        }
        for name, overrides in cases.items():
            with self.subTest(name), tempfile.TemporaryDirectory() as assets, tempfile.TemporaryDirectory() as out:
                release(assets, **overrides)
                with self.assertRaises(site.Refused):
                    site.build(assets, "v0.4.0", "stable", DOWNLOAD, SITE, out)
                self.assertEqual(os.listdir(out), [])

    def test_wrong_tag_or_channel(self):
        release(self.assets.name)
        for tag, channel in (("v0.4.1", "stable"), ("desktop-v0.4.0", "stable"), ("v0.4.0-dev.1234", "dev")):
            with self.subTest(tag=tag, channel=channel), self.assertRaises(site.Refused):
                self.build(tag=tag, channel=channel)

    def test_bases_must_be_https(self):
        release(self.assets.name)
        with self.assertRaises(site.Refused):
            site.build(self.assets.name, "v0.4.0", "stable", "http://example.com", SITE, self.out.name)


class Downgrade(unittest.TestCase):
    def published(self, version="0.4.0", build="1234"):
        """The pointer of an already published stable release."""
        with tempfile.TemporaryDirectory() as assets, tempfile.TemporaryDirectory() as out:
            release(assets, version=version, build=build)
            site.build(assets, f"v{version}", "stable", DOWNLOAD, SITE, out)
            with open(os.path.join(out, "desktop", "channels", "stable.json"), encoding="utf-8") as f:
                return json.load(f)

    def publish(self, version, build, current, **options):
        with tempfile.TemporaryDirectory() as assets, tempfile.TemporaryDirectory() as out:
            release(assets, version=version, build=build)
            site.build(assets, f"v{version}", "stable", DOWNLOAD, SITE, out, current=current, **options)
            return sorted(os.listdir(out))

    def test_newer_releases_are_published(self):
        current = self.published("0.4.0", "1234")
        for version, build in (("0.4.0", "1235"), ("0.4.1", "1235"), ("0.10.0", "1300"), ("1.0.0", "1235")):
            with self.subTest(version=version, build=build):
                self.assertEqual(self.publish(version, build, current), ["desktop"])

    def test_the_same_or_an_older_release_is_refused(self):
        current = self.published("0.4.0", "1234")
        # The same release again, an older build, an older version (also
        # with a higher build), and a version that only sorts higher as text.
        for version, build in (("0.4.0", "1234"), ("0.4.0", "1233"), ("0.3.9", "1233"), ("0.3.9", "2000")):
            with self.subTest(version=version, build=build), self.assertRaises(site.Refused):
                self.publish(version, build, current)
        with self.assertRaises(site.Refused):
            self.publish("0.9.0", "2000", self.published("0.10.0", "1234"))

    def test_a_refused_release_writes_nothing(self):
        current = self.published("0.4.0", "1234")
        with tempfile.TemporaryDirectory() as assets, tempfile.TemporaryDirectory() as out:
            release(assets, version="0.3.9", build="1200")
            with self.assertRaises(site.Refused):
                site.build(assets, "v0.3.9", "stable", DOWNLOAD, SITE, out, current=current)
            self.assertEqual(os.listdir(out), [])

    def test_allow_downgrade(self):
        current = self.published("0.4.0", "1234")
        self.assertEqual(self.publish("0.3.9", "1200", current, allow_downgrade=True), ["desktop"])

    def test_the_published_pointer_must_be_this_channel_s_and_valid(self):
        current = self.published("0.4.0", "1234")
        current["channel"] = "dev"
        with self.assertRaises(site.Refused):
            self.publish("0.4.1", "1235", current)
        with self.assertRaises(site.Refused):
            self.publish("0.4.1", "1235", {"schema": 1})


class Check(unittest.TestCase):
    def pointer(self):
        with tempfile.TemporaryDirectory() as assets, tempfile.TemporaryDirectory() as out:
            release(assets, packages="signed")
            site.build(assets, "v0.4.0", "stable", DOWNLOAD, SITE, out)
            with open(os.path.join(out, "desktop", "channels", "stable.json"), encoding="utf-8") as f:
                return json.load(f)

    def test_accepts_what_build_writes(self):
        site.check_pointer(self.pointer(), "stable")

    def test_accepts_a_pointer_without_install_fields(self):
        # The published pointer of a release before #548, read as --current.
        pointer = self.pointer()
        for platform in site.SILENT:
            for key in site.INSTALL_KEYS:
                del pointer["platforms"][platform][key]
        site.check_pointer(pointer, "stable")

    def test_refuses(self):
        def broken(change):
            pointer = self.pointer()
            change(pointer)
            return pointer

        cases = {
            "schema": lambda p: p.update(schema=1),
            "channel": lambda p: p.update(channel="dev"),
            "build zero": lambda p: p.update(build=0),
            "build too large": lambda p: p.update(build=65536),
            "build as text": lambda p: p.update(build="1234"),
            "tag": lambda p: p.update(tag="v0.4.1"),
            "local time": lambda p: p.update(published_at="2026-11-01T16:00:00+08:00"),
            "a platform missing": lambda p: p["platforms"].pop("linux-x64-rpm"),
            "http url": lambda p: p["platforms"]["macos-x64"].update(url="http://example.com/" + FILES["macos-x64"]),
            "url of another file": lambda p: p["platforms"]["macos-x64"].update(url=DOWNLOAD + "/v0.4.0/other.zip"),
            "zero length": lambda p: p["platforms"]["windows-x64"].update(length=0),
            "short hash": lambda p: p["platforms"]["windows-x64"].update(sha256="abc"),
            "no signature": lambda p: p["platforms"]["windows-x64"].update(ed_signature=""),
            "partial install fields": lambda p: p["platforms"]["macos-x64"].pop("install_sha256"),
            "install fields on windows": lambda p: p["platforms"]["windows-x64"].update(
                {key: value for key, value in p["platforms"]["macos-x64"].items() if key.startswith("install_")}),
            "install_url of another file": lambda p: p["platforms"]["macos-arm64"].update(install_url=DOWNLOAD + "/v0.4.0/other.pkg"),
            "http install_url": lambda p: p["platforms"]["macos-arm64"].update(
                install_url="http://example.com/" + p["platforms"]["macos-arm64"]["install_file"]),
            "zero install_length": lambda p: p["platforms"]["macos-arm64"].update(install_length=0),
            "short install_sha256": lambda p: p["platforms"]["macos-arm64"].update(install_sha256="abc"),
            "empty install_ed_signature": lambda p: p["platforms"]["macos-arm64"].update(install_ed_signature=""),
        }
        for name, change in cases.items():
            with self.subTest(name), self.assertRaises(site.Refused):
                site.check_pointer(broken(change), "stable")
        with self.assertRaises(site.Refused):
            site.check_pointer(self.pointer(), "dev")


if __name__ == "__main__":
    unittest.main()
