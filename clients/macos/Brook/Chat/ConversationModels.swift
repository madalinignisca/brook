import BrookCore
import Foundation
import Observation

/// Starting conversations and managing channels (spec 2026-09-30-mac-conversations).
protocol ConversationClient: AnyObject, Sendable {
    func openDm(handle: String) async throws -> FfiChannel
    func createChannel(name: String, topic: String?, isPublic: Bool) async throws -> FfiChannel
    func listPublicChannels() async throws -> [FfiChannel]
    func joinChannel(channelId: String) async throws -> FfiChannel
    func addMember(channelId: String, handle: String) async throws
    func updateChannel(channelId: String, name: String?, topic: String?, archived: Bool?) async throws -> FfiChannel
    func deleteChannel(channelId: String) async throws
}

extension FfiBrookClient: ConversationClient {}

enum Handle {
    /// What was typed, as a handle: trimmed, without a leading `@`.
    static func clean(_ typed: String) -> String {
        var s = typed.trimmingCharacters(in: .whitespacesAndNewlines)
        while s.hasPrefix("@") { s.removeFirst() }
        return s
    }

    /// The server answers an unknown handle with `validation.error` (422). `not_found` is kept for
    /// a server that might say it, though Add Member treats it as "already gone" first.
    static func unknown(_ error: Error) -> Bool {
        guard case let .Api(code, _)? = error as? LoginError else { return false }
        return code == "validation.error" || code == "not_found"
    }

    static let noOne = "No one has that handle."
}

/// "New Message…": a handle, and the direct message with them.
@MainActor
@Observable
final class StartConversationModel {
    var handle = ""
    private(set) var busy = false
    private(set) var error: String?
    /// The direct message that was opened (or found): select it.
    private(set) var opened: FfiChannel?

    private let me: String
    private let client: any ConversationClient

    init(myHandle: String, client: any ConversationClient) {
        me = myHandle
        self.client = client
    }

    /// Why Start can't be pressed yet, or nil.
    var problem: String? {
        let h = Handle.clean(handle)
        if h.isEmpty { return "Enter their handle." }
        if h.caseInsensitiveCompare(me) == .orderedSame { return "That's you." }
        return nil
    }

    var canSubmit: Bool { problem == nil && !busy && opened == nil }

    func submit() async {
        guard canSubmit else { return }
        busy = true
        error = nil
        defer { busy = false }
        do {
            opened = try await client.openDm(handle: Handle.clean(handle))
        } catch {
            self.error = Handle.unknown(error) ? Handle.noOne : "Couldn't start the conversation. Try again."
        }
    }
}

/// "New Channel…" (global admins).
@MainActor
@Observable
final class NewChannelModel {
    static let nameLimit = 128
    static let topicLimit = 512

    var name = ""
    var topic = ""
    var isPublic = false
    private(set) var busy = false
    private(set) var error: String?
    private(set) var created: FfiChannel?

    private let client: any ConversationClient

    init(client: any ConversationClient) { self.client = client }

    var problem: String? {
        let n = name.trimmingCharacters(in: .whitespacesAndNewlines).unicodeScalars.count
        if n == 0 { return "Enter a name." }
        if n > Self.nameLimit { return "A name can be up to \(Self.nameLimit) characters." }
        if topic.trimmingCharacters(in: .whitespacesAndNewlines).unicodeScalars.count > Self.topicLimit {
            return "A topic can be up to \(Self.topicLimit) characters."
        }
        return nil
    }

    var canSubmit: Bool { problem == nil && !busy && created == nil }

    func submit() async {
        guard canSubmit else { return }
        busy = true
        error = nil
        defer { busy = false }
        let trimmedTopic = topic.trimmingCharacters(in: .whitespacesAndNewlines)
        do {
            created = try await client.createChannel(
                name: name.trimmingCharacters(in: .whitespacesAndNewlines),
                topic: trimmedTopic.isEmpty ? nil : trimmedTopic, isPublic: isPublic)
        } catch {
            if case .Api(let code, _)? = error as? LoginError, code == "authz.forbidden" {
                self.error = "Only admins can create channels."
            } else {
                self.error = "Couldn't create the channel. Try again."
            }
        }
    }
}

