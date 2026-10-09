// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import BrookCore
import XCTest

@testable import Brook

/// The iOS re-read (`rereadOnReady`): every newest-page fetch also closes the gap behind it.
/// Message ids are zero-padded ("a05") so string order is time order, as UUIDv7 is.
///
/// `FakeChat.pages` is a queue: each `channelHistory` call takes the next page (when the call is
/// released, for a call held by `olderGate`). A test lists the pages in the order the calls are
/// made; an exhausted queue answers an empty page.
@MainActor
final class TimelineRereadTests: XCTestCase {
    private func ids(_ t: TimelineModel) -> [String] { t.messages.map(\.id) }
    private func page(_ ids: String...) -> [FfiMessage] { ids.map { msg($0, "body \($0)") } }

    private func timeline(_ chat: FakeChat, flag: Bool = true, active: Bool = true) -> TimelineModel {
        TimelineModel(channelId: "c", client: chat, isActive: { active }, rereadOnReady: flag)
    }

    /// Until `condition` holds, or give up after a while (re-reads run in their own tasks).
    private func settle(_ condition: () -> Bool) async {
        for _ in 0 ..< 200 where !condition() {
            await Task.yield()
            try? await Task.sleep(for: .milliseconds(5))
        }
    }

    /// Hand the main actor to the jobs already queued, a few times. Main-actor jobs run in the
    /// order they were queued, and a task started by `apply` does all its synchronous work (joining
    /// the drain, or making its history call) before its first suspension, so a few turns are enough
    /// for it to have shown what it does. Used only where a fetch is HELD, so the state cannot move
    /// on by itself; elsewhere `isRereading` says when everything has finished.
    private func letQueuedJobsRun() async {
        for _ in 0 ..< 10 { await Task.yield() }
    }

    private func asked(_ chat: FakeChat) -> [String?] { chat.historyAsked.withLock { $0 } }

    /// Every history call so far is answered and no fetch is running.
    private func idle(_ chat: FakeChat, _ t: TimelineModel, calls: Int) -> Bool {
        chat.historyCalls.withLock { $0 } >= calls && !t.loading
    }

    // 1
    func testWithoutTheFlagAReadyOnlyRetriesAFailedHead() async {
        let chat = FakeChat()
        chat.pages = [page("a01")]
        // The default init, as the Mac builds it: the default itself is what is under test.
        let t = TimelineModel(channelId: "c", client: chat, isActive: { true })
        await t.load()
        t.apply(.ready)
        // `isRereading` is true from the moment a re-read is asked for, so with the flag wrongly on
        // this waits for the extra fetch to finish instead of looking too early.
        await settle { !t.isRereading }
        XCTAssertEqual(chat.historyCalls.withLock { $0 }, 1, "the Mac's ready re-read the newest page")
    }

    // 2
    func testWithTheFlagEveryReadyRereadsTheNewestPage() async {
        let chat = FakeChat()
        chat.pages = [page("a01")]
        let t = timeline(chat)
        await t.load()
        // One at a time: two readys delivered back to back, before the first fetch has started, are
        // served by that one fetch (the drain's existing rule; test 7 covers a ready that arrives
        // while a fetch runs).
        t.apply(.ready)
        await settle { self.idle(chat, t, calls: 2) }
        t.apply(.ready)
        await settle { !t.isRereading }
        XCTAssertEqual(chat.historyCalls.withLock { $0 }, 3, "an extra fetch was scheduled")
    }

    // 3
    func testARereadThatOverlapsMerges() async {
        let chat = FakeChat()
        chat.pages = [page("a01", "a02", "a03"), page("a03", "a04")]
        let t = timeline(chat)
        await t.load()
        await t.reread().value
        XCTAssertEqual(ids(t), ["a01", "a02", "a03", "a04"])
        XCTAssertEqual(asked(chat), [nil, nil], "paged back though the page met what was shown")
        XCTAssertEqual(t.replaced, 0)
    }

    // 4
    func testARereadThatDoesNotMeetPagesBackUntilAPageMeets() async {
        let chat = FakeChat()
        chat.pages = [page("a05", "a06"), page("a10", "a11"), page("a08", "a09"), page("a06", "a07")]
        let t = timeline(chat)
        await t.load()
        await t.reread().value
        XCTAssertEqual(ids(t), ["a05", "a06", "a07", "a08", "a09", "a10", "a11"])
        XCTAssertEqual(asked(chat), [nil, nil, "a10", "a08"])
        XCTAssertEqual(t.replaced, 0)
    }

