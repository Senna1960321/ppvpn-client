#!/usr/bin/env bash
# Compile the real native hosts and exercise callbacks with Robolectric.
# Requires JDK 17+, Android SDK 36, Gradle 9.3.1 and the pinned Flutter cache.
set -euo pipefail
app=$(cd "$(dirname "$0")/.." && pwd)
work=${PPVPN_ANDROID_PUSH_TEST_DIR:-"$app/build/android-push-tests"}
: "${PPVPN_TEST_FLUTTER_JAR:?set PPVPN_TEST_FLUTTER_JAR to the pinned Android Flutter embedding jar}"
mkdir -p "$work/app/src/main" "$work/app/src/test/kotlin/com/peakpassvpn/ppvpn"
cp -R "$app/runner/android/app/src/main/" "$work/app/src/main/"
cp "$app/checks/android/PushTest.kt" "$work/app/src/test/kotlin/com/peakpassvpn/ppvpn/"
cat > "$work/settings.gradle.kts" <<'GRADLE'
pluginManagement { repositories { google(); mavenCentral(); gradlePluginPortal() } }
dependencyResolutionManagement { repositories { google(); mavenCentral() } }
rootProject.name = "android-push-tests"
include(":app")
GRADLE
cat > "$work/gradle.properties" <<'GRADLE'
android.useAndroidX=true
org.gradle.workers.max=2
org.gradle.jvmargs=-Xmx2g
GRADLE
cat > "$work/app/build.gradle.kts" <<'GRADLE'
plugins { id("com.android.application") version "9.1.0" }
android {
    namespace = "com.peakpassvpn.ppvpn"
    compileSdk = 36
    defaultConfig { applicationId = "com.peakpassvpn.ppvpn"; minSdk = 26; targetSdk = 36 }
    testOptions {
        unitTests.isIncludeAndroidResources = true
        unitTests.all {
            it.maxHeapSize = "1g"
            it.systemProperty("robolectric.dependency.repo.url", "https://repo.maven.apache.org/maven2")
            it.testLogging.events("passed", "failed", "skipped")
        }
    }
}
dependencies {
    implementation(files(System.getenv("PPVPN_TEST_FLUTTER_JAR")))
    testImplementation("junit:junit:4.13.2")
    testImplementation("org.robolectric:robolectric:4.17")
    testImplementation("androidx.work:work-testing:2.11.2")
}
GRADLE
cat > "$work/app/src/main/AndroidManifest.xml" <<'MANIFEST'
<manifest xmlns:android="http://schemas.android.com/apk/res/android">
    <uses-permission android:name="android.permission.INTERNET"/>
    <application android:theme="@android:style/Theme.Material.Light.NoActionBar">
        <service android:name=".TunnelService" android:process=":tunnel" android:exported="false" android:permission="android.permission.BIND_VPN_SERVICE"/>
        <activity android:name=".MainActivity" android:exported="true"/>
    </application>
</manifest>
MANIFEST
rm -f -- "$work/app/google-services.json"
python3 "$app/scripts/configure-android-push.py" "$work"
cd "$work"
"${GRADLE:-gradle}" --no-daemon :app:testDebugUnitTest
