// APNs events can precede both the Flutter engine and Dart's channel handler.
// Accessed on the main queue by PPVPNPush. Kept separate from UIKit so the
// cold-start/rotation contract can be tested without APNs credentials.
import Foundation

final class PushInbox {
  private var token: String?
  private var pending: [String: String]?
  private var deliver: ((String, [String: String]) -> Void)?

  func start(deliver: @escaping (String, [String: String]) -> Void) {
    self.deliver = deliver
    if let token { deliver("token", ["token": token]) }
    if let pending {
      self.pending = nil
      deliver("opened", pending)
    }
  }

  func receiveToken(_ token: String) {
    self.token = token
    deliver?("token", ["token": token])
  }

  func receiveTap(_ info: [AnyHashable: Any]) {
    let extra = info["extra"] as? [AnyHashable: Any] ?? info
    var payload: [String: String] = [:]
    // Only an identifier crosses into Dart. It fetches the current account's
    // message; a push's body / URL never becomes trusted inbox content.
    if let id = extra["message_id"] as? String { payload["message_id"] = id }
    else if let id = extra["message_id"] as? NSNumber { payload["message_id"] = id.stringValue }
    if let deliver { deliver("opened", payload) }
    else { pending = payload }
  }
}
