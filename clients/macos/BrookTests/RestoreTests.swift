import BrookCore
@testable import Brook
import Foundation
import Security
import XCTest

/// Staying signed in: the launch restore (plan #80 P4) and when persistence is on (P3).
@MainActor
final class RestoreTests: XCTestCase {
    private var suite: String!
    private var defaults: UserDefaults!

    override func setUp() async throws {
        suite = "brook.tests.\(UUID().uuidString)"
        defaults = UserDefaults(suiteName: suite)
    }

    override func tearDown() async throws {
        defaults.removePersistentDomain(forName: suite)
    }

    private let on = SessionPersistence.on(slot: UnusedSlot(), dataDir: "/data")

    private func store(
        _ fake: FakeClient, persistence: SessionPersistence, lastServer: String? = "https://h"
    ) -> (SessionStore, FactoryRecorder) {
        if let lastServer { defaults.set(lastServer, forKey: Settings.lastServerKey) }
        let recorder = FactoryRecorder { fake }
        let settings = Settings(defaults: defaults, environment: [:])
        return (SessionStore(settings: settings, persistence: persistence, makeClient: recorder.factory), recorder)
    }

    private func restored(_ outcome: FfiRestoreOutcome) async -> (SessionStore, FakeClient, FactoryRecorder) {
        let fake = FakeClient(result: .failure(.UnexpectedResponse))
        fake.setRestore(outcome)
        let (store, recorder) = store(fake, persistence: on)
        XCTAssertEqual(store.phase, .restoring, "the form flashed before the restore")
        await store.restoreAtLaunch()
        return (store, fake, recorder)
    }

    func testAStoredSessionSignsInAtLaunchOnTheLastServer() async {
        let (store, fake, recorder) = await restored(.loggedIn(user: alice))
        XCTAssertEqual(store.phase, .signedIn(alice))
        XCTAssertTrue(store.client === fake)
        XCTAssertEqual(recorder.all.map(\.server), ["https://h"])
        XCTAssertEqual(fake.persistence, ["/data"], "restored without persistence on")
    }

    func testNothingStoredShowsTheFormQuietly() async {
        let (store, _, _) = await restored(.notSignedIn)
        XCTAssertEqual(store.phase, .signedOut(error: nil))
        XCTAssertNil(store.client)
    }

    func testALockedKeychainSaysSo() async {
        let (store, _, _) = await restored(.unavailable)
        XCTAssertEqual(store.phase, .signedOut(error: SessionStore.Message.keychainUnavailable))
    }

    func testOfflineSaysTheSessionIsKept() async {
        let (store, _, _) = await restored(.offline)
        XCTAssertEqual(store.phase, .signedOut(error: SessionStore.Message.restoreOffline))
        XCTAssertNil(store.client)
    }

    func testTheRestoreRunsOncePerProcess() async {
        let (store, fake, _) = await restored(.notSignedIn)
        await store.restoreAtLaunch()
        XCTAssertEqual(fake.restores, 1)
    }

    /// The window's `.task` can run again while the first restore is still in flight.
    func testASecondCallDuringTheRestoreDoesNothing() async {
        let fake = FakeClient(result: .failure(.UnexpectedResponse), gated: true)
        fake.setRestore(.loggedIn(user: alice))
        let (store, recorder) = store(fake, persistence: on)
        let first = Task { await store.restoreAtLaunch() }
        for _ in 0 ..< 500 where recorder.all.isEmpty { try? await Task.sleep(for: .milliseconds(2)) }
        await store.restoreAtLaunch() // returns at once: the first one owns the launch
        fake.release()
        await first.value
        XCTAssertEqual(recorder.all.count, 1, "a second client was made for the same launch")
        XCTAssertEqual(store.phase, .signedIn(alice))
    }

    func testNoPersistenceOrNoServerNeverRestores() async {
        for (persistence, server) in [(SessionPersistence.off, "https://h"), (on, nil)] as [(SessionPersistence, String?)] {
            defaults.removePersistentDomain(forName: suite)
            let fake = FakeClient(result: .failure(.UnexpectedResponse))
            let (store, recorder) = store(fake, persistence: persistence, lastServer: server)
            XCTAssertEqual(store.phase, .signedOut(error: nil))
            await store.restoreAtLaunch()
            XCTAssertEqual(fake.restores, 0)
            XCTAssertTrue(recorder.all.isEmpty)
        }
    }

    /// The session is stored by the client that signed in; with persistence off, no client
    /// ever gets it (quitting signs out, as before).
    func testASignInStoresTheSessionOnlyWhenPersistenceIsOn() async {
        for (persistence, expected) in [(on, ["/data"]), (SessionPersistence.off, [])] as [(SessionPersistence, [String])] {
            let fake = FakeClient(result: .success(.loggedIn(session: aliceSession)))
            let (store, _) = store(fake, persistence: persistence, lastServer: nil)
            await store.signIn(server: "https://h", handle: "alice", password: "pw")
            XCTAssertEqual(store.phase, .signedIn(alice))
            XCTAssertEqual(fake.persistence, expected)
        }
    }

