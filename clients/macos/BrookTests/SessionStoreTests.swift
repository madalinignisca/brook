import BrookCore
@testable import Brook
import Synchronization
import XCTest

@MainActor
final class SessionStoreTests: XCTestCase {
    private var suite: String!
    private var defaults: UserDefaults!

    override func setUp() async throws {
        suite = "brook.tests.\(UUID().uuidString)"
        defaults = UserDefaults(suiteName: suite)
    }

    override func tearDown() async throws {
        defaults.removePersistentDomain(forName: suite)
    }

    private func store(
        _ client: FfiBrookClient, environment: [String: String] = [:]
    ) -> (SessionStore, FactoryRecorder) {
        let recorder = FactoryRecorder { client }
        let settings = Settings(defaults: defaults, environment: environment)
        return (SessionStore(settings: settings, makeClient: recorder.factory), recorder)
    }

    /// Waits (bounded) until the fake has seen `count` login calls.
    private func waitForCalls(_ fake: FakeClient, _ count: Int) async {
        for _ in 0 ..< 500 where fake.calls.count < count { try? await Task.sleep(for: .milliseconds(2)) }
    }

    func testEmptyHandleOrPasswordNeverReachesTheClient() async {
        let fake = FakeClient(result: .success(.loggedIn(session: aliceSession)))
        let (store, recorder) = store(fake)
        await store.signIn(server: "https://h", handle: "  ", password: "pw")
        XCTAssertEqual(store.phase, .signedOut(error: "Enter your handle and password."))
        await store.signIn(server: "https://h", handle: "alice", password: "")
        XCTAssertEqual(store.phase, .signedOut(error: "Enter your handle and password."))
        XCTAssertTrue(recorder.all.isEmpty)
    }

    func testPasswordIsPassedExactlyWhileHandleAndServerAreTrimmed() async {
        let fake = FakeClient(result: .success(.loggedIn(session: aliceSession)))
        let (store, recorder) = store(fake)
        await store.signIn(server: " https://h \n", handle: " alice ", password: " p w ")
        XCTAssertEqual(fake.calls, [.init(handle: "alice", password: " p w ")])
        XCTAssertEqual(recorder.all.map(\.server), ["https://h"])
    }

    func testAddressWithCredentialsIsRejectedBeforeAnythingHappens() async {
        let fake = FakeClient(result: .success(.loggedIn(session: aliceSession)))
        let (store, recorder) = store(fake)
        for bad in ["https://u:secret@h", "https://h?x=1", "https://h#f"] {
            await store.signIn(server: bad, handle: "alice", password: "pw")
            XCTAssertEqual(store.phase, .signedOut(error: SessionStore.Message.notJustAnAddress), bad)
        }
        XCTAssertTrue(recorder.all.isEmpty)
        XCTAssertNil(defaults.string(forKey: Settings.lastServerKey))
    }

    func testSecondSignInWhileSigningInIsIgnored() async {
        let fake = FakeClient(result: .success(.loggedIn(session: aliceSession)), gated: true)
        let (store, _) = store(fake)
        let first = Task { _ = await store.signIn(server: "https://h", handle: "alice", password: "pw") }
        await waitForCalls(fake, 1)
        XCTAssertEqual(store.phase, .signingIn)
        await store.signIn(server: "https://h", handle: "alice", password: "pw")
        fake.release()
        await first.value
        XCTAssertEqual(fake.calls.count, 1)
    }

    func testCanRetryAfterAFailedLogin() async {
        let wrong = LoginError.Api(code: "auth.invalid_credentials", message: "Invalid handle or password")
        let fake = FakeClient(result: .failure(wrong))
        let (store, _) = store(fake)
        await store.signIn(server: "https://h", handle: "alice", password: "bad")
        await store.signIn(server: "https://h", handle: "alice", password: "bad2")
        XCTAssertEqual(fake.calls.count, 2)
    }

