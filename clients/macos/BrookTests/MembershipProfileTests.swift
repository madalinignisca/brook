import BrookCore
import Synchronization
import XCTest

@testable import Brook

final class FakeMembership: MembershipClient, @unchecked Sendable {
    let calls = Mutex<[String]>([])
    var failure: LoginError?
    var gate: Gate?

    func removeMember(channelId: String, userId: String) async throws {
        calls.withLock { $0.append("remove:\(channelId):\(userId)") }
        await gate?.wait()
        if let failure { throw failure }
    }
    func leaveChannel(channelId: String) async throws {
        calls.withLock { $0.append("leave:\(channelId)") }
        await gate?.wait()
        if let failure { throw failure }
    }
    func offerOwnership(channelId: String, handle: String) async throws -> FfiChannel {
        calls.withLock { $0.append("offer:\(channelId):\(handle)") }
        await gate?.wait()
        if let failure { throw failure }
        return channel(channelId, "general")
    }
    func withdrawOwnershipOffer(channelId: String, userId: String) async throws {
        calls.withLock { $0.append("withdraw:\(channelId):\(userId)") }
        await gate?.wait()
        if let failure { throw failure }
    }
    func acceptOwnership(channelId: String) async throws -> FfiChannel {
        calls.withLock { $0.append("accept:\(channelId)") }
        await gate?.wait()
        if let failure { throw failure }
        return channel(channelId, "general")
    }
    func declineOwnership(channelId: String) async throws {
        calls.withLock { $0.append("decline:\(channelId)") }
        await gate?.wait()
        if let failure { throw failure }
    }
}

private func member(_ id: String, _ name: String, _ role: String? = "member") -> FfiMember {
    FfiMember(id: id, handle: id, displayName: name, role: role)
}

private func api(_ code: String) -> LoginError { .Api(code: code, message: "") }

final class ChannelPowersTests: XCTestCase {
    func testYouComeFirstThenByName() {
        let p = ChannelPowers(me: "u3", isAdmin: false,
                              members: [member("u1", "zed"), member("u2", "Anna"), member("u3", "Me")])
        XCTAssertEqual(p.rows.map(\.id), ["u3", "u2", "u1"])
    }

    func testAnAdminRemovesAnyoneButThemselves() {
        let p = ChannelPowers(me: "a", isAdmin: true,
                              members: [member("a", "A"), member("o", "O", "owner"), member("m", "M")])
        XCTAssertFalse(p.canRemove(p.members[0]))
        XCTAssertTrue(p.canRemove(p.members[1]))
        XCTAssertTrue(p.canRemove(p.members[2]))
    }

    func testAnOwnerRemovesMembersButNotOtherOwners() {
        let p = ChannelPowers(me: "me", isAdmin: false,
                              members: [member("me", "Me", "owner"), member("o", "O", "owner"), member("m", "M")])
        XCTAssertFalse(p.canRemove(p.members[1]))
        XCTAssertTrue(p.canRemove(p.members[2]))
    }

    func testAMemberOrNoRoleRemovesNobody() {
        let asMember = ChannelPowers(me: "me", isAdmin: false, members: [member("me", "Me"), member("m", "M")])
        XCTAssertFalse(asMember.canRemove(asMember.members[1]))
        let noRoles = ChannelPowers(me: "me", isAdmin: false,
                                    members: [member("me", "Me", nil), member("m", "M", nil)])
        XCTAssertFalse(noRoles.canRemove(noRoles.members[1]))
    }

    func testTheLastOwnerIsKnownFromTheRoles() {
        XCTAssertTrue(ChannelPowers(me: "me", isAdmin: false,
                                    members: [member("me", "Me", "owner"), member("m", "M")]).lastOwner)
        XCTAssertFalse(ChannelPowers(me: "me", isAdmin: false,
                                     members: [member("me", "Me", "owner"), member("o", "O", "owner")]).lastOwner)
        XCTAssertFalse(ChannelPowers(me: "me", isAdmin: false,
                                     members: [member("me", "Me"), member("o", "O", "owner")]).lastOwner)
        XCTAssertFalse(ChannelPowers(me: "me", isAdmin: false, members: [member("me", "Me", nil)]).lastOwner)
    }

    func testADirectMessageIsNeverOfferedForLeaving() {
        XCTAssertFalse(ChannelRow(id: "d", kind: "dm", name: nil).canLeave)
        XCTAssertTrue(ChannelRow(id: "c", kind: "channel", name: "general").canLeave)
    }
}

@MainActor
final class LeaveModelTests: XCTestCase {
    private let plain = ChannelPowers(me: "me", isAdmin: false, members: [member("me", "Me")])

    func testLeavingCallsOnceAndIsDone() async {
        let client = FakeMembership()
        let model = LeaveModel(channelId: "c1", title: "general", powers: plain, client: client)
        await model.confirm()
        XCTAssertTrue(model.done)
        XCTAssertNil(model.error)
        XCTAssertEqual(client.calls.withLock { $0 }, ["leave:c1"])
    }

