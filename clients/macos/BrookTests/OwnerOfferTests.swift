import BrookCore
import XCTest

@testable import Brook

private func member(_ id: String, _ name: String, _ role: String? = "member") -> FfiMember {
    FfiMember(id: id, handle: id, displayName: name, role: role)
}

private func offer(to user: String, by: String = "own") -> FfiOwnerOffer {
    FfiOwnerOffer(userId: user, offeredBy: by, createdAt: "2026-09-26T10:00:00Z")
}

private func api(_ code: String) -> LoginError { .Api(code: code, message: "") }

final class OfferPowersTests: XCTestCase {
    private let members = [member("own", "Owner", "owner"), member("m", "M"), member("o2", "O2", "owner")]

    func testMakeOwnerForAnOwnerOrAdminBesideANonOwnerWithoutAnOffer() {
        let owner = ChannelPowers(me: "own", isAdmin: false, members: members)
        XCTAssertTrue(owner.canOffer(members[1]))
        XCTAssertFalse(owner.canOffer(members[2]), "already an owner")
        XCTAssertFalse(owner.canOffer(members[0]), "yourself")
        let admin = ChannelPowers(me: "a", isAdmin: true, members: members)
        XCTAssertTrue(admin.canOffer(members[1]))
        let plain = ChannelPowers(me: "m2", isAdmin: false, members: members + [member("m2", "M2")])
        XCTAssertFalse(plain.canOffer(members[1]), "a plain member offers nobody")
    }

    func testAPendingOfferGetsWithdrawInsteadOfMakeOwner() {
        var owner = ChannelPowers(me: "own", isAdmin: false, members: members)
        owner.offers = [offer(to: "m")]
        XCTAssertTrue(owner.pending(members[1]))
        XCTAssertFalse(owner.canOffer(members[1]))
        XCTAssertTrue(owner.canWithdraw(members[1]))
        var plain = ChannelPowers(me: "o3", isAdmin: false, members: members + [member("o3", "O3")])
        plain.offers = [offer(to: "m")]
        XCTAssertFalse(plain.canWithdraw(members[1]), "a plain member can't withdraw")
    }
}

@MainActor
final class OfferActionsTests: XCTestCase {
    func testOfferAndWithdrawSendThatMember() async {
        let client = FakeMembership()
        let model = MembersModel(channelId: "c1", client: client)
        await model.offer(member("bob", "Bob"))
        await model.withdraw(member("bob", "Bob"))
        XCTAssertEqual(client.calls.withLock { $0 }, ["offer:c1:bob", "withdraw:c1:bob"])
        XCTAssertNil(model.error)
    }

    func testRefusalsSayWhyAndAlreadySoIsSilent() async {
        for (code, text) in [("authz.forbidden", "You can't offer ownership here." as String?),
                             ("channel.not_member", "They're no longer a member."),
                             ("boom", "Couldn't do that. Try again."),
                             ("channel.already_owner", nil),
                             ("offer.not_found", nil)] {
            let client = FakeMembership()
            client.failure = api(code)
            let model = MembersModel(channelId: "c1", client: client)
            await model.offer(member("bob", "Bob"))
            XCTAssertEqual(model.error, text, code)
        }
    }
}

@MainActor
final class OfferAnswerTests: XCTestCase {
    private func model(_ client: FakeMembership) -> OfferAnswerModel {
        OfferAnswerModel(channelId: "c1", title: "general", offerer: "Ann", client: client)
    }

    func testAcceptAndDeclineCallOnceAndClose() async {
        for (answer, call) in [(true, "accept:c1"), (false, "decline:c1")] {
            let client = FakeMembership()
            let gate = Gate()
            client.gate = gate
            let m = model(client)
            let first = Task { answer ? await m.accept() : await m.decline() }
            while !m.busy { await Task.yield() }
            await m.accept()
            await m.decline()
            gate.open()
            await first.value
            XCTAssertTrue(m.done)
            XCTAssertEqual(client.calls.withLock { $0 }, [call])
        }
    }

    func testAnOfferGoneMeanwhileClosesSilently() async {
        let client = FakeMembership()
        client.failure = api("offer.not_found")
        let m = model(client)
        await m.accept()
        XCTAssertTrue(m.done)
        XCTAssertNil(m.error)
        XCTAssertFalse(m.canDefer)
    }

    func testAFailedAnswerStaysOpenAndOffersLater() async {
        let client = FakeMembership()
        let m = model(client)
        XCTAssertFalse(m.canDefer, "no way out before an answer fails")
        client.failure = .Network(message: "offline")
        await m.decline()
        XCTAssertFalse(m.done)
        XCTAssertEqual(m.error, OfferAnswerModel.failed)
        XCTAssertTrue(m.canDefer)
    }

    func testTheOfferersNameOrSomeone() {
        let members = [member("own", "Owner Ann", "owner")]
        XCTAssertEqual(OfferAnswerModel.offererName(offer(to: "me", by: "own"), members: members), "Owner Ann")
        XCTAssertEqual(OfferAnswerModel.offererName(offer(to: "me", by: "gone"), members: members), "Someone")
    }
}

final class OfferPromptTests: XCTestCase {
    private let mine = offer(to: "me")

    func testAskedOnlyWithAnOfferAndUntilAnswered() {
        let prompt = OfferPrompt()
        XCTAssertTrue(prompt.shows(channelId: "c1", offer: mine, answered: false))
        XCTAssertFalse(prompt.shows(channelId: "c1", offer: nil, answered: false), "the update dropped it")
        XCTAssertFalse(prompt.shows(channelId: "c1", offer: mine, answered: true))
        XCTAssertFalse(prompt.shows(channelId: nil, offer: mine, answered: false))
    }

    func testLaterLastsUntilTheNextOpeningAndOnlyForThatChannel() {
        var prompt = OfferPrompt()
        prompt.later("c1", mine)
        XCTAssertFalse(prompt.shows(channelId: "c1", offer: mine, answered: false))
        XCTAssertTrue(prompt.shows(channelId: "c2", offer: mine, answered: false), "another channel still asks")
        prompt.opened()
        XCTAssertTrue(prompt.shows(channelId: "c1", offer: mine, answered: false), "asked again when reopened")
    }

    /// Withdrawn and offered again while the channel is open: the new offer asks.
    func testANewOfferAsksEvenAfterLater() {
        var prompt = OfferPrompt()
        prompt.later("c1", mine)
        let again = FfiOwnerOffer(userId: "me", offeredBy: "own", createdAt: "2026-09-26T11:00:00Z")
        XCTAssertTrue(prompt.shows(channelId: "c1", offer: again, answered: false))
    }
}

@MainActor
final class OfferToMeTests: XCTestCase {
    func testTheBadgeIsForAnOfferNamingThisUser() {
        let client = FakeRealtime(channels: [])
        let model = ChannelsModel(client: client, me: "me")
        XCTAssertNotNil(model.offerToMe(ChannelRow(id: "c", name: "g", ownerOffers: [offer(to: "me")])))
        XCTAssertNil(model.offerToMe(ChannelRow(id: "c", name: "g", ownerOffers: [offer(to: "else")])))
        XCTAssertNil(ChannelsModel(client: client).offerToMe(ChannelRow(id: "c", name: "g",
                                                                        ownerOffers: [offer(to: "me")])),
                     "nothing while this user's id is unknown")
    }
}
