// The app's channels on iOS (#448), the counterpart of runner/android's
// MainActivity and VpnPlugin. scripts/ci-build.sh appends this file to the
// generated AppDelegate.swift (a file the Runner target already compiles)
// and registers the plugin next to the generated ones.
//
// - com.peakpassvpn.ppvpn/app: `filesDir`, the app's own directory: the
//   App Group's container, which the packet tunnel shares (tunnel.sock),
//   or Application Support in a build without the App Group (unsigned,
//   the simulator). `openUrl` -> Bool: an http(s) page in an
//   SFSafariViewController presented as a page sheet over the app
//   (lib/links.dart, #473, #535), anything else through the system.
//   `closeBrowser`: that page dismissed, if up. `sampleSettings`: the
//   screenshot mode's page and theme in a simulator build (lib/sample.dart).
// - com.peakpassvpn.ppvpn/vpn: `prepare` -> Bool (the VPN configuration is
//   saved; the system asks the user the first time), `start`, `stop`.
// - com.peakpassvpn.ppvpn/vpn-state: the tunnel's NEVPNStatus as off /
//   connecting / on / disconnecting, the current one first.
//
// A build for the simulator also prints the Dart VM service's address for
// flutter test (VMServiceAnnouncer below).
//
// The packet tunnel itself (com.peakpassvpn.ppvpn.tunnel) is not in the
// app yet: it comes with the tunnel host's C functions (#448 step 2).

import Flutter
import NetworkExtension
import SafariServices
import UIKit
import UserNotifications
import os

final class PPVPNPlugin: NSObject, FlutterPlugin, FlutterStreamHandler {
  static let appGroup = "group.com.peakpassvpn.ppvpn"
  static let tunnelBundleID = "com.peakpassvpn.ppvpn.tunnel"

  private var manager: NETunnelProviderManager?
  private var sink: FlutterEventSink?
  private var observer: NSObjectProtocol?
  private weak var browser: SFSafariViewController?
  static func register(with registrar: FlutterPluginRegistrar) {
    let plugin = PPVPNPlugin()
    let messenger = registrar.messenger()
    registrar.addApplicationDelegate(plugin)
    let push = FlutterMethodChannel(name: "com.peakpassvpn.ppvpn/push", binaryMessenger: messenger)
    push.setMethodCallHandler { call, result in
      switch call.method {
      case "start":
        PPVPNPush.shared.inbox.start { method, payload in
          var arguments = payload
          if method == "token" { arguments["bundle_id"] = Bundle.main.bundleIdentifier ?? "com.peakpassvpn.ppvpn" }
          push.invokeMethod(method, arguments: arguments)
        }
        UIApplication.shared.registerForRemoteNotifications()
        result(nil)
      case "requestPermission":
        PPVPNPush.shared.requestPermission()
        result(nil)
      default: result(FlutterMethodNotImplemented)
      }
    }
    #if targetEnvironment(simulator)
      VMServiceAnnouncer.start(registrar)
    #endif
    FlutterMethodChannel(name: "com.peakpassvpn.ppvpn/app", binaryMessenger: messenger)
      .setMethodCallHandler { call, result in
        switch call.method {
        case "filesDir": result(filesDir())
        case "openUrl": plugin.open(call.arguments as? String, result: result)
        case "closeBrowser":
          plugin.browser?.dismiss(animated: true)
          result(nil)
        case "sampleSettings": result(sampleSettings())
        default: result(FlutterMethodNotImplemented)
        }
      }
    FlutterMethodChannel(name: "com.peakpassvpn.ppvpn/vpn", binaryMessenger: messenger)
      .setMethodCallHandler(plugin.handle)
    FlutterEventChannel(name: "com.peakpassvpn.ppvpn/vpn-state", binaryMessenger: messenger)
      .setStreamHandler(plugin)
  }

  func application(_ application: UIApplication, didRegisterForRemoteNotificationsWithDeviceToken deviceToken: Data) {
    PPVPNPush.shared.inbox.receiveToken(deviceToken.map { String(format: "%02x", $0) }.joined())
  }

  func application(_ application: UIApplication, didFailToRegisterForRemoteNotificationsWithError error: Error) {
    NSLog("push: APNs registration failed: %@", error.localizedDescription)
  }