    func testConstructorErrorIsShownAndLoginNeverCalled() async {
        let recorder = FactoryRecorder { throw LoginError.InsecureServerUrl }
        let store = SessionStore(settings: Settings(defaults: defaults, environment: [:]), makeClient: recorder.factory)
        await store.signIn(server: "http://h.example", handle: "alice", password: "pw")
        XCTAssertEqual(store.phase, .signedOut(error: SessionStore.Message.insecure))
        XCTAssertEqual(recorder.all.count, 1)
    }

    func testErrorMessages() async {
        let cases: [(LoginError, String)] = [
            (.Api(code: "auth.invalid_credentials", message: "x"), "Wrong handle or password."),
            (.Api(code: "rate_limited", message: "Slow down"), "Slow down"),
            (.Network(message: "refused"), SessionStore.Message.unreachable),
            (.InvalidServerUrl(message: "x"), SessionStore.Message.invalidAddress),
            (.UnexpectedResponse, SessionStore.Message.unexpected),
            (.NotAuthenticated, SessionStore.Message.signedOut),
        ]
        for (error, expected) in cases {
            let (store, _) = store(FakeClient(result: .failure(error)))
            await store.signIn(server: "https://chat.example.com", handle: "alice", password: "pw")
            XCTAssertEqual(store.phase, .signedOut(error: expected), "\(error)")
        }
    }

    func testNetworkErrorOnALanAddressMentionsLocalNetworkPermission() async {
        let (store, _) = store(FakeClient(result: .failure(.Network(message: "refused"))))
        await store.signIn(server: "http://192.168.1.192:8080", handle: "alice", password: "pw")
        XCTAssertEqual(store.phase, .signedOut(error: SessionStore.Message.unreachableLAN))
    }

    func testServerIsSavedOnlyAfterSuccess() async {
        defaults.set("https://previous", forKey: Settings.lastServerKey)
        let fake = FakeClient(result: .success(.loggedIn(session: aliceSession)), gated: true)
        let (store, _) = store(fake)
        let signIn = Task { _ = await store.signIn(server: "https://new", handle: "alice", password: "pw") }
        await waitForCalls(fake, 1)
        XCTAssertEqual(defaults.string(forKey: Settings.lastServerKey), "https://previous")
        fake.release()
        await signIn.value
        XCTAssertEqual(defaults.string(forKey: Settings.lastServerKey), "https://new")
        XCTAssertEqual(store.phase, .signedIn(alice))
    }

    func testFailedLoginDoesNotSaveTheServer() async {
        let (store, _) = store(FakeClient(result: .failure(.UnexpectedResponse)))
        await store.signIn(server: "https://new", handle: "alice", password: "pw")
        XCTAssertNil(defaults.string(forKey: Settings.lastServerKey))
    }

    func testFactoryReceivesTheResolvedInsecureFlag() async {
        let session = FakeClient(result: .success(.loggedIn(session: aliceSession)))
        let (off, offRecorder) = store(session)
        await off.signIn(server: "https://h", handle: "alice", password: "pw")
        let (on, onRecorder) = store(session, environment: ["BROOK_ALLOW_INSECURE_HTTP": "1"])
        await on.signIn(server: "https://h", handle: "alice", password: "pw")
        XCTAssertEqual(offRecorder.all.map(\.allowInsecureHttp), [false])
        XCTAssertEqual(onRecorder.all.map(\.allowInsecureHttp), [true])
    }

    // MARK: sign out, and following a remote sign-out (spec 2026-09-25-sign-out §4)

    /// Waits (bounded) until `predicate` holds on the store.
    private func until(_ store: SessionStore, _ predicate: (SessionStore) -> Bool) async {
        for _ in 0 ..< 500 where !predicate(store) { try? await Task.sleep(for: .milliseconds(2)) }
    }

    private func signedIn(_ fake: FakeClient) async -> SessionStore {
        let (store, _) = store(fake)
        await store.signIn(server: "https://h", handle: "alice", password: "pw")
        fake.emit(.loggedIn(user: alice))
        XCTAssertEqual(store.phase, .signedIn(alice))
        return store
    }

