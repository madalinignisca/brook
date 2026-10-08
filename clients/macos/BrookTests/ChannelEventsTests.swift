// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import BrookCore
import XCTest
@testable import Brook

/// The server's channel events reach the list without local data (#183): a leave or a
/// removal closes the channel, and an update replaces its row.
@MainActor
final class ChannelEventsTests: XCTestCase {
    private let bob = FfiMember(id: "u2", handle: "bob", displayName: "Bob", role: nil)

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

    func testADeleteRemovesTheRowAndClosesTheOpenChannel() async {
        let (client, model) = await started([channel("c1", "general"), channel("c2", "random")])
        model.openChannel = "c2"
        client.deliver(.channelDelete(channelId: "c2"))
        await drainMain()
        XCTAssertEqual(model.channels.map(\.id), ["c1"])
        XCTAssertEqual(model.closed, "c2")
    }

    func testAnUpdateReplacesTheRowAndKeepsItsUnreadCount() async {
        let (client, model) = await started([channel("c1", "general")])
        client.deliver(.messageNew(message: msg("m1", "hi", channel: "c1"))) // from "u": counts
        await drainMain()
        let unread = model.channels[0].unread
        XCTAssertEqual(unread, 1)
        var renamed = channel("c1", "renamed")
        renamed.members = [bob]
        client.deliver(.channelUpdate(channel: renamed))
        await drainMain()
        XCTAssertEqual(model.channels[0].name, "renamed")
        XCTAssertEqual(model.channels[0].members, [bob])
        XCTAssertEqual(model.channels[0].unread, unread)
    }

    /// Archived channels stay in the list, read-only, so an owner can open one and unarchive it.
    func testAnArchivingUpdateKeepsTheRowMarkedArchivedAndOpen() async {
        let (client, model) = await started([channel("c1", "general"), channel("c2", "random")])
        model.openChannel = "c2"
        var archived = channel("c2", "random")
        archived.archived = true
        client.deliver(.channelUpdate(channel: archived))
        await drainMain()
        XCTAssertEqual(model.channels.map(\.id), ["c1", "c2"])
        XCTAssertEqual(model.channels.map(\.archived), [false, true])
        XCTAssertNil(model.closed, "archiving doesn't close the channel")
    }

    func testAnArchivedChannelIsListedByARead() async {
        var old = channel("c2", "old")
        old.archived = true
        let (_, model) = await started([channel("c1", "general"), old])
        XCTAssertEqual(model.channels.map(\.archived), [false, true])
    }

    /// A late update for a channel just left never brings it back by itself: only the
    /// server's list can, when the user is really a member again.
    func testAnUpdateAfterADeleteDoesNotBringTheChannelBack() async {
        let (client, model) = await started([channel("c1", "general"), channel("c2", "random")])
        client.deliver(.channelDelete(channelId: "c2"))
        client.channels = [channel("c1", "general")] // the server no longer lists it
        client.deliver(.channelUpdate(channel: channel("c2", "random")))
        await settle { false } // let the re-read finish
        XCTAssertEqual(model.channels.map(\.id), ["c1"])
    }

    func testAnUpdateForAChannelNotListedReadsTheList() async {
        let (client, model) = await started([channel("c1", "general")])
        client.channels = [channel("c1", "general"), channel("c3", "new")] // added to c3
        client.deliver(.channelUpdate(channel: channel("c3", "new")))
        await settle { model.channels.count == 2 }
        XCTAssertEqual(model.channels.map(\.id), ["c1", "c3"])
    }
}
