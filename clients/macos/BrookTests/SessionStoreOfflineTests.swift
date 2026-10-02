import BrookCore
@testable import Brook
import Foundation
import XCTest

/// Local data per signed-in client (#62 spec items 5 and 6, plan step 6).
@MainActor
final class SessionStoreOfflineTests: XCTestCase {
    private var suite: String!
    private var defaults: UserDefaults!

    override func setUp() async throws {
        suite = "brook.tests.\(UUID().uuidString)"
        defaults = UserDefaults(suiteName: suite)
    }

    override func tearDown() async throws {
        defaults.removePersistentDomain(forName: suite)
    }

    private func store(_ fake: FakeClient, wait: Duration = .seconds(30)) -> SessionStore {
        let recorder = FactoryRecorder { fake }
        let settings = Settings(defaults: defaults, environment: [:])
        return SessionStore(settings: settings, persistence: .on(slot: UnusedSlot(), dataDir: "/data"),
                            makeClient: recorder.factory, localDataWait: wait)
    }

    private func signedIn() -> FakeClient { FakeClient(result: .success(.loggedIn(session: aliceSession))) }

    private func until(_ what: String, timeout: TimeInterval = 3, _ ok: () -> Bool) async {
        let end = Date().addingTimeInterval(timeout)
        while !ok() {
            if Date() > end { return XCTFail("timed out waiting for \(what)") }
            try? await Task.sleep(for: .milliseconds(10))
        }
    }

    func testItSubscribesThenEnablesThenReadsLosses() async {
        let fake = signedIn()
        let store = store(fake)
        await store.signIn(server: "https://h", handle: "alice", password: "pw")
        await until("local data on") { store.localData == .on }
        XCTAssertEqual(fake.localCalls.withLock { $0 },
                       ["subscribeCacheEvents", "subscribeCacheState", "enable", "enabled", "outboxLost"])
        XCTAssertNotNil(store.feed)
        XCTAssertTrue(store.offersRemoval)
    }

    func testAFailedSignInEnablesNothing() async {
        let fake = FakeClient(result: .failure(.Api(code: "auth.invalid_credentials", message: "")))
        let store = store(fake)
        await store.signIn(server: "https://h", handle: "alice", password: "pw")
        try? await Task.sleep(for: .milliseconds(100))
        XCTAssertTrue(fake.localCalls.withLock { $0 }.isEmpty)
        XCTAssertEqual(store.localData, .off)
    }

    func testAnEnableThatEndsAfterSignOutIsDropped() async {
        let fake = signedIn()
        fake.enableGated.withLock { $0 = true }
        let store = store(fake)
        await store.signIn(server: "https://h", handle: "alice", password: "pw")
        await until("enable started") { fake.localCalls.withLock { $0 }.contains("enable") }
        store.signOut()
        fake.enableGate.open()
        await until("enable ended") { fake.localCalls.withLock { $0 }.contains("enabled") }
        try? await Task.sleep(for: .milliseconds(50))
        XCTAssertNil(store.feed, "a signed-out store kept a feed")
        XCTAssertEqual(store.localData, .off)
        XCTAssertFalse(fake.localCalls.withLock { $0 }.contains("outboxLost"))
    }

    func testASignInWaitsForAnErasingSignOut() async {
        let fake = signedIn()
        let store = store(fake)
        await store.signIn(server: "https://h", handle: "alice", password: "pw")
        await until("on") { store.localData == .on }
        fake.forgetGated.withLock { $0 = true }
        store.signOut(removeData: true)
        await until("erasing") { fake.localCalls.withLock { $0 }.contains("forget") }
        await store.signIn(server: "https://h", handle: "alice", password: "pw")
        try? await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(fake.localCalls.withLock { $0 }.filter { $0 == "enable" }.count, 1,
                       "enabled again while the erase was running")
        fake.forgetGate.open()
        await until("enabled again") { fake.localCalls.withLock { $0 }.filter { $0 == "enable" }.count == 2 }
        let calls = fake.localCalls.withLock { $0 }
        XCTAssertLessThan(calls.firstIndex(of: "forgot")!, calls.lastIndex(of: "enable")!)
    }

