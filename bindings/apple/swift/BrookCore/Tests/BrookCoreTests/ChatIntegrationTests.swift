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
        try await client.deleteMessage(channelId: channel, messageId: first.id)
        try await client.deleteMessage(channelId: channel, messageId: message.id)
        try await client.signOutAndForget()
    }
}
