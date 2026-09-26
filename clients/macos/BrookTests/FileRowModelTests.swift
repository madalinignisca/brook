import BrookCore
import CoreGraphics
import Synchronization
import XCTest

@testable import Brook

/// The attachment row: Open, keep offline, previews (spec 2026-09-26-mac-file-row §4).
@MainActor
final class FileRowModelTests: XCTestCase {
    private func info(_ type: String = "application/pdf", size: UInt64 = 1000) -> FfiFileInfo {
        FfiFileInfo(id: "f1", filename: "report.pdf", originalName: "Report.pdf", size: size,
                    contentType: type, sha256: "ab")
    }

    private final class Opened: @unchecked Sendable { var urls: [URL] = []; var result = true }
    private final class Decoded: @unchecked Sendable { var calls = 0; var image: CGImage? }

    private func model(_ chat: FakeChat, file: FfiFileInfo? = nil, exists: Bool = true, expensive: Bool = false,
                       opened: Opened = Opened(), decoded: Decoded = Decoded()) -> FileRowModel {
        FileRowModel(file: file ?? info(), client: chat,
                     opener: { opened.urls.append($0); return opened.result },
                     exists: { _ in exists }, expensive: { expensive },
                     decode: { _, _ in decoded.calls += 1; return decoded.image })
    }

    private func local() -> FakeChat {
        let chat = FakeChat()
        chat.local = true
        return chat
    }

