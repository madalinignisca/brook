import BrookCore
import XCTest

@testable import Brook

private func chip(_ emoji: String, _ count: Int64, me: Bool = false) -> FfiReaction {
    FfiReaction(emoji: emoji, count: count, me: me)
}

final class ReactionRulesTests: XCTestCase {
    func testANewEmojiGetsAChip() {
        let out = ReactionRules.applying(emoji: "🎉", count: 1, added: true, byMe: false, to: [chip("👍", 2)])
        XCTAssertEqual(out, [chip("👍", 2), chip("🎉", 1)])
    }

    func testTheEventsCountIsTheCountAndOthersLeaveMyFlag() {
        let out = ReactionRules.applying(emoji: "👍", count: 5, added: true, byMe: false, to: [chip("👍", 2, me: true)])
        XCTAssertEqual(out, [chip("👍", 5, me: true)])
    }

    func testZeroRemovesTheChip() {
        let out = ReactionRules.applying(emoji: "👍", count: 0, added: false, byMe: false, to: [chip("👍", 1), chip("🎉", 2)])
        XCTAssertEqual(out, [chip("🎉", 2)])
        XCTAssertEqual(ReactionRules.applying(emoji: "❤️", count: 0, added: false, byMe: false, to: []), [])
    }

    func testMineSetsMeFromAdded() {
        let on = ReactionRules.applying(emoji: "👍", count: 3, added: true, byMe: true, to: [chip("👍", 2)])
        XCTAssertEqual(on, [chip("👍", 3, me: true)])
        let off = ReactionRules.applying(emoji: "👍", count: 2, added: false, byMe: true, to: on)
        XCTAssertEqual(off, [chip("👍", 2, me: false)])
    }

    func testApplyingTheSameEventTwiceChangesNothing() {
        let once = ReactionRules.applying(emoji: "👍", count: 3, added: true, byMe: true, to: [chip("👍", 2)])
        let twice = ReactionRules.applying(emoji: "👍", count: 3, added: true, byMe: true, to: once)
        XCTAssertEqual(once, twice)
    }
}

@MainActor
final class TimelineReactionTests: XCTestCase {
    private func timeline(_ chat: FakeChat, me: String = "me") -> TimelineModel {
        let t = TimelineModel(channelId: "c", client: chat, me: me)
        t.merge([msg("m1", "hi"), msg("m2", "there")])
        return t
    }

    func testAnEventForAMessageHereAdjustsItsChips() {
        let t = timeline(FakeChat())
        t.apply(.reactionUpdate(channelId: "c", messageId: "m1", emoji: "👍", userId: "bob", added: true, count: 2))
        XCTAssertEqual(t.messages[0].reactions, [chip("👍", 2)])
        XCTAssertTrue(t.messages[1].reactions.isEmpty)
    }

    func testMyEventSetsMyFlagAndAnotherChannelOrUnknownMessageIsIgnored() {
        let t = timeline(FakeChat())
        t.apply(.reactionUpdate(channelId: "c", messageId: "m1", emoji: "👍", userId: "me", added: true, count: 1))
        XCTAssertEqual(t.messages[0].reactions, [chip("👍", 1, me: true)])
        t.apply(.reactionUpdate(channelId: "other", messageId: "m2", emoji: "👍", userId: "bob", added: true, count: 1))
        t.apply(.reactionUpdate(channelId: "c", messageId: "nope", emoji: "👍", userId: "bob", added: true, count: 1))
        XCTAssertTrue(t.messages[1].reactions.isEmpty)
        XCTAssertEqual(t.messages.count, 2)
    }

    func testAnEventWithoutMyIdNeverSetsMyFlag() {
        let t = timeline(FakeChat(), me: "")
        t.apply(.reactionUpdate(channelId: "c", messageId: "m1", emoji: "👍", userId: "", added: true, count: 1))
        XCTAssertEqual(t.messages[0].reactions, [chip("👍", 1, me: false)])
    }

    func testTheTogglesAnswerReplacesTheRow() async {
        let chat = FakeChat()
        chat.reactionAnswer = [chip("👍", 4, me: true), chip("🎉", 1)]
        let t = timeline(chat)
        await t.toggleReaction(t.messages[0], emoji: "👍")
        XCTAssertEqual(t.messages[0].reactions, chat.reactionAnswer)
        XCTAssertEqual(chat.toggles.withLock { $0 }, ["m1|👍"])
        XCTAssertNil(t.reactionError)
    }

