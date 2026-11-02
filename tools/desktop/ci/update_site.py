#!/usr/bin/env python3
"""The update site's files for one desktop release: the channel pointer and
the Sparkle feeds, written from the release metadata the package builds
produce. Standard library only.

    update_site.py build --assets DIR --tag TAG --channel dev|stable \\
        --download-base URL --site-base URL --out DIR \\
        [--installers-on-site] [--current POINTER [--allow-downgrade]]
    update_site.py check POINTER --channel dev|stable

`build` reads DIR/release-meta-<platform>.json for all five platforms (a
release with one missing is not published: nothing is written) and writes

    OUT/desktop/channels/<channel>.json            the channel pointer
    OUT/desktop/<channel>/appcast-<platform>.xml   macOS (both) and Windows

An installer that is in DIR next to its metadata is checked against the
metadata's length and SHA-256. Every asset's address is
<download-base>/<tag>/<file>; the feeds are under <site-base>.

With --installers-on-site, the macOS and Windows installers (what the
Sparkle feeds' enclosures fetch) are part of the site too, at

    OUT/desktop/<channel>/<version>/<build>/<file>

and their addresses in the pointer and the feeds are there, under
<site-base>: the site is CDN-cached where GitHub's release downloads are
slow from some networks. They must then be in DIR, so that the copies are
the bytes of the metadata's length, SHA-256 and EdDSA signature. The
release's own assets stay where they are.

With --current, the channel's pointer as published now, the release must be
newer than it by (version, build): running an old tag's release again must
not point the channel and its feeds back at the old version.
--allow-downgrade lifts that, for a deliberate rollback.

`check` applies to a pointer the rules its readers apply, so that a
pointer they would refuse is never deployed.

The pointer (schema 2):

    schema, channel, version, build (integer: the last part of the
    platforms' "<version>.<build>"), tag, published_at (UTC),
    release_url, release_notes_url, sha256sums_url,
    platforms: { <platform>: { file, url, length, sha256, min_os,
        and for the Sparkle platforms ed_signature, appcast_url,
        on Windows installer_arguments,
        and on macOS, when the release signed its pkg, install_file,
        install_url, install_length, install_sha256, install_ed_signature } }

The macOS install_* fields are the .pkg the privileged service installs
silently (#548, docs/host-integration.md section 16). With
--installers-on-site the pkg is copied into the site like the DMG. A
release whose metadata has no install_ed_signature (before #548, or built
without the update key) has none of them, and the apps keep Sparkle's
prompt.
"""

import argparse
import datetime
import hashlib
import json
import os
import re
import shutil
import sys
from email.utils import format_datetime
from xml.sax.saxutils import escape, quoteattr

PLATFORMS = ("macos-arm64", "macos-x64", "windows-x64", "linux-x64-deb", "linux-x64-rpm")
# Updated through Sparkle (macOS) and WinSparkle: an EdDSA signature and a feed each.
SPARKLE = ("macos-arm64", "macos-x64", "windows-x64")
CHANNELS = ("dev", "stable")
# Installed silently by the privileged service from the pkg (#548).
SILENT = ("macos-arm64", "macos-x64")
INSTALL_KEYS = ("install_file", "install_url", "install_length", "install_sha256", "install_ed_signature")
REPOSITORY = "peakpassvpn/ppvpn-client"
HOMEPAGE = "https://www.peakpassvpn.com"
FEED_TITLES = {"macos-arm64": "PPVPN for macOS", "macos-x64": "PPVPN for macOS", "windows-x64": "PPVPN for Windows"}


class Refused(Exception):
    """The input is not a release this tool publishes."""


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def expected_tag(channel, version, build):
    """The release's tag: the desktop app, the CLI and the engine share it."""
    return f"v{version}" if channel == "stable" else f"v{version}-dev.{build}"


