// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import BrookCore
import Foundation
import Synchronization

@testable import Brook

/// A `ChatClient` the conversation tests script. It is its own file so the Mac and iOS test
/// targets, which share the conversation tests, share it too.
final class FakeChat: ChatClient, @unchecked Sendable {
    var pages: [[FfiMessage]] = []

    // ---- This device's local data (OfflineFakes.swift). Off: every call answers
    // `local.unavailable`, as without the Keychain (#79).
    var local = false
    /// `loadHead`/`loadOlder` fail (offline).
    var loadFails = false
    /// Cached pages, handed out in order (the last one repeats).
    var cachePages: [FfiCachedMessages] = []
    /// Holds the next `cachedMessages` call (after it is recorded), once.
    var cacheGate: Gate?
    /// What the cache was asked, in order ("cached:<before>", "loadHead", "loadOlder", …).
    let cacheCalls = Mutex<[String]>([])
    var users: [FfiMember] = []
    /// Pending reads, handed out in order (the last one repeats).
    var pendingReads: [[FfiPendingMessage]] = []
    var queueFailure: Error?
    /// Messages with files: "clientId|body|name:type:transferId,…"; held while `filesGate` is set.
    let queuedFiles = Mutex<[String]>([])
    var filesGate: Gate?
    let cancelled = Mutex<[UInt64]>([])
    // The file cache (FileRowModel tests).
    var openResult: Result<String, Error> = .success("/private/brook/open/abc/report.pdf")
    /// `fileState` answers, handed out in order (the last one repeats); a gate holds one.
    var states: [FfiFileCacheState] = [.notCached]
    var stateGate: Gate?
    var pinFailure: Error?
    var pinGate: Gate?
    var previewGate: Gate?
    let pins = Mutex<[String]>([])
    var previewResult: Result<FfiImagePreview, Error> = .failure(LoginError.Api(code: "file.preview_refused", message: ""))
    let queued = Mutex<[String]>([]) // "clientId|body|reply"
    var unsent: UInt64 = 0
    var lost: UInt64?
    let acknowledged = Mutex<[UInt64]>([])
    var others: [FfiLocalUser] = []
    /// Holds `otherLocalUsers` after it has counted the call.
    var othersGate: Gate?
    let othersAsked = Mutex(0)
    let wiped = Mutex(0)
    let wipeFails = Mutex(false)
    let wipeTried = Mutex(0)
    let sent = Mutex<[String]>([])
    /// The `clientId` of each direct send, in order.
    let sentIds = Mutex<[String?]>([])
    var sendFailure: Error?
    /// The direct send answers with a tombstone (a retry of a message deleted meanwhile).
    var sendAnswersDeleted = false
    var read: [String?] = []

    var historyFailure: Error?
    /// `channelHistory` calls so far, and a gate that holds each one after it is counted.
    let historyCalls = Mutex(0)
    var historyGate: Gate?
    func channelHistory(channelId: String, before: String?) async throws -> [FfiMessage] {
        historyCalls.withLock { $0 += 1 }
        await historyGate?.wait()
        if let historyFailure { throw historyFailure }
        return pages.isEmpty ? [] : pages.removeFirst()
    }
    func sendMessage(channelId: String, body: String, replyToId: String?,
                     clientId: String?) async throws -> FfiMessage {
        sentIds.withLock { $0.append(clientId) }
        sent.withLock { $0.append("\(body)|\(replyToId ?? "-")") }
        if let sendFailure { throw sendFailure }
        return msg("m9", body, channel: channelId, deleted: sendAnswersDeleted)
    }
    func editMessage(channelId: String, messageId: String, body: String) async throws -> FfiMessage {
        sent.withLock { $0.append("edit:\(messageId):\(body)") }
        if let sendFailure { throw sendFailure }
        return msg(messageId, body, channel: channelId)
    }
    func deleteMessage(channelId: String, messageId: String) async throws {}
    func markRead(channelId: String, messageId: String?) async throws { read.append(messageId) }
    let typed = Mutex<[String]>([])
    func sendTyping(channelId: String) async throws { typed.withLock { $0.append(channelId) } }
    var reactionAnswer: [FfiReaction] = []
    var reactionFails = false
    var reactionGate: Gate?
    let toggles = Mutex<[String]>([])
    func toggleReaction(channelId: String, messageId: String, emoji: String) async throws -> [FfiReaction] {
        toggles.withLock { $0.append("\(messageId)|\(emoji)") }
        await reactionGate?.wait()
        if reactionFails { throw LoginError.Timeout }
        return reactionAnswer
    }
    var downloadFailure: Error?
    var downloadBytes = Data("new".utf8)
    func downloadFile(transferId: UInt64, fileId: String, sha256: String, size: UInt64,
                      destination: String) async throws {
        // As core: a failed download leaves nothing at its destination.
        if let downloadFailure { throw downloadFailure }
        try downloadBytes.write(to: URL(fileURLWithPath: destination))
    }
    func cancelTransfer(transferId: UInt64) { cancelled.withLock { $0.append(transferId) } }
    func subscribeTransfers(listener: TransferListener) -> Subscription {
        fatalError("not used by these tests")
    }
}
