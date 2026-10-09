// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import BrookCore
import SwiftUI
import XCTest
@testable import Brook

/// The signed-in session's lifecycle, which the view only forwards to.
@MainActor
final class SignedInSessionTests: XCTestCase {
    private func session(_ client: FakeRealtime) -> SignedInSession {
        let suite = "brook.tests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        addTeardownBlock { defaults.removePersistentDomain(forName: suite) }
        return SignedInSession(client: client, me: "me", defaults: defaults)
    }

    private func lists(_ client: FakeRealtime) -> Int { client.order.withLock { $0.filter { $0 == "list" }.count } }

    /// Leaving the screen (sign-out, local or remote) must cancel the events: a leaked
    /// subscription would keep a signed-out session's model alive and fed.
    func testStopCancelsTheEventSubscription() async {
        let client = FakeRealtime(channels: [channel("c1", "general")])
        let s = session(client)
        await s.start()
        let sub = client.subscription.withLock { $0 }
        XCTAssertNotNil(sub)
        XCTAssertFalse(sub?.cancelled.withLock { $0 } ?? true, "cancelled before stop")
        s.stop()
        XCTAssertTrue(sub?.cancelled.withLock { $0 } ?? false, "stop() left the subscription running")
    }

    /// The foreground re-read goes through the session, which is what the view calls.
    func testSceneChangesReloadThroughTheSession() async {
        let client = FakeRealtime(channels: [channel("c1", "general")])
        let s = session(client)
        await s.start()
        XCTAssertEqual(lists(client), 1)
        await s.sceneChanged(from: .inactive, to: .active)
        XCTAssertEqual(lists(client), 2)
        await s.sceneChanged(from: .active, to: .inactive)
        XCTAssertEqual(lists(client), 2)
    }

    /// iOS's session asks the model for the reconnect re-read, so a rename made while the
    /// socket was down shows without relaunching.
    func testAReconnectShowsWhatChangedWhileTheSocketWasDown() async {
        let client = FakeRealtime(channels: [channel("c1", "general")])
        let s = session(client)
        await s.start()
        client.deliver(.ready)
        await drainMain()
        client.readQueue.withLock { $0 = [[channel("c1", "renamed")]] }
        client.deliver(.ready)
        for _ in 0..<50 where s.channels.channels.first?.name != "renamed" { await drainMain(); await Task.yield() }
        XCTAssertEqual(s.channels.channels.first?.name, "renamed")
    }
}
