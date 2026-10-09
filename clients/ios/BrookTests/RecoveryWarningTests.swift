// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import XCTest
@testable import Brook

/// The one line above the list after a sign-in with a recovery code.
final class RecoveryWarningTests: XCTestCase {
    func testWarnsAtTwoOrFewerAndPluralizes() {
        XCTAssertNil(recoveryCodesWarning(left: nil), "no recovery sign-in: no line")
        XCTAssertNil(recoveryCodesWarning(left: 3))
        XCTAssertEqual(recoveryCodesWarning(left: 2), "You have 2 recovery codes left.")
        XCTAssertEqual(recoveryCodesWarning(left: 1), "You have 1 recovery code left.")
        XCTAssertEqual(recoveryCodesWarning(left: 0), "You have 0 recovery codes left.")
    }
}
