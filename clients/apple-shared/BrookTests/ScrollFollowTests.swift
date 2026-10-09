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

    func testTheTopIsNearWithinOneScreen() {
        XCTAssertTrue(ScrollToLatest.isNearTop(visibleMinY: 0, viewportHeight: 800))
        XCTAssertTrue(ScrollToLatest.isNearTop(visibleMinY: 799, viewportHeight: 800))
    }

    func testTheTopIsNotNearFurtherThanOneScreen() {
        XCTAssertFalse(ScrollToLatest.isNearTop(visibleMinY: 800, viewportHeight: 800))
        XCTAssertFalse(ScrollToLatest.isNearTop(visibleMinY: 5000, viewportHeight: 800))
    }

    func testStayedPutWithinTheToleranceOfTheAsk() {
        XCTAssertTrue(ScrollToLatest.stayedPut(askedAt: 100, now: 100))
        XCTAssertTrue(ScrollToLatest.stayedPut(askedAt: 100, now: 144))
        XCTAssertTrue(ScrollToLatest.stayedPut(askedAt: 100, now: 56))
    }

    func testMovedWhenFurtherThanTheToleranceFromTheAsk() {
        XCTAssertFalse(ScrollToLatest.stayedPut(askedAt: 100, now: 145))
        XCTAssertFalse(ScrollToLatest.stayedPut(askedAt: 100, now: 55))
    }

    func testGrowthAtTheBottomPinsToTheBottom() {
        XCTAssertTrue(ScrollToLatest.pinsToBottom(wasAway: false, oldContentHeight: 1000, newContentHeight: 1040))
    }

    func testGrowthWhileAwayOrNoGrowthDoesNotPin() {
        XCTAssertFalse(ScrollToLatest.pinsToBottom(wasAway: true, oldContentHeight: 1000, newContentHeight: 1040))
        XCTAssertFalse(ScrollToLatest.pinsToBottom(wasAway: false, oldContentHeight: 1000, newContentHeight: 1000))
        XCTAssertFalse(ScrollToLatest.pinsToBottom(wasAway: false, oldContentHeight: 1000, newContentHeight: 900))
    }

    func testFollowsTheUsersOwnMessageEvenWhenAway() {
        XCTAssertTrue(ScrollToLatest.follows(away: true, mine: true))
    }
}
