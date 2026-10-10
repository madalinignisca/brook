// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import BrookCore
import Foundation
import Synchronization
import XCTest

@testable import Brook

@MainActor
final class TimelineModelTests: XCTestCase {
    /// A history page and live events merge by id, in id (time) order, with no duplicates.
    func testHistoryAndLiveEventsMergeByIdInOrder() async {
        let chat = FakeChat()
        chat.pages = [[msg("m1", "a"), msg("m3", "c")]]
        let t = TimelineModel(channelId: "c", client: chat)
        t.apply(.messageNew(message: msg("m2", "b")))  // before the page lands
        await t.load()
        t.apply(.messageNew(message: msg("m3", "c")))  // a duplicate of the page's
        XCTAssertEqual(t.messages.map(\.id), ["m1", "m2", "m3"])
        XCTAssertEqual(chat.read.last, "m3", "the newest shown wasn't marked read")
    }

    func testAnotherChannelsEventsAreIgnored() {
        let t = TimelineModel(channelId: "c", client: FakeChat())
        t.apply(.messageNew(message: msg("m1", "x", channel: "other")))
        t.apply(.messageDelete(channelId: "other", messageId: "m1"))
        XCTAssertTrue(t.messages.isEmpty)
    }

    /// An edit replaces in place; a delete leaves a tombstone that a late copy can't undo.
    func testEditsReplaceAndDeletesStay() {
        let t = TimelineModel(channelId: "c", client: FakeChat())
        t.merge([msg("m1", "first")])
        var edit = msg("m1", "edited")
        edit.editedAt = "2026-09-26T10:01:00Z" // a server edit always carries its time
        t.apply(.messageUpdate(message: edit))
        XCTAssertEqual(t.messages.first?.body, "edited")
        t.apply(.messageDelete(channelId: "c", messageId: "m1"))
        XCTAssertEqual(t.messages.first?.deleted, true)
        XCTAssertEqual(t.messages.first?.body, "")
        t.merge([msg("m1", "edited")])  // a history page fetched before the delete
        XCTAssertEqual(t.messages.first?.deleted, true, "a late copy brought it back")
    }

    /// A new conversation: its first messages land while the newest page is still loading, so the
    /// older-page loader (shown above the first message) asks while a load is running. That ask
    /// must wait for the running load, not give up: nothing asks again, and the spinner stayed
    /// for good instead of becoming "the start of the conversation".
    func testAnOlderPageAskedWhileTheHeadLoadsWaitsAndEndsAtTheStart() async {
        let chat = FakeChat()
        chat.pages = [[msg("m5", "hey")], []]
        let gate = Gate()
        chat.historyGate = gate
        let t = TimelineModel(channelId: "c", client: chat)
        let loading = Task { await t.load() }
        for _ in 0 ..< 40 { await Task.yield(); try? await Task.sleep(for: .milliseconds(5)) }
        t.merge([msg("m4", "first")]) // a live message: the loader appears above it
        XCTAssertTrue(t.loading, "the head is still loading")
        let older = Task { await t.loadOlder() }
        for _ in 0 ..< 10 { await Task.yield(); try? await Task.sleep(for: .milliseconds(5)) }
        gate.open()
        await loading.value
        await older.value
        XCTAssertTrue(t.atStart, "the loader would spin for good")
        XCTAssertEqual(chat.historyCalls.withLock { $0 }, 2, "the head, then one older page")
    }

    /// A failed older page is shown as a failure to retry, not a spinner nothing will end.
    func testAFailedOlderPageIsRetryable() async {
        let chat = FakeChat()
        chat.pages = [[msg("m5", "x")]]
        let t = TimelineModel(channelId: "c", client: chat)
        await t.load()
        chat.historyFailure = LoginError.Network(message: "offline")
        await t.loadOlder()
        XCTAssertTrue(t.olderFailed)
        XCTAssertFalse(t.atStart)
        chat.historyFailure = nil
        chat.pages = [[]]
        await t.loadOlder() // Retry
        XCTAssertFalse(t.olderFailed)
        XCTAssertTrue(t.atStart)
    }

