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

/// Strictly rising, like the server's counter, so tests that don't care about order still apply.
@MainActor private var seqCounter: Int64 = 0
@MainActor private func nextSeq() -> Int64 { seqCounter += 1; return seqCounter }

@MainActor
final class TimelineReactionTests: XCTestCase {
    private func timeline(_ chat: FakeChat, me: String = "me") -> TimelineModel {
        let t = TimelineModel(channelId: "c", client: chat, me: me)
        t.merge([msg("m1", "hi"), msg("m2", "there")])
        return t
    }

    func testAnEventForAMessageHereAdjustsItsChips() {
        let t = timeline(FakeChat())
        t.apply(.reactionUpdate(channelId: "c", messageId: "m1", emoji: "👍", userId: "bob", added: true, count: 2, seq: nextSeq()))
        XCTAssertEqual(t.messages[0].reactions, [chip("👍", 2)])
        XCTAssertTrue(t.messages[1].reactions.isEmpty)
    }

    func testAnOlderEventForTheSameEmojiIsDroppedAndANewerOneApplies() {
        let t = timeline(FakeChat())
        t.apply(.reactionUpdate(channelId: "c", messageId: "m1", emoji: "👍", userId: "bob", added: true, count: 3, seq: 10))
        t.apply(.reactionUpdate(channelId: "c", messageId: "m1", emoji: "👍", userId: "ann", added: true, count: 2, seq: 9))
        XCTAssertEqual(t.messages[0].reactions, [chip("👍", 3)], "an older seq must not put an older count back")
        t.apply(.reactionUpdate(channelId: "c", messageId: "m1", emoji: "👍", userId: "ann", added: false, count: 1, seq: 11))
        XCTAssertEqual(t.messages[0].reactions, [chip("👍", 1)])
        t.apply(.reactionUpdate(channelId: "c", messageId: "m1", emoji: "🎉", userId: "bob", added: true, count: 1, seq: 5))
        XCTAssertEqual(t.messages[0].reactions, [chip("👍", 1), chip("🎉", 1)], "seq is tracked per emoji")
        t.apply(.reactionUpdate(channelId: "c", messageId: "m2", emoji: "👍", userId: "bob", added: true, count: 1, seq: 1))
        XCTAssertEqual(t.messages[1].reactions, [chip("👍", 1)], "and per message")
    }

    func testMyOlderEventStillSetsMyFlagWhenSomeoneElsesNewerEventCarriedTheCount() {
        let t = timeline(FakeChat())
        // Bob's add commits after mine, but his event arrives first.
        t.apply(.reactionUpdate(channelId: "c", messageId: "m1", emoji: "👍", userId: "bob", added: true, count: 2, seq: 10))
        XCTAssertEqual(t.messages[0].reactions, [chip("👍", 2)])
        t.apply(.reactionUpdate(channelId: "c", messageId: "m1", emoji: "👍", userId: "me", added: true, count: 1, seq: 9))
        XCTAssertEqual(t.messages[0].reactions, [chip("👍", 2, me: true)], "the count stays, my flag is set")
        // And an older event of mine does not undo a newer one of mine.
        t.apply(.reactionUpdate(channelId: "c", messageId: "m1", emoji: "👍", userId: "me", added: false, count: 1, seq: 8))
        XCTAssertEqual(t.messages[0].reactions, [chip("👍", 2, me: true)])
    }

    func testAResyncForgetsTheOrderingSoALowerSeqIsHeardAgain() {
        let t = timeline(FakeChat())
        t.apply(.reactionUpdate(channelId: "c", messageId: "m1", emoji: "👍", userId: "bob", added: true, count: 3, seq: 100))
        t.apply(.resync)
        t.apply(.reactionUpdate(channelId: "c", messageId: "m1", emoji: "👍", userId: "bob", added: true, count: 4, seq: 2))
        XCTAssertEqual(t.messages[0].reactions, [chip("👍", 4)])
    }

    func testMyEventSetsMyFlagAndAnotherChannelOrUnknownMessageIsIgnored() {
        let t = timeline(FakeChat())
        t.apply(.reactionUpdate(channelId: "c", messageId: "m1", emoji: "👍", userId: "me", added: true, count: 1, seq: nextSeq()))
        XCTAssertEqual(t.messages[0].reactions, [chip("👍", 1, me: true)])
        t.apply(.reactionUpdate(channelId: "other", messageId: "m2", emoji: "👍", userId: "bob", added: true, count: 1, seq: nextSeq()))
        t.apply(.reactionUpdate(channelId: "c", messageId: "nope", emoji: "👍", userId: "bob", added: true, count: 1, seq: nextSeq()))
        XCTAssertTrue(t.messages[1].reactions.isEmpty)
        XCTAssertEqual(t.messages.count, 2)
    }

