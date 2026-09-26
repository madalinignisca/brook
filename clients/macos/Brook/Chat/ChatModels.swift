import BrookCore
import Foundation
import Observation

/// What the chat views need from the client (a fake in tests).
protocol ChatClient: AnyObject, Sendable {
    func channelHistory(channelId: String, before: String?) async throws -> [FfiMessage]
    func sendMessage(channelId: String, body: String, replyToId: String?) async throws -> FfiMessage
    func editMessage(channelId: String, messageId: String, body: String) async throws -> FfiMessage
    func deleteMessage(channelId: String, messageId: String) async throws
    func markRead(channelId: String, messageId: String?) async throws
    func downloadFile(transferId: UInt64, fileId: String, sha256: String, size: UInt64,
                      destination: String) async throws
    func cancelTransfer(transferId: UInt64)
    func subscribeTransfers(listener: TransferListener) -> Subscription
}

extension FfiBrookClient: ChatClient {}

/// One channel's messages, oldest at the top. Keyed by id and kept in id order (ids are
/// UUIDv7: time order). With local data, pages come from this device's cache first (#62),
/// then the network; whatever order pages and live events land in, the merge gives the same
/// result (see `merge`).
@MainActor
@Observable
final class TimelineModel {
    let channelId: String
    private(set) var messages: [FfiMessage] = []
    private(set) var loading = false
    /// No older page: the start of the channel is on screen.
    private(set) var atStart = false
    /// The network's error. Hidden (`visibleError`) while offline with cached messages shown.
    private(set) var error: String?
    /// Set from the cache's state feed: the last sync couldn't reach the server.
    var offline = false
    /// Current names for authors (a `Users` notice): cached rows keep the name they were
    /// stored with.
    private(set) var authorNames: [String: String] = [:]

    /// Ids deleted, including ones not shown yet: every later copy stays a tombstone.
    private var deleted: Set<String> = []
    /// Messages here came from the cache.
    private var fromCache = false

    private let client: any ChatClient
    static let pageSize: UInt32 = 50

    init(channelId: String, client: any ChatClient) {
        self.channelId = channelId
        self.client = client
    }

    private var cache: (any OfflineClient)? { client as? any OfflineClient }

    /// What the view shows: no network error while offline with cached messages on screen.
    var visibleError: String? { offline && fromCache ? nil : error }

    /// The name to show for a message's author.
    func authorName(_ message: FfiMessage) -> String {
        authorNames[message.authorId] ?? message.authorDisplayName ?? message.authorHandle ?? "Someone"
    }

    /// The cached head first (and, when the cache can't vouch for it, a load and a re-read),
    /// then the network's newest page; then mark read.
    func load() async {
        await readCache(before: nil, loadIfIncomplete: true)
        await fetch(before: nil)
        if let newest = messages.last {
            try? await client.markRead(channelId: channelId, messageId: newest.id)
        }
    }

    /// The page before the oldest shown (scrolled to the top): the cache's, loading it when
    /// the cache can't vouch for it; the network's when there's no local data.
    func loadOlder() async {
        guard !atStart, !loading, let oldest = messages.first else { return }
        if await readCache(before: oldest.id, loadIfIncomplete: true) { return }
        await fetch(before: oldest.id)
    }

    /// Re-read the cached head (the cache changed for this channel, or was reset).
    func refill() async {
        await readCache(before: nil, loadIfIncomplete: false)
    }

    /// Current names for these authors, from the cache.
    func refreshAuthors(_ ids: [String]) async {
        guard let users = try? await cache?.cachedUsers(ids: ids) else { return }
        for u in users { authorNames[u.id] = u.displayName }
    }

    /// One cached page, merged: true if the cache answered (so the network isn't needed for
    /// paging). An incomplete page is loaded and read again; an empty, complete page before
    /// `before` is the start of the channel.
    @discardableResult
    private func readCache(before: String?, loadIfIncomplete: Bool) async -> Bool {
        guard let cache else { return false }
        do {
            var page = try await cache.cachedMessages(channelId: channelId, before: before, limit: Self.pageSize)
            if page.needsNetwork, loadIfIncomplete {
                if before == nil {
                    try? await cache.loadHead(channelId: channelId, limit: Self.pageSize)
                } else {
                    try? await cache.loadOlder(channelId: channelId, limit: Self.pageSize)
                }
                page = try await cache.cachedMessages(channelId: channelId, before: before, limit: Self.pageSize)
            }
            if before != nil, page.messages.isEmpty, !page.needsNetwork { atStart = true }
            if !page.messages.isEmpty { fromCache = true }
            merge(page.messages)
            return true
        } catch {
            return false // no local data (yet): the network path
        }
    }

