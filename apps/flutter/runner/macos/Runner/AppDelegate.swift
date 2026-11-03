import Cocoa
@preconcurrency import FlutterMacOS
import ServiceManagement
import UserNotifications

@main
class AppDelegate: FlutterAppDelegate {
  override func applicationWillFinishLaunching(_ notification: Notification) {
    // Notification responses can start a terminated app. Install before
    // didFinishLaunching and before Dart creates its channel listener.
    UNUserNotificationCenter.current().delegate = MacPush.shared
    super.applicationWillFinishLaunching(notification)
  }

  override func applicationDidFinishLaunching(_ notification: Notification) {
    // FlutterAppDelegate has no didFinishLaunching implementation (the
    // optional Objective-C protocol method still typechecks as a super call).
    Task { await MacPush.retireLegacyAgent() }
  }

  override func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }
  override func applicationSupportsSecureRestorableState(_ app: NSApplication) -> Bool { true }

  override func application(_ application: NSApplication, didRegisterForRemoteNotificationsWithDeviceToken token: Data) {
    MacPush.shared.inbox.receiveToken(token.map { String(format: "%02x", $0) }.joined())
  }

  override func application(_ application: NSApplication, didFailToRegisterForRemoteNotificationsWithError error: Error) {
    NSLog("push: APNs registration failed: %@", error.localizedDescription)
  }
}

@MainActor
final class MacPush: NSObject, UNUserNotificationCenterDelegate {
  static let shared = MacPush()
  let inbox = MacPushInbox()
  private var channel: FlutterMethodChannel?

  func attach(messenger: FlutterBinaryMessenger) {
    let channel = FlutterMethodChannel(name: "com.peakpassvpn.ppvpn/push", binaryMessenger: messenger)
    self.channel = channel
    channel.setMethodCallHandler { [weak self] call, result in
      guard let self else { result(nil); return }
      switch call.method {
      case "start":
        self.inbox.start { method, payload in
          var arguments = payload
          arguments["bundle_id"] = Bundle.main.bundleIdentifier
          channel.invokeMethod(method, arguments: arguments)
        }
        // Ad-hoc CI builds do not have an APNs provisioning profile.
        if Bundle.main.object(forInfoDictionaryKey: "PPVPNAPNsEnabled") as? Bool == true {
          NSApplication.shared.registerForRemoteNotifications()
        }
        result(nil)
      case "requestPermission":
        self.requestPermission()
        result(nil)
      default:
        result(FlutterMethodNotImplemented)
      }
    }
  }

  private func requestPermission() {
    guard Bundle.main.object(forInfoDictionaryKey: "PPVPNAPNsEnabled") as? Bool == true else { return }
    let center = UNUserNotificationCenter.current()
    center.getNotificationSettings { settings in
      guard settings.authorizationStatus == .notDetermined else { return }
      DispatchQueue.main.async {
        UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .badge, .sound]) { _, error in
          if let error { NSLog("push: notification permission failed: %@", error.localizedDescription) }
          DispatchQueue.main.async { NSApplication.shared.registerForRemoteNotifications() }
        }
      }
    }
  }

  // The plist is kept for one migration generation so SMAppService can find
  // the old job. Its executable is no longer shipped or registered.
  static func retireLegacyAgent() async {
    let service = SMAppService.agent(plistName: "com.peakpassvpn.ppvpn.desktop.agent.plist")
    // Only notRegistered confirms cleanup. A lookup failure (notFound)
    // after replacing the old bundle must still attempt removal.
    guard service.status != .notRegistered else { return }
    do {
      try await service.unregister()
      UserDefaults.standard.removeObject(forKey: "pushAgentRegisteredBuild")
    } catch {
      // The core also removes the old agent's credentials. A later launch
      // retries unregistering if macOS could not remove the job this time.
      NSLog("push: legacy agent removal failed: %@", error.localizedDescription)
    }
  }

  nonisolated func userNotificationCenter(
    _ center: UNUserNotificationCenter, willPresent notification: UNNotification,
    withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void
  ) {
    completionHandler([.banner, .list, .sound])
  }

  nonisolated func userNotificationCenter(
    _ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse,
    withCompletionHandler completionHandler: @escaping () -> Void
  ) {
    if response.actionIdentifier == UNNotificationDefaultActionIdentifier {
      let payload = MacPushInbox.tapPayload(response.notification.request.content.userInfo)
      DispatchQueue.main.async { self.inbox.receiveTapPayload(payload) }
    }
    completionHandler()
  }
}