    func testSignOutEndsTheSessionQuietlyAndSignsOutOfCore() async {
        let fake = FakeClient(result: .success(.loggedIn(session: aliceSession)))
        let store = await signedIn(fake)
        store.signOut()
        XCTAssertEqual(store.phase, .signedOut(error: nil))
        XCTAssertNil(store.client)
        for _ in 0 ..< 500 where fake.logouts == 0 { try? await Task.sleep(for: .milliseconds(2)) }
        XCTAssertEqual(fake.logouts, 1, "core never signed out")
    }

    func testARemoteSignOutShowsTheSignInScreenWithTheMessage() async {
        let fake = FakeClient(result: .success(.loggedIn(session: aliceSession)))
        let store = await signedIn(fake)
        fake.setCoreState(.loggedOut)
        fake.emit(.loggedOut)
        await until(store) { $0.phase != .signedIn(alice) }
        XCTAssertEqual(store.phase, .signedOut(error: SessionStore.Message.signedOut))
        XCTAssertNil(store.client)
    }

    func testAFreshClientsInitialLoggedOutIsNotASignOut() async {
        let fake = FakeClient(result: .success(.loggedIn(session: aliceSession)), gated: true)
        let (store, _) = store(fake)
        let signIn = Task { await store.signIn(server: "https://h", handle: "alice", password: "pw") }
        await waitForCalls(fake, 1)
        fake.emit(.loggedOut) // core's initial state, before any sign-in
        fake.release()
        _ = await signIn.value
        try? await Task.sleep(for: .milliseconds(20))
        XCTAssertEqual(store.phase, .signedIn(alice))
    }

    /// Core signed in and then lost the session before the app handled its own login result:
    /// that sign-out wins, and the late login result is ignored.
    func testASignOutBeforeTheLoginResultIsHandledWins() async {
        let fake = FakeClient(result: .success(.loggedIn(session: aliceSession)), gated: true)
        let (store, _) = store(fake)
        let signIn = Task { await store.signIn(server: "https://h", handle: "alice", password: "pw") }
        await waitForCalls(fake, 1)
        fake.emit(.loggedIn(user: alice))
        fake.emit(.loggedOut)
        fake.setCoreState(.loggedOut)
        fake.release()
        _ = await signIn.value
        XCTAssertEqual(store.phase, .signedOut(error: SessionStore.Message.signedOut))
        XCTAssertNil(store.client)
    }

    /// The subscription keeps only the latest value: a LoggedIn then LoggedOut can arrive as
    /// just LoggedOut. The app reads core's state when its login completes, so this sign-out is
    /// caught, never mistaken for a fresh client's initial state.
    func testACoalescedSignOutDuringSignInIsCaught() async {
        let fake = FakeClient(result: .success(.loggedIn(session: aliceSession)), gated: true)
        let (store, _) = store(fake)
        let signIn = Task { await store.signIn(server: "https://h", handle: "alice", password: "pw") }
        await waitForCalls(fake, 1)
        fake.emit(.loggedOut) // the LoggedIn before it was coalesced away
        fake.setCoreState(.loggedOut)
        fake.release()
        _ = await signIn.value
        XCTAssertEqual(store.phase, .signedOut(error: SessionStore.Message.signedOut))
        XCTAssertNil(store.client)
    }

    /// A LoggedOut delivered late (the fresh client's initial state, held up in delivery) while
    /// core is in fact signed in: not a sign-out.
    func testALateInitialLoggedOutNeverEndsAGoodSession() async {
        let fake = FakeClient(result: .success(.loggedIn(session: aliceSession)))
        let store = await signedIn(fake) // core's state: LoggedIn
        fake.emit(.loggedOut)
        try? await Task.sleep(for: .milliseconds(20))
        XCTAssertEqual(store.phase, .signedIn(alice))
    }

    func testARemoteSignOutAfterTheUsersOwnHasNoMessage() async {
        let fake = FakeClient(result: .success(.loggedIn(session: aliceSession)))
        let store = await signedIn(fake)
        store.signOut()
        fake.emit(.loggedOut)
        try? await Task.sleep(for: .milliseconds(20))
        XCTAssertEqual(store.phase, .signedOut(error: nil))
    }

