import BrookCore
import Foundation
import Synchronization
import XCTest

@testable import Brook

/// A file system for staging tests: each URL's facts, and every start and stop counted.
final class FakeFileAccess: FileAccess, @unchecked Sendable {
    let files: [String: (regular: Bool, size: UInt64, type: String?)]
    let refuseAccess: Set<String>
    let starts = Mutex<[String]>([])
    let stops = Mutex<[String]>([])
    init(_ files: [String: (regular: Bool, size: UInt64, type: String?)], refuseAccess: Set<String> = []) {
        self.files = files
        self.refuseAccess = refuseAccess
    }
    func startAccessing(_ url: URL) -> Bool {
        if refuseAccess.contains(url.lastPathComponent) { return false }
        starts.withLock { $0.append(url.lastPathComponent) }
        return true
    }
    func stopAccessing(_ url: URL) { stops.withLock { $0.append(url.lastPathComponent) } }
    func facts(_ url: URL) -> (regular: Bool, size: UInt64, type: String?)? { files[url.lastPathComponent] }
    var balanced: Bool { starts.withLock { $0.sorted() } == stops.withLock { $0.sorted() } }
}

func fileURL(_ name: String) -> URL { URL(fileURLWithPath: "/tmp/brook-test/\(name)") }

/// Staging and sending files (spec 2026-09-26-mac-send-files §1, §2, §4).
@MainActor
final class SendFilesTests: XCTestCase {
    private func composer(_ chat: FakeChat, _ access: FakeFileAccess) -> ComposerModel {
        let c = ComposerModel(channelId: "c", client: chat, onMessage: { _ in })
        c.fileAccess = access
        return c
    }

    private let ok: (regular: Bool, size: UInt64, type: String?) = (true, 1200, "image/png")

    func testStagingRefusesAsGtkAndStopsTheAccessItStarted() {
        let access = FakeFileAccess([
            "a.png": ok, "folder": (false, 0, nil), "empty.txt": (true, 0, "text/plain"),
            "huge.bin": (true, maxFileBytes() + 1, nil),
        ], refuseAccess: ["locked.txt"])
        let chat = FakeChat()
        chat.local = true
        let c = composer(chat, access)
        let cases: [(String, String)] = [
            ("folder", "Only files can be sent (not folders or devices)."),
            ("empty.txt", "empty.txt is empty."),
            ("huge.bin", "huge.bin is larger than 100 MiB."),
            ("locked.txt", "locked.txt couldn't be read."),
            ("gone.txt", "gone.txt couldn't be read."),
        ]
        for (name, text) in cases {
            c.attach([fileURL(name)])
            XCTAssertEqual(c.error, text, name)
        }
        XCTAssertTrue(c.staged.isEmpty)
        XCTAssertTrue(access.balanced, "a refused file kept its access")
        c.attach([fileURL("a.png")])
        c.attach([fileURL("a.png")]) // the same one again: ignored
        XCTAssertEqual(c.staged.map(\.name), ["a.png"])
        XCTAssertEqual(c.staged.first?.contentType, "image/png")
    }

    func testAtMostTenFilesAndRemovingOneEndsItsAccess() {
        var table: [String: (regular: Bool, size: UInt64, type: String?)] = [:]
        for i in 0 ..< 11 { table["f\(i)"] = ok }
        let access = FakeFileAccess(table)
        let chat = FakeChat()
        chat.local = true
        let c = composer(chat, access)
        c.attach((0 ..< 11).map { fileURL("f\($0)") })
        XCTAssertEqual(c.staged.count, 10)
        XCTAssertEqual(c.error, "A message can carry up to 10 files.")
        c.remove(c.staged[0])
        XCTAssertEqual(access.stops.withLock { $0 }, ["f0"])
    }

    func testFilesCrossWithTheirNamesTypesAndIdsAndSuccessClearsAndReleases() async {
        let access = FakeFileAccess(["a.png": ok, "b.bin": (true, 5, nil)])
        let chat = FakeChat()
        chat.local = true
        let c = composer(chat, access)
        c.attach([fileURL("a.png"), fileURL("b.bin")])
        let ids = c.staged.map(\.transferId)
        XCTAssertNotEqual(ids[0], ids[1])
        XCTAssertTrue(c.canSend, "files with no text can't be sent")
        await c.send()
        let row = chat.queuedFiles.withLock { $0 }.first!.split(separator: "|", omittingEmptySubsequences: false).map(String.init)
        XCTAssertEqual(row[1], "", "an empty body")
        XCTAssertEqual(row[2], "a.png:image/png:\(ids[0]),b.bin:application/octet-stream:\(ids[1])")
        XCTAssertTrue(c.staged.isEmpty)
        XCTAssertNil(c.error)
        XCTAssertTrue(access.balanced, "a sent file kept its access")
    }

