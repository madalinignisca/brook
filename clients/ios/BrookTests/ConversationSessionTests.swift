// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import BrookCore
import SwiftUI
import XCTest
@testable import Brook

/// The open conversation's lifecycle, which the view only forwards to. `FakeRealtime` is the
/// list's client (and delivers server events); `FakeChat` is the conversation's.
@MainActor
final class ConversationSessionTests: XCTestCase {
    /// Whether the app counts as active, flipped by a test.
    private final class Activity: @unchecked Sendable { var active = true }

    private func list(_ rows: [FfiChannel]) async -> (FakeRealtime, ChannelsModel) {
        let client = FakeRealtime(channels: rows)
        let suite = "brook.tests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        addTeardownBlock { defaults.removePersistentDomain(forName: suite) }
        let model = ChannelsModel(client: client, me: "me", isActive: { true }, defaults: defaults)
        await model.start()
        return (client, model)
    }

    private func session(_ id: String, _ channels: ChannelsModel, _ chat: FakeChat,
                         activity: Activity = Activity()) -> ConversationSession {
        ConversationSession(channelId: id, channels: channels, client: chat, me: "me",
                            isActive: { activity.active })
    }

    private func shown(_ s: ConversationSession) -> [String] { s.timeline.messages.map(\.id) }

    /// Until `condition` holds, or give up after a while (re-reads run in their own tasks).
    private func settle(_ condition: () -> Bool) async {
        for _ in 0 ..< 200 where !condition() {
            await Task.yield()
            try? await Task.sleep(for: .milliseconds(5))
        }
    }

    private func history(_ chat: FakeChat) -> Int { chat.historyCalls.withLock { $0 } }

    func testOpeningSetsTheOpenChannelAndItsEventsAndLeavingClearsThem() async {
        let (client, channels) = await list([channel("c1", "general")])
        let s = session("c1", channels, FakeChat())
        await s.start()
        XCTAssertEqual(channels.openChannel, "c1")
        client.deliver(.messageNew(message: msg("m1", "hi", channel: "c1")))
        await drainMain()
        XCTAssertEqual(shown(s), ["m1"], "an event for the open channel did not reach its timeline")

        s.stop()
        XCTAssertNil(channels.openChannel, "stop() left the channel open")
        XCTAssertNil(channels.timeline, "stop() left the timeline receiving events")
        client.deliver(.messageNew(message: msg("m2", "later", channel: "c1")))
        await drainMain()
        XCTAssertEqual(shown(s), ["m1"], "an event reached a stopped conversation")
    }

    /// Opening B before A has stopped (a fast back and open): A's stop must not close B.
    func testLeavingAnOlderConversationLeavesTheNewerOneOpen() async {
        let (client, channels) = await list([channel("c1", "general"), channel("c2", "random")])
        let a = session("c1", channels, FakeChat())
        let b = session("c2", channels, FakeChat())
        await a.start()
        await b.start()
        a.stop()
        XCTAssertEqual(channels.openChannel, "c2", "A's stop closed B")
        client.deliver(.messageNew(message: msg("m1", "hi", channel: "c2")))
        await drainMain()
        XCTAssertEqual(shown(b), ["m1"], "A's stop cut B off from its events")
    }

    /// The same channel opened again before the first view stopped: the channel id matches, so only
    /// the timeline tells the two sessions apart.
    func testLeavingAndReopeningTheSameChannelKeepsTheNewOneOpen() async {
        let (client, channels) = await list([channel("c1", "general")])
        let first = session("c1", channels, FakeChat())
        let second = session("c1", channels, FakeChat())
        await first.start()
        await second.start()
        first.stop()
        XCTAssertEqual(channels.openChannel, "c1")
        XCTAssertTrue(channels.timeline === second.timeline, "the first stop took the second's timeline")
        client.deliver(.messageNew(message: msg("m1", "hi", channel: "c1")))
        await drainMain()
        XCTAssertEqual(shown(second), ["m1"])
    }

    /// Proves iOS passes `rereadOnReady`: without it a `ready` only retries a failed head.
    func testTheOpenConversationRereadsOnEveryReady() async {
        let (client, channels) = await list([channel("c1", "general")])
        let chat = FakeChat()
        let s = session("c1", channels, chat)
        await s.start()
        XCTAssertEqual(history(chat), 1)
        client.deliver(.ready)
        await drainMain()
        await settle { !s.timeline.isRereading }
        XCTAssertEqual(history(chat), 2, "the first ready did not re-read")
        client.deliver(.ready)
        await drainMain()
        await settle { !s.timeline.isRereading }
        XCTAssertEqual(history(chat), 3, "the second ready did not re-read")
    }

