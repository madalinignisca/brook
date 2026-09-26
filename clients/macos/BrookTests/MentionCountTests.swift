import BrookCore
import XCTest

@testable import Brook

/// Unread mentions per channel (#195, #196): the server's count online, this device's cache
/// when it has one, kept through updates (which say 0), raised live, cleared by reading.
@MainActor
final class MentionCountTests: XCTestCase {
    private func listed(_ id: String, mentions: Int64) -> FfiChannel {
        var c = channel(id, id)
        c.unreadMentions = mentions
        return c
    }

    private func cached(_ id: String, unread: Int64, mentions: Int64) -> FfiCachedChannel {
        var c = cachedChannel(id, id, unread: unread)
        c.unreadMentions = mentions
        return c
    }

    func testWithoutLocalDataTheServersCountShows() async {
        let client = FakeRealtime(channels: [listed("c1", mentions: 3)])
        let model = ChannelsModel(client: client)
        await model.start()
        XCTAssertEqual(model.channels.first?.unreadMentions, 3)
        XCTAssertEqual(model.mentions(model.channels[0]), 3)
    }

    func testTheCachesCountWinsAndFollowsItsNotices() async {
        let client = FakeRealtime(channels: [listed("c1", mentions: 5), listed("c2", mentions: 5)])
        client.cached = [cached("c1", unread: 4, mentions: 1), cached("c2", unread: 4, mentions: 2)]
        let model = ChannelsModel(client: client)
        model.openChannel = "c2"
        await model.start()
        XCTAssertEqual(model.channels.map(\.unreadMentions), [1, 0], "the open channel's stays 0")
        client.cached = [cached("c1", unread: 6, mentions: 3), cached("c2", unread: 6, mentions: 3)]
        await model.cacheChannelsChanged()
        XCTAssertEqual(model.channels.map(\.unreadMentions), [3, 0])
    }

    func testAnUpdateKeepsTheRowsCount() async {
        let client = FakeRealtime(channels: [listed("c1", mentions: 2)])
        let model = ChannelsModel(client: client)
        await model.start()
        client.deliver(.channelUpdate(channel: listed("c1", mentions: 0))) // per-user counts say 0
        await drainMain()
        XCTAssertEqual(model.channels.first?.unreadMentions, 2)
    }

    func testALiveMentionRaisesItAndReadingClearsIt() async {
        let client = FakeRealtime(channels: [channel("c1", "general")])
        let model = ChannelsModel(client: client, me: "me", isActive: { true })
        await model.start()
        client.deliver(.messageNew(message: msg("m1", "plain", channel: "c1")))
        var named = msg("m2", "@me", channel: "c1")
        named.mentions = ["me"]
        var everyone = msg("m3", "@channel", channel: "c1")
        everyone.mentionEveryone = true
        client.deliver(.messageNew(message: named))
        client.deliver(.messageNew(message: everyone))
        await drainMain()
        XCTAssertEqual(model.channels[0].unread, 3)
        XCTAssertEqual(model.channels[0].unreadMentions, 2, "named and everyone, not the plain one")
        model.openChannel = "c1"
        XCTAssertEqual(model.channels[0].unreadMentions, 0)
        XCTAssertNil(model.mentions(model.channels[0]))
    }
}

final class MentionRuleTests: XCTestCase {
    func testOnlySomeoneElsesLiveMessageMentionsMe() {
        var mine = msg("m1", "@channel", channel: "c")
        mine.mentionEveryone = true
        mine.authorId = "me"
        XCTAssertFalse(NotificationPlanner.mentions(mine, me: "me"), "my own @channel")
        var gone = msg("m2", "", channel: "c")
        gone.mentions = ["me"]
        gone.deleted = true
        XCTAssertFalse(NotificationPlanner.mentions(gone, me: "me"), "a deleted one")
        var named = msg("m3", "@me", channel: "c")
        named.mentions = ["me"]
        XCTAssertTrue(NotificationPlanner.mentions(named, me: "me"))
    }
}
