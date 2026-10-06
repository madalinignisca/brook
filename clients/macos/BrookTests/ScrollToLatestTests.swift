import Testing

@testable import Brook

@Suite struct ScrollToLatestTests {
    @Test func atTheBottomIsNotAway() {
        #expect(!ScrollToLatest.isAway(contentHeight: 2000, offset: 1400, viewportHeight: 600))
    }

    @Test func slightlyUpIsNotAway() {
        #expect(!ScrollToLatest.isAway(contentHeight: 2000, offset: 1330, viewportHeight: 600))
    }

    @Test func farUpIsAway() {
        #expect(ScrollToLatest.isAway(contentHeight: 2000, offset: 800, viewportHeight: 600))
    }

    @Test func shortContentIsNeverAway() {
        #expect(!ScrollToLatest.isAway(contentHeight: 300, offset: 0, viewportHeight: 600))
    }
}
