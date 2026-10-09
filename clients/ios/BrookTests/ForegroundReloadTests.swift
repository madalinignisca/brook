// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import BrookCore
import SwiftUI
import XCTest
@testable import Brook

/// What the list does when the app comes back from the background.
@MainActor
final class ForegroundReloadTests: XCTestCase {
    private func started() async -> (FakeRealtime, ChannelsModel) {
        let client = FakeRealtime(channels: [channel("c1", "general")])
        let suite = "brook.tests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        addTeardownBlock { defaults.removePersistentDomain(forName: suite) }
        let model = ChannelsModel(client: client, me: "me", isActive: { true }, defaults: defaults)
        await model.start()
        return (client, model)
    }

    private func lists(_ client: FakeRealtime) -> Int { client.order.withLock { $0.filter { $0 == "list" }.count } }

    /// `start()` reads once. Coming back to the foreground reads again, by whichever path:
    /// iOS goes background -> inactive -> active when the app returns, so the second step
    /// (inactive -> active) is the one that happens in practice. Going active -> inactive (a
    /// notification pulled down, the app switcher) must not read.
    func testComingBackToTheForegroundReadsTheListAgain() async {
        let (client, model) = await started()
        XCTAssertEqual(lists(client), 1)

        await model.sceneChanged(from: .background, to: .active)
        XCTAssertEqual(lists(client), 2)

        await model.sceneChanged(from: .active, to: .inactive)
        XCTAssertEqual(lists(client), 2, "leaving the foreground read the list")

        await model.sceneChanged(from: .inactive, to: .active)
        XCTAssertEqual(lists(client), 3, "inactive -> active is the real way back from suspension")

        await model.sceneChanged(from: .active, to: .active)
        XCTAssertEqual(lists(client), 3, "a no-change must not read")
    }
}