    func testAHungSignOutLeavesTheNextSignInOnlineOnlyNotWaitingForever() async {
        let fake = signedIn()
        let store = store(fake, wait: .milliseconds(300))
        await store.signIn(server: "https://h", handle: "alice", password: "pw")
        await until("on") { store.localData == .on }
        fake.forgetGated.withLock { $0 = true } // never released
        store.signOut(removeData: true)
        await store.signIn(server: "https://h", handle: "alice", password: "pw")
        await until("gave up waiting", timeout: 2) { store.localData == .off }
        XCTAssertEqual(fake.localCalls.withLock { $0 }.filter { $0 == "enable" }.count, 1)
        fake.forgetGate.open()
    }

    func testASignOutWhileEnablingOffersRemovalAndRemovesAfterTheEnable() async {
        let fake = signedIn()
        fake.enableGated.withLock { $0 = true }
        let store = store(fake)
        await store.signIn(server: "https://h", handle: "alice", password: "pw")
        await until("enabling") { fake.localCalls.withLock { $0 }.contains("enable") }
        XCTAssertTrue(store.offersRemoval, "no sheet while the stores were opening")
        store.signOut(removeData: true)
        try? await Task.sleep(for: .milliseconds(100))
        XCTAssertFalse(fake.localCalls.withLock { $0 }.contains("forget"), "removed before the enable ended")
        fake.enableGate.open()
        await until("removed") { fake.localCalls.withLock { $0 }.contains("forgot") }
    }

    private func warning(removalFails: Bool, complete: Bool) async -> String? {
        defaults.removeObject(forKey: Settings.lastServerKey) // each run a fresh launch, not a restore
        let fake = signedIn()
        let store = store(fake)
        await store.signIn(server: "https://h", handle: "alice", password: "pw")
        await until("on") { store.localData == .on }
        fake.forgetFails.withLock { $0 = removalFails }
        fake.setSignOutComplete(complete)
        store.signOut(removeData: true)
        await until("signed out") { fake.localCalls.withLock { $0 }.contains("forgot") }
        try? await Task.sleep(for: .milliseconds(50))
        return store.signOutWarning
    }

    func testAFailedRemovalAndAnIncompleteSignOutAreBothSaid() async {
        let both = await warning(removalFails: true, complete: false)
        XCTAssertEqual(both, SessionStore.Message.removalAndSignOutIncomplete)
        let removal = await warning(removalFails: true, complete: true)
        XCTAssertEqual(removal, SessionStore.Message.removalIncomplete)
        let fine = await warning(removalFails: false, complete: true)
        XCTAssertNil(fine)
    }

    // ---- Opened-order ranks go with the user's local data ----

    private func seedRanks(_ users: String...) {
        for u in users { defaults.set(["c1": 1], forKey: "ChannelOpenedRanks.\(u)") }
    }

    private func hasRanks(_ user: String) -> Bool { defaults.object(forKey: "ChannelOpenedRanks.\(user)") != nil }

    private func signOutAfterSignIn(removeData: Bool, removalFails: Bool = false) async {
        let fake = signedIn()
        let store = store(fake)
        await store.signIn(server: "https://h", handle: "alice", password: "pw")
        await until("on") { store.localData == .on }
        fake.forgetFails.withLock { $0 = removalFails }
        seedRanks(alice.id, "other")
        store.signOut(removeData: removeData)
        await until("removal ran") { !removeData || fake.localCalls.withLock { $0 }.contains("forgot") }
        try? await Task.sleep(for: .milliseconds(100))
    }

    func testRemovingDataDropsThatUsersRanksAndKeepsAnothers() async {
        await signOutAfterSignIn(removeData: true)
        XCTAssertFalse(hasRanks(alice.id), "the signed-out user's ranks were left behind")
        XCTAssertTrue(hasRanks("other"), "another user's ranks went too")
    }

