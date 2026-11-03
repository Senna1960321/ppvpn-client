// The generated runner's window (flutter create), plus the channel
// lib/activation.dart listens on (#401 step 4). scripts/ci-build.sh copies
// this over the generated file.
//
// A click on a notification comes back as ppvpn://push/<id>, which the push
// agent opens; opening the app again
// (the Dock icon, Finder) comes as a reopen. Both go to Dart; what arrives
// before Dart asks ("ready") is held until then. The application menu's
// Quit (⌘Q) goes to Dart as "quit" too, so that it quits as the tray's Quit
// does: the cores stopped, a silent update handed over (#548); before Dart
// listens it quits at once.
//
// It connects APNs to Dart (AppDelegate.swift), runs Sparkle (Updater
// below, the channel lib/updates.dart calls) and holds launch at login
// (LoginItem below, the channel lib/settings.dart calls).

import Cocoa
import FlutterMacOS
import ServiceManagement
import os

class MainFlutterWindow: NSWindow {
  private var activation: Activation?
  private var updater: Updater?
  private var loginItem: LoginItem?

  override func awakeFromNib() {
    let flutterViewController = FlutterViewController()
    let windowFrame = self.frame
    self.contentViewController = flutterViewController
    self.setFrame(windowFrame, display: true)

    RegisterGeneratedPlugins(registry: flutterViewController)
    activation = Activation(messenger: flutterViewController.engine.binaryMessenger)
    MacPush.shared.attach(messenger: flutterViewController.engine.binaryMessenger)
    updater = Updater(messenger: flutterViewController.engine.binaryMessenger)
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
    NotificationCenter.default.addObserver(
      self, selector: #selector(didFinishLaunching(_:)),
      name: NSApplication.didFinishLaunchingNotification, object: nil)
  }

  /// The menu's terminate: items, to [quit].
  @objc private func didFinishLaunching(_ notification: Notification) {
    for top in NSApp.mainMenu?.items ?? [] {
      for item in top.submenu?.items ?? []
      where item.action == #selector(NSApplication.terminate(_:)) {
        item.target = self
        item.action = #selector(quit(_:))
      }
    }
  }

  private var quitting = false

  /// Once: Dart quits within `quitLimit` (handing over a silent update
  /// waits at most a minute for the service), else the app terminates.
  @objc private func quit(_ sender: Any?) {
    guard ready else {
      NSApp.terminate(sender)
      return
    }
    guard !quitting else { return }
    quitting = true
    channel.invokeMethod("activate", arguments: "quit")
    DispatchQueue.main.asyncAfter(deadline: .now() + Activation.quitLimit) {
      NSApp.terminate(nil)
    }
  }

  private static let quitLimit: TimeInterval = 90

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
/// `setAutoCheck` (a Bool).
@MainActor
private final class Updater: NSObject {
  private let channel: FlutterMethodChannel
  private let feedURL: String?
  private var controller: SparkleUpdaterController?

  init(messenger: FlutterBinaryMessenger) {
    channel = FlutterMethodChannel(name: "ppvpn/update", binaryMessenger: messenger)
    let info = Bundle.main.infoDictionary ?? [:]
    let feed = (info["PPVPNUpdateFeedURL"] as? String)?.trimmingCharacters(in: .whitespaces) ?? ""
    let key = (info["SUPublicEDKey"] as? String)?.trimmingCharacters(in: .whitespaces) ?? ""
    feedURL = feed.isEmpty ? nil : feed
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
        self.controller?.checkForUpdates(nil)
        result(nil)
      case "setAutoCheck":
        if let enabled = call.arguments as? Bool {
          self.controller?.updater.automaticallyChecksForUpdates = enabled
        }
        result(nil)
      default:
        result(FlutterMethodNotImplemented)
      }
    }
  }

  /// SPUStandardUpdaterController from the bundled framework, started.
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
    let controller = type.init(startingUpdater: true, updaterDelegate: delegate, userDriverDelegate: nil)
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
}
