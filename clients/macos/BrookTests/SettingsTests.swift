// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

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

    @MainActor
    func testShowUsernamesCaption() {
        XCTAssertEqual(SettingsView.showUsernamesCaption,
                       "Shows people as @username instead of their display name, everywhere they appear.")
    }

    /// The address survives restarts; nothing from the environment overrides it.
    func testServerPrefillIsTheSavedAddressElseLocalhost() {
        XCTAssertEqual(Settings(defaults: defaults, environment: [:]).serverPrefill, "https://localhost")
        defaults.set("https://last.example", forKey: Settings.lastServerKey)
        let env = ["BROOK_SERVER": "https://env.example"]
        XCTAssertEqual(Settings(defaults: defaults, environment: env).serverPrefill, "https://last.example")
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

    /// Off unless chosen: the owner's decision, one constant for the reader and every view.
    func testImagePreviewsAreOffUntilChosen() {
        XCTAssertFalse(Settings.showImagePreviewsDefault)
        XCTAssertEqual(Settings(defaults: defaults, environment: [:]).showImagePreviews, Settings.showImagePreviewsDefault)
        defaults.set(true, forKey: Settings.showImagePreviewsKey)
        XCTAssertTrue(Settings(defaults: defaults, environment: [:]).showImagePreviews)
        defaults.set(false, forKey: Settings.showImagePreviewsKey)
        XCTAssertFalse(Settings(defaults: defaults, environment: [:]).showImagePreviews)
        XCTAssertEqual(Settings.showImagePreviewsKey, "ShowImagePreviews")
    }
}