    /// Retry swaps the button for a spinner whose `onAppear` asks again: with a failing network the
    /// two asks must be one request, not a chain of them.
    func testRetryAndTheSpinnerItShowsAskOnce() async {
        let chat = FakeChat()
        chat.pages = [[msg("m5", "x")]]
        let t = TimelineModel(channelId: "c", client: chat)
        await t.load()
        chat.historyFailure = LoginError.Network(message: "offline")
        await t.loadOlder() // fails: the Retry button
        XCTAssertTrue(t.olderFailed)
        let before = chat.historyCalls.withLock { $0 }
        let gate = Gate()
        chat.historyGate = gate
        let retry = Task { await t.loadOlder() } // the click
        for _ in 0 ..< 20 { await Task.yield(); try? await Task.sleep(for: .milliseconds(5)) }
        let spinnerAppears = Task { await t.loadOlder() } // the spinner's onAppear
        for _ in 0 ..< 20 { await Task.yield(); try? await Task.sleep(for: .milliseconds(5)) }
        gate.open()
        await retry.value
        await spinnerAppears.value
        XCTAssertEqual(chat.historyCalls.withLock { $0 } - before, 1, "one click, one request")
        XCTAssertTrue(t.olderFailed)
    }

    /// No older page behind a newest page that failed to load: it would end at an empty "start of the
    /// conversation" over history that never arrived.
    func testNoOlderPageIsAskedBehindAHeadThatFailed() async {
        let chat = FakeChat()
        chat.historyFailure = LoginError.Network(message: "offline")
        let gate = Gate()
        chat.historyGate = gate
        let t = TimelineModel(channelId: "c", client: chat)
        let loading = Task { await t.load() }
        for _ in 0 ..< 40 { await Task.yield(); try? await Task.sleep(for: .milliseconds(5)) }
        t.merge([msg("m4", "live")])
        let older = Task { await t.loadOlder() }
        for _ in 0 ..< 10 { await Task.yield(); try? await Task.sleep(for: .milliseconds(5)) }
        gate.open()
        await loading.value
        await older.value
        XCTAssertTrue(t.headFailed)
        XCTAssertFalse(t.atStart)
        XCTAssertEqual(chat.historyCalls.withLock { $0 }, 1, "only the failed head was asked for")
    }

    /// A caller cancelled while it waited must not start a request afterwards.
    func testACancelledOlderAskStartsNothing() async {
        let chat = FakeChat()
        chat.pages = [[msg("m5", "x")]]
        let t = TimelineModel(channelId: "c", client: chat)
        await t.load()
        let before = chat.historyCalls.withLock { $0 }
        let ask = Task {
            try? await Task.sleep(for: .milliseconds(200))
            await t.loadOlder()
        }
        ask.cancel()
        await ask.value
        XCTAssertEqual(chat.historyCalls.withLock { $0 }, before)
    }

    /// An empty older page means the start of the channel; no more pages are asked for.
    func testAnEmptyOlderPageIsTheStart() async {
        let chat = FakeChat()
        chat.pages = [[msg("m5", "x")], []]
        let t = TimelineModel(channelId: "c", client: chat)
        await t.load()
        XCTAssertFalse(t.atStart)
        await t.loadOlder()
        XCTAssertTrue(t.atStart)
    }
}

@MainActor
final class ComposerModelTests: XCTestCase {
    /// Sent: the box clears and the message reaches the timeline at once.
    func testASendClearsAndHandsTheMessageOver() async {
        let chat = FakeChat()
        var got: [String] = []
        let c = ComposerModel(channelId: "c", client: chat, onMessage: { got.append($0.id) })
        c.reply(to: msg("q", "quoted"))
        c.text = "hello"
        await c.send()
        XCTAssertEqual(c.text, "")
        XCTAssertNil(c.replyingTo)
        XCTAssertEqual(got, ["m9"])
        XCTAssertEqual(chat.sent.withLock { $0 }, ["hello|q"])
    }

