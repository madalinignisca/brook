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
        /// Stop a message's uploads (Retry resumes them).
        case cancel
    }

    /// Upload progress by transfer id (files of messages on their way).
    private(set) var progress: [UInt64: (done: UInt64, total: UInt64)] = [:]
    private var transfers: Subscription?

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
        case .cancel:
            // Any of its files' transfers cancels the whole message's sending.
            if let first = message.files.first, let chat = client as? any ChatClient {
                chat.cancelTransfer(transferId: first.transferId)
            }
        }
        await reload()
    }

    // ---- Upload progress ----

    func startProgress() {
        guard transfers == nil, let chat = client as? any ChatClient else { return }
        transfers = chat.subscribeTransfers(listener: PendingTransferBridge(self))
    }

    func stopProgress() {
        transfers?.cancel()
        transfers = nil
    }

    func transferred(_ event: FfiTransferEvent) {
        progress[event.transferId] = (event.done, event.total)
    }

    /// A file's line in its bubble.
    func fileLine(_ file: FfiPendingFile) -> String {
        if let code = file.error { return "\(file.filename): \(Self.fileErrorText(code))" }
        if file.uploaded { return "\(file.filename): uploaded" }
        if let p = progress[file.transferId], p.total > 0 {
            return "\(file.filename): \(p.done * 100 / p.total)%"
        }
        return file.filename
    }

    /// A queued file's problem, as GTK's `file_error_text`.
    static func fileErrorText(_ code: String) -> String {
        switch code {
        case "file.too_large": "Too large for the server"
        case "file.quota_exceeded": "Over your storage quota"
        case "file.bad_content_type": "The server refused its type"
        case "outbox.duplicate_file": "The same file is in this message twice"
        case "outbox.snapshot_damaged": "The saved copy is damaged: delete and send it again"
        default: "Not uploaded"
        }
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
        case let .failed(code) where code == "transfer.cancelled":
            "Cancelled"
        case let .failed(code) where code == "outbox.snapshot_damaged":
            "Not sent: a file's saved copy is damaged"
        case let .failed(code) where code.hasPrefix("file.") || code == "outbox.duplicate_file":
            "Not sent: a file was refused"
        case .failed:
            "Not sent"
        }
    }

    static func actions(_ message: FfiPendingMessage) -> [Action] {
        switch message.state {
        case .pending, .sending: message.files.isEmpty ? [] : [.cancel] // uploads can stop
        case .accepted: []
        case let .failed(code) where code == "message.reply_target_gone": [.sendWithoutQuote, .delete]
        case .failed: [.retry, .delete] // a permission can come back
        }
    }
}

/// Upload progress to the pending model, in order, on the main thread.
final class PendingTransferBridge: TransferListener, @unchecked Sendable {
    private weak var model: PendingModel?
    init(_ model: PendingModel) { self.model = model }

    func onTransfer(event: FfiTransferEvent) {
        DispatchQueue.main.async { [weak self] in
            MainActor.assumeIsolated { self?.model?.transferred(event) }
        }
    }

    /// Progress was missed: the bubbles re-read their state.
    func onResync() {
        DispatchQueue.main.async { [weak self] in
            MainActor.assumeIsolated {
                guard let model = self?.model else { return }
                Task { await model.reload() }
            }
        }
    }
}
