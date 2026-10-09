// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import XCTest

@testable import BrookCore

/// Second-factor codes must not appear in any textual rendering Swift can produce, and a
/// session still renders as its user. (Tokens no longer cross the FFI, so there are none to test.)
final class RedactionTests: XCTestCase {
    private var session: FfiSession {
        FfiSession(
            user: FfiUser(id: "u1", handle: "alice", displayName: "Alice", globalRole: "admin", statusText: nil)
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