    // 5
    func testARereadThatNeverMeetsReplacesAfterThreePagesAndResets() async {
        let chat = FakeChat()
        // The load, an empty older page (the start of the channel), then the re-read's four pages.
        chat.pages = [page("a01", "a02"), [], page("a20"), page("a18"), page("a16"), page("a14")]
        let t = timeline(chat)
        await t.load()
        await t.loadOlder()
        XCTAssertTrue(t.atStart)
        await t.reread().value
        XCTAssertEqual(ids(t), ["a14", "a16", "a18", "a20"])
        XCTAssertFalse(t.atStart, "the replace left atStart on, so the loader never asks again")
        // `olderFailed` is not asserted: it cannot be true together with `atStart` (a new ask clears
        // it first), and every successful head fetch clears it anyway, so no test of the replace
        // could tell whether the replace branch did.
        XCTAssertEqual(t.replaced, 1)
        XCTAssertEqual(asked(chat), [nil, "a01", nil, "a20", "a18", "a16"], "four calls for the re-read")
    }

    // 6
    func testAnOlderPageStartedBeforeAReplacingRereadIsDiscarded() async {
        let chat = FakeChat()
        // The load; the re-read's four pages; then the held older page's answer (taken when the
        // gate opens, after the re-read took its pages) and the answer to the restarted ask.
        chat.pages = [page("a01", "a02"), page("a20"), page("a18"), page("a16"), page("a14"),
                      page("a00"), page("a13")]
        let t = timeline(chat)
        await t.load()
        chat.olderGate = Gate()
        let gate = chat.olderGate!
        let older = Task { await t.loadOlder() }
        await settle { self.asked(chat).count == 2 } // the older request is held
        t.apply(.ready)
        await settle { t.replaced == 1 }
        XCTAssertEqual(ids(t), ["a14", "a16", "a18", "a20"])
        gate.open()
        await older.value
        XCTAssertFalse(ids(t).contains("a00"), "an older page of replaced history was merged")
        XCTAssertEqual(asked(chat).last, "a14", "loadOlder did not start over from the new oldest")
        XCTAssertFalse(t.atStart)
        XCTAssertEqual(ids(t), ["a13", "a14", "a16", "a18", "a20"])
    }

    // 7
    func testAReadyDuringThePageBackJoinsIt() async {
        let chat = FakeChat()
        chat.pages = [page("a05", "a06"), page("a10", "a11"), page("a06", "a07"), page("a06", "a10", "a11", "a12")]
        let t = timeline(chat)
        await t.load()
        chat.olderGate = Gate()
        let gate = chat.olderGate!
        t.apply(.ready)
        await settle { self.asked(chat).count == 3 } // the head, then its page-back, held
        t.apply(.ready)
        await letQueuedJobsRun()
        XCTAssertEqual(asked(chat), [nil, nil, "a10"], "a second head fetch started beside the page-back")
        gate.open()
        await settle { !t.isRereading }
        XCTAssertEqual(asked(chat), [nil, nil, "a10", nil], "exactly one more head fetch follows")
        XCTAssertEqual(chat.historyMaxInFlight.withLock { $0 }, 1, "two history calls ran at once")
        XCTAssertEqual(ids(t), ["a05", "a06", "a07", "a10", "a11", "a12"])
    }

    // 8
    func testAPageBackThatFailsKeepsTheShownMessagesAndSaysSo() async {
        let chat = FakeChat()
        chat.pages = [page("a05", "a06"), page("a10", "a11")]
        let t = timeline(chat)
        await t.load()
        chat.olderGate = Gate()
        let gate = chat.olderGate!
        let reread = t.reread()
        await settle { self.asked(chat).count == 3 } // held in the page-back
        chat.historyFailure = LoginError.Network(message: "offline") // the held call fails on release
        gate.open()
        await reread.value
        XCTAssertEqual(ids(t), ["a05", "a06"], "a half-fetched re-read was merged: a hole in the middle")
        XCTAssertEqual(t.error, "Couldn't load messages.")
    }

    // 9
    func testARereadWhileActiveMarksTheNewestRead() async {
        let chat = FakeChat()
        chat.pages = [page("m1"), page("m1", "m2")]
        let t = timeline(chat)
        await t.load()
        await t.reread().value
        XCTAssertEqual(chat.read.last, "m2")
        XCTAssertFalse(t.readOwed)
    }