    func testKeepingDataKeepsTheRanks() async {
        await signOutAfterSignIn(removeData: false)
        XCTAssertTrue(hasRanks(alice.id), "a keep-data sign-out dropped the ranks")
    }

    func testTheRanksGoEvenWhenTheRemovalFails() async {
        await signOutAfterSignIn(removeData: true, removalFails: true)
        XCTAssertFalse(hasRanks(alice.id), "a failed removal left the ranks")
    }

    func testCleaningUpOtherAccountsDropsTheirRanksNotTheCurrentOnes() async {
        let chat = FakeChat()
        chat.local = true
        chat.others = [FfiLocalUser(origin: "o", userId: "u9", unsent: 0),
                       FfiLocalUser(origin: "o", userId: "u8", unsent: 0)]
        seedRanks("u9", "u8", "me")
        let feed = CacheFeed(client: chat, defaults: defaults)
        feed.state(FfiCacheState(syncing: false, lastSyncedUnixMs: 1, offline: false))
        for _ in 0 ..< 300 where chat.wiped.withLock({ $0 }) == 0 { try? await Task.sleep(for: .milliseconds(10)) }
        try? await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(chat.wiped.withLock { $0 }, 1)
        XCTAssertFalse(hasRanks("u9"))
        XCTAssertFalse(hasRanks("u8"))
        XCTAssertTrue(hasRanks("me"), "the current user's ranks went")
    }

    func testEveryUnfinishedSignOutIsWaitedForNotOnlyTheLatest() async {
        let fake = signedIn()
        let store = store(fake, wait: .milliseconds(300))
        await store.signIn(server: "https://h", handle: "alice", password: "pw")
        await until("on") { store.localData == .on }
        fake.forgetGated.withLock { $0 = true } // this erase hangs
        store.signOut(removeData: true)
        await store.signIn(server: "https://h", handle: "alice", password: "pw") // A: gives up
        await until("A gave up", timeout: 2) { store.localData == .off }
        store.signOut(removeData: false)
        await store.signIn(server: "https://h", handle: "alice", password: "pw") // B
        try? await Task.sleep(for: .milliseconds(150))
        XCTAssertEqual(fake.localCalls.withLock { $0 }.filter { $0 == "enable" }.count, 1,
                       "B enabled while the first erase was still running")
        fake.forgetGate.open()
    }

    func testAFailedEnableIsOnlineOnlyWithNoSheetAndNoFeed() async {
        let fake = signedIn()
        fake.enableResult.withLock { $0 = false }
        let store = store(fake)
        await store.signIn(server: "https://h", handle: "alice", password: "pw")
        await until("failed") { store.localData == .failed }
        XCTAssertFalse(store.offersRemoval, "a sheet offering to remove data that isn't there")
        XCTAssertNil(store.feed)
    }

    func testAKeepDataSignOutDoesntWaitForAnEnable() async {
        let fake = signedIn()
        fake.enableGated.withLock { $0 = true } // never released in time
        let store = store(fake)
        await store.signIn(server: "https://h", handle: "alice", password: "pw")
        await until("enabling") { fake.localCalls.withLock { $0 }.contains("enable") }
        store.signOut(removeData: false)
        await until("logged out", timeout: 1) { fake.logouts == 1 }
        fake.enableGate.open()
    }

    func testACancelledWaitEndsAtOnce() async {
        let never = Task<Void, Never> { await withCheckedContinuation { (_: CheckedContinuation<Void, Never>) in } }
        let started = Date()
        let waiter = Task { await SessionStore.waitAll([never], upTo: .seconds(30)) }
        try? await Task.sleep(for: .milliseconds(50))
        waiter.cancel()
        let settled = await waiter.value
        XCTAssertFalse(settled)
        XCTAssertLessThan(Date().timeIntervalSince(started), 2, "a cancelled wait ran on")
    }