    func testEachRefusalSaysWhy() async {
        for (code, text) in [("channel.last_owner", LeaveModel.lastOwner),
                             ("authz.forbidden", "You can't leave this channel."),
                             ("boom", "Couldn't leave. Try again.")] {
            let client = FakeMembership()
            client.failure = api(code)
            let model = LeaveModel(channelId: "c1", title: "general", powers: plain, client: client)
            await model.confirm()
            XCTAssertEqual(model.error, text, code)
            XCTAssertFalse(model.done, code)
        }
    }

    func testAlreadyOutCountsAsDone() async {
        let client = FakeMembership()
        client.failure = api("not_found")
        let model = LeaveModel(channelId: "c1", title: "general", powers: plain, client: client)
        await model.confirm()
        XCTAssertTrue(model.done)
        XCTAssertNil(model.error)
    }

    func testTheLastOwnerIsWarnedAndNothingIsSent() async {
        let client = FakeMembership()
        let owner = ChannelPowers(me: "me", isAdmin: false, members: [member("me", "Me", "owner")])
        let model = LeaveModel(channelId: "c1", title: "general", powers: owner, client: client)
        XCTAssertEqual(model.warning, LeaveModel.lastOwner)
        XCTAssertFalse(model.canLeave)
        await model.confirm()
        XCTAssertEqual(client.calls.withLock { $0 }, [])
    }
}

@MainActor
final class MembersModelTests: XCTestCase {
    func testRemoveSendsThatMember() async {
        let client = FakeMembership()
        let model = MembersModel(channelId: "c1", client: client)
        await model.remove("u2")
        XCTAssertEqual(client.calls.withLock { $0 }, ["remove:c1:u2"])
        XCTAssertNil(model.error)
        XCTAssertNil(model.busy)
    }

    func testEachRefusalSaysWhyAndAlreadyGoneIsSilent() async {
        for (code, text) in [("channel.last_owner", "They're its last owner." as String?),
                             ("authz.forbidden", "You can't remove members here."),
                             ("boom", "Couldn't remove them. Try again."),
                             ("not_found", nil)] {
            let client = FakeMembership()
            client.failure = api(code)
            let model = MembersModel(channelId: "c1", client: client)
            await model.remove("u2")
            XCTAssertEqual(model.error, text, code)
        }
    }

    func testOneRemovalAtATime() async {
        let client = FakeMembership()
        let gate = Gate()
        client.gate = gate
        let model = MembersModel(channelId: "c1", client: client)
        let first = Task { await model.remove("u2") }
        while model.busy == nil { await Task.yield() }
        await model.remove("u3")
        gate.open()
        await first.value
        XCTAssertEqual(client.calls.withLock { $0 }, ["remove:c1:u2"])
    }
}

@MainActor
final class ProfileModelTests: XCTestCase {
    private func loaded(_ client: FakeAccount = FakeAccount()) async -> (FakeAccount, ProfileModel) {
        let model = ProfileModel(client: client)
        await model.load()
        return (client, model)
    }

    func testOnlyAChangedFieldIsSent() async {
        let (client, model) = await loaded()
        model.name = "  New  "
        await model.save()
        XCTAssertEqual(client.calls.withLock { $0 }, ["profile:New|-"])
        XCTAssertEqual(model.saved?.displayName, "New")
    }

    func testClearingTheStatusSendsEmpty() async {
        let client = FakeAccount()
        client.profile.statusText = "away"
        let (_, model) = await loaded(client)
        XCTAssertEqual(model.status, "away")
        model.status = ""
        await model.save()
        XCTAssertEqual(client.calls.withLock { $0 }, ["profile:-|"])
        XCTAssertNil(model.saved?.statusText)
    }

    func testTheLimitsAreTheServers() async {
        let (_, model) = await loaded()
        model.name = "   "
        XCTAssertEqual(model.problem, "Enter a name.")
        model.name = String(repeating: "a", count: 64)
        XCTAssertNil(model.problem)
        // 60 letters and a family emoji: 61 characters on screen, 65 code points to the server.
        model.name = String(repeating: "a", count: 60) + "👨‍👩‍👧"
        XCTAssertEqual(model.problem, "A name can be up to 64 characters.")
        model.name = "Me"
        model.status = String(repeating: "s", count: 101)
        XCTAssertEqual(model.problem, "A status can be up to 100 characters.")
        XCTAssertFalse(model.canSave)
    }

    func testSaveNeedsAChangeAndRunsOnce() async {
        let client = FakeAccount()
        let gate = Gate()
        client.gate = gate
        let (_, model) = await loaded(client)
        XCTAssertFalse(model.canSave, "nothing changed")
        model.status = "busy"
        XCTAssertTrue(model.canSave)
        let first = Task { await model.save() }
        while !model.busy { await Task.yield() }
        XCTAssertFalse(model.canSave, "while saving")
        await model.save()
        gate.open()
        await first.value
        XCTAssertEqual(client.calls.withLock { $0 }, ["profile:-|busy"])
    }

    func testARefusalSaysWhyAndKeepsTheEdit() async {
        let client = FakeAccount()
        let (_, model) = await loaded(client)
        client.failure = .Api(code: "profile.invalid", message: "")
        model.name = "Bad"
        await model.save()
        XCTAssertEqual(model.error, ProfileModel.refused)
        XCTAssertEqual(model.name, "Bad")
        XCTAssertNil(model.saved)
    }
}
