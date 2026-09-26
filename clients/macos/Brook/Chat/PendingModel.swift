import BrookCore
import Foundation
import Observation

/// A channel's messages that haven't gone out yet (#62 spec item 2), shown as bubbles below
/// the history. Texts and actions follow GTK's `pending_text`.
@MainActor
@Observable
final class PendingModel {
    enum Action: Hashable {
        case retry
        case sendWithoutQuote
        case delete
    }

    let channelId: String
    /// The newest `pendingMessages` read.
    private(set) var all: [FfiPendingMessage] = []
    /// The open timeline: a bubble whose message is already there isn't shown.
    weak var timeline: TimelineModel?

    private let client: any OfflineClient
    /// Each read's number: only the newest one started applies, whenever it finishes.
    private var generation = 0

    init(channelId: String, client: any OfflineClient) {
        self.channelId = channelId
        self.client = client
    }

    /// The bubbles to show: never one whose message has arrived (a later read may still list
    /// it as accepted until the cache catches up).
    var visible: [FfiPendingMessage] {
        let shown = Set(timeline?.messages.compactMap(\.clientId) ?? [])
        return all.filter { !shown.contains($0.clientId) }
    }

    func reload() async {
        generation += 1
        let mine = generation
        guard let list = try? await client.pendingMessages(channelId: channelId), mine == generation else {
            return
        }
        all = list
    }

    func perform(_ action: Action, on message: FfiPendingMessage) async {
        switch action {
        case .retry: try? await client.retrySend(clientId: message.clientId)
        case .sendWithoutQuote: try? await client.retryWithoutReply(clientId: message.clientId)
        case .delete: _ = try? await client.deletePending(clientId: message.clientId)
        }
        await reload()
    }

    /// The codes that mean this user can't post in the channel (any more).
    static let cantPost: Set<String> = ["not_found", "authz.forbidden", "http_403", "http_404"]

    static func text(_ message: FfiPendingMessage) -> String {
        switch message.state {
        case .pending, .sending, .accepted: // accepted: sent, the cache hasn't caught up
            "Sending…"
        case let .failed(code) where code == "message.reply_target_gone":
            "Not sent: the quoted message was deleted"
        case let .failed(code) where cantPost.contains(code):
            "Not sent: you can't post here any more"
        case .failed:
            "Not sent"
        }
    }

    static func actions(_ message: FfiPendingMessage) -> [Action] {
        switch message.state {
        case .pending, .sending, .accepted: []
        case let .failed(code) where code == "message.reply_target_gone": [.sendWithoutQuote, .delete]
        case .failed: [.retry, .delete] // a permission can come back
        }
    }
}
