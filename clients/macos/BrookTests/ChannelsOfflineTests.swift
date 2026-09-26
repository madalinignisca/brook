import BrookCore
import XCTest

@testable import Brook

func cachedChannel(_ id: String, _ name: String, unread: Int64 = 0) -> FfiCachedChannel {
    FfiCachedChannel(id: id, kind: "public", name: name, archived: false, unreadCount: unread, members: [], ownerOffers: [])
}

/// The channel list offline (#62 spec item 1): network first, the cache when it fails, the
/// cache's unread counts either way, and the cache's notices.
@MainActor
final class ChannelsOfflineTests: XCTestCase {
    func testARealtimeFailureStillListsTheChannels() async {
        let client = FakeRealtime(channels: [channel("c1", "general")])
        client.realtimeFails = true
        let model = ChannelsModel(client: client)
        await model.start()
        XCTAssertEqual(model.channels.map(\.id), ["c1"])
    }

    func testTheCacheFillsTheListWhenTheNetworkFails() async {
        let client = FakeRealtime(channels: [])
        client.listFails = true
        client.cached = [cachedChannel("c1", "general", unread: 3)]
        let model = ChannelsModel(client: client)
        await model.start()
        XCTAssertEqual(model.channels.map(\.id), ["c1"])
        XCTAssertEqual(model.channels.first?.unread, 3)
        XCTAssertNil(model.error)
    }

    func testBothFailingShowsTheError() async {
        let client = FakeRealtime(channels: [])
        client.listFails = true
        let model = ChannelsModel(client: client)
        await model.start()
        XCTAssertEqual(model.error, "Couldn't load channels.")
    }

    func testANetworkListTakesTheCachesUnreadCountsAndTheOpenOneStaysZero() async {
        let client = FakeRealtime(channels: [channel("c1", "general"), channel("c2", "random")])
        client.cached = [cachedChannel("c1", "general", unread: 2), cachedChannel("c2", "random", unread: 5)]
        let model = ChannelsModel(client: client)
        model.openChannel = "c2"
        await model.start()
        XCTAssertEqual(model.channels.map(\.unread), [2, 0])
        client.cached = [cachedChannel("c1", "general", unread: 4), cachedChannel("c2", "random", unread: 9)]
        await model.cacheChannelsChanged()
        XCTAssertEqual(model.channels.map(\.unread), [4, 0], "the open channel's badge moved")
    }

    func testARemovedChannelGoesAndAnOlderReadCantBringItBack() async {
        let client = FakeRealtime(channels: [channel("c1", "general"), channel("c2", "random")])
        let model = ChannelsModel(client: client)
        await model.start()
        model.openChannel = "c2"
        let gate = Gate()
        client.listGate = gate
        let older = Task { await model.reloadList() } // started before the removal
        for _ in 0 ..< 20 { await Task.yield() }
        model.cacheRemoved(["c2"])
        XCTAssertEqual(model.channels.map(\.id), ["c1"])
        XCTAssertEqual(model.closed, "c2")
        gate.open()
        await older.value
        XCTAssertEqual(model.channels.map(\.id), ["c1"], "a read from before the removal brought it back")
    }

    func testAChannelReaddedLaterComesBack() async {
        let client = FakeRealtime(channels: [channel("c1", "general"), channel("c2", "random")])
        let model = ChannelsModel(client: client)
        await model.start()
        model.cacheRemoved(["c2"])
        await model.reloadList() // after the removal: re-added since
        XCTAssertEqual(model.channels.map(\.id), ["c1", "c2"], "a re-added channel stayed hidden")
    }
}
