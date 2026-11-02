// The generated runner's window (flutter create), plus the channel
// lib/activation.dart listens on (#401 step 4). scripts/ci-build.sh copies
// this over the generated file.
//
// A click on a notification comes back as ppvpn://push/<id>, which the push
// agent opens; opening the app again
// (the Dock icon, Finder) comes as a reopen. Both go to Dart; what arrives
// before Dart asks ("ready") is held until then.
//
// It also keeps the bundled push agent registered (PushAgentService below),
// in a packaged app (scripts/package-macos.sh), runs Sparkle (Updater
// below, the channel lib/updates.dart calls) and holds launch at login
// (LoginItem below, the channel lib/settings.dart calls).

import Cocoa
import FlutterMacOS
import ServiceManagement
import os

class MainFlutterWindow: NSWindow {
  private var activation: Activation?
  private var pushAgent: PushAgentService?
  private var updater: Updater?
  private var loginItem: LoginItem?

  override func awakeFromNib() {
    let flutterViewController = FlutterViewController()
    let windowFrame = self.frame
    self.contentViewController = flutterViewController
    self.setFrame(windowFrame, display: true)

    RegisterGeneratedPlugins(registry: flutterViewController)
    activation = Activation(messenger: flutterViewController.engine.binaryMessenger)
    let pushAgent = PushAgentService()
    pushAgent.start()
    self.pushAgent = pushAgent
    updater = Updater(messenger: flutterViewController.engine.binaryMessenger) { [weak pushAgent] in
      pushAgent?.prepareForUpdate()
    }
    loginItem = LoginItem(messenger: flutterViewController.engine.binaryMessenger)

    super.awakeFromNib()
  }
}

private final class Activation: NSObject {
  private let channel: FlutterMethodChannel
  private var pending: [String] = []
  private var ready = false

  init(messenger: FlutterBinaryMessenger) {
    channel = FlutterMethodChannel(name: "ppvpn/activation", binaryMessenger: messenger)
    super.init()
    channel.setMethodCallHandler { [weak self] call, result in
      guard let self, call.method == "ready" else {
        result(FlutterMethodNotImplemented)
        return
      }
      self.ready = true
      result(self.pending)
      self.pending = []
    }
    // The window loads before the app finishes launching. Apple event
    // handlers set at will-finish replace AppKit's and come before the
    // launch's own URL.
    NotificationCenter.default.addObserver(
      self, selector: #selector(willFinishLaunching(_:)),
      name: NSApplication.willFinishLaunchingNotification, object: nil)
  }

  @objc private func willFinishLaunching(_ notification: Notification) {
    let events = NSAppleEventManager.shared()
    events.setEventHandler(
      self, andSelector: #selector(getURL(_:withReplyEvent:)),
      forEventClass: AEEventClass(kInternetEventClass), andEventID: AEEventID(kAEGetURL))
    events.setEventHandler(
      self, andSelector: #selector(reopen(_:withReplyEvent:)),
      forEventClass: AEEventClass(kCoreEventClass), andEventID: AEEventID(kAEReopenApplication))
  }

  @objc private func getURL(_ event: NSAppleEventDescriptor, withReplyEvent reply: NSAppleEventDescriptor) {
    if let url = event.paramDescriptor(forKeyword: AEKeyword(keyDirectObject))?.stringValue {
      send(url)
    }
  }

  @objc private func reopen(_ event: NSAppleEventDescriptor, withReplyEvent reply: NSAppleEventDescriptor) {
    send("show")
  }

  private func send(_ item: String) {
    if ready {
      channel.invokeMethod("activate", arguments: item)
    } else {
      pending.append(item)
    }
  }
}

private let agentLog = Logger(subsystem: "com.peakpassvpn.ppvpn.desktop", category: "push-agent")