    func testASecondInstanceSaysItWontRemember() async {
        let fake = FakeClient(result: .success(.loggedIn(session: aliceSession)))
        let (store, _) = store(fake, persistence: .secondInstance)
        XCTAssertEqual(store.phase, .signedOut(error: SessionStore.Message.secondInstance))
        await store.restoreAtLaunch()
        await store.signIn(server: "https://h", handle: "alice", password: "pw")
        XCTAssertEqual(fake.restores, 0)
        XCTAssertEqual(fake.persistence, [], "a second instance touched the stored session")
    }

    func testARemoteSignOutAfterARestoreIsFollowed() async {
        let (store, fake, _) = await restored(.loggedIn(user: alice))
        fake.setCoreState(.loggedOut)
        fake.emit(.loggedOut)
        for _ in 0 ..< 500 where store.phase == .signedIn(alice) { try? await Task.sleep(for: .milliseconds(2)) }
        XCTAssertEqual(store.phase, .signedOut(error: SessionStore.Message.signedOut))
    }

    func testASignOutThatCouldNotForgetTheStoredCopySaysSo() async {
        let (store, fake, _) = await restored(.loggedIn(user: alice))
        fake.setSignOutComplete(false)
        store.signOut()
        XCTAssertEqual(store.phase, .signedOut(error: nil))
        for _ in 0 ..< 500 where store.phase == .signedOut(error: nil) { try? await Task.sleep(for: .milliseconds(2)) }
        XCTAssertEqual(store.phase, .signedOut(error: SessionStore.Message.signOutIncomplete))
    }

    func testACompleteSignOutStaysQuiet() async {
        let (store, fake, _) = await restored(.loggedIn(user: alice))
        store.signOut()
        for _ in 0 ..< 500 where fake.logouts == 0 { try? await Task.sleep(for: .milliseconds(2)) }
        try? await Task.sleep(for: .milliseconds(20))
        XCTAssertEqual(store.phase, .signedOut(error: nil))
    }

    // MARK: Choosing persistence (P3)

    private func dir() -> URL {
        FileManager.default.temporaryDirectory.appending(path: "brook-\(UUID().uuidString)", directoryHint: .isDirectory)
    }

    func testNoKeychainGroupInTheSignatureMeansOff() {
        let result = SessionPersistence.choose(accessGroup: nil, dataDir: dir(), calls: Probe(errSecItemNotFound)) { _ in true }
        guard case .off = result else { return XCTFail("\(result)") }
    }

    func testAKeychainFaultMeansOffButALockedKeychainDoesNot() {
        let missing = SessionPersistence.choose(
            accessGroup: "T.dev.brook.shared", dataDir: dir(), calls: Probe(errSecMissingEntitlement)) { _ in true }
        guard case .off = missing else { return XCTFail("\(missing)") }
        let locked = SessionPersistence.choose(
            accessGroup: "T.dev.brook.shared", dataDir: dir(), calls: Probe(errSecInteractionNotAllowed)) { _ in true }
        guard case let .on(_, dataDir) = locked else { return XCTFail("\(locked)") }
        XCTAssertTrue(dataDir.contains("brook-"))
    }

    func testTheLockIsTakenBeforeTheKeychainIsTouched() {
        let probe = Probe(errSecItemNotFound)
        let result = SessionPersistence.choose(accessGroup: "T.dev.brook.shared", dataDir: dir(), calls: probe) { _ in false }
        guard case .secondInstance = result else { return XCTFail("\(result)") }
        XCTAssertEqual(probe.calls, 0, "a second instance touched the keychain")
    }

    /// `flock` belongs to the open file, so a second open in this process conflicts just as a
    /// second app would.
    func testOnlyOneHolderOfTheInstanceLock() throws {
        let d = dir()
        try FileManager.default.createDirectory(at: d, withIntermediateDirectories: true)
        let url = d.appending(path: "instance.lock")
        XCTAssertTrue(InstanceLock.acquire(url))
        XCTAssertFalse(InstanceLock.acquire(url))
    }
}

/// Answers every keychain call with one status, counting them.
private final class Probe: SecItemCalls, @unchecked Sendable {
    let status: OSStatus
    private(set) var calls = 0
    init(_ status: OSStatus) { self.status = status }
    func add(_: [String: Any]) -> OSStatus { calls += 1; return status }
    func copyMatching(_: [String: Any]) -> (OSStatus, Data?) { calls += 1; return (status, nil) }
    func update(_: [String: Any], _: [String: Any]) -> OSStatus { calls += 1; return status }
    func delete(_: [String: Any]) -> OSStatus { calls += 1; return status }
}
