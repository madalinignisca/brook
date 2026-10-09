// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import BrookCore
import Foundation
import Synchronization
import XCTest

@testable import Brook

final class FakeChat: ChatClient, @unchecked Sendable {
    var pages: [[FfiMessage]] = []

    // ---- This device's local data (OfflineFakes.swift). Off: every call answers
    // `local.unavailable`, as without the Keychain (#79).
    var local = false
    /// `loadHead`/`loadOlder` fail (offline).
    var loadFails = false
    /// Cached pages, handed out in order (the last one repeats).
    var cachePages: [FfiCachedMessages] = []
    /// Holds the next `cachedMessages` call (after it is recorded), once.
    var cacheGate: Gate?
    /// What the cache was asked, in order ("cached:<before>", "loadHead", "loadOlder", …).
    let cacheCalls = Mutex<[String]>([])
    var users: [FfiMember] = []
    /// Pending reads, handed out in order (the last one repeats).
    var pendingReads: [[FfiPendingMessage]] = []
    var queueFailure: Error?
    /// Messages with files: "clientId|body|name:type:transferId,…"; held while `filesGate` is set.
    let queuedFiles = Mutex<[String]>([])
    var filesGate: Gate?
    let cancelled = Mutex<[UInt64]>([])
    // The file cache (FileRowModel tests).
    var openResult: Result<String, Error> = .success("/private/brook/open/abc/report.pdf")
    /// `fileState` answers, handed out in order (the last one repeats); a gate holds one.
    var states: [FfiFileCacheState] = [.notCached]
    var stateGate: Gate?
    var pinFailure: Error?
    var pinGate: Gate?
    var previewGate: Gate?
    let pins = Mutex<[String]>([])
    var previewResult: Result<FfiImagePreview, Error> = .failure(LoginError.Api(code: "file.preview_refused", message: ""))
    let queued = Mutex<[String]>([]) // "clientId|body|reply"
    var unsent: UInt64 = 0
    var lost: UInt64?
    let acknowledged = Mutex<[UInt64]>([])
    var others: [FfiLocalUser] = []
    /// Holds `otherLocalUsers` after it has counted the call.
    var othersGate: Gate?
    let othersAsked = Mutex(0)
    let wiped = Mutex(0)
    let wipeFails = Mutex(false)
    let wipeTried = Mutex(0)
    let sent = Mutex<[String]>([])
    /// The `clientId` of each direct send, in order.
    let sentIds = Mutex<[String?]>([])
    var sendFailure: Error?
    var read: [String?] = []

    var historyFailure: Error?
    /// `channelHistory` calls so far, and a gate that holds each one after it is counted.
    let historyCalls = Mutex(0)
    var historyGate: Gate?
    func channelHistory(channelId: String, before: String?) async throws -> [FfiMessage] {
        historyCalls.withLock { $0 += 1 }
        await historyGate?.wait()
        if let historyFailure { throw historyFailure }
        return pages.isEmpty ? [] : pages.removeFirst()
    }
    func sendMessage(channelId: String, body: String, replyToId: String?,
                     clientId: String?) async throws -> FfiMessage {
        sentIds.withLock { $0.append(clientId) }
        sent.withLock { $0.append("\(body)|\(replyToId ?? "-")") }
        if let sendFailure { throw sendFailure }
        return msg("m9", body, channel: channelId)
    }
    func editMessage(channelId: String, messageId: String, body: String) async throws -> FfiMessage {
        sent.withLock { $0.append("edit:\(messageId):\(body)") }
        if let sendFailure { throw sendFailure }
        return msg(messageId, body, channel: channelId)
    }
    func deleteMessage(channelId: String, messageId: String) async throws {}
    func markRead(channelId: String, messageId: String?) async throws { read.append(messageId) }
    let typed = Mutex<[String]>([])
    func sendTyping(channelId: String) async throws { typed.withLock { $0.append(channelId) } }
    var reactionAnswer: [FfiReaction] = []
    var reactionFails = false
    var reactionGate: Gate?
    let toggles = Mutex<[String]>([])
    func toggleReaction(channelId: String, messageId: String, emoji: String) async throws -> [FfiReaction] {
        toggles.withLock { $0.append("\(messageId)|\(emoji)") }
        await reactionGate?.wait()
        if reactionFails { throw LoginError.Timeout }
        return reactionAnswer
    }
    var downloadFailure: Error?
    var downloadBytes = Data("new".utf8)
    func downloadFile(transferId: UInt64, fileId: String, sha256: String, size: UInt64,
                      destination: String) async throws {
        // As core: a failed download leaves nothing at its destination.
        if let downloadFailure { throw downloadFailure }
        try downloadBytes.write(to: URL(fileURLWithPath: destination))
    }
    func cancelTransfer(transferId: UInt64) { cancelled.withLock { $0.append(transferId) } }
    func subscribeTransfers(listener: TransferListener) -> Subscription {
        fatalError("not used by these tests")
    }
}

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