/// "Browse Channels…": public channels you haven't joined.
@MainActor
@Observable
final class PublicChannelsModel {
    private(set) var channels: [FfiChannel] = []
    private(set) var loaded = false
    private(set) var busy: String?
    private(set) var error: String?
    /// The channel that was just joined: select it.
    private(set) var joined: FfiChannel?

    private let client: any ConversationClient

    init(client: any ConversationClient) { self.client = client }

    func load() async {
        do {
            // The server lists only non-archived ones; an archived one can't be joined anyway.
            channels = try await client.listPublicChannels().filter { !$0.archived }
            error = nil
        } catch {
            self.error = "Couldn't load the channels. Try again."
        }
        loaded = true
    }

    func join(_ id: String) async {
        guard busy == nil else { return }
        busy = id
        error = nil
        defer { busy = nil }
        do {
            let channel = try await client.joinChannel(channelId: id)
            channels.removeAll { $0.id == id }
            joined = channel
        } catch {
            self.error = "Couldn't join. Try again."
        }
    }
}

/// What the open channel's menu offers, and does (owners and admins, channels only).
@MainActor
@Observable
final class ChannelManagementModel {
    let channel: ChannelRow
    private(set) var busy = false
    private(set) var error: String?
    /// The action that finished, for the sheet to close (and Delete to clear the selection).
    private(set) var done = false

    private let powers: ChannelPowers
    private let client: any ConversationClient

    init(channel: ChannelRow, powers: ChannelPowers, client: any ConversationClient) {
        self.channel = channel
        self.powers = powers
        self.client = client
    }

    /// Add member, rename, archive, delete: an owner or a global admin, and never on a DM.
    var canManage: Bool { channel.kind != "dm" && powers.canManage }

    func addMember(_ typed: String) async {
        let handle = Handle.clean(typed)
        guard canManage, !handle.isEmpty else { return }
        await run { try await self.client.addMember(channelId: self.channel.id, handle: handle) } failure: {
            Handle.unknown($0) ? Handle.noOne : Self.text($0, "Only an owner or admin can add members.")
        }
    }

    /// Why a rename can't be sent (the server's limits), or nil.
    static func renameProblem(name: String, topic: String) -> String? {
        let n = name.trimmingCharacters(in: .whitespacesAndNewlines).unicodeScalars.count
        if n == 0 { return "Enter a name." }
        if n > NewChannelModel.nameLimit { return "A name can be up to \(NewChannelModel.nameLimit) characters." }
        if topic.trimmingCharacters(in: .whitespacesAndNewlines).unicodeScalars.count > NewChannelModel.topicLimit {
            return "A topic can be up to \(NewChannelModel.topicLimit) characters."
        }
        return nil
    }

    func rename(name: String, topic: String) async {
        let n = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard canManage, Self.renameProblem(name: name, topic: topic) == nil else { return }
        let t = topic.trimmingCharacters(in: .whitespacesAndNewlines)
        await run {
            _ = try await self.client.updateChannel(
                channelId: self.channel.id, name: n == self.channel.name ? nil : n,
                topic: t == (self.channel.topic ?? "") ? nil : t, archived: nil)
        } failure: { Self.text($0, "Only an owner or admin can rename it.") }
    }

    func setArchived(_ archived: Bool) async {
        guard canManage else { return }
        await run {
            _ = try await self.client.updateChannel(channelId: self.channel.id, name: nil, topic: nil, archived: archived)
        } failure: { Self.text($0, "Only an owner or admin can do that.") }
    }

    func delete() async {
        guard canManage else { return }
        await run { try await self.client.deleteChannel(channelId: self.channel.id) } failure: {
            Self.text($0, "Only an owner or admin can delete it.")
        }
    }

    private func run(_ call: () async throws -> Void, failure: (Error) -> String) async {
        guard !busy, !done else { return }
        busy = true
        error = nil
        defer { busy = false }
        do {
            try await call()
            done = true
        } catch let LoginError.Api(code, _) where code == "not_found" {
            done = true // already gone: the event on its way redraws the list
        } catch {
            self.error = failure(error)
        }
    }

    private static func text(_ error: Error, _ forbidden: String) -> String {
        if case .Api(let code, _)? = error as? LoginError, code == "authz.forbidden" { return forbidden }
        return "Couldn't do that. Try again."
    }
}
