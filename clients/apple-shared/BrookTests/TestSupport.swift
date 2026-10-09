// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

// Test fakes and helpers used by tests in both test targets (Mac and, from step 5, iOS). They
// were cut out of CallModelTests.swift and ChatModelTests.swift, which stay in the Mac target
// because their other contents need AppKit, WebRTC or Mac-only types.

import BrookCore
import Foundation
import Synchronization
import XCTest

@testable import Brook

final class FakeSubscription: Subscription, @unchecked Sendable {
    let cancelled = Mutex(false)
    init() { super.init(noHandle: NoHandle()) }
    required init(unsafeFromHandle _: UInt64) { fatalError("never lifted from Rust") }
    override func cancel() { cancelled.withLock { $0 = true } }
}

/// Records the order of calls; lets a test deliver server events.
final class FakeRealtime: FfiBrookClientProtocol, @unchecked Sendable {
    let order = Mutex<[String]>([])
    let listener = Mutex<ServerEventListener?>(nil)
    /// The subscription last handed out, so a test can see whether it was cancelled.
    let subscription = Mutex<FakeSubscription?>(nil)
    var channels: [FfiChannel]
    /// Successive answers for `listChannels` (empty: always `channels`).
    let readQueue = Mutex<[[FfiChannel]]>([])
    init(channels: [FfiChannel]) { self.channels = channels }
    /// For the offline tests (#62): a failing realtime start or network list, and the cache's
    /// channels (nil: `local.unavailable`).
    var realtimeFails = false
    var listFails = false
    var cached: [FfiCachedChannel]?
    /// Holds the network list until opened (a read that finishes late).
    var listGate: Gate?

    func subscribeEvents(listener: ServerEventListener) -> Subscription {
        order.withLock { $0.append("subscribe") }
        self.listener.withLock { $0 = listener }
        let sub = FakeSubscription()
        subscription.withLock { $0 = sub }
        return sub
    }
    func startRealtime() async throws {
        order.withLock { $0.append("start") }
        if realtimeFails { throw LoginError.Network(message: "offline") }
    }
    func listChannels() async throws -> [FfiChannel] {
        order.withLock { $0.append("list") }
        // Tests that need the list to change between reads queue its snapshots (the last repeats).
        let snapshot = readQueue.withLock { q in q.count > 1 ? q.removeFirst() : (q.first ?? channels) }
        if let listGate { await listGate.wait() }
        if listFails { throw LoginError.Network(message: "offline") }
        return snapshot
    }
    /// nil: joining fails. Set: join waits for the gate, then returns a handle.
    var joinGate: Gate?
    func joinCall(channelId: String, engine: FfiMediaEngine, publish: Bool) async throws -> FfiCallHandle {
        guard let joinGate else { throw LoginError.Disconnected }
        await joinGate.wait()
        return FakeCallHandle()
    }
    func login(handle: String, password: String) async throws -> LoginResult { throw LoginError.Disconnected }
    func subscribe(listener: AuthStateListener) -> Subscription { FakeSubscription() }
    func changePassword(current: String, new: String, signOutOtherDevices: Bool) async throws -> Bool? { nil }
    func logout() async {}
    func enablePersistence(slot: FfiKeySlot, dataDir: String) {}
    func restore() async -> FfiRestoreOutcome { .notSignedIn }
    func signOutComplete() -> Bool { true }
    func authState() -> FfiAuthState { .loggedOut }
    func completeTotp(challenge: FfiTotpChallenge, code: String) async throws -> UInt32? { nil }
    func completeRecovery(challenge: FfiTotpChallenge, recoveryCode: String) async throws -> UInt32? { nil }
    func cancelTotp(challenge: FfiTotpChallenge) async {}
    func me() async throws -> FfiMe { throw LoginError.NotAuthenticated }
    func totpEnroll(password: String) async throws -> FfiTotpEnrollment { throw LoginError.NotAuthenticated }
    func totpActivate(code: String) async throws -> [String] { [] }
    func totpDisable(password: String, factor: FfiSecondFactor) async throws {}
    func totpRegenerateRecoveryCodes(password: String, factor: FfiSecondFactor) async throws -> [String] { [] }
    func adminResetTotp(userId: String, adminPassword: String) async throws {}
    func adminResetPassword(userId: String, adminPassword: String, new: String) async throws {}
    func listUsers() async throws -> [FfiUserSummary] { [] }
    func createUser(handle: String, displayName: String, password: String, adminPassword: String) async throws -> FfiUserSummary {
        FfiUserSummary(id: "", handle: handle, displayName: displayName, globalRole: "member")
    }

