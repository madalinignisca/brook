import BrookCore
import XCTest

@testable import Brook

@MainActor
final class FakeNotifier: Notifying {
    var posted: [(channel: String, title: String, body: String)] = []
    var removed: [String] = []
    func post(channelId: String, title: String, body: String) { posted.append((channelId, title, body)) }
    func remove(channelId: String) { removed.append(channelId) }
}

/// Notifications and live unread counts (spec 2026-09-26-mac-notifications §4).
@MainActor
final class NotificationsTests: XCTestCase {
    private func from(_ author: String, _ body: String, channel: String = "c2", deleted: Bool = false) -> FfiMessage {
        var m = msg("m1", body, channel: channel, deleted: deleted)
        m.authorId = author
        m.authorDisplayName = "Bo"
        return m
    }

    func testWhatCounts() {
        let other = from("u9", "hi")
        XCTAssertTrue(NotificationPlanner.counts(other, me: "me", openChannel: "c1", appActive: true))
        XCTAssertFalse(NotificationPlanner.counts(from("u9", "hi", channel: "c1"), me: "me", openChannel: "c1", appActive: true),
                       "a message being read")
        XCTAssertTrue(NotificationPlanner.counts(from("u9", "hi", channel: "c1"), me: "me", openChannel: "c1", appActive: false),
                      "the open channel with the app in the background")
        XCTAssertFalse(NotificationPlanner.counts(from("me", "mine"), me: "me", openChannel: nil, appActive: true))
        XCTAssertFalse(NotificationPlanner.counts(other, me: nil, openChannel: nil, appActive: true), "id unknown")
        XCTAssertFalse(NotificationPlanner.counts(other, me: "", openChannel: nil, appActive: true), "id empty")
        XCTAssertFalse(NotificationPlanner.counts(from("u9", "", deleted: true), me: "me", openChannel: nil, appActive: true))
    }

    func testWhatItSays() {
        XCTAssertEqual(NotificationPlanner.body(from("u9", "hello"), me: "me"), "Bo: hello")
        var byId = from("u9", "look")
        byId.mentions = ["me"]
        XCTAssertEqual(NotificationPlanner.body(byId, me: "me"), "Bo mentioned you: look")
        var everyone = from("u9", "all")
        everyone.mentionEveryone = true
        XCTAssertEqual(NotificationPlanner.body(everyone, me: "me"), "Bo mentioned you: all")
        var file = from("u9", "")
        file.attachments = [FfiFileInfo(id: "f", filename: "a", originalName: "a", size: 1, contentType: "x", sha256: nil)]
        XCTAssertEqual(NotificationPlanner.body(file, me: "me"), "Bo sent a file")
    }

    func testALiveMessageRaisesItsChannelsBadgeAndNotifiesOnce() async {
        let client = FakeRealtime(channels: [channel("c1", "general"), channel("c2", "random")])
        let notifier = FakeNotifier()
        let model = ChannelsModel(client: client, me: "me", notifier: notifier, isActive: { true })
        await model.start()
        model.openChannel = "c1"
        model.handle(.messageNew(message: from("u9", "hi", channel: "c2")))
        model.handle(.messageNew(message: from("u9", "again", channel: "c2")))
        model.handle(.messageNew(message: from("u9", "seen", channel: "c1"))) // being read
        model.handle(.messageNew(message: from("me", "mine", channel: "c2")))
        XCTAssertEqual(model.channels.map(\.unread), [0, 2])
        XCTAssertEqual(notifier.posted.map(\.channel), ["c2", "c2"])
        XCTAssertEqual(notifier.posted.first?.title, "random")
        XCTAssertEqual(notifier.posted.last?.body, "Bo: again")
    }

    func testACacheRefreshOverridesTheLiveCount() async {
        let client = FakeRealtime(channels: [channel("c2", "random")])
        client.cached = [cachedChannel("c2", "random", unread: 5)]
        let model = ChannelsModel(client: client, me: "me", notifier: FakeNotifier(), isActive: { true })
        await model.start()
        model.handle(.messageNew(message: from("u9", "hi", channel: "c2")))
        await model.cacheChannelsChanged()
        XCTAssertEqual(model.channels.first?.unread, 5)
    }

    func testNothingCountsOrNotifiesWithTheIdUnknown() async {
        let client = FakeRealtime(channels: [channel("c2", "random")])
        let notifier = FakeNotifier()
        let model = ChannelsModel(client: client, me: nil, notifier: notifier, isActive: { true })
        await model.start()
        model.handle(.messageNew(message: from("u9", "hi", channel: "c2")))
        XCTAssertEqual(model.channels.first?.unread, 0)
        XCTAssertTrue(notifier.posted.isEmpty)
    }
}
