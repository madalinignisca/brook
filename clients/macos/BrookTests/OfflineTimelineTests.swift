import BrookCore
import XCTest

@testable import Brook

/// A channel read from this device's cache first (#62 spec item 1), and a merge whose result
/// doesn't depend on the order pages and events land in.
@MainActor
final class OfflineTimelineTests: XCTestCase {
    private func edited(_ id: String, _ body: String, at: String?) -> FfiMessage {
        var m = msg(id, body)
        m.editedAt = at
        return m
    }

    func testTheCacheIsDrawnFirstThenTheNetwork() async {
        let chat = FakeChat()
        chat.local = true
        chat.cachePages = [cachedPage([msg("m1", "cached")])]
        chat.pages = [[msg("m1", "cached"), msg("m2", "from the network")]]
        let t = TimelineModel(channelId: "c", client: chat)
        await t.load()
        XCTAssertEqual(t.messages.map(\.id), ["m1", "m2"])
        XCTAssertEqual(chat.cacheCalls.withLock { $0 }, ["cached:-"], "a complete page needs no load")
    }

    func testAnIncompleteHeadIsLoadedAndReadAgain() async {
        let chat = FakeChat()
        chat.local = true
        chat.cachePages = [cachedPage([], needsNetwork: true), cachedPage([msg("m1", "loaded")])]
        let t = TimelineModel(channelId: "c", client: chat)
        await t.load()
        XCTAssertEqual(chat.cacheCalls.withLock { $0 }, ["cached:-", "loadHead", "cached:-"])
        XCTAssertEqual(t.messages.map(\.id), ["m1"])
    }

    func testOlderPagesComeFromTheCacheAndAnEmptyCompletePageIsTheStart() async {
        let chat = FakeChat()
        chat.local = true
        chat.cachePages = [cachedPage([msg("m5", "newest")])]
        let t = TimelineModel(channelId: "c", client: chat)
        await t.load()
        chat.cachePages = [cachedPage([msg("m3", "a"), msg("m4", "b")])]
        await t.loadOlder()
        XCTAssertEqual(t.messages.map(\.id), ["m3", "m4", "m5"])
        XCTAssertFalse(t.atStart)
        chat.cachePages = [cachedPage([], needsNetwork: true), cachedPage([])]
        await t.loadOlder()
        XCTAssertEqual(chat.cacheCalls.withLock { Array($0.suffix(3)) }, ["cached:m3", "loadOlder", "cached:m3"])
        XCTAssertTrue(t.atStart)
    }

    func testAFailedLoadOfAnIncompletePageFallsBackToTheNetwork() async {
        let chat = FakeChat()
        chat.local = true
        chat.cachePages = [cachedPage([msg("m5", "newest")])]
        let t = TimelineModel(channelId: "c", client: chat)
        await t.load()
        chat.cachePages = [cachedPage([], needsNetwork: true)]
        chat.loadFails = true
        chat.pages = [[msg("m3", "from the network")]]
        await t.loadOlder()
        XCTAssertEqual(t.messages.map(\.id), ["m3", "m5"], "paging stuck on a failed load")
    }

    func testALoadThatBringsNothingAlsoFallsBackToTheNetwork() async {
        let chat = FakeChat()
        chat.local = true
        chat.cachePages = [cachedPage([msg("m5", "newest")])]
        let t = TimelineModel(channelId: "c", client: chat)
        await t.load()
        chat.cachePages = [cachedPage([], needsNetwork: true)] // before and after the load
        chat.pages = [[msg("m3", "from the network")]]
        await t.loadOlder()
        XCTAssertEqual(t.messages.map(\.id), ["m3", "m5"], "paging stuck after a load that brought nothing")
    }

    func testEditTimesWithAndWithoutFractionsOrderAsTimes() {
        XCTAssertTrue(TimelineModel.isNewer("2026-09-26T10:00:00.5Z", than: "2026-09-26T10:00:00Z"))
        XCTAssertFalse(TimelineModel.isNewer("2026-09-26T10:00:00Z", than: "2026-09-26T10:00:00.5Z"))
        XCTAssertTrue(TimelineModel.isNewer(nil, than: nil), "never-edited copies: the incoming one")
    }

    func testWithoutLocalDataItsTheNetworkAsBefore() async {
        let chat = FakeChat()
        chat.pages = [[msg("m1", "network")]]
        let t = TimelineModel(channelId: "c", client: chat)
        await t.load()
        XCTAssertEqual(t.messages.map(\.id), ["m1"])
        XCTAssertTrue(chat.cacheCalls.withLock { $0 }.isEmpty)
    }

    func testADeleteBeforeItsPageKeepsTheMessageDeleted() {
        let t = TimelineModel(channelId: "c", client: FakeChat())
        t.apply(.messageDelete(channelId: "c", messageId: "m1")) // not shown yet
        t.merge([msg("m1", "late copy")])
        XCTAssertEqual(t.messages.first?.deleted, true)
        XCTAssertEqual(t.messages.first?.body, "")
    }

    func testAStaleCopyNeverUndoesAnEditWhicheverLandsFirst() {
        let a = TimelineModel(channelId: "c", client: FakeChat())
        a.merge([edited("m1", "new", at: "2026-09-26T10:05:00Z")])
        a.merge([edited("m1", "old", at: nil)]) // a cached page read before the edit
        XCTAssertEqual(a.messages.first?.body, "new")
        let b = TimelineModel(channelId: "c", client: FakeChat())
        b.merge([edited("m1", "old", at: nil)])
        b.merge([edited("m1", "new", at: "2026-09-26T10:05:00Z")])
        XCTAssertEqual(b.messages.first?.body, "new")
        b.merge([edited("m1", "newer", at: "2026-09-26T10:06:00Z")])
        XCTAssertEqual(b.messages.first?.body, "newer")
    }

    func testARenameReachesCachedRows() async {
        let chat = FakeChat()
        chat.local = true
        chat.users = [FfiMember(id: "u", handle: "u", displayName: "Robert", role: nil)]
        let t = TimelineModel(channelId: "c", client: chat)
        t.merge([msg("m1", "hi")]) // stored as "U"
        XCTAssertEqual(t.authorName(t.messages[0]), "U")
        await t.refreshAuthors(["u"])
        XCTAssertEqual(t.authorName(t.messages[0]), "Robert")
    }

    func testTheNetworkErrorHidesWhileOfflineWithCachedMessages() async {
        let chat = FakeChat()
        chat.local = true
        chat.cachePages = [cachedPage([msg("m1", "cached")])]
        chat.historyFailure = LoginError.Network(message: "offline")
        let t = TimelineModel(channelId: "c", client: chat)
        await t.load()
        XCTAssertNotNil(t.error)
        let feed = CacheFeed(client: chat)
        feed.timeline = t
        feed.state(FfiCacheState(syncing: false, lastSyncedUnixMs: nil, offline: true))
        XCTAssertNil(t.visibleError, "an error over messages that are showing")
        t.offline = false
        XCTAssertNotNil(t.visibleError)
    }
}
