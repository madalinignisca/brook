import XCTest

@testable import BrookCore

/// Tokens must not appear in any textual rendering Swift can produce of a session.
final class RedactionTests: XCTestCase {
    private let access = "access-sentinel-A"
    private let refresh = "refresh-sentinel-R"

    private var session: FfiSession {
        FfiSession(
            accessToken: access,
            refreshToken: refresh,
            user: FfiUser(id: "u1", handle: "alice", displayName: "Alice", globalRole: "admin")
        )
    }

    private func renderings(of value: Any) -> [String: String] {
        var dumped = ""
        dump(value, to: &dumped)
        return [
            "print": "\(value)",
            "debugPrint": { var s = ""; debugPrint(value, to: &s); return s }(),
            "String(reflecting:)": String(reflecting: value),
            "dump": dumped,
        ]
    }

    private func assertRedacted(_ value: Any, file: StaticString = #filePath, line: UInt = #line) {
        for (how, text) in renderings(of: value) {
            XCTAssertFalse(text.contains(access), "\(how) leaked the access token: \(text)", file: file, line: line)
            XCTAssertFalse(text.contains(refresh), "\(how) leaked the refresh token: \(text)", file: file, line: line)
        }
    }

    func testSessionNeverRendersTokens() {
        assertRedacted(session)
    }

    func testLoginResultNeverRendersTokens() {
        assertRedacted(LoginResult.loggedIn(session: session))
    }

    func testRedactedRenderingStillIdentifiesTheUser() {
        XCTAssertTrue("\(session)".contains("alice"))
    }

    /// Recovery codes and authenticator codes never render either.
    func testSecondFactorsNeverRenderTheirCode() {
        for factor in [FfiSecondFactor.code(code: "481516"), .recovery(code: "rcode-sentinel-X")] {
            for (how, text) in renderings(of: factor) {
                XCTAssertFalse(text.contains("481516") || text.contains("rcode-sentinel-X"),
                               "\(how) leaked a code: \(text)")
            }
        }
    }
}
