import BrookCore
import Foundation

/// What the models read and queue through this device's local data (a fake in tests). Every
/// call may answer `local.unavailable`: local data off (no Keychain group, #79), this user's
/// stores not open yet, or failed. Callers then take the network path, as GTK does.
protocol OfflineClient: AnyObject, Sendable {
    func cachedChannels() async throws -> [FfiCachedChannel]
    func cachedMessages(channelId: String, before: String?, limit: UInt32) async throws -> FfiCachedMessages
    func loadHead(channelId: String, limit: UInt32) async throws
    func loadOlder(channelId: String, limit: UInt32) async throws
    func cachedUsers(ids: [String]) async throws -> [FfiMember]
    func sendQueued(channelId: String, body: String, replyToId: String?, clientId: String) async throws -> String
    func sendQueuedWithFiles(channelId: String, body: String, replyToId: String?, clientId: String,
                             files: [FfiOutgoingFile]) async throws -> FfiSendReceipt
    func pendingMessages(channelId: String) async throws -> [FfiPendingMessage]
    func retrySend(clientId: String) async throws
    func retryWithoutReply(clientId: String) async throws
    func deletePending(clientId: String) async throws -> FfiDeleted
    func unsentCount() async -> UInt64
    func outboxLost() -> UInt64?
    func acknowledgeOutboxLost(n: UInt64)
    func otherLocalUsers() async throws -> [FfiLocalUser]
    func wipeOtherLocalUsers() async throws
    // The file cache (#149, #155): Open, keep available offline, previews.
    func openFile(transferId: UInt64, fileId: String) async throws -> String
    func fileState(fileId: String) async throws -> FfiFileCacheState
    func pinFile(fileId: String) async throws
    func unpinFile(fileId: String) async throws
    func previewFile(transferId: UInt64, fileId: String) async throws -> FfiImagePreview
    func subscribeCacheEvents(listener: CacheEventListener) -> Subscription
    func subscribeCacheState(listener: CacheStateListener) -> Subscription
}

extension FfiBrookClient: OfflineClient {}

extension Error {
    /// Core's "no local data here (yet)": take the network path instead.
    var isLocalUnavailable: Bool {
        if case let .Api(code, _) = self as? LoginError, code == "local.unavailable" { return true }
        return false
    }
}

/// A row in the channel list, from the network (`FfiChannel`) or this device's cache
/// (`FfiCachedChannel`, with its locally computed unread count).
struct ChannelRow: Identifiable, Equatable {
    let id: String
    let kind: String
    let name: String?
    let archived: Bool
    var unread: Int64
    let members: [FfiMember]
    let ownerOffers: [FfiOwnerOffer]

    init(_ channel: FfiChannel, unread: Int64 = 0) {
        (id, kind, name, archived) = (channel.id, channel.kind, channel.name, channel.archived)
        self.unread = unread
        members = channel.members
        ownerOffers = channel.ownerOffers
    }

    init(_ channel: FfiCachedChannel) {
        (id, kind, name, archived) = (channel.id, channel.kind, channel.name, channel.archived)
        unread = channel.unreadCount
        members = channel.members
        ownerOffers = channel.ownerOffers
    }

    init(id: String, kind: String = "public", name: String?, archived: Bool = false, unread: Int64 = 0,
         members: [FfiMember] = [], ownerOffers: [FfiOwnerOffer] = []) {
        (self.id, self.kind, self.name, self.archived, self.unread) = (id, kind, name, archived, unread)
        self.members = members
        self.ownerOffers = ownerOffers
    }
}
