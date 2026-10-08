// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

@testable import Brook
import XCTest

final class AppTitleTests: XCTestCase {
    func testDebugBuildIsLabelled() {
        XCTAssertEqual(AppTitle.window(debug: true), "Brook (Debug)")
    }

    /// Release must stay exactly "Brook".
    func testReleaseTitleIsPlain() {
        XCTAssertEqual(AppTitle.window(debug: false), "Brook")
    }

    /// The title the app really uses; the test host is a Debug build.
    func testMainTitleMatchesTheBuildConfiguration() {
        #if DEBUG
            XCTAssertEqual(AppTitle.main, "Brook (Debug)")
        #endif
    }
}