/// Keeps the bundled push agent (Contents/Library/LoginItems/PPVPN Agent.app)
/// registered with launchd, and restarts or re-registers it when it stopped
/// running, with the files, defaults key and timings the former native app
/// used, so that an update from it takes over its registration, and the
/// unregistering before an update (Updater).
@MainActor
private final class PushAgentService {
  static let label = "com.peakpassvpn.ppvpn.desktop.agent"
  static let agentBundleID = "com.peakpassvpn.ppvpn.desktop.agent"
  private static let registeredBuildKey = "pushAgentRegisteredBuild"
  /// Touched by the agent every 60 s (crates/desktop push_agent.rs).
  private static let heartbeatFile = "push-agent.heartbeat"
  private static let healFile = "push-agent.heal"
  /// Three missed heartbeats.
  private static let staleAfter: TimeInterval = 180
  private static let healInterval: TimeInterval = 24 * 60 * 60

  private struct HealRecord: Codable {
    var version: String
    var at: Date
  }

  private let service = SMAppService.agent(plistName: "\(label).plist")
  /// The data directory the agent shares (rust/src/dirs.rs).
  private let dataDirectory = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
    .appendingPathComponent("PPVPN", isDirectory: true)
  /// What a heal is rate-limited by: the build.
  private let version = Bundle.main.object(forInfoDictionaryKey: "CFBundleVersion") as? String ?? "0"
  private var watchingSince = Date()
  private var checkTask: Task<Void, Never>?
  private var wakeObserver: NSObjectProtocol?

  /// Registers the agent (if needed) and starts watching its heartbeat.
  func start() {
    // launchd would point at wherever this copy is; only an installed app
    // may own the agent (notifications also need an Applications folder).
    // A build without the agent (scripts/ci-build.sh alone) has none to own.
    guard Self.isInstalled,
      FileManager.default.fileExists(
        atPath: Bundle.main.bundleURL.appendingPathComponent("Contents/Library/LaunchAgents/\(Self.label).plist").path)
    else {
      agentLog.info("push agent: not registering from \(Bundle.main.bundlePath, privacy: .public)")
      return
    }
    register()
    scheduleCheck()
    wakeObserver = NSWorkspace.shared.notificationCenter.addObserver(
      forName: NSWorkspace.didWakeNotification, object: nil, queue: .main
    ) { [weak self] _ in
      MainActor.assumeIsolated {
        self?.watchingSince = Date()
        self?.scheduleCheck()
      }
    }
  }

  /// Called right before Sparkle replaces the app. launchd kills an agent
  /// whose bundle is replaced under it (Launch Constraint Violation) and
  /// keeps the job half-dead for about a minute; unregistering first lets
  /// the agent exit cleanly, and the new build registers it at launch.
  /// If the install does not happen after all, the agent is registered
  /// again.
  func prepareForUpdate() {
    guard service.status == .enabled else { return }
    checkTask?.cancel()
    do {
      try service.unregister()
      UserDefaults.standard.removeObject(forKey: Self.registeredBuildKey)
      agentLog.info("push agent: unregistered for the update")
    } catch {
      agentLog.error("push agent: unregister before the update failed: \(error.localizedDescription, privacy: .public)")
      return
    }
    Task { [weak self] in
      try? await Task.sleep(for: .seconds(120))
      guard let self else { return }
      agentLog.info("push agent: still running after the update started; registering again")
      self.register()
      self.scheduleCheck()
    }
  }

  private static var isInstalled: Bool {
    let path = Bundle.main.bundlePath
    let home = FileManager.default.homeDirectoryForCurrentUser.path
    return path.hasPrefix("/Applications/") || path.hasPrefix(home + "/Applications/")
  }

  private func register() {
    switch service.status {
    case .enabled:
      // Builds signed without a Team ID get a new code requirement each:
      // launchd refuses the updated agent (OS_REASON_CODESIGNING) until it
      // is registered again, so re-register once per build.
      if UserDefaults.standard.string(forKey: Self.registeredBuildKey) != version {
        reregisterNewBuild()
      }
      return
    case .requiresApproval:
      // Switched off by the user in System Settings → Login Items.
      agentLog.info("push agent: disabled in Login Items")
      return
    case .notRegistered, .notFound:
      break
    @unknown default:
      break
    }
    do {
      try service.register()
      UserDefaults.standard.set(version, forKey: Self.registeredBuildKey)
      agentLog.info("push agent: registered, status \(self.service.status.rawValue)")
    } catch {
      agentLog.error("push agent: register failed: \(error.localizedDescription, privacy: .public)")
    }
  }

