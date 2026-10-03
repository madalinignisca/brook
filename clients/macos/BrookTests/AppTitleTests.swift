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
}
