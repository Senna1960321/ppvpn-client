"""Native cold-start buffering and generated iOS launch wiring (#539)."""
from pathlib import Path
import importlib.util
import shutil
import subprocess
import tempfile
import unittest

FLUTTER = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("install_ios_plugin", FLUTTER / "scripts/install-ios-plugin.py")
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)

TEMPLATE = '''import Flutter
import UIKit
@main
@objc class AppDelegate: FlutterAppDelegate, FlutterImplicitEngineDelegate {
  override func application(
    _ application: UIApplication,
    didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]?
  ) -> Bool {
    return super.application(application, didFinishLaunchingWithOptions: launchOptions)
  }
  func didInitializeImplicitFlutterEngine(_ engineBridge: FlutterImplicitEngineBridge) {
    GeneratedPluginRegistrant.register(with: engineBridge.pluginRegistry)
  }
}
'''


class IOSPushTest(unittest.TestCase):
    def test_installs_receiver_before_launch_returns_and_channel_after_engine(self):
        with tempfile.TemporaryDirectory() as tmp:
            delegate = Path(tmp) / "AppDelegate.swift"
            delegate.write_text(TEMPLATE)
            installer.install(delegate)
            code = delegate.read_text()
            self.assertLess(code.index("PPVPNPush.install()"), code.index("return super.application"))
            self.assertLess(code.index("func didInitializeImplicitFlutterEngine"), code.index("PPVPNPlugin.register(with:"))
            self.assertIn('engineBridge.pluginRegistry.registrar(forPlugin: "PPVPNPlugin")!', code)
            self.assertIn("final class PushInbox", code)
            self.assertEqual(code.count("import UserNotifications"), 1)

    def test_changed_template_fails_without_writing_partial_runner(self):
        with tempfile.TemporaryDirectory() as tmp:
            delegate = Path(tmp) / "AppDelegate.swift"
            source = TEMPLATE.replace("return super.application", "return changed.application")
            delegate.write_text(source)
            with self.assertRaisesRegex(SystemExit, "no didFinishLaunching"):
                installer.install(delegate)
            self.assertEqual(delegate.read_text(), source)

    @unittest.skipUnless(shutil.which("swiftc"), "Swift compiler required for native mailbox regression")
    def test_cold_start_token_rotation_and_taps_use_actual_swift_mailbox(self):
        with tempfile.TemporaryDirectory() as tmp:
            main = Path(tmp) / "main.swift"
            main.write_text('''import Foundation
let inbox = PushInbox()
var events: [(String, [String: String])] = []
inbox.receiveToken("before-engine")
inbox.receiveTap(["extra": ["message_id": 42, "deep_link": "file:///ignored"], "body": "ignored"])
inbox.start { events.append(($0, $1)) }
precondition(events.count == 2)
precondition(events[0].0 == "token" && events[0].1 == ["token": "before-engine"])
precondition(events[1].0 == "opened" && events[1].1 == ["message_id": "42"])
inbox.receiveToken("rotated")
precondition(events.last!.1 == ["token": "rotated"])
inbox.receiveTap(["message_id": "43", "deep_link": "https://ignored.test"])
precondition(events.last!.1 == ["message_id": "43"])
inbox.receiveTap(["body": "untrusted"])
precondition(events.last!.0 == "opened" && events.last!.1.isEmpty)
events.removeAll()
inbox.start { events.append(($0, $1)) }
precondition(events.count == 1 && events[0].1 == ["token": "rotated"])
print("cold-start, token rotation, payload filtering, and one-time tap delivery passed")
''')
            binary = Path(tmp) / "check"
            subprocess.run(["swiftc", str(FLUTTER / "runner/ios/Runner/PushInbox.swift"), str(main), "-o", str(binary)], check=True)
            subprocess.run([str(binary)], check=True)


if __name__ == "__main__":
    unittest.main()
