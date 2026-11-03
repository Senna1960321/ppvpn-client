"""Generated runner integration: Firebase is optional, VPN declarations survive."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
import xml.etree.ElementTree as ET

SCRIPT = Path(__file__).resolve().parents[1] / "scripts/configure-android-push.py"
SPEC = importlib.util.spec_from_file_location("android_push_config", SCRIPT)
CONFIG = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CONFIG)
ANDROID = "{http://schemas.android.com/apk/res/android}"


class AndroidPushConfigTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.app = self.root / "app"
        (self.app / "src/main").mkdir(parents=True)
        self.gradle = self.app / "build.gradle.kts"
        self.gradle.write_text('plugins { id("com.android.application") version "9.1.0" }\n')
        self.manifest = self.app / "src/main/AndroidManifest.xml"
        self.manifest.write_text('''<manifest xmlns:android="http://schemas.android.com/apk/res/android">
            <uses-permission android:name="android.permission.INTERNET"/>
            <application android:label="PPVPN">
                <activity android:name=".MainActivity" android:exported="true"/>
                <service android:name=".TunnelService" android:process=":tunnel"
                    android:exported="false" android:permission="android.permission.BIND_VPN_SERVICE">
                    <intent-filter><action android:name="android.net.VpnService"/></intent-filter>
                </service>
            </application>
        </manifest>''')

    def config(self, package):
        path = self.root / "firebase.json"
        path.write_text(json.dumps({"client": [{"client_info": {
            "android_client_info": {"package_name": package}
        }}]}))
        return path

    def test_no_firebase_keeps_vpn_and_adds_native_delivery(self):
        CONFIG.configure(self.root)
        self.assertFalse((self.app / "google-services.json").exists())
        self.assertIn('if (file("google-services.json").isFile)', self.gradle.read_text())
        tree = ET.parse(self.manifest)
        application = tree.find("application")
        services = {service.get(ANDROID + "name"): service for service in application.findall("service")}
        tunnel = services[".TunnelService"]
        self.assertEqual(tunnel.get(ANDROID + "foregroundServiceType"), "systemExempted")
        self.assertEqual(tunnel.get(ANDROID + "process"), ":tunnel")
        self.assertEqual(tunnel.get(ANDROID + "permission"), "android.permission.BIND_VPN_SERVICE")
        self.assertEqual(tunnel.find("intent-filter/action").get(ANDROID + "name"), "android.net.VpnService")
        push = services[".PushMessagingService"]
        self.assertEqual(push.get(ANDROID + "exported"), "false")
        self.assertEqual(push.find("intent-filter/action").get(ANDROID + "name"), "com.google.firebase.MESSAGING_EVENT")
        permissions = {item.get(ANDROID + "name") for item in tree.findall("uses-permission")}
        self.assertTrue({"android.permission.INTERNET", "android.permission.POST_NOTIFICATIONS",
                         "android.permission.FOREGROUND_SERVICE",
                         "android.permission.FOREGROUND_SERVICE_SYSTEM_EXEMPTED"} <= permissions)

    def test_correct_package_config_is_copied(self):
        source = self.config("com.peakpassvpn.ppvpn")
        CONFIG.configure(self.root, str(source))
        self.assertEqual((self.app / "google-services.json").read_bytes(), source.read_bytes())

    def test_wrong_package_fails_before_runner_changes(self):
        original = self.gradle.read_text()
        with self.assertRaisesRegex(SystemExit, "no com.peakpassvpn.ppvpn Android client"):
            CONFIG.configure(self.root, str(self.config("another.app")))
        self.assertFalse((self.app / "google-services.json").exists())
        self.assertEqual(self.gradle.read_text(), original)

    def test_refuses_to_inject_twice(self):
        CONFIG.configure(self.root)
        with self.assertRaisesRegex(SystemExit, "already configured"):
            CONFIG.configure(self.root)

    def test_missing_vpn_service_fails_build(self):
        self.manifest.write_text('<manifest><application/></manifest>')
        with self.assertRaisesRegex(SystemExit, "no TunnelService"):
            CONFIG.configure(self.root)


if __name__ == "__main__":
    unittest.main()
