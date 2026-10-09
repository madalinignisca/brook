// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import BrookCore
import XCTest

@testable import Brook

/// The banner over the box while replying names the author as the setting says (#238).
@MainActor
final class ReplyBannerTests: XCTestCase {
    private func replying(to name: String?, _ handle: String?) -> FfiMessage {
        var m = msg("m1", "hi")
        (m.authorDisplayName, m.authorHandle) = (name, handle)
        return m
    }

    func testReplyBannerFollowsShowUsernames() {
        let bob = replying(to: "Bob", "bob")
        XCTAssertEqual(ComposerView.replyBanner(bob, showUsernames: false), "Replying to Bob")
        XCTAssertEqual(ComposerView.replyBanner(bob, showUsernames: true), "Replying to @bob")
        for on in [false, true] {
            XCTAssertEqual(ComposerView.replyBanner(replying(to: nil, nil), showUsernames: on),
                           "Replying to a message")
            XCTAssertEqual(ComposerView.replyBanner(replying(to: " ", ""), showUsernames: on),
                           "Replying to a message")
        }
    }
}