    // 10
    func testARereadWhileInactiveLeavesTheReadOwed() async {
        let chat = FakeChat()
        chat.pages = [page("m1"), page("m1", "m2")]
        let active = Flag()
        let t = TimelineModel(channelId: "c", client: chat, isActive: { active.on }, rereadOnReady: true)
        await t.load() // marks m1 read: the app is active for the load
        let readBefore = chat.read.count
        active.on = false
        await t.reread().value
        XCTAssertEqual(chat.read.count, readBefore, "read while the app was not active")
        XCTAssertTrue(t.readOwed)
        active.on = true
        t.appBecameActive()
        await settle { chat.read.last == "m2" }
        XCTAssertEqual(chat.read.last, "m2")
    }

    private final class Flag: @unchecked Sendable { var on = true }

    /// The 👍 count on the first message (nil: none), and a live reaction event for it.
    private func reactionCount(_ t: TimelineModel) -> Int64? {
        t.messages.first?.reactions.first { $0.emoji == "👍" }?.count
    }

    private func react(_ t: TimelineModel, count: Int64, seq: Int64) {
        t.apply(.reactionUpdate(channelId: "c", messageId: "m1", emoji: "👍", userId: "bob",
                                added: true, count: count, seq: seq))
    }

    // 11
    func testAReadyClearsTheReactionMarks() async {
        let chat = FakeChat()
        chat.pages = [page("m1"), page("m1")]
        let t = timeline(chat)
        await t.load()
        react(t, count: 1, seq: 10)
        XCTAssertEqual(reactionCount(t), 1)
        t.apply(.ready)
        await settle { self.idle(chat, t, calls: 2) }
        react(t, count: 2, seq: 5) // a lower seq: only heard if the marks were forgotten
        XCTAssertEqual(reactionCount(t), 2, "the reconnect kept the old order marks")
    }

    // 12
    func testAForegroundRereadKeepsTheReactionMarks() async {
        let chat = FakeChat()
        chat.pages = [page("m1"), page("m1")]
        let t = timeline(chat)
        await t.load()
        react(t, count: 1, seq: 10)
        await t.reread().value
        react(t, count: 2, seq: 5)
        XCTAssertNil(reactionCount(t), "a foreground re-read forgot the order marks")
    }

    // 13
    func testAReplaceKeepsMessagesThatArrivedDuringThePageBack() async {
        let chat = FakeChat()
        chat.pages = [page("a01", "a02"), page("a20"), page("a18"), page("a16"), page("a14")]
        let t = timeline(chat)
        await t.load()
        chat.olderGate = Gate()
        let gate = chat.olderGate!
        t.apply(.ready)
        await settle { self.asked(chat).count == 3 } // the first page-back is held
        t.apply(.messageNew(message: msg("a25", "live")))
        gate.open()
        await settle { t.replaced == 1 }
        await settle { self.idle(chat, t, calls: 5) }
        XCTAssertEqual(ids(t), ["a14", "a16", "a18", "a20", "a25"])
    }

    // 14
    func testTheGapAnchorIsTakenWhenTheReadyArrives() async {
        let chat = FakeChat()
        chat.pages = [page("a05", "a06"), page("a10", "a11", "a12"), page("a08", "a09"), page("a06", "a07")]
        let t = timeline(chat)
        await t.load()
        t.apply(.ready)
        t.apply(.messageNew(message: msg("a12", "live"))) // before the drain runs
        await settle { self.idle(chat, t, calls: 4) }
        XCTAssertTrue(asked(chat).contains("a10"), "the live message hid the gap: no page-back")
        XCTAssertEqual(ids(t), ["a05", "a06", "a07", "a08", "a09", "a10", "a11", "a12"])
    }

    // 15
    func testAnOlderPageWaitingOnAReplacingRereadAsksFromTheNewOldest() async {
        let chat = FakeChat()
        chat.local = true
        chat.loadFails = true
        // The cache has nothing it can vouch for: every read says "needs the network".
        chat.cachePages = [cachedPage([], needsNetwork: true)]
        chat.pages = [page("a01", "a02"), page("a20"), page("a18"), page("a16"), page("a14"), page("a13")]
        let t = timeline(chat)
        await t.load()
        chat.cacheGate = Gate()
        let gate = chat.cacheGate!
        let older = Task { await t.loadOlder() }
        // Held inside its cache read, after it noted a01 as its oldest.
        await settle { chat.cacheCalls.withLock { $0.contains("cached:a01") } }
        t.apply(.ready)
        await settle { t.replaced == 1 }
        gate.open()
        await older.value
        XCTAssertFalse(asked(chat).contains("a01"), "loadOlder asked for a page before replaced history")
        XCTAssertEqual(asked(chat).last, "a14")
    }

