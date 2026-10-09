// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import BrookCore
import XCTest

@testable import Brook

/// A message is read only when it can be seen: the app active (#145's note, as GTK #177).
@MainActor
final class ReadWhenActiveTests: XCTestCase {
    private final class Active: @unchecked Sendable { var value = true }

    private func settle() async { for _ in 0 ..< 10 { await Task.yield(); try? await Task.sleep(for: .milliseconds(5)) } }

    func testInTheBackgroundAMessageIsOwedAndReadOnceActive() async {
        let chat = FakeChat()
        let active = Active()
        active.value = false
        let t = TimelineModel(channelId: "c", client: chat, isActive: { active.value })
        t.apply(.messageNew(message: msg("m1", "hi")))
        await settle()
        XCTAssertTrue(chat.read.isEmpty, "read while the app was in the background")
        XCTAssertTrue(t.readOwed)
        active.value = true
        t.appBecameActive()
        await settle()
        XCTAssertEqual(chat.read, ["m1"])
        XCTAssertFalse(t.readOwed)
    }

    func testInFrontItsReadAtOnce() async {
        let chat = FakeChat()
        let t = TimelineModel(channelId: "c", client: chat, isActive: { true })
        t.apply(.messageNew(message: msg("m1", "hi")))
        await settle()
        XCTAssertEqual(chat.read, ["m1"])
        XCTAssertFalse(t.readOwed)
    }

    func testTheOpenChannelsBadgeShowsWhileItsUnseen() async {
        let client = FakeRealtime(channels: [channel("c", "general")])
        let model = ChannelsModel(client: client)
        await model.start()
        model.openChannel = "c"
        let t = TimelineModel(channelId: "c", client: FakeChat(), isActive: { false })
        model.timeline = t
        model.handle(.messageNew(message: msg("m1", "hi")))
        let row = ChannelRow(id: "c", name: "general", unread: 1)
        XCTAssertEqual(model.unread(row), 1, "an unseen message in the open channel had no badge")
        t.appBecameActive()
        XCTAssertNil(model.unread(row))
    }
}

/// The server's events reach the open conversation through `ChannelsModel` (kept apart from
/// `ChannelEventsTests`, which needs no `TimelineModel`, so that file doesn't depend on it).
@MainActor
final class TimelineEventsTests: XCTestCase {
    // Copies of the private helpers in `ChannelEventsTests`: ten test-only lines are cheaper
    // than widening the originals into the shared test support file.
    private func started(_ channels: [FfiChannel]) async -> (FakeRealtime, ChannelsModel) {
        let client = FakeRealtime(channels: channels)
        let model = ChannelsModel(client: client, me: "me", isActive: { true }, defaults: isolatedDefaults())
        await model.start()
        return (client, model)
    }

    /// Until `condition` holds, or give up (a list re-read runs in its own task).
    private func settle(_ condition: () -> Bool) async {
        for _ in 0..<50 where !condition() {
            await drainMain()
            await Task.yield()
        }
    }

    /// `.ready` reaches the open timeline through the model's event path: a head load that
    /// failed is tried again on the reconnect.
    func testReadyRetriesTheOpenTimelinesFailedHead() async {
        let (client, model) = await started([channel("c1", "general")])
        let chat = FakeChat()
        chat.historyFailure = LoginError.Network(message: "offline")
        let t = TimelineModel(channelId: "c1", client: chat)
        model.openChannel = "c1"
        model.timeline = t
        await t.load()
        XCTAssertNotNil(t.error)
        chat.historyFailure = nil
        chat.pages = [[msg("m1", "back", channel: "c1")]]
        client.deliver(.ready)
        await settle { chat.historyCalls.withLock { $0 } >= 2 && t.error == nil && !t.loading }
        XCTAssertEqual(chat.historyCalls.withLock { $0 }, 2)
        XCTAssertNil(t.error)
    }
}
