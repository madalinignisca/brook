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

    func testGrowthAtTheVeryBottomWhileIdlePinsToTheBottom() {
        XCTAssertTrue(ScrollToLatest.pinsToBottom(oldDistanceFromBottom: 0, oldContentHeight: 1000,
                                                  newContentHeight: 1040, scrolling: false))
        XCTAssertTrue(ScrollToLatest.pinsToBottom(oldDistanceFromBottom: ScrollToLatest.gap, oldContentHeight: 1000,
                                                  newContentHeight: 1040, scrolling: false))
    }

    func testGrowthDoesNotPinWhenAboveTheBottomScrollingOrNotGrowing() {
        // Above the bottom, even inside the "away" band: a slow drag up must not be pulled back.
        XCTAssertFalse(ScrollToLatest.pinsToBottom(oldDistanceFromBottom: ScrollToLatest.gap + 1, oldContentHeight: 1000,
                                                   newContentHeight: 1040, scrolling: false))
        XCTAssertFalse(ScrollToLatest.pinsToBottom(oldDistanceFromBottom: 0, oldContentHeight: 1000,
                                                   newContentHeight: 1040, scrolling: true))
        XCTAssertFalse(ScrollToLatest.pinsToBottom(oldDistanceFromBottom: 0, oldContentHeight: 1000,
                                                   newContentHeight: 1000, scrolling: false))
        XCTAssertFalse(ScrollToLatest.pinsToBottom(oldDistanceFromBottom: 0, oldContentHeight: 1000,
                                                   newContentHeight: 900, scrolling: false))
    }

    func testAShrinkAtTheVeryBottomWhileIdlePinsToTheBottom() {
        XCTAssertTrue(ScrollToLatest.pinsAfterShrink(oldDistanceFromBottom: 0, oldViewportHeight: 700,
                                                     newViewportHeight: 400, scrolling: false))
        XCTAssertTrue(ScrollToLatest.pinsAfterShrink(oldDistanceFromBottom: ScrollToLatest.gap, oldViewportHeight: 700,
                                                     newViewportHeight: 650, scrolling: false))
    }

    func testAShrinkDoesNotPinWhenAboveTheBottomScrollingOrNotShrinking() {
        XCTAssertFalse(ScrollToLatest.pinsAfterShrink(oldDistanceFromBottom: ScrollToLatest.gap + 1, oldViewportHeight: 700,
                                                      newViewportHeight: 400, scrolling: false))
        XCTAssertFalse(ScrollToLatest.pinsAfterShrink(oldDistanceFromBottom: 0, oldViewportHeight: 700,
                                                      newViewportHeight: 400, scrolling: true))
        XCTAssertFalse(ScrollToLatest.pinsAfterShrink(oldDistanceFromBottom: 0, oldViewportHeight: 700,
                                                      newViewportHeight: 700, scrolling: false))
        XCTAssertFalse(ScrollToLatest.pinsAfterShrink(oldDistanceFromBottom: 0, oldViewportHeight: 400,
                                                      newViewportHeight: 700, scrolling: false))
    }

    func testFollowsTheUsersOwnMessageEvenWhenAway() {
        XCTAssertTrue(ScrollToLatest.follows(away: true, mine: true))
    }
}