    // 17
    func testALiveMessageWithALowerIdThanTheFetchedNewestSurvivesAReplace() async {
        let chat = FakeChat()
        chat.pages = [page("a01", "a02"), page("a20"), page("a18"), page("a16"), page("a14")]
        let t = timeline(chat)
        await t.load()
        chat.olderGate = Gate()
        let gate = chat.olderGate!
        t.apply(.ready)
        await settle { self.asked(chat).count == 3 } // the first page-back is held
        // Ids are made when a send starts, so a message committed now can sort below a20.
        t.apply(.messageNew(message: msg("a10", "late")))
        gate.open()
        await settle { !t.isRereading }
        XCTAssertEqual(ids(t), ["a10", "a14", "a16", "a18", "a20"], "a live message below the fetched newest was dropped")
    }

    // 18
    func testALiveEditToAFetchedMessageDuringThePageBackSurvives() async {
        let chat = FakeChat()
        chat.pages = [page("a01", "a02"), page("a20"), page("a18"), page("a16"), page("a14")]
        let t = timeline(chat)
        await t.load()
        chat.olderGate = Gate()
        let gate = chat.olderGate!
        t.apply(.ready)
        await settle { self.asked(chat).count == 3 }
        var edited = msg("a18", "edited")
        edited.editedAt = "2026-09-26T11:00:00Z"
        t.apply(.messageUpdate(message: edited)) // a18 is in the page the page-back brings
        gate.open()
        await settle { !t.isRereading }
        XCTAssertEqual(t.messages.first { $0.id == "a18" }?.body, "edited", "the fetched copy undid a live edit")
    }

    // 19
    func testARereadAskedDuringAReplacingTurnAsksOnlyTheHeadPage() async {
        let chat = FakeChat()
        // The load; turn A (head and three page-backs, so it replaces); turn B's head page.
        chat.pages = [page("a01", "a02"), page("a20"), page("a18"), page("a16"), page("a14"), page("a20", "a21")]
        let t = timeline(chat)
        await t.load()
        chat.olderGate = Gate()
        let gate = chat.olderGate!
        t.apply(.ready)
        await settle { self.asked(chat).count == 3 }
        t.apply(.ready) // noted from the old, gappy a02, while turn A runs
        await letQueuedJobsRun()
        gate.open()
        await settle { !t.isRereading }
        XCTAssertEqual(asked(chat), [nil, nil, "a20", "a18", "a16", nil], "turn B paged back over what turn A proved")
        XCTAssertEqual(t.replaced, 1)
        XCTAssertEqual(ids(t), ["a14", "a16", "a18", "a20", "a21"])
    }

    // 20
    func testAFailedRereadKeepsItsAnchorForTheNextOne() async {
        let chat = FakeChat()
        chat.pages = [page("a05", "a06"), page("a10", "a11")]
        let t = timeline(chat)
        await t.load()
        chat.olderGate = Gate()
        let gate = chat.olderGate!
        let failing = t.reread()
        await settle { self.asked(chat).count == 3 } // held in the page-back
        t.apply(.messageNew(message: msg("a40", "live"))) // now the newest shown message is a40
        chat.historyFailure = LoginError.Network(message: "offline")
        gate.open()
        await failing.value
        XCTAssertNotNil(t.error)
        // The next re-read must still reach back to a06, not to a40: a gap is still open.
        chat.historyFailure = nil
        chat.pages = [page("a30"), page("a06", "a07")]
        await t.reread().value
        XCTAssertEqual(Array(asked(chat).suffix(2)), [nil, "a30"], "the anchor was lost with the failed try")
        XCTAssertEqual(ids(t), ["a05", "a06", "a07", "a30", "a40"])
    }

    // 16
    func testAResyncTakesTheGapAnchorWhenItArrives() async {
        let chat = FakeChat()
        chat.pages = [page("a05", "a06"), page("a10", "a11", "a12"), page("a08", "a09"), page("a06", "a07")]
        let t = timeline(chat)
        await t.load()
        t.apply(.resync)
        t.apply(.messageNew(message: msg("a12", "live"))) // the binding's kept events, before the fetch
        await settle { self.idle(chat, t, calls: 4) }
        XCTAssertTrue(asked(chat).contains("a10"), "the live message hid the gap: no page-back")
        XCTAssertEqual(ids(t), ["a05", "a06", "a07", "a08", "a09", "a10", "a11", "a12"])
    }
}
