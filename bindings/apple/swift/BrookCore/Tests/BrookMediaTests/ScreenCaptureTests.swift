import XCTest
@testable import BrookMedia

final class ScreenCaptureTests: XCTestCase {
    /// Screens are sent at most 1920×1080, aspect kept, even dimensions (H.264 needs them).
    func testSizeFitsWithinFullHDKeepingAspect() {
        let retina = ScreenCapture.size(for: CGSize(width: 1512, height: 982), scale: 2)  // 3024×1964
        XCTAssertLessThanOrEqual(retina.width, 1920)
        XCTAssertLessThanOrEqual(retina.height, 1080)
        XCTAssertEqual(Double(retina.width) / Double(retina.height), 3024.0 / 1964.0, accuracy: 0.01)
        XCTAssertEqual(retina.width % 2, 0)
        XCTAssertEqual(retina.height % 2, 0)
        let small = ScreenCapture.size(for: CGSize(width: 801, height: 601), scale: 1)
        XCTAssertEqual(small.width, 800)  // not upscaled; made even
        XCTAssertEqual(small.height, 600)
    }
}