    func testAFailedToggleChangesNothingAndSaysSo() async {
        let chat = FakeChat()
        chat.reactionFails = true
        let t = timeline(chat)
        t.apply(.reactionUpdate(channelId: "c", messageId: "m1", emoji: "👍", userId: "bob", added: true, count: 1))
        await t.toggleReaction(t.messages[0], emoji: "👍")
        XCTAssertEqual(t.messages[0].reactions, [chip("👍", 1)])
        XCTAssertEqual(t.reactionError, "Couldn't react. Try again.")
    }

    func testASecondTapOnTheSameMessageWhileRunningSendsNothing() async {
        let chat = FakeChat()
        let gate = Gate()
        chat.reactionGate = gate
        let t = timeline(chat)
        let first = Task { await t.toggleReaction(t.messages[0], emoji: "👍") }
        while chat.toggles.withLock({ $0.isEmpty }) { await Task.yield() }
        await t.toggleReaction(t.messages[0], emoji: "👍") // in flight: sends nothing
        await t.toggleReaction(t.messages[0], emoji: "🎉") // the same message: nothing either
        gate.open()
        await first.value
        XCTAssertEqual(chat.toggles.withLock { $0 }, ["m1|👍"])
        await t.toggleReaction(t.messages[1], emoji: "🎉") // another message is its own
        XCTAssertEqual(chat.toggles.withLock { $0 }, ["m1|👍", "m2|🎉"])
    }

    /// The answer is a snapshot: an event that arrived while it was in flight is newer.
    func testAnEventDuringTheToggleMakesItsOlderAnswerIgnored() async {
        let chat = FakeChat()
        let gate = Gate()
        chat.reactionGate = gate
        chat.reactionAnswer = [chip("👍", 4)] // the older snapshot
        let t = timeline(chat)
        let toggling = Task { await t.toggleReaction(t.messages[0], emoji: "👍") }
        while chat.toggles.withLock({ $0.isEmpty }) { await Task.yield() }
        t.apply(.reactionUpdate(channelId: "c", messageId: "m1", emoji: "👍", userId: "bob", added: true, count: 5))
        gate.open()
        await toggling.value
        XCTAssertEqual(t.messages[0].reactions, [chip("👍", 5)], "the older answer put the count back")
    }

    func testAnEventForAnotherMessageDoesNotMakeTheAnswerIgnored() async {
        let chat = FakeChat()
        let gate = Gate()
        chat.reactionGate = gate
        chat.reactionAnswer = [chip("👍", 1, me: true)]
        let t = timeline(chat)
        let toggling = Task { await t.toggleReaction(t.messages[0], emoji: "👍") }
        while chat.toggles.withLock({ $0.isEmpty }) { await Task.yield() }
        t.apply(.reactionUpdate(channelId: "c", messageId: "m2", emoji: "🎉", userId: "bob", added: true, count: 1))
        gate.open()
        await toggling.value
        XCTAssertEqual(t.messages[0].reactions, [chip("👍", 1, me: true)])
    }

    func testTheReactionErrorClears() async {
        let chat = FakeChat()
        chat.reactionFails = true
        let t = timeline(chat)
        await t.toggleReaction(t.messages[0], emoji: "👍")
        XCTAssertNotNil(t.reactionError)
        t.clearReactionError()
        XCTAssertNil(t.reactionError)
    }

    func testADeletedMessageTakesNoReactions() async {
        let chat = FakeChat()
        let t = TimelineModel(channelId: "c", client: chat, me: "me")
        t.merge([msg("m1", "", deleted: true)])
        await t.toggleReaction(t.messages[0], emoji: "👍")
        XCTAssertEqual(chat.toggles.withLock { $0 }, [])
    }
}

/// The tint is `NotificationPlanner.mentions`; its own/deleted cases are in `MentionRuleTests`.
final class MentionTintTests: XCTestCase {
    func testTheTintFollowsTheMentionRuleForNamedAndEveryone() {
        var named = msg("m1", "hi")
        named.mentions = ["me"]
        XCTAssertTrue(NotificationPlanner.mentions(named, me: "me"))
        var everyone = msg("m2", "hi")
        everyone.mentionEveryone = true
        XCTAssertTrue(NotificationPlanner.mentions(everyone, me: "me"))
        XCTAssertFalse(NotificationPlanner.mentions(msg("m3", "hi"), me: "me"))
    }
}