    /// A refusal gives the text and the reply back, and says why.
    func testAFailedSendGivesTheTextBack() async {
        let chat = FakeChat()
        chat.sendFailure = LoginError.Api(code: "message.reply_target_gone", message: "")
        let c = ComposerModel(channelId: "c", client: chat, onMessage: { _ in })
        c.reply(to: msg("q", "quoted"))
        c.text = "hello"
        await c.send()
        XCTAssertEqual(c.text, "hello")
        XCTAssertEqual(c.replyingTo?.id, "q")
        XCTAssertEqual(c.error, "The message you replied to was deleted.")
    }

    /// Send stays off for a box with only spaces and new lines (Return adds a line on iOS, so a
    /// stray one is easy to leave), and turns on with a word.
    func testOnlySpacesCannotBeSent() {
        let c = ComposerModel(channelId: "c", client: FakeChat(), onMessage: { _ in })
        c.text = "  \n "
        XCTAssertFalse(c.canSend)
        c.text = "hi"
        XCTAssertTrue(c.canSend)
    }

    /// A send refused because the session ended gives the text back and says so, not "Couldn't send.".
    func testASignedOutSendSaysSo() async {
        let chat = FakeChat()
        chat.sendFailure = LoginError.NotAuthenticated
        let c = ComposerModel(channelId: "c", client: chat, onMessage: { _ in })
        c.text = "hello"
        await c.send()
        XCTAssertEqual(c.text, "hello")
        XCTAssertEqual(c.error, "You were signed out.")
    }

    /// A network failure may have delivered it: the text comes back, but the words don't
    /// claim it wasn't sent.
    func testANetworkFailureDoesntClaimItWasntSent() async {
        let chat = FakeChat()
        chat.sendFailure = LoginError.Network(message: "offline")
        let c = ComposerModel(channelId: "c", client: chat, onMessage: { _ in })
        c.text = "hello"
        await c.send()
        XCTAssertEqual(c.text, "hello")
        XCTAssertTrue(c.error?.contains("may not have been sent") == true, c.error ?? "")
    }

    /// The direct send (taken when local data is off, as on iOS) carries a lowercase UUID.
    func testADirectSendPassesAClientId() async throws {
        let chat = FakeChat()
        let c = ComposerModel(channelId: "c", client: chat, onMessage: { _ in })
        c.text = "hello"
        await c.send()
        let ids = chat.sentIds.withLock { $0 }
        XCTAssertEqual(ids.count, 1)
        let id = try XCTUnwrap(ids[0])
        XCTAssertNotNil(UUID(uuidString: id))
        XCTAssertEqual(id, id.lowercased())
    }

    /// After a network failure the message may be on the server; sending the same text again
    /// must carry the same id so the server keeps one copy.
    func testSendingTheSameTextAgainAfterAFailureReusesItsClientId() async {
        let chat = FakeChat()
        chat.sendFailure = LoginError.Network(message: "offline")
        let c = ComposerModel(channelId: "c", client: chat, onMessage: { _ in })
        c.text = "hello"
        await c.send()
        XCTAssertEqual(c.text, "hello")
        chat.sendFailure = nil
        await c.send()
        let ids = chat.sentIds.withLock { $0 }
        XCTAssertEqual(ids.count, 2)
        XCTAssertNotNil(ids[0])
        XCTAssertEqual(ids[0], ids[1])
    }

    /// Failing, then typing something else and putting the same words back, is a new message:
    /// only an unchanged box may reuse the failed id.
    func testChangingThenRestoringTheTextGetsANewClientId() async {
        let chat = FakeChat()
        chat.sendFailure = LoginError.Network(message: "offline")
        let c = ComposerModel(channelId: "c", client: chat, onMessage: { _ in })
        c.text = "hello"
        await c.send()
        c.text = "hello!"
        c.text = "hello"
        await c.send()
        let ids = chat.sentIds.withLock { $0 }
        XCTAssertEqual(ids.count, 2)
        XCTAssertNotEqual(ids[0], ids[1])
    }

