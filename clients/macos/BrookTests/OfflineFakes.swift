import BrookCore
import Foundation

@testable import Brook

let localUnavailable = LoginError.Api(code: "local.unavailable", message: "")

extension FakeChat: OfflineClient {
    private func need() throws { if !local { throw localUnavailable } }
    private func record(_ call: String) { cacheCalls.withLock { $0.append(call) } }

    func cachedChannels() async throws -> [FfiCachedChannel] {
        try need()
        record("channels")
        return []
    }

    func cachedMessages(channelId: String, before: String?, limit: UInt32) async throws -> FfiCachedMessages {
        try need()
        record("cached:\(before ?? "-")")
        guard !cachePages.isEmpty else { return FfiCachedMessages(messages: [], needsNetwork: false) }
        return cachePages.count > 1 ? cachePages.removeFirst() : cachePages[0]
    }

    func loadHead(channelId: String, limit: UInt32) async throws {
        try need()
        record("loadHead")
        if loadFails { throw LoginError.Network(message: "offline") }
    }
    func loadOlder(channelId: String, limit: UInt32) async throws {
        try need()
        record("loadOlder")
        if loadFails { throw LoginError.Network(message: "offline") }
    }

    func cachedUsers(ids: [String]) async throws -> [FfiMember] {
        try need()
        return users.filter { ids.contains($0.id) }
    }

    func sendQueued(channelId: String, body: String, replyToId: String?, clientId: String) async throws -> String {
        try need()
        queued.withLock { $0.append("\(clientId)|\(body)|\(replyToId ?? "-")") }
        if let queueFailure { throw queueFailure }
        return clientId
    }

    func sendQueuedWithFiles(channelId: String, body: String, replyToId: String?, clientId: String,
                             files: [FfiOutgoingFile]) async throws -> FfiSendReceipt {
        try need()
        if let filesGate { await filesGate.wait() }
        let list = files.map { "\($0.filename):\($0.contentType):\($0.transferId ?? 0)" }.joined(separator: ",")
        queuedFiles.withLock { $0.append("\(clientId)|\(body)|\(list)") }
        if let queueFailure { throw queueFailure }
        return FfiSendReceipt(clientId: clientId, files: [])
    }

    func pendingMessages(channelId: String) async throws -> [FfiPendingMessage] {
        try need()
        record("pending")
        guard !pendingReads.isEmpty else { return [] }
        return pendingReads.count > 1 ? pendingReads.removeFirst() : pendingReads[0]
    }

    func retrySend(clientId: String) async throws { try need(); record("retry:\(clientId)") }
    func retryWithoutReply(clientId: String) async throws { try need(); record("noquote:\(clientId)") }
    func deletePending(clientId: String) async throws -> FfiDeleted {
        try need()
        record("delete:\(clientId)")
        return .removed
    }

    func unsentCount() async -> UInt64 { local ? unsent : 0 }
    func outboxLost() -> UInt64? { lost }
    func acknowledgeOutboxLost(n: UInt64) {
        acknowledged.withLock { $0.append(n) }
        if lost == n { lost = nil }
    }

    func otherLocalUsers() async throws -> [FfiLocalUser] {
        try need()
        return others
    }

    func wipeOtherLocalUsers() async throws {
        try need()
        wiped.withLock { $0 += 1 }
        others = []
    }

    func openFile(transferId: UInt64, fileId: String) async throws -> String {
        record("open") // before the local check: a call without local data shows too
        try need()
        return try openResult.get()
    }

    func fileState(fileId: String) async throws -> FfiFileCacheState {
        try need()
        let answer = states.count > 1 ? states.removeFirst() : states[0]
        if let gate = stateGate {
            stateGate = nil
            await gate.wait()
        }
        return answer
    }

    func pinFile(fileId: String) async throws {
        try need()
        pins.withLock { $0.append("pin") }
        if let pinGate { await pinGate.wait() }
        if let pinFailure { throw pinFailure }
    }

    func unpinFile(fileId: String) async throws {
        try need()
        pins.withLock { $0.append("unpin") }
        if let pinFailure { throw pinFailure }
    }

    func previewFile(transferId: UInt64, fileId: String) async throws -> FfiImagePreview {
        try need()
        record("preview")
        return try previewResult.get()
    }

    func subscribeCacheEvents(listener: CacheEventListener) -> Subscription { FakeSubscription() }
    func subscribeCacheState(listener: CacheStateListener) -> Subscription { FakeSubscription() }
}

func cachedPage(_ messages: [FfiMessage], needsNetwork: Bool = false) -> FfiCachedMessages {
    FfiCachedMessages(messages: messages, needsNetwork: needsNetwork)
}