def load_release(assets):
    """platform -> release metadata, for all five platforms, checked against
    each other and against the installers that are present."""
    metas = {}
    for platform in PLATFORMS:
        path = os.path.join(assets, f"release-meta-{platform}.json")
        if not os.path.isfile(path):
            raise Refused(f"release-meta-{platform}.json is missing: all of {', '.join(PLATFORMS)} are needed")
        with open(path, encoding="utf-8") as f:
            meta = json.load(f)
        if meta.get("schema") != 1:
            raise Refused(f"{platform}: unsupported release-meta schema {meta.get('schema')!r}")
        if meta.get("platform") != platform:
            raise Refused(f"{platform}: metadata is for {meta.get('platform')!r}")
        for key in ("version", "build", "file", "sha256", "published_at"):
            if not isinstance(meta.get(key), str) or not meta[key]:
                raise Refused(f"{platform}: {key} is missing")
        if not isinstance(meta.get("length"), int) or isinstance(meta.get("length"), bool):
            raise Refused(f"{platform}: length is not an integer")
        if os.path.basename(meta["file"]) != meta["file"]:
            raise Refused(f"{platform}: file is not a bare file name")
        check_file(assets, platform, meta["file"], meta["length"], meta["sha256"])
        if meta.get("install_ed_signature"):
            if platform not in SILENT:
                raise Refused(f"{platform}: install_ed_signature is for macOS only")
            if not isinstance(meta["install_ed_signature"], str):
                raise Refused(f"{platform}: install_ed_signature is not a string")
            install = meta.get("install_file")
            if not isinstance(install, str) or os.path.basename(install) != install or not install.endswith(".pkg"):
                raise Refused(f"{platform}: install_file is not a bare .pkg file name")
            if not isinstance(meta.get("install_length"), int) or isinstance(meta.get("install_length"), bool):
                raise Refused(f"{platform}: install_length is not an integer")
            if not isinstance(meta.get("install_sha256"), str):
                raise Refused(f"{platform}: install_sha256 is missing")
            check_file(assets, platform, install, meta["install_length"], meta["install_sha256"])
        metas[platform] = meta
    for key in ("version", "build", "channel"):
        values = {meta.get(key) for meta in metas.values()}
        if len(values) != 1:
            raise Refused(f"the platforms disagree on {key}: {sorted(map(str, values))}")
    return metas


def installer_path(channel, version, build, file):
    """A Sparkle installer's place in the site (--installers-on-site)."""
    return f"desktop/{channel}/{version}/{build}/{file}"


def build_pointer(metas, tag, channel, download_base, site_base, installers_on_site=False):
    first = metas[PLATFORMS[0]]
    version, full_build = first["version"], first["build"]
    if first.get("channel") != channel:
        raise Refused(f"the release was built for channel {first.get('channel')!r}, not {channel}")
    prefix = version + "."
    if not full_build.startswith(prefix) or not full_build[len(prefix):].isdigit():
        raise Refused(f"build {full_build!r} is not {version}.<number>")
    build = int(full_build[len(prefix):])
    if tag != expected_tag(channel, version, build):
        raise Refused(f"tag {tag} is not {expected_tag(channel, version, build)}")
    download_base, site_base = download_base.rstrip("/"), site_base.rstrip("/")
    release_url = f"https://github.com/{REPOSITORY}/releases/tag/{tag}"
    platforms = {}
    for platform in PLATFORMS:
        meta = metas[platform]
        url = f"{download_base}/{tag}/{meta['file']}"
        if installers_on_site and platform in SPARKLE:
            url = f"{site_base}/{installer_path(channel, version, build, meta['file'])}"
        entry = {
            "file": meta["file"],
            "url": url,
            "length": meta["length"],
            "sha256": meta["sha256"],
            "min_os": meta.get("min_os") or "",
        }
        if platform in SPARKLE:
            entry["ed_signature"] = meta.get("ed_signature") or ""
            entry["appcast_url"] = f"{site_base}/desktop/{channel}/appcast-{platform}.xml"
        if meta.get("installer_arguments"):
            entry["installer_arguments"] = meta["installer_arguments"]
        package = signed_package(meta)
        if package:
            install_url = f"{download_base}/{tag}/{package}"
            if installers_on_site:
                install_url = f"{site_base}/{installer_path(channel, version, build, package)}"
            entry.update({
                "install_file": package,
                "install_url": install_url,
                "install_length": meta["install_length"],
                "install_sha256": meta["install_sha256"],
                "install_ed_signature": meta["install_ed_signature"],
            })
        platforms[platform] = entry
    pointer = {
        "schema": 2,
        "channel": channel,
        "version": version,
        "build": build,
        "tag": tag,
        "published_at": max(meta["published_at"] for meta in metas.values()),
        "release_url": release_url,
        "release_notes_url": release_url,
        "sha256sums_url": f"{download_base}/{tag}/SHA256SUMS",
        "platforms": platforms,
    }
    check_pointer(pointer, channel)
    return pointer


