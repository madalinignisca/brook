import BrookCore
import XCTest
@testable import Brook

private func person(_ id: String, _ handle: String, _ name: String) -> FfiMember {
    FfiMember(id: id, handle: handle, displayName: name, role: nil)
}

private func dm(_ id: String, with other: FfiMember) -> FfiChannel {
    FfiChannel(id: id, kind: "dm", name: nil, archived: false, topic: nil, isPublic: false, unreadMentions: 0,
               members: [person("me", "me", "Me"), other], ownerOffers: [])
}

extension XCTestCase {
    /// Defaults of its own, so a test that opens a channel never saves its rank into the
    /// app's real preferences.
    @MainActor func isolatedDefaults() -> UserDefaults {
        let suite = "brook.tests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        addTeardownBlock { defaults.removePersistentDomain(forName: suite) }
        return defaults
    }
}

/// The sidebar's last-used order (#219): when the list re-sorts, and what it never moves.
/// Channels alpha, bravo and charlie (c1, c2, c3) read `#alpha` < `#bravo` < `#charlie`, so
/// with no other key the order is c1, c2, c3.
@MainActor
final class ChannelOrderTests: XCTestCase {
    private var defaults: UserDefaults!
    private var suite: String!

    override func setUp() {
        suite = "brook.tests.\(UUID().uuidString)"
        defaults = UserDefaults(suiteName: suite)
    }

    override func tearDown() {
        defaults.removePersistentDomain(forName: suite)
    }

    private let three = [channel("c1", "alpha"), channel("c2", "bravo"), channel("c3", "charlie")]

    private func cache(_ keys: [String: String?]) -> [FfiCachedChannel] {
        keys.sorted { $0.key < $1.key }.map { cachedChannel($0.key, "n", lastMessageId: $0.value) }
    }

    private func model(_ client: FakeRealtime, me: String? = "me") -> ChannelsModel {
        ChannelsModel(client: client, me: me, isActive: { true }, defaults: defaults)
    }

    private func started(_ list: [FfiChannel]? = nil, cached: [FfiCachedChannel]? = nil) async -> (FakeRealtime, ChannelsModel) {
        let client = FakeRealtime(channels: list ?? three)
        client.cached = cached
        let model = model(client)
        await model.start()
        return (client, model)
    }

    private func live(_ model: ChannelsModel, _ id: String, in channel: String) {
        model.handle(.messageNew(message: msg(id, "hi", channel: channel)))
    }

    private func ids(_ model: ChannelsModel) -> [String] { model.channels.map(\.id) }

    func testAListLoadSortsByTheCachesNewestMessageWithNoneLast() async {
        // Out of order on purpose; c2 has no message at all.
        let (_, model) = await started(cached: cache(["c3": "m1", "c1": "m3", "c2": nil]))
        XCTAssertEqual(ids(model), ["c1", "c3", "c2"])
    }

    func testChannelsComeBeforeDirectMessages() async {
        let list = three + [dm("d1", with: person("b", "bob", "Bob"))]
        let cached = [cachedChannel("d1", "n", lastMessageId: "m9", kind: "dm"), cachedChannel("c1", "n", lastMessageId: "m1")]
        let (_, model) = await started(list, cached: cached)
        XCTAssertEqual(ids(model), ["c1", "c2", "c3", "d1"], "a DM with the newest message stays below the channels")
    }

    func testALiveMessageMovesItsConversationToTheTopOfItsSection() async {
        let (_, model) = await started()
        XCTAssertEqual(ids(model), ["c1", "c2", "c3"])
        live(model, "m5", in: "c3")
        XCTAssertEqual(ids(model), ["c3", "c1", "c2"])
        live(model, "m6", in: "c2")
        XCTAssertEqual(ids(model), ["c2", "c3", "c1"])
        XCTAssertEqual(model.channels.first { $0.id == "c3" }?.unread, 1, "the badge is unchanged by the order")
    }

    func testAMessageNotNewerThanItsKeyMovesNothing() async {
        let (_, model) = await started(cached: cache(["c1": "m5", "c2": "m5", "c3": nil]))
        model.openChannel = "c2" // a click: ranks c2 above c1 at the next re-sort
        live(model, "m5", in: "c1") // not newer than m5
        XCTAssertEqual(ids(model), ["c1", "c2", "c3"])
        live(model, "m4", in: "c1")
        XCTAssertEqual(ids(model), ["c1", "c2", "c3"])
    }

