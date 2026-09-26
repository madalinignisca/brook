import BrookCore
import Foundation
import Observation

/// Leaving a channel and removing its members (#183).
protocol MembershipClient: AnyObject, Sendable {
    func removeMember(channelId: String, userId: String) async throws
    func leaveChannel(channelId: String) async throws
}

extension FfiBrookClient: MembershipClient {}

/// Who may do what in a channel, from its members' roles (#183) and the global role. The
/// server decides every call; this only decides what the UI offers.
struct ChannelPowers {
    let me: String
    let isAdmin: Bool
    let members: [FfiMember]

    private var myRole: String? { members.first { $0.id == me }?.role }

    /// Remove: an admin removes anyone, an owner anyone but another owner. Never yourself
    /// (that's Leave), and without roles (an older server) only an admin.
    func canRemove(_ member: FfiMember) -> Bool {
        guard member.id != me else { return false }
        if isAdmin { return true }
        return myRole == "owner" && member.role != "owner"
    }

    /// The roles say leaving would take the channel's only owner.
    var lastOwner: Bool {
        myRole == "owner" && members.filter { $0.role == "owner" }.count == 1
    }

    /// You first, then by name. Names aren't unique, so the list shows handles too.
    var rows: [FfiMember] {
        members.sorted { a, b in
            if (a.id == me) != (b.id == me) { return a.id == me }
            return a.displayName.localizedCaseInsensitiveCompare(b.displayName) == .orderedAscending
        }
    }
}

/// "Leave <channel>?": the confirmation's state. The list changes only by the server's
/// `channel.delete`, never here.
@MainActor
@Observable
final class LeaveModel {
    static let lastOwner = "You're its last owner. Delete the channel instead."

    let channelId: String
    let title: String
    private(set) var busy = false
    private(set) var error: String?
    /// Left (or already out): the confirmation closes.
    private(set) var done = false
    /// Said before trying, from the roles; Leave is disabled while it's set.
    let warning: String?

    private let client: any MembershipClient

    init(channelId: String, title: String, powers: ChannelPowers, client: any MembershipClient) {
        self.channelId = channelId
        self.title = title
        self.client = client
        warning = powers.lastOwner ? Self.lastOwner : nil
    }

    var canLeave: Bool { !busy && !done && warning == nil }

    func confirm() async {
        guard canLeave else { return }
        busy = true
        error = nil
        defer { busy = false }
        do {
            try await client.leaveChannel(channelId: channelId)
            done = true
        } catch let LoginError.Api(code, _) where code == "not_found" {
            done = true // already out
        } catch {
            self.error = Self.text(error)
        }
    }

    static func text(_ error: Error) -> String {
        switch error as? LoginError {
        case let .Api(code, _) where code == "channel.last_owner": lastOwner
        case let .Api(code, _) where code == "authz.forbidden": "You can't leave this channel."
        default: "Couldn't leave. Try again."
        }
    }
}

/// Removing members from the open channel's member list.
@MainActor
@Observable
final class MembersModel {
    let channelId: String
    /// The member being removed (its Remove is busy).
    private(set) var removing: String?
    private(set) var error: String?

    private let client: any MembershipClient

    init(channelId: String, client: any MembershipClient) {
        self.channelId = channelId
        self.client = client
    }

    /// The list refreshes from the server's `channel.update`, never optimistically.
    func remove(_ userId: String) async {
        guard removing == nil else { return }
        removing = userId
        error = nil
        defer { removing = nil }
        do {
            try await client.removeMember(channelId: channelId, userId: userId)
        } catch let LoginError.Api(code, _) where code == "not_found" {
            // Already gone: the update on its way redraws the list.
        } catch {
            self.error = Self.text(error)
        }
    }

    static func text(_ error: Error) -> String {
        switch error as? LoginError {
        case let .Api(code, _) where code == "channel.last_owner": "They're its last owner."
        case let .Api(code, _) where code == "authz.forbidden": "You can't remove members here."
        default: "Couldn't remove them. Try again."
        }
    }
}

extension ChannelRow {
    /// A DM can't be left (the server's `channel.dm`), so it isn't offered.
    var canLeave: Bool { kind != "dm" }
}
