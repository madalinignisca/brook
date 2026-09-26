import BrookCore
import Foundation
import Observation

/// Leaving a channel and removing its members (#183).
protocol MembershipClient: AnyObject, Sendable {
    func removeMember(channelId: String, userId: String) async throws
    func leaveChannel(channelId: String) async throws
    // Ownership offers (#190).
    func offerOwnership(channelId: String, handle: String) async throws -> FfiChannel
    func withdrawOwnershipOffer(channelId: String, userId: String) async throws
    func acceptOwnership(channelId: String) async throws -> FfiChannel
    func declineOwnership(channelId: String) async throws
}

extension FfiBrookClient: MembershipClient {}

/// Who may do what in a channel, from its members' roles (#183) and the global role. The
/// server decides every call; this only decides what the UI offers.
struct ChannelPowers {
    let me: String
    let isAdmin: Bool
    let members: [FfiMember]
    /// Pending ownership offers (#190).
    var offers: [FfiOwnerOffer] = []

    private var myRole: String? { members.first { $0.id == me }?.role }

    /// A channel row's powers for this user: its members, their roles and its pending offers.
    init(_ channel: ChannelRow, me: String, isAdmin: Bool) {
        self.init(me: me, isAdmin: isAdmin, members: channel.members, offers: channel.ownerOffers)
    }

    init(me: String, isAdmin: Bool, members: [FfiMember], offers: [FfiOwnerOffer] = []) {
        (self.me, self.isAdmin, self.members, self.offers) = (me, isAdmin, members, offers)
    }

    /// Remove: an admin removes anyone, an owner anyone but another owner. Never yourself
    /// (that's Leave), and without roles (an older server) only an admin.
    func canRemove(_ member: FfiMember) -> Bool {
        guard member.id != me else { return false }
        if isAdmin { return true }
        return myRole == "owner" && member.role != "owner"
    }

    /// "Make Owner": an owner or an admin, beside a non-owner who isn't you and hasn't a
    /// pending offer (that one gets Withdraw).
    func canOffer(_ member: FfiMember) -> Bool {
        guard member.id != me, member.role != "owner", !pending(member) else { return false }
        return isAdmin || myRole == "owner"
    }

    /// "Withdraw": the same people who may offer, beside a pending offer.
    func canWithdraw(_ member: FfiMember) -> Bool {
        pending(member) && (isAdmin || myRole == "owner")
    }

    /// An offer to make them an owner is waiting for their answer.
    func pending(_ member: FfiMember) -> Bool {
        offers.contains { $0.userId == member.id }
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

/// The open channel's member actions: remove, and offer or withdraw ownership. One at a time.
@MainActor
@Observable
final class MembersModel {
    let channelId: String
    /// The member an action is running for (every member action waits for it).
    private(set) var busy: String?
    private(set) var error: String?

    private let client: any MembershipClient

    init(channelId: String, client: any MembershipClient) {
        self.channelId = channelId
        self.client = client
    }

    /// The list refreshes from the server's `channel.update`, never optimistically.
    func remove(_ userId: String) async {
        guard busy == nil else { return }
        busy = userId
        error = nil
        defer { busy = nil }
        do {
            try await client.removeMember(channelId: channelId, userId: userId)
        } catch let LoginError.Api(code, _) where code == "not_found" {
            // Already gone: the update on its way redraws the list.
        } catch {
            self.error = Self.text(error)
        }
    }

    /// Offer `member` ownership. The row changes when the server's update arrives.
    func offer(_ member: FfiMember) async {
        await act(member.id) { _ = try await $0.offerOwnership(channelId: self.channelId, handle: member.handle) }
    }

    func withdraw(_ member: FfiMember) async {
        await act(member.id) { try await $0.withdrawOwnershipOffer(channelId: self.channelId, userId: member.id) }
    }

    private func act(_ id: String, _ call: (any MembershipClient) async throws -> Void) async {
        guard busy == nil else { return }
        busy = id
        error = nil
        defer { busy = nil }
        do {
            try await call(client)
        } catch let LoginError.Api(code, _) where code == "channel.already_owner" || code == "offer.not_found" {
            // Already so: the update on its way redraws the row.
        } catch {
            self.error = Self.offerText(error)
        }
    }

    static func offerText(_ error: Error) -> String {
        switch error as? LoginError {
        case let .Api(code, _) where code == "authz.forbidden": "You can't offer ownership here."
        case let .Api(code, _) where code == "channel.not_member": "They're no longer a member."
        default: "Couldn't do that. Try again."
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

/// "<name> offered you ownership of <title>": Accept or Decline, and nothing else until an
/// answer fails (then "Ask Me Later", so a failure can't lock the window).
@MainActor
@Observable
final class OfferAnswerModel {
    static let failed = "Couldn't answer. Try again."

    let channelId: String
    let title: String
    let offerer: String
    private(set) var busy = false
    private(set) var error: String?
    /// Answered (or the offer is gone): the sheet closes.
    private(set) var done = false
    /// An answer failed: the sheet may be put off until the channel is next opened.
    private(set) var canDefer = false

    private let client: any MembershipClient

    init(channelId: String, title: String, offerer: String, client: any MembershipClient) {
        (self.channelId, self.title, self.offerer, self.client) = (channelId, title, offerer, client)
    }

    /// Who offered, as the channel lists them; "Someone" once they're gone from it.
    static func offererName(_ offer: FfiOwnerOffer, members: [FfiMember]) -> String {
        members.first { $0.id == offer.offeredBy }?.displayName ?? "Someone"
    }

    func accept() async { await answer { _ = try await $0.acceptOwnership(channelId: self.channelId) } }
    func decline() async { await answer { try await $0.declineOwnership(channelId: self.channelId) } }

    private func answer(_ call: (any MembershipClient) async throws -> Void) async {
        guard !busy, !done else { return }
        busy = true
        error = nil
        defer { busy = false }
        do {
            try await call(client)
            done = true
        } catch let LoginError.Api(code, _) where code == "offer.not_found" {
            done = true // withdrawn meanwhile, or the offerer left
        } catch {
            self.error = Self.failed
            canDefer = true
        }
    }
}

/// When the open channel asks its ownership question: while an offer to this user is on it,
/// it isn't answered, and that offer wasn't put off for this opening. Nothing but a new
/// opening clears "Ask Me Later", so it can't re-present at once. It's per channel and per
/// offer, so another channel, or a new offer here, still asks.
struct OfferPrompt {
    private(set) var deferred: String?

    private static func key(_ channelId: String, _ offer: FfiOwnerOffer) -> String {
        "\(channelId)|\(offer.offeredBy)|\(offer.createdAt)"
    }

    /// A channel was opened (or closed): any "later" is over.
    mutating func opened() { deferred = nil }

    /// "Ask Me Later" for this offer, until the channel is next opened.
    mutating func later(_ channelId: String, _ offer: FfiOwnerOffer) { deferred = Self.key(channelId, offer) }

    func shows(channelId: String?, offer: FfiOwnerOffer?, answered: Bool) -> Bool {
        guard let channelId, let offer, !answered else { return false }
        return deferred != Self.key(channelId, offer)
    }
}