    func deliver(_ event: FfiServerEvent) { listener.withLock { $0 }?.onEvent(event: event) }
}

/// A one-shot gate a fake can wait on.
final class Gate: @unchecked Sendable {
    private let state = Mutex<(open: Bool, waiters: [CheckedContinuation<Void, Never>])>((false, []))
    func wait() async {
        await withCheckedContinuation { c in
            let open = state.withLock { s -> Bool in
                if !s.open { s.waiters.append(c) }
                return s.open
            }
            if open { c.resume() }
        }
    }
    func open() {
        let waiters = state.withLock { s -> [CheckedContinuation<Void, Never>] in
            s.open = true
            defer { s.waiters = [] }
            return s.waiters
        }
        waiters.forEach { $0.resume() }
    }
}

/// A call handle as join_call returns it (a class), for CallCenter.
final class FakeCallHandle: FfiCallHandle, @unchecked Sendable {
    init() { super.init(noHandle: NoHandle()) }
    required init(unsafeFromHandle _: UInt64) { fatalError("never lifted from Rust") }
    override func leave() async throws {}
    override func setMedia(audio: Bool, video: Bool) async throws {}
    override func subscribeState(listener: CallStateListener) -> Subscription { FakeSubscription() }
    override func localCandidate(pc: FfiPcKind, candidate: FfiIceCandidate?) {}
    override func engineFailed(message: String) {}
}

func channel(_ id: String, _ name: String) -> FfiChannel {
    FfiChannel(id: id, kind: "public", name: name, archived: false, topic: nil, isPublic: false, unreadMentions: 0, members: [], ownerOffers: [])
}

/// Let main-queue deliveries (the event/state bridges hop through it) run.
@MainActor
func drainMain() async {
    await withCheckedContinuation { c in DispatchQueue.main.async { c.resume() } }
}

