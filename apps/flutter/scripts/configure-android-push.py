#!/usr/bin/env python3
"""Add the native FCM host and VPN foreground-service declarations."""
from pathlib import Path
import json
import os
import shutil
import sys
import xml.etree.ElementTree as ET

ANDROID = "http://schemas.android.com/apk/res/android"
ET.register_namespace("android", ANDROID)

def configure(project: Path, config: str | None = None) -> None:
    app = project / "app"
    if config:
        source = Path(config).resolve()
        data = json.loads(source.read_text())
        clients = data.get("client", [])
        if not any(client.get("client_info", {}).get("android_client_info", {}).get("package_name") == "com.peakpassvpn.ppvpn" for client in clients):
            raise SystemExit("google-services.json has no com.peakpassvpn.ppvpn Android client")
        shutil.copyfile(source, app / "google-services.json")
    gradle = app / "build.gradle.kts"
    text = gradle.read_text()
    if "com.google.gms.google-services" in text:
        raise SystemExit("Android push is already configured; regenerate the runner first")
    text = text.replace("plugins {", 'plugins {\n    id("com.google.gms.google-services") version "4.4.4" apply false\n', 1)
    text += '''
// A developer/CI build without Firebase config still supports the VPN
// service's self-hosted fallback. Configuration remains outside git.
if (file("google-services.json").isFile) {
    apply(plugin = "com.google.gms.google-services")
}
dependencies {
    implementation("com.google.firebase:firebase-messaging:26.0.1")
    implementation("androidx.work:work-runtime:2.11.2")
}
'''
    gradle.write_text(text)
    manifest = app / "src/main/AndroidManifest.xml"
    tree = ET.parse(manifest)
    root = tree.getroot()
    def attrs(**values): return {f"{{{ANDROID}}}{key}": value for key, value in values.items()}
    for name in ("POST_NOTIFICATIONS", "FOREGROUND_SERVICE", "FOREGROUND_SERVICE_SYSTEM_EXEMPTED"):
        ET.SubElement(root, "uses-permission", attrs(name=f"android.permission.{name}"))
    application = root.find("application")
    if application is None: raise SystemExit("AndroidManifest.xml has no application")
    tunnel = next((service for service in application.findall("service") if service.get(f"{{{ANDROID}}}name") == ".TunnelService"), None)
    if tunnel is None: raise SystemExit("AndroidManifest.xml has no TunnelService")
    tunnel.set(f"{{{ANDROID}}}foregroundServiceType", "systemExempted")
    service = ET.SubElement(application, "service", attrs(name=".PushMessagingService", exported="false"))
    events = ET.SubElement(service, "intent-filter")
    ET.SubElement(events, "action", attrs(name="com.google.firebase.MESSAGING_EVENT"))
    ET.SubElement(application, "meta-data", attrs(name="com.google.firebase.messaging.default_notification_channel_id", value="ppvpn-messages"))
    ET.SubElement(application, "meta-data", attrs(name="com.google.firebase.messaging.default_notification_icon", resource="@drawable/ic_notification"))
    ET.indent(tree, space="    ")
    tree.write(manifest, encoding="unicode", xml_declaration=True)

if __name__ == "__main__":
    configure(Path(sys.argv[1]), os.environ.get("ANDROID_GOOGLE_SERVICES_FILE"))