    func testAnEventWithoutMyIdNeverSetsMyFlag() {
        let t = timeline(FakeChat(), me: "")
        t.apply(.reactionUpdate(channelId: "c", messageId: "m1", emoji: "👍", userId: "", added: true, count: 1, seq: nextSeq()))
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
        t.apply(.reactionUpdate(channelId: "c", messageId: "m1", emoji: "👍", userId: "bob", added: true, count: 1, seq: nextSeq()))
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
        t.apply(.reactionUpdate(channelId: "c", messageId: "m1", emoji: "👍", userId: "bob", added: true, count: 5, seq: nextSeq()))
        gate.open()
        await toggling.value
        XCTAssertEqual(t.messages[0].reactions, [chip("👍", 5)], "the older answer put the count back")
    }

    /// The server's real order: your own echo (an event) arrives first, then the answer.
    func testMyOwnEchoBeforeTheAnswerLeavesTheRowRight() async {
        let chat = FakeChat()
        let gate = Gate()
        chat.reactionGate = gate
        chat.reactionAnswer = [chip("👍", 3, me: true)]
        let t = timeline(chat)
        let toggling = Task { await t.toggleReaction(t.messages[0], emoji: "👍") }
        while chat.toggles.withLock({ $0.isEmpty }) { await Task.yield() }
        t.apply(.reactionUpdate(channelId: "c", messageId: "m1", emoji: "👍", userId: "me", added: true, count: 3, seq: nextSeq()))
        gate.open()
        await toggling.value
        XCTAssertEqual(t.messages[0].reactions, [chip("👍", 3, me: true)])
    }

    /// A failed toggle releases the lock: the next tap on the same message is sent.
    func testAfterAFailedToggleTheNextTapIsSent() async {
        let chat = FakeChat()
        chat.reactionFails = true
        let t = timeline(chat)
        await t.toggleReaction(t.messages[0], emoji: "👍")
        chat.reactionFails = false
        await t.toggleReaction(t.messages[0], emoji: "👍")
        XCTAssertEqual(chat.toggles.withLock { $0 }, ["m1|👍", "m1|👍"])
    }

    func testAnEventForAnotherMessageDoesNotMakeTheAnswerIgnored() async {
        let chat = FakeChat()
        let gate = Gate()
        chat.reactionGate = gate
        chat.reactionAnswer = [chip("👍", 1, me: true)]
        let t = timeline(chat)
        let toggling = Task { await t.toggleReaction(t.messages[0], emoji: "👍") }
        while chat.toggles.withLock({ $0.isEmpty }) { await Task.yield() }
        t.apply(.reactionUpdate(channelId: "c", messageId: "m2", emoji: "🎉", userId: "bob", added: true, count: 1, seq: nextSeq()))
        gate.open()
        await toggling.value
        XCTAssertEqual(t.messages[0].reactions, [chip("👍", 1, me: true)])
    }

    /// Not by the test calling `clear`: the error takes itself down after its lifetime.
    func testTheReactionErrorExpiresByItself() async {
        let chat = FakeChat()
        chat.reactionFails = true
        let t = TimelineModel(channelId: "c", client: chat, me: "me", errorLifetime: .milliseconds(60))
        t.merge([msg("m1", "hi")])
        await t.toggleReaction(t.messages[0], emoji: "👍")
        XCTAssertNotNil(t.reactionError)
        for _ in 0..<200 where t.reactionError != nil { try? await Task.sleep(for: .milliseconds(10)) }
        XCTAssertNil(t.reactionError, "it never went away")
    }

    /// A newer error isn't taken down by an older one's timer.
    func testANewerErrorIsNotClearedByAnOlderTimer() async {
        let chat = FakeChat()
        chat.reactionFails = true
        let t = TimelineModel(channelId: "c", client: chat, me: "me", errorLifetime: .milliseconds(150))
        t.merge([msg("m1", "hi")])
        await t.toggleReaction(t.messages[0], emoji: "👍") // timer 1 at +150 ms
        try? await Task.sleep(for: .milliseconds(100))
        await t.toggleReaction(t.messages[0], emoji: "👍") // timer 2 at +250 ms
        try? await Task.sleep(for: .milliseconds(80)) // +180: timer 1 has fired
        XCTAssertNotNil(t.reactionError, "the older timer cleared the newer error")
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