  /// The screenshot mode's page and theme (lib/sample.dart) in a simulator
  /// build, from the environment or the launch arguments (`simctl launch
  /// <device> <bundle> -PPVPN_FLUTTER_SAMPLE nodes`, which land in the
  /// defaults): Dart does not see the variables simctl passes. Nil in a
  /// device build.
  private static func sampleSettings() -> [String: String]? {
    #if targetEnvironment(simulator)
      var settings: [String: String] = [:]
      for (key, name) in [("page", "PPVPN_FLUTTER_SAMPLE"), ("theme", "PPVPN_FLUTTER_THEME")] {
        if let value = ProcessInfo.processInfo.environment[name] ?? UserDefaults.standard.string(forKey: name) {
          settings[key] = value
        }
      }
      return settings
    #else
      return nil
    #endif
  }

  private static func filesDir() -> String? {
    let files = FileManager.default
    let dir =
      files.containerURL(forSecurityApplicationGroupIdentifier: appGroup)
      ?? files.urls(for: .applicationSupportDirectory, in: .userDomainMask).first
    guard let dir else { return nil }
    try? files.createDirectory(at: dir, withIntermediateDirectories: true)
    return dir.path
  }

  private func open(_ text: String?, result: @escaping FlutterResult) {
    guard let text, let url = URL(string: text) else { return result(false) }
    guard ["http", "https"].contains(url.scheme?.lowercased() ?? ""), let top = Self.topController() else {
      return UIApplication.shared.open(url) { ok in result(ok) }
    }
    browser?.dismiss(animated: false)
    let page = SFSafariViewController(url: url)
    page.dismissButtonStyle = .close
    // A sheet, the app showing above it (the canvas's 手机 · 登录：授权页).
    page.modalPresentationStyle = .pageSheet
    top.present(page, animated: true)
    browser = page
    result(true)
  }

  /// The controller on top of the app's key window, to present over.
  private static func topController() -> UIViewController? {
    let window = UIApplication.shared.connectedScenes
      .compactMap { $0 as? UIWindowScene }
      .flatMap(\.windows)
      .first(where: \.isKeyWindow)
    var top = window?.rootViewController
    while let presented = top?.presentedViewController { top = presented }
    return top
  }

  func handle(_ call: FlutterMethodCall, result: @escaping FlutterResult) {
    switch call.method {
    case "prepare":
      prepare { ok in result(ok) }
    case "start":
      guard let manager else {
        return result(FlutterError(code: "not-prepared", message: "prepare the VPN first", details: nil))
      }
      do {
        try manager.connection.startVPNTunnel()
        result(nil)
      } catch {
        result(FlutterError(code: "failed", message: error.localizedDescription, details: nil))
      }
    case "stop":
      manager?.connection.stopVPNTunnel()
      result(nil)
    default:
      result(FlutterMethodNotImplemented)
    }
  }

  /// The app's one tunnel configuration, saved (and enabled) when it is
  /// missing: saving it is what makes the system ask the user. False when
  /// the user declined or the system refused.
  private func prepare(_ done: @escaping (Bool) -> Void) {
    NETunnelProviderManager.loadAllFromPreferences { [weak self] managers, error in
      DispatchQueue.main.async {
        guard let self else { return done(false) }
        if let error {
          NSLog("vpn: load failed: \(error.localizedDescription)")
          return done(false)
        }
        let manager = managers?.first ?? NETunnelProviderManager()
        if manager.protocolConfiguration == nil {
          let tunnel = NETunnelProviderProtocol()
          tunnel.providerBundleIdentifier = Self.tunnelBundleID
          tunnel.serverAddress = "PPVPN"
          manager.protocolConfiguration = tunnel
          manager.localizedDescription = "PPVPN"
        }
        if manager.isEnabled && managers?.first != nil {
          self.use(manager)
          return done(true)
        }
        manager.isEnabled = true
        manager.saveToPreferences { error in
          if let error {
            NSLog("vpn: save failed: \(error.localizedDescription)")
            return DispatchQueue.main.async { done(false) }
          }
          // Saved: loaded again before it can start (NETunnelProviderManager).
          manager.loadFromPreferences { error in
            DispatchQueue.main.async {
              if let error {
                NSLog("vpn: reload failed: \(error.localizedDescription)")
                return done(false)
              }
              self.use(manager)
              done(true)
            }
          }
        }
      }
    }
  }