    private static let image: CGImage = {
        let ctx = CGContext(data: nil, width: 2, height: 2, bitsPerComponent: 8, bytesPerRow: 8,
                            space: CGColorSpace(name: CGColorSpace.sRGB)!,
                            bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
        return ctx.makeImage()!
    }()

    private func imageBytes() -> FfiImagePreview {
        FfiImagePreview(kind: .png, width: 2, height: 2, bytes: Data([1, 2, 3]))
    }

    // ---- Open ----

    func testOpenOpensExactlyThePathCoreReturned() async {
        let chat = local()
        let opened = Opened()
        let m = model(chat, opened: opened)
        await m.reloadKeep()
        await m.open()
        XCTAssertEqual(opened.urls, [URL(fileURLWithPath: "/private/brook/open/abc/report.pdf")])
        XCTAssertNil(m.message)
    }

    func testOpenErrorsSayWhatToDo() async {
        let cases: [(String, String)] = [
            ("file.open_refused", "Can't be opened from Brook. Save it instead"),
            ("file.unknown", "Not available yet. Try again in a moment"),
            ("file.gone", "No longer available."),
        ]
        for (code, text) in cases {
            let chat = local()
            chat.openResult = .failure(LoginError.Api(code: code, message: ""))
            let m = model(chat)
            await m.reloadKeep()
            await m.open()
            XCTAssertEqual(m.message, text, code)
            XCTAssertEqual(m.gone, code == "file.gone", code)
        }
    }

    func testAGoneCopyAndNoAppAreSaid() async {
        let chat = local()
        let gone = model(chat, exists: false)
        await gone.reloadKeep()
        await gone.open()
        XCTAssertEqual(gone.message, "Not available yet. Try again in a moment")
        let opened = Opened()
        opened.result = false
        let noApp = model(local(), opened: opened)
        await noApp.reloadKeep()
        await noApp.open()
        XCTAssertEqual(noApp.message, "No app opens this. Save it instead")
    }

    func testWithoutLocalDataThereIsNoOpenNoToggleNoPreview() async {
        let chat = FakeChat() // every call: local.unavailable
        let opened = Opened()
        let m = model(chat, file: info("image/png"), opened: opened)
        await m.reloadKeep()
        XCTAssertFalse(m.hasLocalData)
        await m.open()
        await m.startPreview()
        XCTAssertTrue(opened.urls.isEmpty)
        guard case .none = m.preview else { return XCTFail("a preview without local data") }
    }

    // ---- Keep available offline ----

    func testTheToggleShowsTheCachesThreeStates() {
        XCTAssertEqual(FileRowModel.view(.notCached), .off)
        XCTAssertEqual(FileRowModel.view(.cached), .off)
        XCTAssertEqual(FileRowModel.view(.pinned(cached: false, done: 0, size: 9, transfer: 4)), .fetching(4))
        XCTAssertEqual(FileRowModel.view(.pinned(cached: true, done: 9, size: 9, transfer: nil)), .kept)
    }

    func testOneToggleAtATimeThenTheCachesWord() async {
        let chat = local()
        let gate = Gate()
        chat.pinGate = gate
        chat.states = [.notCached, .pinned(cached: false, done: 0, size: 9, transfer: nil)]
        let m = model(chat)
        await m.reloadKeep()
        let first = Task { await m.toggleKeep() }
        for _ in 0 ..< 20 { await Task.yield() }
        XCTAssertTrue(m.keepBusy)
        await m.toggleKeep() // a second click while busy: ignored
        gate.open()
        await first.value
        XCTAssertEqual(chat.pins.withLock { $0 }, ["pin"])
        XCTAssertEqual(m.keep, .fetching(nil))
        XCTAssertFalse(m.keepBusy)
    }

    func testAFailedToggleSaysWhyAndTakesTheCachesStateNotAGuess() async {
        let chat = local()
        chat.pinFailure = LoginError.Api(code: "file.unknown", message: "")
        // Meanwhile the file became kept (another device's pin landing, a Files event).
        chat.states = [.notCached, .pinned(cached: true, done: 9, size: 9, transfer: nil)]
        let m = model(chat)
        await m.reloadKeep()
        await m.toggleKeep()
        XCTAssertEqual(m.message, "Not available yet. Try again in a moment")
        XCTAssertEqual(m.keep, .kept, "the toggle was set back over a newer state")
    }

    func testAnOlderReadNeverOverwritesANewerOne() async {
        let chat = local()
        let gate = Gate()
        chat.stateGate = gate
        chat.states = [.notCached, .pinned(cached: true, done: 9, size: 9, transfer: nil)]
        let m = model(chat)
        let older = Task { await m.reloadKeep() } // held, answers notCached
        for _ in 0 ..< 20 { await Task.yield() }
        await m.reloadKeep() // newer: kept
        gate.open()
        await older.value
        XCTAssertEqual(m.keep, .kept)
    }

    func testAFilesEventRereadsRegisteredRowsAndGoneRowsDropOut() async {
        let chat = local()
        chat.states = [.notCached, .pinned(cached: true, done: 9, size: 9, transfer: nil)]
        let feed = CacheFeed(client: chat)
        let m = model(chat)
        feed.register(m)
        await m.reloadKeep()
        feed.handle(.files(ids: ["f1"]))
        for _ in 0 ..< 20 { await Task.yield(); try? await Task.sleep(for: .milliseconds(5)) }
        XCTAssertEqual(m.keep, .kept)
        var gone: FileRowModel? = model(chat)
        feed.register(gone!)
        XCTAssertEqual(feed.registeredRows, 2)
        gone = nil
        XCTAssertEqual(feed.registeredRows, 1, "a row that went stayed registered")
        _ = gone
    }

    // ---- Previews ----

    func testOnlyDeclaredImagesUpTo16MiBArePreviewable() {
        let chat = local()
        XCTAssertTrue(model(chat, file: info("image/png")).previewable)
        XCTAssertTrue(model(chat, file: info("image/jpeg; charset=binary")).previewable)
        XCTAssertFalse(model(chat, file: info("application/pdf")).previewable)
        XCTAssertFalse(model(chat, file: info("image/svg+xml")).previewable)
        XCTAssertFalse(model(chat, file: info("image/png", size: previewMaxBytes() + 1)).previewable)
    }

    func testSmallImagesPreviewByThemselvesAndLargerOnesWaitForAClick() async {
        let decoded = Decoded()
        decoded.image = Self.image
        let chat = local()
        chat.previewResult = .success(imageBytes())
        let small = model(chat, file: info("image/png", size: 1000), decoded: decoded)
        await small.reloadKeep()
        await small.startPreview()
        guard case .shown = small.preview else { return XCTFail("a small image didn't preview") }
        let large = model(chat, file: info("image/png", size: FileRowModel.autoMaxBytes + 1), decoded: decoded)
        await large.reloadKeep()
        await large.startPreview()
        guard case .offer = large.preview else { return XCTFail("a large image fetched by itself") }
        await large.showPreview()
        guard case .shown = large.preview else { return XCTFail("Show preview didn't show it") }
    }

    func testOnAnExpensiveConnectionOnlyAFileAlreadyHerePreviewsByItself() async {
        let decoded = Decoded()
        decoded.image = Self.image
        let chat = local()
        chat.previewResult = .success(imageBytes())
        chat.states = [.notCached]
        let away = model(chat, file: info("image/png"), expensive: true, decoded: decoded)
        await away.reloadKeep()
        await away.startPreview()
        guard case .offer = away.preview else { return XCTFail("fetched on an expensive connection") }
        chat.states = [.cached]
        let here = model(chat, file: info("image/png"), expensive: true, decoded: decoded)
        await here.reloadKeep()
        await here.startPreview()
        guard case .shown = here.preview else { return XCTFail("a cached image didn't preview") }
    }

    func testAnyFailureMeansNoPreview() async {
        let chat = local() // previewFile refuses
        let decoded = Decoded()
        let m = model(chat, file: info("image/png"), decoded: decoded)
        await m.reloadKeep()
        await m.startPreview()
        guard case .none = m.preview else { return XCTFail("a refused file showed something") }
        XCTAssertEqual(decoded.calls, 0)
        chat.previewResult = .success(imageBytes()) // bytes, but the decoder gives nothing
        let n = model(chat, file: info("image/png"), decoded: decoded)
        await n.reloadKeep()
        await n.startPreview()
        guard case .none = n.preview else { return XCTFail("a failed decode showed something") }
    }
}
