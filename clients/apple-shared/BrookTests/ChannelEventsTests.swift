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

    /// A call that ended while the socket was down is never announced as ended: the server just
    /// sends nothing for it after the next `ready`. So `ready` must drop every badge, and the
    /// calls still running get theirs back from the `channel.call` the server re-sends.
    func testReadyClearsLiveCalls() async {
        let (client, model) = await started([channel("c1", "general")])
        client.deliver(.channelCall(channelId: "c1", callId: "k1", participantCount: 2))
        await drainMain()
        XCTAssertNotNil(model.badge(model.channels[0]))

        // The reconnect: the call ended meanwhile, so nothing is re-sent.
        client.deliver(.ready)
        await drainMain()
        XCTAssertNil(model.badge(model.channels[0]), "a badge for a call that ended survived ready")
        XCTAssertTrue(model.liveCalls.isEmpty)

        // The call is still running: the server re-sends it after ready.
        client.deliver(.channelCall(channelId: "c1", callId: "k1", participantCount: 2))
        await drainMain()
        XCTAssertEqual(model.badge(model.channels[0]), "● Call · 2")
    }

    /// iOS keeps no cache, so a rename made while the socket was down is only seen by re-reading
    /// on the reconnect's `ready`. The first `ready` does not (start() read already), and the
    /// default (the Mac) never does: its cache feed re-reads instead.
    func testReconnectReadyRereadsTheListOnlyWhenAsked() async {
        func lists(_ c: FakeRealtime) -> Int { c.order.withLock { $0.filter { $0 == "list" }.count } }
        for (asked, expected) in [(true, 2), (false, 1)] {
            let client = FakeRealtime(channels: [channel("c1", "general")])
            let suite = "brook.tests.\(UUID().uuidString)"
            let defaults = UserDefaults(suiteName: suite)!
            addTeardownBlock { defaults.removePersistentDomain(forName: suite) }
            let model = ChannelsModel(client: client, me: "me", isActive: { true }, defaults: defaults,
                                      rereadOnReconnect: asked)
            await model.start()
            client.deliver(.ready) // the first ready
            await drainMain(); await Task.yield()
            XCTAssertEqual(lists(client), 1, "asked: \(asked): the first ready read again")
            client.readQueue.withLock { $0 = [[channel("c1", "renamed")]] }
            client.deliver(.ready) // a reconnect
            for _ in 0..<50 where lists(client) < expected { await drainMain(); await Task.yield() }
            await drainMain()
            XCTAssertEqual(lists(client), expected, "asked: \(asked)")
            XCTAssertEqual(model.channels.first?.name, asked ? "renamed" : "general", "asked: \(asked)")
        }
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
