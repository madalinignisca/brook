import BrookCore
@testable import Brook
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
}