    func testAnErrorKeepsEverythingWithGtksText() async {
        let codes: [(String, String)] = [
            ("outbox.too_many_files", "Too many files for one message."),
            ("outbox.file_too_large", "A file is too large to send."),
            ("outbox.empty_file", "An empty file can't be sent."),
            ("outbox.file_unreadable", "A file couldn't be read. Is it still there?"),
            ("outbox.store", "Couldn't prepare the files. Is the disk full?"),
            ("local.unavailable", "Sending files needs this Mac's storage, which isn't available yet."),
        ]
        for (code, text) in codes {
            let access = FakeFileAccess(["a.png": ok])
            let chat = FakeChat()
            chat.local = true
            chat.queueFailure = LoginError.Api(code: code, message: "")
            let c = composer(chat, access)
            c.attach([fileURL("a.png")])
            c.text = "look"
            await c.send()
            XCTAssertEqual(c.error, text, code)
            XCTAssertEqual(c.staged.count, 1, code)
            XCTAssertEqual(c.text, "look", code)
            XCTAssertTrue(access.stops.withLock { $0 }.isEmpty, "a kept file lost its access (\(code))")
        }
    }

    func testNoLocalDataClosesAttach() async {
        let access = FakeFileAccess(["a.png": ok])
        let chat = FakeChat()
        chat.local = true
        chat.queueFailure = localUnavailable
        let c = composer(chat, access)
        c.attach([fileURL("a.png")])
        await c.send()
        XCTAssertFalse(c.canAttach)
    }

    func testNothingStagedChangesWhilePreparing() async {
        let access = FakeFileAccess(["a.png": ok, "b.png": ok])
        let chat = FakeChat()
        chat.local = true
        let gate = Gate()
        chat.filesGate = gate
        let c = composer(chat, access)
        c.attach([fileURL("a.png")])
        let sending = Task { await c.send() }
        for _ in 0 ..< 20 { await Task.yield(); try? await Task.sleep(for: .milliseconds(5)) }
        XCTAssertTrue(c.preparing)
        XCTAssertFalse(c.canAttach)
        XCTAssertFalse(c.canSend)
        c.remove(c.staged[0])
        c.attach([fileURL("b.png")])
        XCTAssertEqual(c.staged.map(\.name), ["a.png"], "the staged files changed during the enqueue")
        XCTAssertTrue(access.stops.withLock { $0 }.isEmpty)
        gate.open()
        await sending.value
        XCTAssertFalse(c.preparing)
    }

    func testTheSameFilesAfterAFailureKeepTheIdAndARemovalGetsANewOne() async {
        let access = FakeFileAccess(["a.png": ok, "b.png": ok])
        let chat = FakeChat()
        chat.local = true
        chat.queueFailure = LoginError.Api(code: "outbox.store", message: "")
        let c = composer(chat, access)
        c.attach([fileURL("a.png"), fileURL("b.png")])
        await c.send()
        await c.send()
        c.remove(c.staged[1])
        await c.send()
        let ids = chat.queuedFiles.withLock { $0 }.map { String($0.split(separator: "|")[0]) }
        XCTAssertEqual(ids[0], ids[1], "a retry of the same message got a new id")
        XCTAssertNotEqual(ids[1], ids[2], "a removed file kept the id")
    }

    func testNotWhileEditing() {
        let chat = FakeChat()
        chat.local = true
        let c = composer(chat, FakeFileAccess([:]))
        c.edit(msg("m1", "text"))
        XCTAssertFalse(c.canAttach)
    }
}

/// Bubbles for messages with files (spec §3).
@MainActor
final class PendingFilesTests: XCTestCase {
    private func file(_ id: UInt64, _ name: String, uploaded: Bool = false, error: String? = nil) -> FfiPendingFile {
        FfiPendingFile(fileClientId: "f\(id)", transferId: id, filename: name, size: 10, uploaded: uploaded, error: error)
    }

    func testFileFailuresReadAsGtk() {
        let cases: [(String, String)] = [
            ("transfer.cancelled", "Cancelled"),
            ("outbox.snapshot_damaged", "Not sent: a file's saved copy is damaged"),
            ("file.quota_exceeded", "Not sent: a file was refused"),
            ("outbox.duplicate_file", "Not sent: a file was refused"),
        ]
        for (code, text) in cases {
            XCTAssertEqual(PendingModel.text(pendingMessage("q", .failed(code: code))), text, code)
        }
        XCTAssertEqual(PendingModel.fileErrorText("file.too_large"), "Too large for the server")
        XCTAssertEqual(PendingModel.fileErrorText("file.bad_content_type"), "The server refused its type")
    }

    func testFileLinesFollowProgressAndCancelStopsTheMessage() async {
        let chat = FakeChat()
        chat.local = true
        let p = PendingModel(channelId: "c", client: chat)
        XCTAssertEqual(p.fileLine(file(7, "a.png")), "a.png")
        p.transferred(FfiTransferEvent(transferId: 7, done: 50, total: 200, state: .running))
        XCTAssertEqual(p.fileLine(file(7, "a.png")), "a.png: 25%")
        XCTAssertEqual(p.fileLine(file(7, "a.png", uploaded: true)), "a.png: uploaded")
        XCTAssertEqual(p.fileLine(file(8, "b.png", error: "file.quota_exceeded")), "b.png: Over your storage quota")
        var uploading = pendingMessage("q", .sending)
        uploading.files = [file(7, "a.png"), file(8, "b.png")]
        XCTAssertEqual(PendingModel.actions(uploading), [.cancel])
        XCTAssertEqual(PendingModel.actions(pendingMessage("t", .sending)), [], "a text message offered Cancel")
        await p.perform(.cancel, on: uploading)
        XCTAssertEqual(chat.cancelled.withLock { $0 }, [7])
    }
}