    private func fetch(before: String?) async {
        loading = true
        defer { loading = false }
        do {
            let page = try await client.channelHistory(channelId: channelId, before: before)
            if page.isEmpty, before != nil { atStart = true }
            merge(page)
            error = nil
        } catch {
            self.error = "Couldn't load messages."
        }
    }

    /// A live event, if it's this channel's.
    func apply(_ event: FfiServerEvent) {
        switch event {
        case let .messageNew(message), let .messageUpdate(message):
            guard message.channelId == channelId else { return }
            merge([message])
            if case .messageNew = event {
                Task { try? await client.markRead(channelId: channelId, messageId: message.id) }
            }
        case let .messageDelete(channel, messageId):
            guard channel == channelId else { return }
            deleted.insert(messageId) // even before its page lands
            if let i = messages.firstIndex(where: { $0.id == messageId }) {
                messages[i] = Self.tombstone(messages[i])
            }
        case .resync:
            Task { await fetch(before: nil) }
        case .ready, .channelCall:
            break
        }
    }

    /// Insert or replace by id, keeping id order, so arrival order doesn't matter:
    /// - a deleted id stays deleted (a late copy can't bring it back);
    /// - the body and its edited mark change only for a newer `editedAt` (a missing one is
    ///   older than any), so a stale page never undoes an edit;
    /// - every other field (names, the quoted excerpt, files) takes the incoming copy.
    func merge(_ incoming: [FfiMessage]) {
        guard !incoming.isEmpty else { return }
        var byId = Dictionary(messages.map { ($0.id, $0) }, uniquingKeysWith: { _, new in new })
        for var m in incoming {
            if m.deleted { deleted.insert(m.id) }
            if deleted.contains(m.id) {
                byId[m.id] = Self.tombstone(m)
                continue
            }
            if let old = byId[m.id], !Self.isNewer(m.editedAt, than: old.editedAt) {
                m.body = old.body
                m.editedAt = old.editedAt
            }
            byId[m.id] = m
        }
        messages = byId.values.sorted { $0.id < $1.id }
    }

    /// `a` is a later edit than `b` (nil: never edited, older than any edit). Timestamps are
    /// the server's RFC 3339 UTC strings, which order as text.
    private static func isNewer(_ a: String?, than b: String?) -> Bool {
        switch (a, b) {
        case (nil, _): false
        case (.some, nil): true
        case let (.some(a), .some(b)): a > b
        }
    }

    private static func tombstone(_ m: FfiMessage) -> FfiMessage {
        var gone = m
        gone.body = ""
        gone.deleted = true
        gone.attachments = []
        return gone
    }
}

/// The message box: text, what it replies to, or the message being edited.
@MainActor
@Observable
final class ComposerModel {
    var text = ""
    private(set) var replyingTo: FfiMessage?
    private(set) var editing: FfiMessage?
    private(set) var sending = false
    private(set) var error: String?

    private let channelId: String
    private let client: any ChatClient
    /// Where a sent or edited message goes (the timeline, before the live event arrives).
    private let onMessage: (FfiMessage) -> Void

    init(channelId: String, client: any ChatClient, onMessage: @escaping (FfiMessage) -> Void) {
        self.channelId = channelId
        self.client = client
        self.onMessage = onMessage
    }

    func reply(to message: FfiMessage) {
        editing = nil
        replyingTo = message
    }

    func edit(_ message: FfiMessage) {
        replyingTo = nil
        editing = message
        text = message.body
    }

    func cancel() {
        replyingTo = nil
        if editing != nil { text = "" }
        editing = nil
    }

    /// A message needs text or files (#126), so an edit may clear the caption of a message
    /// that has files, but not empty a text-only one.
    var canSend: Bool {
        guard !sending else { return false }
        let empty = text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        return !empty || editing?.attachments.isEmpty == false
    }

