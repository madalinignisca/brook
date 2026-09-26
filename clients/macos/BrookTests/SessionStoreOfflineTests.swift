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

    func testAFailedRemovalWarnsAndWinsOverTheIncompleteOne() async {
        let fake = signedIn()
        let store = store(fake)
        await store.signIn(server: "https://h", handle: "alice", password: "pw")
        await until("on") { store.localData == .on }
        fake.forgetFails.withLock { $0 = true }
        fake.setSignOutComplete(false)
        store.signOut(removeData: true)
        await until("warned") { store.signOutWarning != nil }
        XCTAssertEqual(store.signOutWarning, SessionStore.Message.removalIncomplete)
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

    func testOneLossAtATimeAcknowledgedExactlyThenANewerOneShows() {
        let chat = FakeChat()
        chat.lost = 3
        let feed = CacheFeed(client: chat)
        feed.checkLost()
        XCTAssertTrue(feed.lostAlert)
        chat.lost = 5 // a newer loss while 3 is on screen
        feed.checkLost()
        feed.dismissLost()
        XCTAssertEqual(chat.acknowledged.withLock { $0 }, [3], "acknowledged something not shown")
        XCTAssertTrue(feed.lostAlert, "the newer loss didn't show")
        feed.dismissLost()
        XCTAssertEqual(chat.acknowledged.withLock { $0 }, [3, 5])
        XCTAssertFalse(feed.lostAlert)
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
        XCTAssertEqual(feed.notice, "Another account's saved messages were removed from this Mac, including 3 unsent messages.")
    }

    func testNoticeWordingForUnknownAndNone() {
        let unknown = [FfiLocalUser(origin: "o", userId: "u", unsent: nil)]
        XCTAssertTrue(CacheFeed.noticeText(unknown).hasSuffix("which may have included unsent messages."))
        let none = [FfiLocalUser(origin: "o", userId: "u", unsent: 0)]
        XCTAssertEqual(CacheFeed.noticeText(none), "Another account's saved messages were removed from this Mac.")
    }

    func testNoOthersNoWipeNoNotice() async {
        let chat = FakeChat()
        chat.local = true
        let feed = CacheFeed(client: chat)
        feed.state(FfiCacheState(syncing: false, lastSyncedUnixMs: 1, offline: false))
        await settle()
        XCTAssertEqual(chat.wiped.withLock { $0 }, 0)
        XCTAssertNil(feed.notice)
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
        XCTAssertTrue(feed.lostAlert)
    }
}