    func testEitherSignOutClosesTheStoresBeforeTheNextSignInOpensThem() async {
        for removeData in [false, true] {
            defaults.removeObject(forKey: Settings.lastServerKey)
            let fake = signedIn()
            let store = store(fake)
            await store.signIn(server: "https://h", handle: "alice", password: "pw")
            await until("on") { store.localData == .on }
            fake.closeGated.withLock { $0 = true }
            store.signOut(removeData: removeData)
            await until("closing") { fake.localCalls.withLock { $0 }.contains("close") }
            await store.signIn(server: "https://h", handle: "alice", password: "pw")
            try? await Task.sleep(for: .milliseconds(100))
            XCTAssertEqual(fake.localCalls.withLock { $0 }.filter { $0 == "enable" }.count, 1,
                           "enabled while the stores were closing (removeData: \(removeData))")
            fake.closeGate.open()
            await until("enabled again") { fake.localCalls.withLock { $0 }.filter { $0 == "enable" }.count == 2 }
            let calls = fake.localCalls.withLock { $0 }
            XCTAssertLessThan(calls.firstIndex(of: "closed")!, calls.lastIndex(of: "enable")!)
        }
    }

    func testSignOutResetsTheFeed() async {
        let fake = signedIn()
        let store = store(fake)
        await store.signIn(server: "https://h", handle: "alice", password: "pw")
        await until("on") { store.localData == .on }
        store.signOut(removeData: false)
        XCTAssertNil(store.feed)
        XCTAssertFalse(store.offersRemoval)
    }
}

/// The sign-out sheet's model (#62 spec item 6, as GTK's).
@MainActor
final class SignOutModelTests: XCTestCase {
    func testTickedByDefaultAndTheTextFollowsTheBox() async {
        let chat = FakeChat()
        chat.local = true
        chat.unsent = 3
        let m = SignOutModel(client: chat)
        XCTAssertTrue(m.removeData)
        await m.probe()
        XCTAssertTrue(m.body.contains("will be removed"))
        XCTAssertEqual(m.warning, "3 messages haven't been sent yet. Removing this device's data deletes them.")
        m.removeData = false
        XCTAssertTrue(m.body.contains("stay saved"))
        XCTAssertNil(m.warning, "a removal warning while keeping the data")
    }

    func testStoresNotKnownOpenSayMayNotZero() async {
        let chat = FakeChat() // its cached calls answer local.unavailable
        let m = SignOutModel(client: chat)
        await m.probe()
        XCTAssertEqual(m.warning, "Unsent messages on this Mac may be deleted.")
    }

    func testOpenStoresWithNothingUnsentWarnNothing() async {
        let chat = FakeChat()
        chat.local = true
        let m = SignOutModel(client: chat)
        await m.probe()
        XCTAssertNil(m.warning)
    }
}

/// The cache's notices (#62 spec items 3 to 5 and 7).
@MainActor
final class CacheFeedTests: XCTestCase {
    private func settle() async { for _ in 0 ..< 10 { await Task.yield(); try? await Task.sleep(for: .milliseconds(5)) } }

    func testStatesFromABackgroundQueueArriveInOrderOnTheMainThread() async {
        let feed = CacheFeed(client: FakeChat())
        let bridge = CacheStateBridge(feed)
        await withCheckedContinuation { (done: CheckedContinuation<Void, Never>) in
            DispatchQueue.global().async {
                for i in 0 ..< 50 {
                    bridge.onCacheState(state: FfiCacheState(syncing: false, lastSyncedUnixMs: nil, offline: i % 2 == 0))
                }
                bridge.onCacheState(state: FfiCacheState(syncing: false, lastSyncedUnixMs: nil, offline: false))
                done.resume()
            }
        }
        await settle()
        XCTAssertFalse(feed.offline, "a state delivered out of order won")
    }

    // ---- The offline banner waits before it shows ----

    func testBannerGateStartsOneTimerAndARepeatWhileWaitingStartsNone() {
        var g = OfflineBannerGate()
        XCTAssertEqual(g.onState(offline: true, shown: false), .start(1))
        XCTAssertEqual(g.onState(offline: true, shown: false), .keep, "no second timer")
        XCTAssertTrue(g.onFire(1))
    }

