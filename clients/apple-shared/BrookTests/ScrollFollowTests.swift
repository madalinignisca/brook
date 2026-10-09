// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import XCTest

@testable import Brook

/// When a new newest message moves the view. XCTest, not Swift Testing like `ScrollToLatestTests`,
/// because `clients/ios/build.sh` can only require XCTest names.
final class ScrollFollowTests: XCTestCase {
    func testFollowsANewMessageAtTheBottom() {
        XCTAssertTrue(ScrollToLatest.follows(away: false, mine: false))
    }

    func testStaysPutForSomeoneElsesMessageWhenAway() {
        XCTAssertFalse(ScrollToLatest.follows(away: true, mine: false))
    }

    func testFollowsTheUsersOwnMessageEvenWhenAway() {
        XCTAssertTrue(ScrollToLatest.follows(away: true, mine: true))
    }
}
