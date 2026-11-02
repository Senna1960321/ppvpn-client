#!/usr/bin/env python3
"""Configure the generated Android runner's Play and website APK flavors."""
import json
import os
from pathlib import Path
import sys
from urllib.parse import urlsplit


def configure(path, site, channel):
    feed = ""
    if site and channel:
        parsed = urlsplit(site)
        if parsed.scheme != "https" or not parsed.hostname or parsed.username or parsed.password or parsed.query or parsed.fragment:
            raise ValueError("APK update site must be an HTTPS base URL without credentials, query or fragment")
        if channel not in ("dev", "stable"):
            raise ValueError("APK release channel must be dev or stable")
        feed = site.rstrip("/") + "/android/" + channel + "/latest.json"
    text = path.read_text()
    marker = "    buildTypes {"
    if text.count(marker) != 1:
        raise ValueError("Android template must have one buildTypes block")
    # The outer string is Kotlin, its contents the Java literal Gradle puts
    # in BuildConfig. Escape Kotlin interpolation as well as JSON quoting.
    literal = json.dumps(json.dumps(feed)).replace("$", "\\$")
    flavors = '''    buildFeatures { buildConfig = true }
    flavorDimensions += "distribution"
    productFlavors {
        create("play") {
            dimension = "distribution"
            buildConfigField("String", "PPVPN_UPDATE_FEED", "\\\"\\\"")
        }
        create("apk") {
            dimension = "distribution"
            buildConfigField("String", "PPVPN_UPDATE_FEED", FEED)
        }
    }
'''.replace("FEED)", literal + ")")
    path.write_text(text.replace(marker, flavors + marker) + '\n'
                    + 'dependencies { implementation("androidx.core:core:1.17.0") }\n')


if __name__ == "__main__":
    configure(Path(sys.argv[1]), os.environ.get("PPVPN_UPDATE_SITE", ""),
              os.environ.get("PPVPN_RELEASE_CHANNEL", ""))