  /// Checks a full window after launch or wake, then every five minutes.
  private func scheduleCheck() {
    checkTask?.cancel()
    checkTask = Task { [weak self] in
      try? await Task.sleep(for: .seconds(Self.staleAfter + 20))
      while !Task.isCancelled {
        self?.check()
        try? await Task.sleep(for: .seconds(5 * 60))
      }
    }
  }

  private func check() {
    guard service.status == .enabled else { return }
    let heartbeat = heartbeatDate()
    guard isDown(heartbeat: heartbeat, now: Date(), agentRunning: Self.agentRunning) else { return }
    // A clean exit (SIGTERM) is not restarted by KeepAlive: try a plain
    // restart first, then re-register (rate-limited) if that fails.
    agentLog.info("push agent: not running; restarting")
    Self.kickstart()
    Task { [weak self] in
      try? await Task.sleep(for: .seconds(10))
      guard let self, !Self.agentRunning else { return }
      let now = Date()
      guard self.shouldHeal(heartbeat: heartbeat, now: now) else { return }
      agentLog.info("push agent: restart failed; re-registering")
      self.writeHeal(HealRecord(version: self.version, at: now))
      self.reregister()
    }
  }

  /// Not running and its heartbeats missed for a full window since launch
  /// or wake: worth a plain restart (not rate-limited) before any heal. A
  /// stale heartbeat alone is only a hint (the Mac may have slept).
  private func isDown(heartbeat: Date?, now: Date, agentRunning: Bool) -> Bool {
    guard now.timeIntervalSince(watchingSince) >= Self.staleAfter else { return false }
    if let heartbeat, now.timeIntervalSince(heartbeat) < Self.staleAfter { return false }
    return !agentRunning
  }

  /// Down after a restart, and no heal of this build in the last 24 h.
  private func shouldHeal(heartbeat: Date?, now: Date) -> Bool {
    guard isDown(heartbeat: heartbeat, now: now, agentRunning: false) else { return false }
    guard let last = readHeal(), last.version == version else { return true }
    return now.timeIntervalSince(last.at) >= Self.healInterval
  }

  private func heartbeatDate() -> Date? {
    let path = dataDirectory.appendingPathComponent(Self.heartbeatFile).path
    return (try? FileManager.default.attributesOfItem(atPath: path))?[.modificationDate] as? Date
  }

  private func readHeal() -> HealRecord? {
    guard let data = try? Data(contentsOf: dataDirectory.appendingPathComponent(Self.healFile)) else {
      return nil
    }
    let decoder = JSONDecoder()
    decoder.dateDecodingStrategy = .iso8601
    return try? decoder.decode(HealRecord.self, from: data)
  }

  private func writeHeal(_ record: HealRecord) {
    let encoder = JSONEncoder()
    encoder.dateEncodingStrategy = .iso8601
    guard let data = try? encoder.encode(record) else { return }
    try? data.write(to: dataDirectory.appendingPathComponent(Self.healFile), options: .atomic)
  }

  private static var agentRunning: Bool {
    !NSRunningApplication.runningApplications(withBundleIdentifier: agentBundleID).isEmpty
  }

  private static func kickstart() {
    let process = Process()
    process.executableURL = URL(fileURLWithPath: "/bin/launchctl")
    process.arguments = ["kickstart", "gui/\(getuid())/\(label)"]
    do {
      try process.run()
    } catch {
      agentLog.error("push agent: kickstart failed: \(error.localizedDescription, privacy: .public)")
    }
  }