    /// Quoting another message and going back to the first, or dropping the quote and quoting
    /// it again, is a changed message: it gets a new id.
    func testChangingTheQuoteGetsANewClientId() async {
        let chat = FakeChat()
        chat.sendFailure = LoginError.Network(message: "offline")
        let c = ComposerModel(channelId: "c", client: chat, onMessage: { _ in })
        let q = msg("q", "quoted")
        c.reply(to: q)
        c.text = "hello"
        await c.send() // fails: the box comes back with the quote
        c.reply(to: msg("r", "other"))
        c.reply(to: q)
        await c.send()
        c.cancel() // drop the quote ...
        c.text = "hello"
        c.reply(to: q) // ... and quote it again
        await c.send()
        let ids = chat.sentIds.withLock { $0 }
        XCTAssertEqual(ids.count, 3)
        XCTAssertNotEqual(ids[0], ids[1])
        XCTAssertNotEqual(ids[1], ids[2])
    }

    /// "hello" fails but arrives and is then deleted; sending the unchanged box must not reuse
    /// its id, or the server would answer with the tombstone and nothing would be posted.
    func testDeletingAMessageDropsTheDraft() async {
        let chat = FakeChat()
        chat.sendFailure = LoginError.Network(message: "offline")
        let c = ComposerModel(channelId: "c", client: chat, onMessage: { _ in })
        c.text = "hello"
        await c.send()
        chat.sendFailure = nil
        await c.delete(msg("m1", "hello"))
        await c.send()
        let ids = chat.sentIds.withLock { $0 }
        XCTAssertEqual(ids.count, 2)
        XCTAssertNotEqual(ids[0], ids[1])
    }

    /// Clearing the restored box and typing the same words again is a new message.
    func testClearingTheBoxGetsANewClientId() async {
        let chat = FakeChat()
        chat.sendFailure = LoginError.Network(message: "offline")
        let c = ComposerModel(channelId: "c", client: chat, onMessage: { _ in })
        c.text = "hello"
        await c.send()
        c.text = ""
        c.text = "hello"
        await c.send()
        let ids = chat.sentIds.withLock { $0 }
        XCTAssertEqual(ids.count, 2)
        XCTAssertNotEqual(ids[0], ids[1])
    }

    /// "hello" fails but arrives; the user edits that message to "goodbye", then sends "hello"
    /// again. The new "hello" must not carry the delivered one's id, or it would be lost.
    func testStartingAnEditDropsTheDraft() async {
        let chat = FakeChat()
        chat.sendFailure = LoginError.Network(message: "offline")
        let c = ComposerModel(channelId: "c", client: chat, onMessage: { _ in })
        c.text = "hello"
        await c.send()
        chat.sendFailure = nil
        c.edit(msg("m1", "hello"))
        c.text = "goodbye"
        await c.send()
        c.text = "hello"
        await c.send()
        let ids = chat.sentIds.withLock { $0 }
        XCTAssertEqual(ids.count, 2) // the edit sends no id
        XCTAssertNotEqual(ids[0], ids[1])
    }

    /// The retry's answer is a tombstone: the message was deleted after it arrived. It is not
    /// shown as a live message, and the draft is gone.
    func testARetryAnsweredWithADeletedMessageIsNotShown() async {
        let chat = FakeChat()
        chat.sendFailure = LoginError.Network(message: "offline")
        var got: [String] = []
        let c = ComposerModel(channelId: "c", client: chat, onMessage: { got.append($0.id) })
        c.text = "hello"
        await c.send()
        chat.sendFailure = nil
        chat.sendAnswersDeleted = true
        await c.send()
        XCTAssertEqual(got, [])
        XCTAssertEqual(c.text, "")
        XCTAssertNil(c.error)
        chat.sendAnswersDeleted = false
        c.text = "hello"
        await c.send()
        let ids = chat.sentIds.withLock { $0 }
        XCTAssertEqual(ids.count, 3)
        XCTAssertNotEqual(ids[1], ids[2])
    }

