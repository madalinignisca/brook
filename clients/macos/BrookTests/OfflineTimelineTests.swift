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

    /// Offline with this Mac's cache: the newest page's network fetch fails, and the cached history is
    /// still paged (and ends at the start); only the network fallback is held back, with a Retry.
    func testCachedHistoryStillPagesBehindAFailedHead() async {
        let chat = FakeChat()
        chat.local = true
        chat.historyFailure = LoginError.Network(message: "offline")
        chat.cachePages = [cachedPage([msg("m5", "newest")])]
        let t = TimelineModel(channelId: "c", client: chat)
        await t.load()
        XCTAssertTrue(t.headFailed)
        XCTAssertTrue(t.offersOlder, "cached history can still be paged")
        chat.cachePages = [cachedPage([msg("m3", "a"), msg("m4", "b")])]
        await t.loadOlder()
        XCTAssertEqual(t.messages.map(\.id), ["m3", "m4", "m5"])
        chat.cachePages = [cachedPage([])]
        await t.loadOlder()
        XCTAssertTrue(t.atStart, "an empty complete cached page is the start, offline too")
        XCTAssertFalse(t.olderFailed)
    }

    /// Behind a failed head the cache cannot answer: the network is not asked, the Retry shows.
    func testWhenTheCacheCannotAnswerBehindAFailedHeadTheRetryShows() async {
        let chat = FakeChat()
        chat.local = true
        chat.historyFailure = LoginError.Network(message: "offline")
        chat.cachePages = [cachedPage([msg("m5", "newest")])]
        let t = TimelineModel(channelId: "c", client: chat)
        await t.load()
        let before = chat.historyCalls.withLock { $0 }
        chat.cachePages = [cachedPage([], needsNetwork: true)]
        chat.loadFails = true
        await t.loadOlder()
        XCTAssertTrue(t.olderFailed)
        XCTAssertEqual(chat.historyCalls.withLock { $0 }, before, "the network was asked behind a failed head")
        XCTAssertFalse(t.atStart)
        // The head comes back: the Retry is not left over (the loader asks again by itself).
        chat.historyFailure = nil
        chat.pages = [[msg("m6", "newer")]]
        t.apply(.ready)
        await settle("no retry on ready") { t.error == nil && !t.loading }
        XCTAssertFalse(t.headFailed)
        XCTAssertFalse(t.olderFailed, "a Retry left over from behind the failed head")
    }

    /// A newest-page fetch that starts while an older ask reads the cache decides whether the network may
    /// be asked for an older page: if it fails, none is asked (an empty answer would read as the start).
    func testAHeadFetchStartingDuringAnOlderCacheReadIsWaitedFor() async {
        let chat = FakeChat()
        chat.local = true
        chat.cachePages = [cachedPage([msg("m5", "newest")])]
        let t = TimelineModel(channelId: "c", client: chat)
        await t.load() // head fine
        let gate = Gate()
        chat.historyGate = gate
        chat.historyFailure = LoginError.Network(message: "offline")
        chat.cachePages = [cachedPage([], needsNetwork: true)]
        chat.loadFails = true
        let older = Task { await t.loadOlder() } // the cache cannot answer: the network fallback
        for _ in 0 ..< 3 { await Task.yield() }
        t.apply(.resync) // a head fetch begins (held in the gate)
        for _ in 0 ..< 20 { await Task.yield(); try? await Task.sleep(for: .milliseconds(5)) }
        gate.open()
        await older.value
        await settle("head did not finish") { !t.loading }
        XCTAssertFalse(t.atStart, "an empty older answer behind a failed head read as the start")
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

    private func authored(_ name: String?, _ handle: String?) -> FfiMessage {
        var m = msg("m1", "hi")
        (m.authorDisplayName, m.authorHandle) = (name, handle)
        return m
    }

    func testAuthorNameFollowsShowUsernames() async {
        let chat = FakeChat()
        chat.local = true
        chat.users = [FfiMember(id: "u", handle: "bob", displayName: "Robert", role: nil)]
        let t = TimelineModel(channelId: "c", client: chat)
        t.merge([authored("Bob", "bob")]) // stored as "Bob" / "bob"
        XCTAssertEqual(t.authorName(t.messages[0], showUsernames: false), "Bob")
        XCTAssertEqual(t.authorName(t.messages[0], showUsernames: true), "@bob")
        // A `Users` notice renames him: the cached row follows, the handle stays.
        await t.refreshAuthors(["u"])
        XCTAssertEqual(t.authorName(t.messages[0], showUsernames: false), "Robert")
        XCTAssertEqual(t.authorName(t.messages[0], showUsernames: true), "@bob")
    }

    func testAuthorNameWithoutAHandleOrAName() {
        let t = TimelineModel(channelId: "c", client: FakeChat())
        t.merge([authored("Bot", nil)])
        XCTAssertEqual(t.authorName(t.messages[0], showUsernames: true), "Bot")
        XCTAssertEqual(t.authorName(t.messages[0], showUsernames: false), "Bot")
        let gone = TimelineModel(channelId: "c", client: FakeChat())
        gone.merge([authored(nil, nil)])
        XCTAssertEqual(gone.authorName(gone.messages[0], showUsernames: true), "Someone")
        XCTAssertEqual(gone.authorName(gone.messages[0], showUsernames: false), "Someone")
    }

    /// The refreshed handle replaces the message's own, not only the name.
    func testARefreshReplacesTheHandleToo() async {
        let chat = FakeChat()
        chat.local = true
        chat.users = [FfiMember(id: "u", handle: "bobby", displayName: "Bob", role: nil)]
        let t = TimelineModel(channelId: "c", client: chat)
        t.merge([authored("Bob", "bob")])
        await t.refreshAuthors(["u"])
        XCTAssertEqual(t.authorName(t.messages[0], showUsernames: true), "@bobby")
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

    // ---- A failed head load is retried when the connection returns.

    private func failedHead(_ chat: FakeChat, isActive: @escaping @MainActor () -> Bool = { true }) async -> TimelineModel {
        chat.historyFailure = LoginError.Network(message: "offline")
        let t = TimelineModel(channelId: "c", client: chat, isActive: isActive)
        await t.load()
        XCTAssertNotNil(t.error)
        return t
    }

    /// Wait until `done` holds (the retry runs in a task), failing after a while.
    private func settle(_ what: String, _ done: () -> Bool) async {
        for _ in 0..<200 where !done() { try? await Task.sleep(for: .milliseconds(10)) }
        XCTAssertTrue(done(), what)
    }

    func testReadyRetriesAFailedHeadLoad() async {
        let chat = FakeChat()
        let t = await failedHead(chat)
        chat.historyFailure = nil
        chat.pages = [[msg("m1", "back")]]
        t.apply(.ready)
        await settle("no retry on ready") { t.error == nil && !t.loading }
        XCTAssertFalse(t.headFailed, "a loader hidden for good behind a head that recovered")
        XCTAssertFalse(t.olderFailed, "a Retry left over from behind the failed head")
        XCTAssertEqual(t.messages.map(\.id), ["m1"])
        XCTAssertEqual(chat.historyCalls.withLock { $0 }, 2)
    }

    func testOfflineGoingFalseRetriesAFailedHeadLoad() async {
        let chat = FakeChat()
        let t = await failedHead(chat)
        t.offline = true
        chat.historyFailure = nil
        chat.pages = [[msg("m1", "back")]]
        t.offline = false
        await settle("no retry when offline cleared") { t.error == nil && !t.loading }
        XCTAssertEqual(t.messages.map(\.id), ["m1"])
    }

    func testReadyWithoutAnErrorFetchesNothing() async {
        let chat = FakeChat()
        chat.pages = [[msg("m1", "a")]]
        let t = TimelineModel(channelId: "c", client: chat)
        await t.load()
        t.apply(.ready)
        t.offline = true
        t.offline = false
        try? await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(chat.historyCalls.withLock { $0 }, 1)
    }

    func testAFailingRetryKeepsTheErrorAndDoesNotLoop() async {
        let chat = FakeChat()
        let t = await failedHead(chat)
        t.apply(.ready)
        await settle("retry didn't run") { chat.historyCalls.withLock { $0 } == 2 && !t.loading }
        try? await Task.sleep(for: .milliseconds(200)) // a loop would keep calling
        XCTAssertEqual(chat.historyCalls.withLock { $0 }, 2)
        XCTAssertNotNil(t.error)
    }

    func testReadyAndOnlineTogetherRunOneFetch() async {
        let chat = FakeChat()
        let t = await failedHead(chat)
        t.offline = true
        chat.historyGate = Gate()
        t.apply(.ready)
        t.offline = false // while the first is in flight
        await settle("retry didn't start") { chat.historyCalls.withLock { $0 } >= 2 }
        try? await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(chat.historyCalls.withLock { $0 }, 2, "two fetches at once")
        chat.historyGate?.open()
        await settle("retry didn't finish") { !t.loading }
    }

    /// A resync while a retry's fetch is in flight neither runs beside it nor is lost: one
    /// more head fetch follows when it ends.
    func testResyncDuringARetryWaitsThenFetchesAgain() async {
        let chat = FakeChat()
        let t = await failedHead(chat)
        chat.historyGate = Gate()
        t.apply(.ready) // the retry, held at the gate
        await settle("retry didn't start") { chat.historyCalls.withLock { $0 } == 2 }
        t.apply(.resync)
        try? await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(chat.historyCalls.withLock { $0 }, 2, "a second fetch beside the first")
        chat.historyFailure = nil
        chat.historyGate?.open()
        await settle("resync's fetch didn't follow") { chat.historyCalls.withLock { $0 } == 3 && !t.loading }
        try? await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(chat.historyCalls.withLock { $0 }, 3)
        XCTAssertNil(t.error)
    }

    /// Several resyncs while one fetch is in flight are one more fetch, not one each.
    func testResyncsDuringAFetchCoalesce() async {
        let chat = FakeChat()
        let t = await failedHead(chat)
        chat.historyGate = Gate()
        t.apply(.resync)
        await settle("resync didn't start") { chat.historyCalls.withLock { $0 } == 2 }
        t.apply(.resync)
        t.apply(.resync)
        chat.historyGate?.open()
        await settle("follow-up didn't run") { chat.historyCalls.withLock { $0 } == 3 && !t.loading }
        try? await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(chat.historyCalls.withLock { $0 }, 3)
    }

    // ---- Reading what a coalesced head fetch brought.

    /// A retry that succeeds marks the newest message read, as `load()` does.
    func testSuccessfulRetryMarksTheNewestRead() async {
        let chat = FakeChat()
        let t = await failedHead(chat)
        chat.historyFailure = nil
        chat.pages = [[msg("m1", "a"), msg("m2", "b")]]
        t.apply(.ready)
        await settle("retry didn't mark read") { !chat.read.isEmpty }
        XCTAssertEqual(chat.read, ["m2"])
    }

    /// With the app in the background the retry's read is owed, not sent.
    func testRetryInTheBackgroundOwesTheReadInstead() async {
        let chat = FakeChat()
        let t = await failedHead(chat, isActive: { false })
        chat.historyFailure = nil
        chat.pages = [[msg("m1", "a"), msg("m2", "b")]]
        t.apply(.ready)
        await settle("retry didn't finish") { t.error == nil && !t.loading && t.messages.count == 2 }
        try? await Task.sleep(for: .milliseconds(100))
        XCTAssertTrue(chat.read.isEmpty, "read while the app was in the background")
        XCTAssertTrue(t.readOwed)
        t.appBecameActive()
        await settle("owed read not sent") { !chat.read.isEmpty }
        XCTAssertEqual(chat.read, ["m2"])
    }

    /// A retry that fails marks nothing.
    func testFailedRetryMarksNothingRead() async {
        let chat = FakeChat()
        let t = await failedHead(chat)
        t.apply(.ready)
        await settle("retry didn't run") { chat.historyCalls.withLock { $0 } == 2 && !t.loading }
        try? await Task.sleep(for: .milliseconds(100))
        XCTAssertTrue(chat.read.isEmpty)
    }

    /// A resync that starts a head fetch while `load()` reads the cache: `load()` waits for
    /// that fetch (not just queues behind it) and marks the page's newest read, not the
    /// cached one.
    func testLoadWaitsForAResyncsHeadFetchAndReadsItsNewest() async {
        let chat = FakeChat()
        chat.local = true
        chat.cachePages = [cachedPage([msg("m1", "stale")])]
        chat.pages = [[msg("m1", "a"), msg("m2", "b")]]
        let cacheGate = Gate() // the fake takes it when it holds the read
        chat.cacheGate = cacheGate
        chat.historyGate = Gate()
        let t = TimelineModel(channelId: "c", client: chat, isActive: { true })
        var finished = false
        let loading = Task { await t.load(); finished = true }
        await settle("load didn't reach the cache") { chat.cacheCalls.withLock { $0.contains("cached:-") } }
        t.apply(.resync) // its fetch is held at the history gate
        await settle("resync didn't fetch") { chat.historyCalls.withLock { $0 } == 1 }
        cacheGate.open()
        try? await Task.sleep(for: .milliseconds(100))
        XCTAssertFalse(finished, "load returned before the head page was merged")
        XCTAssertTrue(chat.read.isEmpty, "read before the head page arrived: \(chat.read)")
        chat.historyGate?.open()
        await loading.value
        XCTAssertEqual(t.messages.map(\.id), ["m1", "m2"])
        XCTAssertEqual(chat.read, ["m2"])
    }
}