  /// Right at launch a re-registration still gets the old code requirement
  /// (the system picks the new bundle up a little later), so wait before
  /// re-registering and try again (for up to ~6 minutes) until the agent
  /// runs. The previous build's agent keeps working meanwhile. These
  /// attempts do not count against the heal limit.
  private func reregisterNewBuild() {
    agentLog.info("push agent: new build \(self.version, privacy: .public); re-registering shortly")
    Task { [weak self] in
      for delay in [30, 60, 60, 90, 120] {
        try? await Task.sleep(for: .seconds(delay))
        guard let self else { return }
        await self.reregisterAndWait()
        if Self.agentRunning {
          UserDefaults.standard.set(self.version, forKey: Self.registeredBuildKey)
          agentLog.info("push agent: running for build \(self.version, privacy: .public)")
          return
        }
      }
      agentLog.error("push agent: not running after re-registering build \(self?.version ?? "?", privacy: .public)")
    }
  }

  private func reregisterAndWait() async {
    do {
      try await service.unregister()
    } catch {
      agentLog.error("push agent: unregister failed: \(error.localizedDescription, privacy: .public)")
    }
    do {
      try service.register()
    } catch {
      agentLog.error("push agent: register failed: \(error.localizedDescription, privacy: .public)")
    }
    try? await Task.sleep(for: .seconds(10))
  }

  /// Unregisters, waits until launchd has removed the job, then registers
  /// again: registering while the old job is still being torn down keeps
  /// its stale code requirement and the agent fails with EX_CONFIG.
  private func reregister() {
    Task {
      do {
        try await service.unregister()
      } catch {
        agentLog.error("push agent: unregister failed: \(error.localizedDescription, privacy: .public)")
      }
      do {
        try service.register()
        agentLog.info("push agent: re-registered, status \(self.service.status.rawValue)")
      } catch {
        agentLog.error("push agent: register failed: \(error.localizedDescription, privacy: .public)")
      }
    }
  }
}

/// Launch at login through `SMAppService.mainApp`: the system holds the state,
/// read back on each `state`.
///
/// Channel ppvpn/login-item: `state` → {enabled, needsApproval}, `set` (a
/// Bool; a FlutterError with the system's description when it refused),
/// `openSettings` (System Settings › General › Login Items).
@MainActor
private final class LoginItem {
  private let channel: FlutterMethodChannel

  init(messenger: FlutterBinaryMessenger) {
    channel = FlutterMethodChannel(name: "ppvpn/login-item", binaryMessenger: messenger)
    channel.setMethodCallHandler { call, result in
      switch call.method {
      case "state":
        let status = SMAppService.mainApp.status
        result(["enabled": status == .enabled, "needsApproval": status == .requiresApproval])
      case "set":
        guard let enabled = call.arguments as? Bool else {
          return result(FlutterError(code: "arguments", message: "set takes a Bool", details: nil))
        }
        do {
          if enabled {
            try SMAppService.mainApp.register()
          } else {
            try SMAppService.mainApp.unregister()
          }
          result(nil)
        } catch {
          result(FlutterError(code: "failed", message: error.localizedDescription, details: nil))
        }
      case "openSettings":
        SMAppService.openSystemSettingsLoginItems()
        result(nil)
      default:
        result(FlutterMethodNotImplemented)
      }
    }
  }
}

private let updateLog = Logger(subsystem: "com.peakpassvpn.ppvpn.desktop", category: "update")

/// Sparkle 2: the appcast and the EdDSA key are the Info.plist keys
/// PPVPNUpdateFeedURL and SUPublicEDKey, which scripts/package-macos.sh
/// writes, and it adds Sparkle.framework to Contents/Frameworks.
/// The generated runner does not link Sparkle: the framework is loaded at
/// run time, and a build without it or without both keys (ci-build.sh
/// alone) has updates off, with no 「检查更新…」.
///
/// Channel ppvpn/update: `state` → {available, autoCheck}, `check`,
/// `setAutoCheck` (a Bool), `setPromptChecks` (temporary scheduling),
/// `isInstalling`, `prepareForUpdate` (accepted service handoff).
@MainActor
private final class Updater: NSObject {
  private let channel: FlutterMethodChannel
  private let feedURL: String?
  private let beforeInstall: @MainActor () -> Void
  private var controller: SparkleUpdaterController?
  private var started = false
  private var installing = false