def check_pointer(pointer, channel):
    """The readers' rules; raises Refused on the first one broken."""

    def need(condition, message):
        if not condition:
            raise Refused(f"pointer: {message}")

    need(isinstance(pointer, dict), "not an object")
    need(pointer.get("schema") == 2, f"schema is {pointer.get('schema')!r}, not 2")
    need(channel in CHANNELS and pointer.get("channel") == channel, f"channel is {pointer.get('channel')!r}, not {channel}")
    version, build, tag = pointer.get("version"), pointer.get("build"), pointer.get("tag")
    need(isinstance(version, str) and re.fullmatch(r"\d+\.\d+\.\d+", version), "version is not x.y.z")
    need(isinstance(build, int) and not isinstance(build, bool) and 0 < build <= 65535, "build is not an integer between 1 and 65535")
    need(tag == expected_tag(channel, version, build), f"tag {tag!r} does not match the version and build")
    need(isinstance(pointer.get("published_at"), str) and re.fullmatch(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ", pointer["published_at"]), "published_at is not a UTC timestamp")
    for key in ("release_url", "release_notes_url", "sha256sums_url"):
        need(isinstance(pointer.get(key), str) and pointer[key].startswith("https://"), f"{key} is not an https URL")
    platforms = pointer.get("platforms")
    need(isinstance(platforms, dict) and set(platforms) == set(PLATFORMS), "platforms are not exactly the five")
    for platform in PLATFORMS:
        entry = platforms[platform]
        need(isinstance(entry, dict), f"{platform} is not an object")
        need(isinstance(entry.get("file"), str) and entry["file"], f"{platform}: file is missing")
        need(isinstance(entry.get("url"), str) and entry["url"].startswith("https://") and entry["url"].endswith("/" + entry["file"]), f"{platform}: url is not an https URL of the file")
        need(isinstance(entry.get("length"), int) and not isinstance(entry.get("length"), bool) and entry["length"] > 0, f"{platform}: length is not a positive integer")
        need(isinstance(entry.get("sha256"), str) and re.fullmatch(r"[0-9a-f]{64}", entry["sha256"]), f"{platform}: sha256 is not 64 hex characters")
        need(isinstance(entry.get("min_os"), str), f"{platform}: min_os is not a string")
        if platform in SPARKLE:
            need(isinstance(entry.get("ed_signature"), str) and entry["ed_signature"], f"{platform}: ed_signature is missing (built without the update key)")
            need(isinstance(entry.get("appcast_url"), str) and entry["appcast_url"].startswith("https://"), f"{platform}: appcast_url is not an https URL")
        install_keys = sorted(key for key in entry if key.startswith("install_"))
        if not install_keys:
            continue
        need(platform in SILENT, f"{platform}: install_* fields are for macOS only")
        need(install_keys == sorted(INSTALL_KEYS), f"{platform}: install_* fields must be {', '.join(INSTALL_KEYS)} together, not {', '.join(install_keys)}")
        need(isinstance(entry["install_file"], str) and entry["install_file"].endswith(".pkg"), f"{platform}: install_file is not a .pkg")
        need(isinstance(entry["install_url"], str) and entry["install_url"].startswith("https://") and entry["install_url"].endswith("/" + entry["install_file"]), f"{platform}: install_url is not an https URL of install_file")
        need(isinstance(entry["install_length"], int) and not isinstance(entry["install_length"], bool) and entry["install_length"] > 0, f"{platform}: install_length is not a positive integer")
        need(isinstance(entry["install_sha256"], str) and re.fullmatch(r"[0-9a-f]{64}", entry["install_sha256"]), f"{platform}: install_sha256 is not 64 hex characters")
        need(isinstance(entry["install_ed_signature"], str) and entry["install_ed_signature"], f"{platform}: install_ed_signature is empty")


def appcast(pointer, platform):
    """The platform's feed with this release as its one item. The version
    Sparkle compares is "<version>.<build>", the apps' bundle and file version.
    The item has no sparkle:releaseNotesLink: the releases have no notes for
    users, and both Sparkle and WinSparkle would show the whole GitHub release
    page in their notes pane; without the link they show a plain update prompt."""
    entry = pointer["platforms"][platform]
    published = datetime.datetime.strptime(pointer["published_at"], "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=datetime.timezone.utc)
    attributes = [("url", entry["url"])]
    if platform.startswith("windows"):
        # Sparkle on macOS skips an item whose sparkle:os is not its own, so
        # only the Windows feed names one.
        attributes.append(("sparkle:os", platform))
        if entry.get("installer_arguments"):
            attributes.append(("sparkle:installerArguments", entry["installer_arguments"]))
    attributes += [
        ("length", str(entry["length"])),
        ("type", "application/octet-stream"),
        ("sparkle:edSignature", entry["ed_signature"]),
    ]
    enclosure = "\n".join(f"        {name}={quoteattr(value)}" for name, value in attributes)
    minimum = f"\n      <sparkle:minimumSystemVersion>{escape(entry['min_os'])}</sparkle:minimumSystemVersion>" if entry["min_os"] else ""
    return f"""<?xml version="1.0" encoding="utf-8"?>
<rss version="2.0" xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle">
  <channel>
    <title>{escape(FEED_TITLES[platform])}</title>
    <link>{escape(HOMEPAGE)}</link>
    <item>
      <title>PPVPN {escape(pointer['version'])}</title>
      <pubDate>{format_datetime(published)}</pubDate>
      <sparkle:version>{escape(pointer['version'])}.{pointer['build']}</sparkle:version>
      <sparkle:shortVersionString>{escape(pointer['version'])}</sparkle:shortVersionString>{minimum}
      <enclosure
{enclosure} />
    </item>
  </channel>
</rss>
"""


def check_file(assets, platform, name, length, sha256):
    """An installer that is present must be the one its metadata describes."""
    path = os.path.join(assets, name)
    if os.path.isfile(path):
        if os.path.getsize(path) != length:
            raise Refused(f"{platform}: {name} is not {length} bytes")
        if sha256_file(path) != sha256:
            raise Refused(f"{platform}: {name} does not have the metadata's SHA-256")


def signed_package(meta):
    """The pkg the service installs silently, when the release signed it."""
    return meta["install_file"] if meta.get("install_ed_signature") else None


def release_order(pointer):
    """What "newer" compares: the version's numbers, then the build."""
    return tuple(int(part) for part in pointer["version"].split(".")) + (pointer["build"],)


def refuse_downgrade(pointer, current, channel):
    """`current` is the channel's published pointer; `pointer` must be newer."""
    check_pointer(current, channel)
    if release_order(pointer) <= release_order(current):
        raise Refused(
            f"{pointer['version']} build {pointer['build']} is not newer than the published "
            f"{current['version']} build {current['build']} (--allow-downgrade to publish it anyway)")


def build(assets, tag, channel, download_base, site_base, out, current=None, allow_downgrade=False,
          installers_on_site=False):
    """Writes the pointer and the feeds (and with installers_on_site the Sparkle
    installers); nothing is written unless all of it can be."""
    for name, url in (("download-base", download_base), ("site-base", site_base)):
        if not url.startswith("https://"):
            raise Refused(f"--{name} is not an https URL")
    metas = load_release(assets)
    copies = {}
    if installers_on_site:
        for platform in SPARKLE:
            meta = metas[platform]
            source = os.path.join(assets, meta["file"])
            if not os.path.isfile(source):
                raise Refused(f"{platform}: {meta['file']} is missing: --installers-on-site puts it in the site")
            build_number = meta["build"][len(meta["version"]) + 1:]
            copies[installer_path(channel, meta["version"], build_number, meta["file"])] = source
            package = signed_package(meta)
            if package:
                source = os.path.join(assets, package)
                if not os.path.isfile(source):
                    raise Refused(f"{platform}: {package} is missing: --installers-on-site puts it in the site")
                copies[installer_path(channel, meta["version"], build_number, package)] = source
    pointer = build_pointer(metas, tag, channel, download_base, site_base, installers_on_site)
    if current is not None and not allow_downgrade:
        refuse_downgrade(pointer, current, channel)
    files = {f"desktop/channels/{channel}.json": json.dumps(pointer, indent=2, ensure_ascii=False) + "\n"}
    for platform in SPARKLE:
        files[f"desktop/{channel}/appcast-{platform}.xml"] = appcast(pointer, platform)
    for relative, source in copies.items():
        path = os.path.join(out, relative)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        shutil.copyfile(source, path)
    for relative, text in files.items():
        path = os.path.join(out, relative)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8") as f:
            f.write(text)
    return sorted([*files, *copies])


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    commands = parser.add_subparsers(dest="command", required=True)
    b = commands.add_parser("build")
    b.add_argument("--assets", required=True)
    b.add_argument("--tag", required=True)
    b.add_argument("--channel", required=True, choices=CHANNELS)
    b.add_argument("--download-base", required=True)
    b.add_argument("--site-base", required=True)
    b.add_argument("--out", required=True)
    b.add_argument("--installers-on-site", action="store_true",
                   help="put the macOS and Windows installers (and the signed macOS pkgs) in the site and point the feeds there")
    b.add_argument("--current", help="the channel's published pointer; the release must be newer")
    b.add_argument("--allow-downgrade", action="store_true", help="publish even if not newer than --current")
    c = commands.add_parser("check")
    c.add_argument("pointer")
    c.add_argument("--channel", required=True, choices=CHANNELS)
    args = parser.parse_args(argv)
    try:
        if args.command == "build":
            current = None
            if args.current:
                with open(args.current, encoding="utf-8") as f:
                    current = json.load(f)
            for path in build(args.assets, args.tag, args.channel, args.download_base, args.site_base, args.out,
                              current=current, allow_downgrade=args.allow_downgrade,
                              installers_on_site=args.installers_on_site):
                print(path)
        else:
            with open(args.pointer, encoding="utf-8") as f:
                check_pointer(json.load(f), args.channel)
            print("pointer ok")
    except Refused as refused:
        print(f"update_site.py: {refused}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
