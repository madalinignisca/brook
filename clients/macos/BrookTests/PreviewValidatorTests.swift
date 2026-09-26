import XCTest

@testable import Brook

/// The decoder's reply is untrusted (previews spec §2, §5): exact sizes pass, anything else
/// is no preview.
final class PreviewValidatorTests: XCTestCase {
    private func image(_ w: Int, _ h: Int, bytes: Int? = nil, code: Int = 0,
                       header: (Int, Int) = (1000, 800)) -> CGImage? {
        PreviewValidator.image(code: code, width: w, height: h, rgba: Data(count: bytes ?? w * h * 4),
                               header: (width: header.0, height: header.1))
    }

    func testExactSizesPass() {
        let img = image(720, 576)
        XCTAssertEqual(img?.width, 720)
        XCTAssertEqual(img?.height, 576)
        XCTAssertEqual(img?.bitsPerPixel, 32)
    }

    func testAReplyRotatedByExifWithinTheHeaderPasses() {
        XCTAssertNotNil(image(576, 720), "the sides swapped by EXIF orientation")
    }

    func testEveryMalformedReplyIsRefused() {
        XCTAssertNil(image(10, 10, bytes: 399), "a short buffer")
        XCTAssertNil(image(10, 10, bytes: 401), "one extra byte")
        XCTAssertNil(image(0, 10, bytes: 0), "a zero side")
        XCTAssertNil(image(-4, -4, bytes: 64), "negative sides")
        XCTAssertNil(image(721, 10), "a side over 720")
        XCTAssertNil(image(10, 10, code: ReplyCode.timeout.rawValue), "a failure code")
        XCTAssertNil(image(100, 100, header: (50, 50)), "bigger than the image itself")
        XCTAssertNil(PreviewValidator.image(code: 0, width: Int.max / 2, height: 3, rgba: Data(count: 12),
                                            header: (width: Int.max, height: Int.max)),
                     "a size that overflows")
    }
}