    func testAClickMovesNoRowAndTheNextReSortAppliesIt() async {
        let (_, model) = await started(cached: cache(["c1": "m5", "c2": "m5", "c3": "m5"]))
        XCTAssertEqual(ids(model), ["c1", "c2", "c3"])
        model.openChannel = "c3"
        XCTAssertEqual(ids(model), ["c1", "c2", "c3"], "a click moved a row")
        await model.reloadList()
        XCTAssertEqual(ids(model), ["c3", "c1", "c2"], "a re-sort applies the whole rule, the click included")
    }

    func testTheBackFillOfTheOpenConversationMovesNoRowButItsKeyCounts() async {
        let (client, model) = await started(cached: cache(["c1": "m5", "c2": "m5", "c3": "m1"]))
        model.openChannel = "c3"
        client.cached = cache(["c1": "m5", "c2": "m5", "c3": "m8"]) // the history back-fill opening started
        await model.cacheChannelsChanged()
        XCTAssertEqual(ids(model), ["c1", "c2", "c3"], "the back-fill moved the open conversation")
        live(model, "m4", in: "c2") // a re-sort for another reason applies the raised key
        XCTAssertEqual(ids(model), ["c1", "c2", "c3"], "m4 is older than c2's key: no re-sort")
        live(model, "m9", in: "c2")
        XCTAssertEqual(ids(model), ["c2", "c3", "c1"], "c3's m8 counts at the next re-sort")
    }

    func testACacheNoticeMovingAnotherConversationReSorts() async {
        let (client, model) = await started(cached: cache(["c1": "m5", "c2": "m5", "c3": nil]))
        model.openChannel = "c2"
        client.cached = cache(["c1": "m6", "c2": "m5", "c3": nil])
        await model.cacheChannelsChanged()
        XCTAssertEqual(ids(model), ["c1", "c2", "c3"])
        client.cached = cache(["c1": "m6", "c2": "m5", "c3": "m7"])
        await model.cacheChannelsChanged()
        XCTAssertEqual(ids(model), ["c3", "c1", "c2"])
    }

    func testANoticeThatMovesNoKeyReSortsNothing() async {
        let (client, model) = await started(cached: cache(["c1": "m5", "c2": "m5", "c3": nil]))
        model.openChannel = "c2"
        model.openChannel = nil
        client.cached = cache(["c1": "m5", "c2": "m5", "c3": nil])
        await model.cacheChannelsChanged() // a count changed, say, not a key
        XCTAssertEqual(ids(model), ["c1", "c2", "c3"], "c2's click showed without a re-sort")
    }

    func testABackFillFinishingAfterAnotherClickMovesNoRow() async {
        let (client, model) = await started(cached: cache(["c1": "m1", "c2": "m5", "c3": nil]))
        XCTAssertEqual(ids(model), ["c2", "c1", "c3"])
        model.openChannel = "c1"
        model.openChannel = "c2" // c1's back-fill is still running
        client.cached = cache(["c1": "m9", "c2": "m5", "c3": nil])
        await model.cacheChannelsChanged()
        XCTAssertEqual(ids(model), ["c2", "c1", "c3"], "c1's own back-fill moved it")
    }

    func testTheCacheNoticeBeforeItsLiveMessageReSortsOnce() async {
        let (client, model) = await started(cached: cache(["c1": "m5", "c2": "m5", "c3": nil]))
        client.cached = cache(["c1": "m5", "c2": "m5", "c3": "m8"])
        await model.cacheChannelsChanged()
        XCTAssertEqual(ids(model), ["c3", "c1", "c2"])
        model.openChannel = "c2" // would show at a second re-sort
        live(model, "m8", in: "c3")
        XCTAssertEqual(ids(model), ["c3", "c1", "c2"], "the live message re-sorted a second time")
    }

    func testTheCacheNoticeAfterItsLiveMessageForTheOpenConversationStillReSorts() async {
        let (client, model) = await started(cached: cache(["c1": "m5", "c2": "m5", "c3": "m1"]))
        model.openChannel = "c3"
        client.cached = cache(["c1": "m5", "c2": "m5", "c3": "m8"])
        await model.cacheChannelsChanged() // advances c3's key quietly
        XCTAssertEqual(ids(model), ["c1", "c2", "c3"])
        live(model, "m8", in: "c3") // not swallowed by the key the notice raised
        XCTAssertEqual(ids(model), ["c3", "c1", "c2"])
    }