  init(messenger: FlutterBinaryMessenger, beforeInstall: @escaping @MainActor () -> Void) {
    channel = FlutterMethodChannel(name: "ppvpn/update", binaryMessenger: messenger)
    let info = Bundle.main.infoDictionary ?? [:]
    let feed = (info["PPVPNUpdateFeedURL"] as? String)?.trimmingCharacters(in: .whitespaces) ?? ""
    let key = (info["SUPublicEDKey"] as? String)?.trimmingCharacters(in: .whitespaces) ?? ""
    feedURL = feed.isEmpty ? nil : feed
    self.beforeInstall = beforeInstall
    super.init()
    if feedURL != nil, !key.isEmpty {
      controller = Self.makeController(delegate: self)
    } else {
      updateLog.info("updates off: no feed or no public key in this build")
    }
    channel.setMethodCallHandler { [weak self] call, result in
      guard let self else { return result(nil) }
      switch call.method {
      case "state":
        result(["available": self.controller != nil,
                "autoCheck": self.controller?.updater.automaticallyChecksForUpdates ?? false])
      case "check":
        self.startUpdater()
        self.controller?.checkForUpdates(nil)
        result(nil)
      case "setAutoCheck", "setPromptChecks":
        if let enabled = call.arguments as? Bool {
          self.controller?.updater.automaticallyChecksForUpdates = enabled
        }
        self.startUpdater()
        result(nil)
      case "isInstalling":
        result(self.installing)
      case "prepareForUpdate":
        self.beforeInstall()
        result(nil)
      default:
        result(FlutterMethodNotImplemented)
      }
    }
  }

  private func startUpdater() {
    guard !started, let controller else { return }
    started = controller.updater.startUpdater(nil)
  }

  /// Created without scheduling yet: Dart first migrates the user's choice,
  /// then selects silent service updates or the existing prompt fallback.
  private static func makeController(delegate: AnyObject) -> SparkleUpdaterController? {
    guard let url = Bundle.main.privateFrameworksURL?.appendingPathComponent("Sparkle.framework"),
      let bundle = Bundle(url: url), bundle.load(),
      let controllerClass = NSClassFromString("SPUStandardUpdaterController"),
      let updaterClass = NSClassFromString("SPUUpdater")
    else {
      updateLog.info("updates off: no Sparkle.framework in this build")
      return nil
    }
    // Sparkle's classes have these methods (SPUStandardUpdaterController.h,
    // SPUUpdater.h); the protocols only let Swift call them.
    class_addProtocol(controllerClass, SparkleUpdaterController.self)
    class_addProtocol(updaterClass, SparkleUpdater.self)
    guard let type = controllerClass as? SparkleUpdaterController.Type else { return nil }
    let controller = type.init(startingUpdater: false, updaterDelegate: delegate, userDriverDelegate: nil)
    updateLog.info("updates on")
    return controller
  }

  // SPUUpdaterDelegate, which Sparkle asks with respondsToSelector:.

  @objc(feedURLStringForUpdater:)
  nonisolated func feedURLString(for updater: AnyObject) -> String? {
    MainActor.assumeIsolated { feedURL }
  }

  @objc(updater:willInstallUpdate:)
  nonisolated func updater(_ updater: AnyObject, willInstallUpdate item: AnyObject) {
    MainActor.assumeIsolated {
      updateLog.info("update: installing")
      installing = true
      beforeInstall()
    }
  }
}

/// What the runner calls of SPUStandardUpdaterController.
@objc private protocol SparkleUpdaterController: NSObjectProtocol {
  init(startingUpdater: Bool, updaterDelegate: AnyObject?, userDriverDelegate: AnyObject?)
  var updater: SparkleUpdater { get }
  func checkForUpdates(_ sender: Any?)
}

/// What the runner calls of SPUUpdater.
@objc private protocol SparkleUpdater: NSObjectProtocol {
  var automaticallyChecksForUpdates: Bool { get set }
  func startUpdater(_ error: AutoreleasingUnsafeMutablePointer<NSError?>?) -> Bool
}
