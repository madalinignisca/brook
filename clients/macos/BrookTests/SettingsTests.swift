@testable import Brook
import XCTest

final class SettingsTests: XCTestCase {
    private var defaults: UserDefaults!
    private var suite: String!

    override func setUp() {
        suite = "brook.tests.\(UUID().uuidString)"
        defaults = UserDefaults(suiteName: suite)
    }

    override func tearDown() {
        defaults.removePersistentDomain(forName: suite)
    }

    func testServerPrefillPrefersEnvironmentThenLastGoodThenLocalhost() {
        XCTAssertEqual(Settings(defaults: defaults, environment: [:]).serverPrefill, "https://localhost")
        defaults.set("https://last.example", forKey: Settings.lastServerKey)
        XCTAssertEqual(Settings(defaults: defaults, environment: [:]).serverPrefill, "https://last.example")
        let env = ["BROOK_SERVER": "https://env.example"]
        XCTAssertEqual(Settings(defaults: defaults, environment: env).serverPrefill, "https://env.example")
    }

    func testInsecureHTTPFromEnvironmentOrHiddenDefaultOnly() {
        XCTAssertFalse(Settings(defaults: defaults, environment: [:]).allowInsecureHTTP)
        XCTAssertFalse(Settings(defaults: defaults, environment: ["BROOK_ALLOW_INSECURE_HTTP": "0"]).allowInsecureHTTP)
        XCTAssertTrue(Settings(defaults: defaults, environment: ["BROOK_ALLOW_INSECURE_HTTP": "1"]).allowInsecureHTTP)
        defaults.set(true, forKey: Settings.allowInsecureKey)
        XCTAssertTrue(Settings(defaults: defaults, environment: [:]).allowInsecureHTTP)
    }

    func testSaveLastGoodServer() {
        Settings(defaults: defaults, environment: [:]).saveLastGoodServer("https://ok.example")
        XCTAssertEqual(defaults.string(forKey: Settings.lastServerKey), "https://ok.example")
    }
}
