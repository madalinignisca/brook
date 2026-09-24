import BrookCore
@testable import Brook
import XCTest

@MainActor
final class LoginFormTests: XCTestCase {
    private var suite: String!
    private var defaults: UserDefaults!

    override func setUp() async throws {
        suite = "brook.tests.\(UUID().uuidString)"
        defaults = UserDefaults(suiteName: suite)
    }

    override func tearDown() async throws {
        defaults.removePersistentDomain(forName: suite)
    }

    private func form(_ result: Result<LoginResult, LoginError>, environment: [String: String] = [:]) -> LoginForm {
        let recorder = FactoryRecorder { FakeClient(result: result) }
        let store = SessionStore(settings: Settings(defaults: defaults, environment: environment), makeClient: recorder.factory)
        let form = LoginForm(store: store)
        form.server = "https://chat.example.com"
        form.handle = "alice"
        form.password = "secret"
        return form
    }

    func testPasswordClearedAfterSuccessfulLogin() async {
        let form = form(.success(.loggedIn(session: aliceSession)))
        await form.submit()
        XCTAssertEqual(form.password, "")
        XCTAssertEqual(form.store.phase, .signedIn(alice))
    }

    func testPasswordClearedAfterTheServerRejectsIt() async {
        let form = form(.failure(.Api(code: "auth.invalid_credentials", message: "no")))
        await form.submit()
        XCTAssertEqual(form.password, "")
        XCTAssertEqual(form.error, "Wrong handle or password.")
    }

    func testPasswordKeptWhenTheFormIsRejectedLocally() async {
        let form = form(.success(.loggedIn(session: aliceSession)))
        form.handle = ""
        await form.submit()
        XCTAssertEqual(form.password, "secret")
    }

    func testPrefillsTheSavedServer() {
        defaults.set("https://saved.example", forKey: Settings.lastServerKey)
        let store = SessionStore(settings: Settings(defaults: defaults, environment: [:]))
        XCTAssertEqual(LoginForm(store: store).server, "https://saved.example")
    }

    func testInsecureWarningOnlyWhenOptedIn() {
        XCTAssertNil(form(.failure(.UnexpectedResponse)).insecureWarning)
        XCTAssertNotNil(form(.failure(.UnexpectedResponse), environment: ["BROOK_ALLOW_INSECURE_HTTP": "1"]).insecureWarning)
    }
}
