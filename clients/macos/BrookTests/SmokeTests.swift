import BrookCore
import XCTest

/// Proves the app target links the Rust core and can call into it.
final class SmokeTests: XCTestCase {
    func testAppLinksAndCallsTheRustCore() throws {
        _ = try FfiBrookClient(baseUrl: "https://brook.invalid", allowInsecureHttp: false)
        XCTAssertThrowsError(
            try FfiBrookClient(baseUrl: "http://192.168.1.50", allowInsecureHttp: false)
        ) { error in
            XCTAssertEqual(error as? LoginError, .InsecureServerUrl)
        }
    }
}