    func testALoggedOutFromThePreviousClientNeverSignsOutTheNextOne() async {
        let first = FakeClient(result: .success(.loggedIn(session: aliceSession)))
        let second = FakeClient(result: .success(.loggedIn(session: aliceSession)))
        let clients = Mutex([first, second])
        let recorder = FactoryRecorder { clients.withLock { $0.removeFirst() } }
        let store = SessionStore(settings: Settings(defaults: defaults, environment: [:]), makeClient: recorder.factory)
        await store.signIn(server: "https://h", handle: "alice", password: "pw")
        first.emit(.loggedIn(user: alice))
        store.signOut()
        await store.signIn(server: "https://h", handle: "alice", password: "pw")
        second.emit(.loggedIn(user: alice))
        first.emit(.loggedOut) // late, from the dropped client
        try? await Task.sleep(for: .milliseconds(20))
        XCTAssertEqual(store.phase, .signedIn(alice))
    }

    // MARK: TOTP second step (spec 2026-09-25-totp-clients §5)

    private func atCodeStep() async -> (SessionStore, FakeClient) {
        let fake = FakeClient(result: .success(.totpRequired(challenge: FakeChallenge())))
        fake.setCoreState(.authenticating)
        let (store, _) = store(fake)
        await store.signIn(server: "https://h", handle: "alice", password: "pw")
        return (store, fake)
    }

    func testThePasswordAloneLeadsToTheCodeStep() async {
        let (store, _) = await atCodeStep()
        XCTAssertEqual(store.phase, .needsCode(error: nil))
        XCTAssertNil(store.client, "signed in with the password alone")
    }

    func testTheRightCodeSignsIn() async {
        let (store, fake) = await atCodeStep()
        await store.submitCode("123 456")
        XCTAssertEqual(fake.totpCalls, ["code:123456"])
        XCTAssertEqual(store.phase, .signedIn(alice))
        XCTAssertNotNil(store.client)
    }

    func testAWrongCodeStaysOnTheCodeStepAndSaysSo() async {
        let (store, fake) = await atCodeStep()
        fake.setTotpResult(.failure(.Api(code: "auth.invalid_code", message: "x")))
        await store.submitCode("000000")
        XCTAssertEqual(store.phase, .needsCode(error: SessionStore.Message.wrongCode))
        await store.submitRecovery("aaaa-bbbb")
        XCTAssertEqual(store.phase, .needsCode(error: SessionStore.Message.wrongRecoveryCode))
    }

    func testAnExpiredChallengeGoesBackToThePassword() async {
        let (store, fake) = await atCodeStep()
        fake.setTotpResult(.failure(.Api(code: "auth.totp_expired", message: "x")))
        await store.submitCode("123456")
        XCTAssertEqual(store.phase, .signedOut(error: SessionStore.Message.codeStepExpired))
    }

    func testBackCancelsTheChallenge() async {
        let (store, fake) = await atCodeStep()
        store.back()
        XCTAssertEqual(store.phase, .signedOut(error: nil))
        for _ in 0 ..< 500 where fake.cancels == 0 { try? await Task.sleep(for: .milliseconds(2)) }
        XCTAssertEqual(fake.cancels, 1)
    }

    func testASupersededChallengeChangesNothing() async {
        let (store, fake) = await atCodeStep()
        fake.setTotpResult(.failure(.ChallengeSuperseded))
        await store.submitCode("123456")
        XCTAssertEqual(store.phase, .needsCode(error: nil))
    }

    func testAMalformedCodeNeverReachesTheClient() async {
        let (store, fake) = await atCodeStep()
        await store.submitCode("12345")
        XCTAssertEqual(store.phase, .needsCode(error: SessionStore.Message.codeFormat))
        await store.submitRecovery("   ")
        XCTAssertEqual(store.phase, .needsCode(error: SessionStore.Message.recoveryFormat))
        XCTAssertTrue(fake.totpCalls.isEmpty)
    }

    func testFewRecoveryCodesLeftAreFlagged() async {
        let (store, fake) = await atCodeStep()
        fake.setTotpResult(.success(2))
        await store.submitRecovery("aaaa-bbbb-cccc-dddd-eeee")
        XCTAssertEqual(store.phase, .signedIn(alice))
        XCTAssertEqual(store.recoveryCodesLeft, 2)
    }
}