    func testBannerGateFlakyFlipRestartsTheWaitAndTheStaleTimerShowsNothing() {
        var g = OfflineBannerGate()
        XCTAssertEqual(g.onState(offline: true, shown: false), .start(1))
        XCTAssertEqual(g.onState(offline: false, shown: false), .hide)
        XCTAssertEqual(g.onState(offline: true, shown: false), .start(2), "a fresh wait")
        XCTAssertFalse(g.onFire(1), "stale timer")
        XCTAssertTrue(g.onFire(2))
    }

    func testBannerGateTimerFiringAfterTheFlagWentFalseShowsNothing() {
        var g = OfflineBannerGate()
        XCTAssertEqual(g.onState(offline: true, shown: false), .start(1))
        XCTAssertEqual(g.onState(offline: false, shown: false), .hide)
        XCTAssertFalse(g.onFire(1))
    }

    func testBannerGateHidesAtOnceWhenShownAndStartsNoTimerWhileShown() {
        var g = OfflineBannerGate()
        XCTAssertEqual(g.onState(offline: true, shown: true), .keep)
        XCTAssertEqual(g.onState(offline: false, shown: true), .hide)
    }

    func testBannerGateStoppedRevealsNothing() {
        var g = OfflineBannerGate()
        XCTAssertEqual(g.onState(offline: true, shown: false), .start(1))
        g.stop()
        XCTAssertFalse(g.onFire(1))
    }

    func testBannerWaitsForOfflineToLastAndAFlipWithinTheDelayNeverShowsIt() async {
        let feed = CacheFeed(client: FakeChat(), bannerDelay: .milliseconds(150))
        feed.state(FfiCacheState(syncing: false, lastSyncedUnixMs: nil, offline: true))
        XCTAssertTrue(feed.offline, "the raw flag is not delayed")
        XCTAssertFalse(feed.showsOfflineBanner)
        feed.state(FfiCacheState(syncing: false, lastSyncedUnixMs: nil, offline: false))
        try? await Task.sleep(for: .milliseconds(400))
        XCTAssertFalse(feed.showsOfflineBanner, "the first wait was cancelled")

        feed.state(FfiCacheState(syncing: false, lastSyncedUnixMs: nil, offline: true))
        let shown = await Self.eventually { feed.showsOfflineBanner }
        XCTAssertTrue(shown, "offline held past the delay")
        feed.state(FfiCacheState(syncing: false, lastSyncedUnixMs: nil, offline: false))
        XCTAssertFalse(feed.showsOfflineBanner, "hidden at once")
    }

    func testStopCancelsAPendingBanner() async {
        let feed = CacheFeed(client: FakeChat(), bannerDelay: .milliseconds(100))
        feed.state(FfiCacheState(syncing: false, lastSyncedUnixMs: nil, offline: true))
        feed.stop()
        feed.state(FfiCacheState(syncing: false, lastSyncedUnixMs: nil, offline: true)) // a late one
        try? await Task.sleep(for: .milliseconds(300))
        XCTAssertFalse(feed.showsOfflineBanner)
    }

    func testStopHidesABannerThatIsOnScreen() async {
        let feed = CacheFeed(client: FakeChat(), bannerDelay: .milliseconds(50))
        feed.state(FfiCacheState(syncing: false, lastSyncedUnixMs: nil, offline: true))
        let shown = await Self.eventually { feed.showsOfflineBanner }
        XCTAssertTrue(shown)
        feed.stop()
        XCTAssertFalse(feed.showsOfflineBanner, "stop takes a shown banner down")
    }

    /// Polls until `condition` holds or `within` passes: a fixed sleep can lose to a busy machine.
    @MainActor static func eventually(within: Duration = .seconds(3), _ condition: @MainActor () -> Bool) async -> Bool {
        let deadline = ContinuousClock.now + within
        while ContinuousClock.now < deadline {
            if condition() { return true }
            try? await Task.sleep(for: .milliseconds(20))
        }
        return condition()
    }

