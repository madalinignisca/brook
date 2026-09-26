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
    let queued = Mutex<[String]>([]) // "clientId|body|reply"
    var unsent: UInt64 = 0
    var lost: UInt64?
    let acknowledged = Mutex<[UInt64]>([])
    var others: [FfiLocalUser] = []
    let wiped = Mutex(0)
    let sent = Mutex<[String]>([])
    var sendFailure: Error?
    var read: [String?] = []

    var historyFailure: Error?
    func channelHistory(channelId: String, before: String?) async throws -> [FfiMessage] {
        if let historyFailure { throw historyFailure }
        return pages.isEmpty ? [] : pages.removeFirst()
    }
    func sendMessage(channelId: String, body: String, replyToId: String?) async throws -> FfiMessage {
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

func msg(_ id: String, _ body: String, channel: String = "c", deleted: Bool = false) -> FfiMessage {
    FfiMessage(id: id, channelId: channel, authorId: "u", authorHandle: "u", authorDisplayName: "U",
               body: body, createdAt: "2026-09-26T10:00:00Z", clientId: nil, deleted: deleted,
               editedAt: nil, replyToId: nil, replyTo: nil, attachments: [])
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