    func testComingBackToTheForegroundRereadsTheConversation() async {
        let (client, channels) = await list([channel("c1", "general")])
        let chat = FakeChat()
        let activity = Activity()
        activity.active = false
        let s = session("c1", channels, chat, activity: activity)
        await s.start()
        // A message arrives while the app is not active: its read is owed.
        client.deliver(.messageNew(message: msg("m1", "hi", channel: "c1")))
        await drainMain()
        XCTAssertTrue(s.timeline.readOwed)
        let before = history(chat)

        // Neither leaving the foreground nor a hop between the two non-active phases re-reads.
        XCTAssertNil(s.sceneChanged(from: .active, to: .inactive))
        XCTAssertNil(s.sceneChanged(from: .background, to: .inactive))
        XCTAssertEqual(history(chat), before)

        // The way back is background -> inactive -> active: the last hop is the trigger. The
        // re-read brings the same newest message; what was owed is then read.
        activity.active = true
        chat.pages = [[msg("m1", "hi", channel: "c1")]]
        await s.sceneChanged(from: .inactive, to: .active)?.value
        XCTAssertEqual(history(chat), before + 1, "returning to the foreground did not re-read")
        // `appBecameActive` sends the read in a task of its own, so wait for it to land.
        await settle { chat.read.last == "m1" }
        XCTAssertEqual(chat.read.last, "m1", "what was owed was not read after the re-read")
    }

    func testAChannelGoneFromTheListClosesTheConversation() async {
        // The live event: `channel.delete` sets `closed` and drops the row.
        let (client, channels) = await list([channel("c1", "general")])
        let s = session("c1", channels, FakeChat())
        await s.start()
        XCTAssertFalse(s.isRemoved)
        client.deliver(.channelDelete(channelId: "c1"))
        await drainMain()
        XCTAssertTrue(s.isRemoved, "a channel.delete did not close the conversation")
        XCTAssertEqual(s.title, "#general", "the title did not survive the removal")
        s.stop()
        XCTAssertNil(channels.openChannel, "stop() after a removal left the channel open")

        // A list re-read without the channel (the reconnect path) never sets `closed`.
        let (client2, channels2) = await list([channel("c1", "general")])
        let s2 = session("c1", channels2, FakeChat())
        await s2.start()
        client2.readQueue.withLock { $0 = [[]] }
        await channels2.reloadList()
        XCTAssertNil(channels2.closed)
        XCTAssertTrue(s2.isRemoved, "a re-read that dropped the row did not close the conversation")
    }

    /// After a removal `timeline` is nil but `openChannel` still names the removed channel. Another
    /// conversation's stop must not take that as "nothing is open" and clear it: only the
    /// `openChannel == channelId` half of the guard says it is not this one's.
    func testLeavingAfterAnotherChannelWasRemovedLeavesItsOpenChannelAlone() async {
        let (client, channels) = await list([channel("c1", "general"), channel("c2", "random")])
        let a = session("c1", channels, FakeChat())
        let b = session("c2", channels, FakeChat())
        await a.start()
        await b.start()
        client.deliver(.channelDelete(channelId: "c2"))
        await drainMain()
        XCTAssertNil(channels.timeline)
        XCTAssertEqual(channels.openChannel, "c2")
        a.stop()
        XCTAssertEqual(channels.openChannel, "c2", "A's stop cleared the open channel of B, which is not A's")
    }

    func testOpeningClearsTheRowsMentionBadge() async {
        let withMention = FfiChannel(id: "c1", kind: "public", name: "general", archived: false, topic: nil,
                                     isPublic: false, unreadMentions: 1, members: [], ownerOffers: [])
        let (_, channels) = await list([withMention])
        let s = session("c1", channels, FakeChat())
        XCTAssertEqual(channels.mentions(channels.channels[0]), 1)
        await s.start()
        XCTAssertNil(channels.mentions(channels.channels[0]), "opening left the mention badge")
    }

    /// Send shows the answer at once, and the live echo of the same message merges into that row.
    func testASentMessageShowsOnceWhenItsEchoArrives() async {
        let (client, channels) = await list([channel("c1", "general")])
        let s = session("c1", channels, FakeChat())
        await s.start()
        s.composer.text = "hi"
        await s.composer.send()
        XCTAssertEqual(shown(s), ["m9"], "the sent message did not show before its echo")
        client.deliver(.messageNew(message: msg("m9", "hi", channel: "c1")))
        await drainMain()
        XCTAssertEqual(shown(s), ["m9"], "the echo made a second row")
    }

    /// `channel.update` archives the open channel: the view swaps the box for the note from this.
    func testArchivingTheChannelMakesItArchived() async {
        let (client, channels) = await list([channel("c1", "general")])
        let s = session("c1", channels, FakeChat())
        await s.start()
        XCTAssertFalse(s.archived)
        var archived = channel("c1", "general")
        archived.archived = true
        client.deliver(.channelUpdate(channel: archived))
        await drainMain()
        XCTAssertTrue(s.archived, "an archiving update did not reach the open conversation")
    }
}