/// The chat, offline and transfer calls: not used by the call tests (the Mac tests stopped
/// building when the bindings grew them; they answer "not signed in" or are never called).
extension FakeRealtime {
    private var unused: LoginError { .NotAuthenticated }
    func acknowledgeOutboxLost(n: UInt64) {}
    func cachedChannels() async throws -> [FfiCachedChannel] {
        guard let cached else { throw LoginError.Api(code: "local.unavailable", message: "") }
        return cached
    }
    func cachedMessages(channelId: String, before: String?, limit: UInt32) async throws -> FfiCachedMessages { throw unused }
    func cancelTransfer(transferId: UInt64) {}
    func channelHistory(channelId: String, before: String?) async throws -> [FfiMessage] { throw unused }
    func deleteMessage(channelId: String, messageId: String) async throws { throw unused }
    func deletePending(clientId: String) async throws -> FfiDeleted { throw unused }
    func downloadFile(transferId: UInt64, fileId: String, sha256: String, size: UInt64, destination: String) async throws { throw unused }
    func editMessage(channelId: String, messageId: String, body: String) async throws -> FfiMessage { throw unused }
    func enableLocalData(slot: any FfiKeySlot, dataDir: String) async -> Bool { false }
    func loadHead(channelId: String, limit: UInt32) async throws { throw unused }
    func loadOlder(channelId: String, limit: UInt32) async throws { throw unused }
    func markRead(channelId: String, messageId: String?) async throws { throw unused }
    func noteLocalDataDir(dataDir: String) {}
    func removeMember(channelId: String, userId: String) async throws { throw unused }
    func leaveChannel(channelId: String) async throws { throw unused }
    func sendTyping(channelId: String) async throws { throw unused }
    func searchMessages(query: String) async throws -> [FfiMessage] { throw unused }
    func toggleReaction(channelId: String, messageId: String, emoji: String) async throws -> [FfiReaction] { throw unused }
    func openDm(handle: String) async throws -> FfiChannel { throw unused }
    func createChannel(name: String, topic: String?, isPublic: Bool) async throws -> FfiChannel { throw unused }
    func listPublicChannels() async throws -> [FfiChannel] { throw unused }
    func joinChannel(channelId: String) async throws -> FfiChannel { throw unused }
    func addMember(channelId: String, handle: String) async throws { throw unused }
    func updateChannel(channelId: String, name: String?, topic: String?, archived: Bool?) async throws -> FfiChannel { throw unused }
    func deleteChannel(channelId: String) async throws { throw unused }
    func offerOwnership(channelId: String, handle: String) async throws -> FfiChannel { throw unused }
    func withdrawOwnershipOffer(channelId: String, userId: String) async throws { throw unused }
    func acceptOwnership(channelId: String) async throws -> FfiChannel { throw unused }
    func declineOwnership(channelId: String) async throws { throw unused }
    func updateProfile(displayName: String?, statusText: String?) async throws -> FfiMe { throw unused }
    func otherLocalUsers() async throws -> [FfiLocalUser] { throw unused }
    func outboxLost() -> UInt64? { nil }
    func pendingMessages(channelId: String) async throws -> [FfiPendingMessage] { throw unused }
    func retrySend(clientId: String) async throws { throw unused }
    func retryWithoutReply(clientId: String) async throws { throw unused }
    func sendMessage(channelId: String, body: String, replyToId: String?) async throws -> FfiMessage { throw unused }
    func sendQueued(channelId: String, body: String, replyToId: String?, clientId: String) async throws -> String { throw unused }
    func sendQueuedWithFiles(channelId: String, body: String, replyToId: String?, clientId: String, files: [FfiOutgoingFile]) async throws -> FfiSendReceipt { throw unused }
    func signOutAndForget() async throws { throw unused }
    func subscribeCacheEvents(listener: any CacheEventListener) -> Subscription { fatalError("unused") }
    func subscribeCacheState(listener: any CacheStateListener) -> Subscription { fatalError("unused") }
    func subscribeTransfers(listener: any TransferListener) -> Subscription { fatalError("unused") }
    // The file cache (#149, #155): unused by these tests.
    func cacheFile(transferId: UInt64, fileId: String) async throws { throw unused }
    func openFile(transferId: UInt64, fileId: String) async throws -> String { throw unused }
    func saveCachedFile(fileId: String, destination: String) async throws -> Bool { throw unused }
    func fileState(fileId: String) async throws -> FfiFileCacheState { throw unused }
    func clearOpenCopies() async {}
    func pinFile(fileId: String) async throws { throw unused }
    func unpinFile(fileId: String) async throws { throw unused }
    func pinnedBytes() async throws -> UInt64 { throw unused }
    func previewFile(transferId: UInt64, fileId: String) async throws -> FfiImagePreview { throw unused }
    func cachedUsers(ids: [String]) async throws -> [FfiMember] { throw unused }
    func closeLocalData() async {}
    func unsentCount() async -> UInt64 { 0 }
    func wipeOtherLocalUsers() async throws { throw unused }
}

extension FakeRealtime: OfflineClient {}

func msg(_ id: String, _ body: String, channel: String = "c", deleted: Bool = false) -> FfiMessage {
    FfiMessage(id: id, channelId: channel, authorId: "u", authorHandle: "u", authorDisplayName: "U",
               body: body, createdAt: "2026-09-26T10:00:00Z", clientId: nil, deleted: deleted,
               editedAt: nil, replyToId: nil, replyTo: nil, attachments: [], mentions: [], mentionEveryone: false, reactions: [])
}