  private func use(_ manager: NETunnelProviderManager) {
    self.manager = manager
    publish()
  }

  func onListen(withArguments arguments: Any?, eventSink events: @escaping FlutterEventSink) -> FlutterError? {
    sink = events
    observer = NotificationCenter.default.addObserver(
      forName: .NEVPNStatusDidChange, object: nil, queue: .main
    ) { [weak self] _ in self?.publish() }
    // The configuration saved before this launch, if any: its status.
    NETunnelProviderManager.loadAllFromPreferences { [weak self] managers, _ in
      DispatchQueue.main.async {
        guard let self else { return }
        if self.manager == nil, let saved = managers?.first { self.manager = saved }
        self.publish()
      }
    }
    return nil
  }

  func onCancel(withArguments arguments: Any?) -> FlutterError? {
    if let observer { NotificationCenter.default.removeObserver(observer) }
    observer = nil
    sink = nil
    return nil
  }

  private func publish() {
    guard let sink else { return }
    switch manager?.connection.status ?? .invalid {
    case .connecting, .reasserting: sink("connecting")
    case .connected: sink("on")
    case .disconnecting: sink("disconnecting")
    default: sink("off")
    }
  }
}

#if targetEnvironment(simulator)
  /// flutter test on the simulator (e2e/ios_smoke.sh) learns the app's VM
  /// service address from the simulator's log, through a `log stream` it
  /// spawns while it launches the app, and the engine prints the address
  /// once, about a second after launch. A stream still attaching then never
  /// sees it, and the tool waits until it gives up. The test's own Dart code
  /// cannot help: flutter test's bootstrap runs it only once the tool has
  /// connected. So the app prints the line again every second for its first
  /// minute, in the form the tool looks for, from the Runner executable
  /// (the tool's predicate takes the app's own image or Flutter's; ci-build.sh
  /// turns off the debug dylib for that). The tool takes the first it reads.
  ///
  /// Not `#if DEBUG`: flutter's Runner target does not set that condition
  /// (only its RunnerTests do), so the announcer was compiled out (#484).
  /// A release build has no VM service, and its vmServiceUrl stays nil.
  enum VMServiceAnnouncer {
    private static let log = Logger(subsystem: "com.peakpassvpn.ppvpn", category: "vm-service")

    static func start(_ registrar: FlutterPluginRegistrar) {
      var left = 60
      Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { timer in
        left -= 1
        if left == 0 { timer.invalidate() }
        guard let url = engine(registrar)?.vmServiceUrl else { return }
        log.notice("The Dart VM service is listening on \(url.absoluteString, privacy: .public)")
      }
    }

    private static func engine(_ registrar: FlutterPluginRegistrar) -> FlutterEngine? {
      if let controller = registrar.viewController as? FlutterViewController { return controller.engine }
      let windows = UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }.flatMap(\.windows)
      return windows.lazy.compactMap { $0.rootViewController as? FlutterViewController }.first?.engine
    }
  }
#endif

/// Installed in didFinishLaunching, before Flutter creates its implicit
/// engine. The singleton retains cold-start taps until Dart calls start.
final class PPVPNPush: NSObject, UNUserNotificationCenterDelegate {
  static let shared = PPVPNPush()
  let inbox = PushInbox()

  static func install() {
    UNUserNotificationCenter.current().delegate = shared
  }

  func requestPermission() {
    let center = UNUserNotificationCenter.current()
    center.getNotificationSettings { settings in
      guard settings.authorizationStatus == .notDetermined else { return }
      center.requestAuthorization(options: [.alert, .badge, .sound]) { _, error in
        if let error { NSLog("push: notification permission failed: %@", error.localizedDescription) }
        DispatchQueue.main.async { UIApplication.shared.registerForRemoteNotifications() }
      }
    }
  }

  func userNotificationCenter(
    _ center: UNUserNotificationCenter, willPresent notification: UNNotification,
    withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void
  ) {
    completionHandler([.banner, .list, .sound])
  }

  func userNotificationCenter(
    _ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse,
    withCompletionHandler completionHandler: @escaping () -> Void
  ) {
    if response.actionIdentifier == UNNotificationDefaultActionIdentifier {
      let info = response.notification.request.content.userInfo
      DispatchQueue.main.async { self.inbox.receiveTap(info) }
    }
    completionHandler()
  }
}