    func testTheLiveMessageBeforeItsCacheNoticeReSortsOnce() async {
        let (client, model) = await started(cached: cache(["c1": "m5", "c2": "m5", "c3": nil]))
        live(model, "m8", in: "c3")
        XCTAssertEqual(ids(model), ["c3", "c1", "c2"])
        model.openChannel = "c2"
        client.cached = cache(["c1": "m5", "c2": "m5", "c3": "m8"])
        await model.cacheChannelsChanged()
        XCTAssertEqual(ids(model), ["c3", "c1", "c2"], "the notice re-sorted a second time")
    }

    /// Display-name order (Amy, Zed) differs from handle order (@amy is Zed): the preference
    /// is on, and the order is still by name.
    func testTheOrderIgnoresThePreference() async {
        let list = [dm("d2", with: person("z", "amy", "Zed")), dm("d1", with: person("a", "zoe", "Amy"))]
        let client = FakeRealtime(channels: list)
        let model = model(client)
        model.showUsernames = true
        await model.start()
        XCTAssertEqual(model.channels.map(\.label), ["@zoe", "@amy"])
        XCTAssertEqual(ids(model), ["d1", "d2"], "sorted by the shown @handle, not the name")
    }

    func testLiveKeysSurviveAListReloadWithoutLocalData() async {
        let (client, model) = await started() // no cache
        live(model, "m5", in: "c3")
        XCTAssertEqual(ids(model), ["c3", "c1", "c2"])
        await model.reloadList()
        XCTAssertEqual(ids(model), ["c3", "c1", "c2"])
        client.channels = three // the server's order does not matter either
        await model.reloadList()
        XCTAssertEqual(ids(model), ["c3", "c1", "c2"])
    }

    func testACacheBehindALiveKeyNeverMovesItBack() async {
        let (client, model) = await started(cached: cache(["c1": nil, "c2": nil, "c3": nil]))
        live(model, "m8", in: "c3")
        client.cached = cache(["c1": "m5", "c2": nil, "c3": "m2"]) // the cache has not caught up
        await model.reloadList()
        XCTAssertEqual(ids(model), ["c3", "c1", "c2"], "the older cached key replaced the live one")
        await model.cacheChannelsChanged()
        XCTAssertEqual(ids(model), ["c3", "c1", "c2"])
    }

    func testSavedRanksSurviveARestartAndANewClickRanksAboveThem() async {
        let (_, first) = await started()
        first.openChannel = "c1"
        first.openChannel = "c2"
        let (_, second) = await started()
        XCTAssertEqual(ids(second), ["c2", "c1", "c3"], "the saved ranks order the first list")
        second.openChannel = "c3"
        await second.reloadList()
        XCTAssertEqual(ids(second), ["c3", "c2", "c1"], "the new click ranks above the saved ones")
    }

    func testRanksOfChannelsGoneFromTheListAreDroppedAndNothingIsSavedWithoutAnAccount() async {
        let (client, first) = await started()
        first.openChannel = "c1"
        first.openChannel = "c3"
        client.channels = [channel("c1", "alpha")]
        await first.reloadList()
        let (_, second) = await started([channel("c1", "alpha"), channel("c3", "charlie")])
        XCTAssertEqual(ids(second), ["c1", "c3"], "c3's rank outlived its channel")

        let anonymous = model(FakeRealtime(channels: three), me: nil)
        let before = defaults.dictionaryRepresentation().keys.sorted()
        await anonymous.start()
        anonymous.openChannel = "c2"
        XCTAssertEqual(defaults.dictionaryRepresentation().keys.sorted(), before, "saved a rank for nobody")
    }

    func testAMessageForAnUnknownChannelAppliesWhenItsRowArrives() async {
        let (client, model) = await started()
        live(model, "m9", in: "c9")
        XCTAssertEqual(ids(model), ["c1", "c2", "c3"])
        client.channels = three + [channel("c9", "zulu")]
        client.deliver(.channelUpdate(channel: channel("c9", "zulu")))
        for _ in 0..<50 where !ids(model).contains("c9") {
            await drainMain()
            await Task.yield()
        }
        XCTAssertEqual(ids(model), ["c9", "c1", "c2", "c3"])
    }
}