    func testTheBannerFollowsTheFeedAndResetsOnStop() {
        let feed = CacheFeed(client: FakeChat())
        let t = TimelineModel(channelId: "c", client: FakeChat())
        feed.timeline = t
        feed.state(FfiCacheState(syncing: false, lastSyncedUnixMs: nil, offline: true))
        XCTAssertTrue(feed.offline)
        XCTAssertTrue(t.offline)
        feed.stop()
        XCTAssertFalse(feed.offline)
    }

    func testOneLossAtATimeAcknowledgedExactlyThenANewerOneShows() async {
        let chat = FakeChat()
        chat.lost = 3
        let feed = CacheFeed(client: chat)
        feed.checkLost()
        XCTAssertEqual(feed.alert, .lost(3))
        chat.lost = 5 // a newer loss while 3 is on screen
        feed.checkLost()
        XCTAssertEqual(feed.alerts, [.lost(3)], "two loss alerts queued")
        feed.dismiss()
        XCTAssertEqual(chat.acknowledged.withLock { $0 }, [3], "acknowledged something not shown")
        await settle()
        XCTAssertEqual(feed.alert, .lost(5), "the newer loss didn't show")
        feed.dismiss()
        await settle()
        XCTAssertEqual(chat.acknowledged.withLock { $0 }, [3, 5])
        XCTAssertNil(feed.alert)
    }

    func testALossAndTheOtherAccountsNoticeShowOneAfterTheOther() async {
        let chat = FakeChat()
        chat.local = true
        chat.lost = 1
        chat.others = [FfiLocalUser(origin: "o", userId: "u", unsent: 0)]
        let feed = CacheFeed(client: chat)
        feed.checkLost()
        feed.state(FfiCacheState(syncing: false, lastSyncedUnixMs: 1, offline: false))
        await settle()
        XCTAssertEqual(feed.alerts.count, 2)
        XCTAssertEqual(feed.alert, .lost(1))
        feed.dismiss()
        XCTAssertEqual(feed.alert?.text, "Another account's saved messages were removed from this Mac.")
    }

    func testAFailedCleanUpIsRetriedAtTheNextSync() async {
        let chat = FakeChat() // not local yet: otherLocalUsers fails
        chat.others = [FfiLocalUser(origin: "o", userId: "u", unsent: 0)]
        let feed = CacheFeed(client: chat)
        feed.state(FfiCacheState(syncing: false, lastSyncedUnixMs: 1, offline: false))
        await settle()
        XCTAssertEqual(chat.wiped.withLock { $0 }, 0)
        chat.local = true
        feed.state(FfiCacheState(syncing: false, lastSyncedUnixMs: 2, offline: false))
        await settle()
        XCTAssertEqual(chat.wiped.withLock { $0 }, 1, "never retried")
    }

    func testFeedEventsReachTheModelsTheyName() async {
        let chat = FakeChat()
        chat.local = true
        chat.users = [FfiMember(id: "u", handle: "u", displayName: "Robert", role: nil)]
        let feed = CacheFeed(client: chat)
        let timeline = TimelineModel(channelId: "c", client: chat)
        timeline.merge([msg("m1", "hi")])
        let pending = PendingModel(channelId: "c", client: chat)
        let realtime = FakeRealtime(channels: [channel("c", "general"), channel("d", "other")])
        let channels = ChannelsModel(client: realtime)
        await channels.start()
        channels.openChannel = "c"
        (feed.timeline, feed.pending, feed.channels) = (timeline, pending, channels)

        feed.handle(.outbox(channelId: "other"))
        await settle()
        XCTAssertFalse(chat.cacheCalls.withLock { $0 }.contains("pending"), "another channel's outbox")
        feed.handle(.outbox(channelId: "c"))
        await settle()
        XCTAssertTrue(chat.cacheCalls.withLock { $0 }.contains("pending"))

        feed.handle(.channels(ids: ["c"]))
        await settle()
        XCTAssertTrue(chat.cacheCalls.withLock { $0 }.contains("cached:-"), "the open channel wasn't refilled")

        feed.handle(.users(ids: ["u"]))
        await settle()
        XCTAssertEqual(timeline.authorName(timeline.messages[0]), "Robert")

        feed.handle(.removed(ids: ["c"]))
        XCTAssertEqual(channels.channels.map(\.id), ["d"])
        XCTAssertEqual(channels.closed, "c")
    }

