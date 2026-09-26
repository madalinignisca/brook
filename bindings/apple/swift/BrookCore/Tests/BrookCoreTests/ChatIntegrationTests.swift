import Foundation
import Synchronization
import XCTest

@testable import BrookCore

/// A key slot in memory (tests only: the app's is the Keychain).
final class MemorySlot: FfiKeySlot, @unchecked Sendable {
    private let slots = Mutex<[String: Data]>([:])
    func load(slot: String) throws -> Data? { slots.withLock { $0[slot] } }
    func create(slot: String, bytes: Data) throws {
        try slots.withLock {
            if $0[slot] != nil { throw FfiKeySlotError.Exists }
            $0[slot] = bytes
        }
    }
    func replace(slot: String, bytes: Data) throws { slots.withLock { $0[slot] = bytes } }
    func delete(slot: String) throws { _ = slots.withLock { $0.removeValue(forKey: slot) } }
}

/// The chat surface against the shared test server (configured by `itest.sh`): send, reply,
/// edit and delete read back through history, and a file sent through the outbox saves back
/// byte for byte.
final class ChatIntegrationTests: XCTestCase {
    func testChatRoundTripAgainstTheTestServer() async throws {
        let env = ProcessInfo.processInfo.environment
        guard let server = env["BROOK_TEST_SERVER"], let handle = env["BROOK_TEST_HANDLE"],
              let password = env["BROOK_TEST_PASSWORD"], let channel = env["BROOK_TEST_CHANNEL"]
        else {
            if env["BROOK_REQUIRE_ITEST"] == "1" { XCTFail("test server not configured") }
            throw XCTSkip("test server not configured")
        }
        let client = try FfiBrookClient(
            baseUrl: server, allowInsecureHttp: env["BROOK_TEST_ALLOW_INSECURE_HTTP"] == "1")
        guard case .loggedIn = try await client.login(handle: handle, password: password) else {
            return XCTFail("not signed in")
        }
        let tag = UUID().uuidString.prefix(8)

        // Send, reply, edit, delete: each read back through history.
        let first = try await client.sendMessage(channelId: channel, body: "itest \(tag)",
                                                 replyToId: nil)
        let reply = try await client.sendMessage(channelId: channel, body: "reply \(tag)",
                                                 replyToId: first.id)
        _ = try await client.editMessage(channelId: channel, messageId: first.id,
                                         body: "itest \(tag) edited")
        var page = try await client.channelHistory(channelId: channel, before: nil)
        let edited = try XCTUnwrap(page.first { $0.id == first.id })
        XCTAssertEqual(edited.body, "itest \(tag) edited")
        XCTAssertNotNil(edited.editedAt)
        let quoted = try XCTUnwrap(page.first { $0.id == reply.id })
        XCTAssertEqual(quoted.replyTo?.id, first.id)
        try await client.deleteMessage(channelId: channel, messageId: reply.id)
        page = try await client.channelHistory(channelId: channel, before: nil)
        XCTAssertFalse(page.contains { $0.id == reply.id && !$0.deleted },
                       "the deleted reply still reads as a message")
        try await client.markRead(channelId: channel, messageId: first.id)

        // A file, captionless, through the outbox; saved back byte for byte.
        let dir = FileManager.default.temporaryDirectory.appending(path: "brook-itest-\(tag)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let on = await client.enableLocalData(slot: MemorySlot(), dataDir: dir.path)
        XCTAssertTrue(on, "local data didn't turn on")
        let bytes = Data((0 ..< 3000).map { UInt8($0 % 251) })
        let source = dir.appending(path: "source.bin")
        try bytes.write(to: source)
        let receipt = try await client.sendQueuedWithFiles(
            channelId: channel, body: "", replyToId: nil, clientId: UUID().uuidString,
            files: [FfiOutgoingFile(path: source.path, filename: "itest.bin",
                                    contentType: "application/octet-stream", transferId: nil)])
        XCTAssertEqual(receipt.files.count, 1)
        var sent: FfiMessage?
        for _ in 0 ..< 100 where sent == nil {
            try await Task.sleep(for: .milliseconds(200))
            sent = try await client.channelHistory(channelId: channel, before: nil)
                .first { $0.clientId == receipt.clientId }
        }
        let message = try XCTUnwrap(sent, "the queued file message never arrived")
        XCTAssertEqual(message.body, "")
        let file = try XCTUnwrap(message.attachments.first)
        XCTAssertEqual(file.size, UInt64(bytes.count))
        let saved = dir.appending(path: "saved.bin")
        try await client.downloadFile(transferId: 7, fileId: file.id,
                                      sha256: try XCTUnwrap(file.sha256), size: file.size,
                                      destination: saved.path)
        XCTAssertEqual(try Data(contentsOf: saved), bytes)

        // The same file through the encrypted cache: cached, saved offline-style, and Open
        // refuses it (`.bin` is Save only). A text file opens into a private copy. History
        // lands in the cache only once the sync has brought the channel in.
        var synced = false
        for _ in 0 ..< 100 where !synced {
            synced = try await client.cachedChannels().contains { $0.id == channel }
            if !synced { try await Task.sleep(for: .milliseconds(200)) }
        }
        XCTAssertTrue(synced, "the channel never reached the cache")
        try await client.loadHead(channelId: channel, limit: 50)
        try await client.cacheFile(transferId: 8, fileId: file.id)
        let state = try await client.fileState(fileId: file.id)
        XCTAssertEqual(state, .cached)
        let fromCache = dir.appending(path: "from-cache.bin")
        let savedFromCache = try await client.saveCachedFile(fileId: file.id, destination: fromCache.path)
        XCTAssertTrue(savedFromCache)
        XCTAssertEqual(try Data(contentsOf: fromCache), bytes)
        do {
            _ = try await client.openFile(transferId: 9, fileId: file.id)
            XCTFail("a .bin file opened")
        } catch LoginError.Api(let code, _) {
            XCTAssertEqual(code, "file.open_refused")
        }
        let text = Data("itest \(tag): plain text\n".utf8)
        let textSource = dir.appending(path: "notes.txt")
        try text.write(to: textSource)
        let textReceipt = try await client.sendQueuedWithFiles(
            channelId: channel, body: "", replyToId: nil, clientId: UUID().uuidString,
            files: [FfiOutgoingFile(path: textSource.path, filename: "notes.txt",
                                    contentType: "text/plain", transferId: nil)])
        var textMessage: FfiMessage?
        for _ in 0 ..< 100 where textMessage == nil {
            try await Task.sleep(for: .milliseconds(200))
            textMessage = try await client.channelHistory(channelId: channel, before: nil)
                .first { $0.clientId == textReceipt.clientId }
        }
        let textFile = try XCTUnwrap(textMessage?.attachments.first, "the text file never arrived")
        try await client.loadHead(channelId: channel, limit: 50)
        let opened = try await client.openFile(transferId: 10, fileId: textFile.id)
        XCTAssertEqual(URL(fileURLWithPath: opened).lastPathComponent, "notes.txt")
        XCTAssertEqual(try Data(contentsOf: URL(fileURLWithPath: opened)), text)
        await client.clearOpenCopies()
        XCTAssertFalse(FileManager.default.fileExists(atPath: opened), "the Open copy stayed")
        try await client.deleteMessage(channelId: channel, messageId: try XCTUnwrap(textMessage).id)
        try await client.deleteMessage(channelId: channel, messageId: first.id)
        try await client.deleteMessage(channelId: channel, messageId: message.id)
        try await client.signOutAndForget()
    }
}
