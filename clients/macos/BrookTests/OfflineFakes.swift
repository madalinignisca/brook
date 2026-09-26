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

    func subscribeCacheEvents(listener: CacheEventListener) -> Subscription { FakeSubscription() }
    func subscribeCacheState(listener: CacheStateListener) -> Subscription { FakeSubscription() }
}

func cachedPage(_ messages: [FfiMessage], needsNetwork: Bool = false) -> FfiCachedMessages {
    FfiCachedMessages(messages: messages, needsNetwork: needsNetwork)
}