    func testEventsFromABackgroundQueueArriveOnTheMainThread() async {
        let chat = FakeChat()
        chat.local = true
        let feed = CacheFeed(client: chat)
        let pending = PendingModel(channelId: "c", client: chat)
        feed.pending = pending
        let bridge = CacheEventBridge(feed)
        await withCheckedContinuation { (done: CheckedContinuation<Void, Never>) in
            DispatchQueue.global().async {
                for _ in 0 ..< 20 { bridge.onCacheEvent(event: .outbox(channelId: "c")) }
                done.resume()
            }
        }
        await settle()
        XCTAssertFalse(chat.cacheCalls.withLock { $0 }.isEmpty) // delivered (and on main: no crash)
    }

    func testOtherAccountsGoAtTheFirstSyncWithTheirUnsentNamed() async {
        let chat = FakeChat()
        chat.local = true
        chat.others = [FfiLocalUser(origin: "https://a", userId: "u9", unsent: 2),
                       FfiLocalUser(origin: "https://b", userId: "u8", unsent: 1)]
        let feed = CacheFeed(client: chat)
        feed.state(FfiCacheState(syncing: true, lastSyncedUnixMs: nil, offline: false))
        await settle()
        XCTAssertEqual(chat.wiped.withLock { $0 }, 0, "wiped before the first sync")
        feed.state(FfiCacheState(syncing: false, lastSyncedUnixMs: 1, offline: false))
        await settle()
        XCTAssertEqual(chat.wiped.withLock { $0 }, 1)
        XCTAssertEqual(feed.alert?.text, "Another account's saved messages were removed from this Mac, including 3 unsent messages.")
    }

    func testNoticeWordingForUnknownAndNone() {
        let unknown = [FfiLocalUser(origin: "o", userId: "u", unsent: nil)]
        XCTAssertTrue(CacheFeed.noticeText(unknown).hasSuffix("which may have included unsent messages."))
        let none = [FfiLocalUser(origin: "o", userId: "u", unsent: 0)]
        XCTAssertEqual(CacheFeed.noticeText(none), "Another account's saved messages were removed from this Mac.")
    }

    func testAStoppedFeedQueuesNothingLate() async {
        let chat = FakeChat()
        chat.local = true
        chat.lost = 1
        chat.others = [FfiLocalUser(origin: "o", userId: "u", unsent: 0)]
        let feed = CacheFeed(client: chat)
        feed.stop()
        feed.checkLost()
        await feed.cleanUpOthers()
        XCTAssertNil(feed.alert, "an alert after sign-out")
    }

    func testNoOthersNoWipeNoNotice() async {
        let chat = FakeChat()
        chat.local = true
        let feed = CacheFeed(client: chat)
        feed.state(FfiCacheState(syncing: false, lastSyncedUnixMs: 1, offline: false))
        await settle()
        XCTAssertEqual(chat.wiped.withLock { $0 }, 0)
        XCTAssertNil(feed.alert)
    }

    func testAResetReloadsTheListAndRechecksLosses() async {
        let realtime = FakeRealtime(channels: [channel("c1", "general")])
        let chat = FakeChat()
        chat.lost = 1
        let feed = CacheFeed(client: chat)
        let channels = ChannelsModel(client: realtime)
        feed.channels = channels
        feed.handle(.reset)
        await settle()
        XCTAssertEqual(realtime.order.withLock { $0 }, ["list"])
        XCTAssertEqual(feed.alert, .lost(1))
    }
}
