@testable import Brook
import XCTest

final class ServerAddressTests: XCTestCase {
    func testAcceptsPlainServerAddresses() {
        for ok in ["https://chat.example.com", "http://192.168.1.192:8080", "https://host.lan/brook"] {
            XCTAssertEqual(try ServerAddress.parse(ok).get(), ok, ok)
        }
    }

    func testTrimsSurroundingWhitespace() {
        XCTAssertEqual(try ServerAddress.parse("  https://chat.example.com \n").get(), "https://chat.example.com")
    }

    /// Userinfo would be persisted as "last server" — a secret in UserDefaults.
    func testRejectsCredentialsQueryAndFragment() {
        for bad in ["https://u:secret@host", "https://u@host", "https://host?x=1", "https://host#f"] {
            XCTAssertEqual(ServerAddress.parse(bad), .failure(.notJustAnAddress), bad)
        }
    }

    func testRejectsMissingSchemeOrHost() {
        for bad in ["chat.example.com", "https://", "ftp://host", ""] {
            XCTAssertEqual(ServerAddress.parse(bad), .failure(.invalid), bad)
        }
    }
}