    /// Different text is a different message: it must not borrow the failed one's id.
    func testChangedTextGetsANewClientId() async {
        let chat = FakeChat()
        chat.sendFailure = LoginError.Network(message: "offline")
        let c = ComposerModel(channelId: "c", client: chat, onMessage: { _ in })
        c.text = "hello"
        await c.send()
        c.text = "hello!"
        await c.send()
        let ids = chat.sentIds.withLock { $0 }
        XCTAssertEqual(ids.count, 2)
        XCTAssertNotEqual(ids[0], ids[1])
    }

    /// After a success the draft is gone: the same text again is a new message. Reusing the id
    /// would make the server return the first "hi" and lose the second.
    func testTheNextMessageAfterASuccessGetsANewClientId() async {
        let chat = FakeChat()
        let c = ComposerModel(channelId: "c", client: chat, onMessage: { _ in })
        c.text = "hi"
        await c.send()
        c.text = "hi"
        await c.send()
        let ids = chat.sentIds.withLock { $0 }
        XCTAssertEqual(ids.count, 2)
        XCTAssertNotNil(ids[0])
        XCTAssertNotEqual(ids[0], ids[1])
    }

    func testAnEditSavesTheNewText() async {
        let chat = FakeChat()
        let c = ComposerModel(channelId: "c", client: chat, onMessage: { _ in })
        c.edit(msg("m1", "old"))
        XCTAssertEqual(c.text, "old")
        c.text = "new"
        await c.send()
        XCTAssertEqual(chat.sent.withLock { $0 }, ["edit:m1:new"])
    }

    func testAnEditMayClearAFileMessagesCaptionButNotATextOnly() async {
        let chat = FakeChat()
        let c = ComposerModel(channelId: "c", client: chat, onMessage: { _ in })
        c.edit(msg("m1", "just text"))
        c.text = "  "
        XCTAssertFalse(c.canSend)
        var withFile = msg("m2", "caption")
        withFile.attachments = [FfiFileInfo(id: "f", filename: "a.png", originalName: "a.png",
                                            size: 1, contentType: "image/png", sha256: "ab")]
        c.edit(withFile)
        c.text = " \n"
        XCTAssertTrue(c.canSend)
        await c.send()
        XCTAssertEqual(chat.sent.withLock { $0 }, ["edit:m2:"])
    }
}

@MainActor
final class SaveModelTests: XCTestCase {
    private func file() -> FfiFileInfo {
        FfiFileInfo(id: "f", filename: "a.txt", originalName: "a.txt", size: 3,
                    contentType: "text/plain", sha256: "ab")
    }

    /// Saving over an existing file ("Replace") that then fails leaves the old file as it was.
    func testAFailedSaveKeepsTheFileItWouldHaveReplaced() async throws {
        let dir = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let dest = dir.appending(path: "a.txt")
        try Data("old".utf8).write(to: dest)
        let chat = FakeChat()
        chat.downloadFailure = LoginError.Api(code: "file.gone", message: "")
        let saves = SaveModel(client: chat)
        await saves.save(file(), to: dest)
        XCTAssertEqual(try Data(contentsOf: dest), Data("old".utf8), "the old copy was lost")
        XCTAssertEqual(saves.states["f"], .failed("No longer available."))
    }

    func testASaveReplacesTheFile() async throws {
        let dir = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let dest = dir.appending(path: "a.txt")
        try Data("old".utf8).write(to: dest)
        let saves = SaveModel(client: FakeChat())
        await saves.save(file(), to: dest)
        XCTAssertEqual(try Data(contentsOf: dest), Data("new".utf8))
        XCTAssertEqual(saves.states["f"], .saved)
    }
}
