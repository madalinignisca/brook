// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import BrookCore
import XCTest
@testable import Brook

/// The Mac's wording, pinned as literals exactly as it read before the device name became
/// `ThisDevice`. Comparing against `ThisDevice` itself would pass whatever it held; the
/// literals fail if the Mac's text drifts.
final class ThisDeviceTests: XCTestCase {
    @MainActor func testTheMacWordingIsUnchanged() {
        XCTAssertEqual(
            SessionStore.Message.unreachableLAN,
            "Couldn't reach the server. If macOS asked to allow local network access, allow it and try again.")
        XCTAssertEqual(
            SessionStore.Message.signOutIncomplete,
            "This Mac couldn't forget your saved sign-in, so Brook may sign you in again at the next launch. Sign in and out again to retry.")
        XCTAssertEqual(
            SessionStore.Message.removalIncomplete,
            "Brook couldn't remove all of this Mac's data. Sign in and out again to retry.")
        XCTAssertEqual(
            SessionStore.Message.removalAndSignOutIncomplete,
            "Brook couldn't remove all of this Mac's data, and may sign you in again at the next launch. Sign in and out again to retry.")
        XCTAssertEqual(
            ComposerModel.explainFiles(LoginError.Api(code: "local.unavailable", message: "")),
            "Sending files needs this Mac's storage, which isn't available yet.")
        XCTAssertEqual(Settings.fallbackServer, "https://localhost")
    }
}