    /// Sends (or saves an edit). The box clears at once; on failure the text and the reply
    /// come back, with why. A network failure may still have delivered it (a direct send
    /// can't be retried safely), so it says so instead of "not sent".
    func send() async {
        guard canSend else { return }
        let (typed, reply, editing) = (text, replyingTo, self.editing)
        // A blank caption goes out as no caption, not as spaces.
        let body = typed.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? "" : typed
        text = ""
        replyingTo = nil
        self.editing = nil
        sending = true
        defer { sending = false }
        do {
            let message: FfiMessage
            if let editing {
                message = try await client.editMessage(channelId: channelId, messageId: editing.id,
                                                       body: body)
            } else {
                message = try await client.sendMessage(channelId: channelId, body: body,
                                                       replyToId: reply?.id)
            }
            error = nil
            onMessage(message)
        } catch {
            if text.isEmpty {  // unless something new was typed meanwhile
                text = typed
                replyingTo = reply
                self.editing = editing
            }
            self.error = Self.explain(error)
        }
    }

    func delete(_ message: FfiMessage) async {
        do {
            try await client.deleteMessage(channelId: channelId, messageId: message.id)
        } catch {
            self.error = "Couldn't delete the message."
        }
    }

    static func explain(_ error: Error) -> String {
        switch error as? LoginError {
        case .Network, .Timeout, .Disconnected:
            "It may not have been sent: check the conversation before sending again."
        case let .Api(code, _) where code == "message.reply_target_gone":
            "The message you replied to was deleted."
        case .NotAuthenticated:
            "You were signed out."
        default:
            "Couldn't send."
        }
    }
}

/// Saving attachments: per file, its progress or its error.
@MainActor
@Observable
final class SaveModel {
    enum State: Equatable {
        case saving(done: UInt64, total: UInt64)
        case saved
        case failed(String)
    }

    private(set) var states: [String: State] = [:]
    private var transfers: [String: UInt64] = [:]  // file id → transfer id
    private let client: any ChatClient
    private var subscription: Subscription?

    init(client: any ChatClient) {
        self.client = client
    }

    func start() {
        guard subscription == nil else { return }
        subscription = client.subscribeTransfers(listener: TransferBridge(self))
    }

    func stop() {
        subscription?.cancel()
        subscription = nil
    }

    func save(_ file: FfiFileInfo, to destination: URL) async {
        guard let sha = file.sha256 else {
            states[file.id] = .failed("This file isn't ready yet.")
            return
        }
        let transfer = UInt64.random(in: 1 ... UInt64.max)
        transfers[file.id] = transfer
        states[file.id] = .saving(done: 0, total: file.size)
        // Into a temporary file first, then moved over the destination: saving over an
        // existing file ("Replace") must not destroy it if the download then fails.
        let temp = FileManager.default.temporaryDirectory
            .appending(path: "brook-save-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: temp) }
        do {
            try await client.downloadFile(transferId: transfer, fileId: file.id, sha256: sha,
                                          size: file.size, destination: temp.path)
            if FileManager.default.fileExists(atPath: destination.path) {
                _ = try FileManager.default.replaceItemAt(destination, withItemAt: temp)
            } else {
                try FileManager.default.moveItem(at: temp, to: destination)
            }
            states[file.id] = .saved
        } catch let LoginError.Api(code, _) where code == "file.gone" {
            states[file.id] = .failed("No longer available.")
        } catch let LoginError.Api(code, _) where code == "transfer.cancelled" {
            states[file.id] = nil
        } catch {
            states[file.id] = .failed("Couldn't save the file.")
        }
        transfers[file.id] = nil
    }

    func cancel(_ file: FfiFileInfo) {
        if let transfer = transfers[file.id] { client.cancelTransfer(transferId: transfer) }
    }

    func progress(_ event: FfiTransferEvent) {
        guard let file = transfers.first(where: { $0.value == event.transferId })?.key,
              case .saving = states[file] else { return }
        states[file] = .saving(done: event.done, total: event.total)
    }
}

/// Delivers transfer progress to the model on the main thread, in order.
final class TransferBridge: TransferListener, @unchecked Sendable {
    private weak var model: SaveModel?
    init(_ model: SaveModel) { self.model = model }

    func onTransfer(event: FfiTransferEvent) {
        DispatchQueue.main.async { [weak self] in
            MainActor.assumeIsolated { self?.model?.progress(event) }
        }
    }

    func onResync() {}  // a save's end state comes from its own call, not from events
}
