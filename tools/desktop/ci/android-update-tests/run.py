#!/usr/bin/env python3
"""Compile the actual APK and Play updater sources and exercise them with Robolectric.

Needs Gradle 9.3.1, JDK 17+, Android SDK platform/build-tools 36, command-line
tools (apkanalyzer), and the Flutter SDK's Android embedding jar. Every
generated Android project lives in --work.
No Flutter/Rust build or Android emulator is required.
"""
import argparse
import os
from pathlib import Path
import shutil
import subprocess

from signature_checks import check_signatures

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[3]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--gradle", required=True)
    parser.add_argument("--flutter-root", required=True, type=Path)
    parser.add_argument("--work", required=True, type=Path)
    args = parser.parse_args()
    jar = args.flutter_root / "bin/cache/artifacts/engine/android-arm64/flutter.jar"
    if not jar.is_file():
        parser.error("Flutter Android embedding jar missing; run flutter precache --android")
    work = args.work.resolve()
    app = work / "app"
    app.mkdir(parents=True, exist_ok=True)
    shutil.copy2(HERE / "settings.gradle.kts", work / "settings.gradle.kts")
    shutil.copy2(HERE / "app.gradle.kts", app / "build.gradle.kts")
    (work / "gradle.properties").write_text(
        "android.useAndroidX=true\norg.gradle.workers.max=2\norg.gradle.jvmargs=-Xmx2g\n")
    env = {**os.environ, "PPVPN_TEST_FLUTTER_JAR": str(jar.resolve()),
           "PPVPN_UPDATE_SITE": "https://updates.example.test/root$literal", "PPVPN_RELEASE_CHANNEL": "stable"}
    subprocess.run(["python3", str(ROOT / "apps/flutter/scripts/android-distribution.py"),
                    str(app / "build.gradle.kts")], env=env, check=True)
    main_source = app / "src/main"
    main_source.mkdir(parents=True, exist_ok=True)
    (main_source / "AndroidManifest.xml").write_text(
        '<manifest xmlns:android="http://schemas.android.com/apk/res/android"><application/></manifest>\n')
    for flavor in ("apk", "play"):
        # Copy the complete production flavor, including its manifest/resources.
        # Gradle decides which flavor sources and permissions enter each variant.
        source = ROOT / "apps/flutter/runner/android/app/src" / flavor
        dest = app / "src" / flavor
        if dest.exists():
            shutil.rmtree(dest)
        shutil.copytree(source, dest)
        test = app / "src" / ("test" + flavor.title()) / "kotlin/com/peakpassvpn/ppvpn"
        test.mkdir(parents=True, exist_ok=True)
        shutil.copy2(HERE / "ChannelHarness.kt", test / "ChannelHarness.kt")
        shutil.copy2(HERE / (flavor + "Test.kt"), test / "UpdatePluginTest.kt")
    subprocess.run([args.gradle, "--no-daemon", "--console=plain", "testApkDebugUnitTest", "testPlayDebugUnitTest"],
                   cwd=work, env=env, check=True)
    check_signatures(app, work, env)


if __name__ == "__main__":
    main()
